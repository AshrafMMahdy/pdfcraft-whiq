//! The White Hat IQ craft-engine server (`pdfcraft-cli mcp` in this fork): MCP over stdio with the
//! "no dead stops" contract the platform's supervisor expects from its craft engines
//! (CONTRACT-PARITY.md in the WordCraft, GridCraft and DeckCraft forks; WHIQ-CHANGES.md here).
//!
//! - One worker thread owns the [`Automation`] session and runs one call at a time; the stdio
//!   loop reads requests on its own thread and never waits on it. `server_status`, `job_status`,
//!   `list_jobs` and `cancel` are answered at once, even mid-call.
//! - A call arriving while another runs waits at most `--busy-after-ms` (300), then answers
//!   `busy` with `retry_after_ms`, `running` and the running job's id.
//! - A call still running after `--job-after-ms` (15000) answers `{job_id, status: "running"}` and
//!   carries on; `job_status` reports its progress and keeps its result. `async: true` on any tool,
//!   or opening a file of `--open-job-mb` (20) or more, is a job at once.
//! - Unsaved work is never lost: checkpoints every `--checkpoint-edits` edits or
//!   `--checkpoint-secs` seconds, and on `evict`, idle (`--idle-evict-secs`), memory
//!   (`--max-memory-mb`, after a call, never during one), hang-up and SIGTERM every dirty
//!   document is written to `<name>.recovered-<stamp>.pdf` in the write root.
//! - Errors are results: `isError`, `structuredContent.error = {code, reason, next_step, …}` with
//!   one of eighteen codes; protocol errors carry the same object in `error.data`.

pub mod contract;
mod convert;
pub mod defs;
mod edit;
mod engine;
mod preview;
mod read;

use std::collections::BTreeMap;
use std::io::{BufRead, Write};
use std::path::PathBuf;
use std::sync::atomic::Ordering;
use std::sync::mpsc;
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use base64::Engine as _;
use serde_json::{Map, Value, json};

use crate::Automation;
use contract::{WhiqError, closest, code, normalize_args, squash, tool_alias, trim_result};
use engine::{Ctx, Engine, Reply};

/// The server's flags (`pdfcraft-cli mcp --help` names them).
#[derive(Clone, Debug)]
pub struct Settings {
    pub author: Option<String>,
    pub busy_after_ms: u64,
    pub job_after_ms: u64,
    pub open_job_bytes: u64,
    pub idle_evict_secs: Option<u64>,
    pub max_memory_mb: Option<u64>,
    pub max_result_bytes: usize,
    /// render_preview's answers carry whole pages (SVG or PNG): their own, larger budget.
    pub preview_max_bytes: usize,
    pub checkpoint_edits: u32,
    pub checkpoint_secs: u64,
    pub no_old_copies: bool,
    pub read_root: Option<PathBuf>,
    pub write_root: Option<PathBuf>,
    pub test_hooks: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            author: None,
            busy_after_ms: 300,
            job_after_ms: 15_000,
            open_job_bytes: 20 * 1024 * 1024,
            idle_evict_secs: None,
            max_memory_mb: None,
            max_result_bytes: 262_144,
            preview_max_bytes: 16 * 1024 * 1024,
            checkpoint_edits: 20,
            checkpoint_secs: 60,
            no_old_copies: false,
            read_root: None,
            write_root: None,
            test_hooks: std::env::var("PDFCRAFT_MCP_TEST_HOOKS").is_ok_and(|v| v == "1"),
        }
    }
}

pub const PROTOCOL_VERSIONS: &[&str] = &["2025-06-18", "2025-03-26", "2024-11-05"];

const INSTRUCTIONS: &str = "PdfCraft is the platform's PDF engine. read a PDF (structured text: headings, Markdown tables; OCR for scanned pages), \
search it (text_find positions), list_content (comments, fields, bookmarks, images, links, attachments), render_preview pages (SVG with a selectable \
text layer, or PNG), edit it in batches (pages, forms, comments, redaction, watermarks, text, images, metadata, bookmarks), convert images / HTML / \
Markdown / text to PDF, combine PDFs and ocr a scan into a searchable PDF. Documents are named by handles (doc-N). Every error names a next step.";

struct Job {
    tool: String,
    started: Instant,
    finished: Option<Instant>,
    status: &'static str,
    /// The `tools/call` result of a finished call.
    result: Option<Value>,
    handed_off: bool,
    ctx: Ctx,
}

struct State {
    running: Option<u64>,
    jobs: BTreeMap<u64, Job>,
    next: u64,
    calls: u64,
    busy_replies: u64,
    last_activity: Instant,
    snapshot: Value,
    shutting_down: bool,
    /// The worker has scanned the write root and published its first status.
    ready: bool,
    /// Calls waiting for the worker, in the order they arrived: they run in that order.
    queue: std::collections::VecDeque<u64>,
    next_ticket: u64,
}

struct Shared {
    state: Mutex<State>,
    cv: Condvar,
    out: Mutex<Box<dyn Write + Send>>,
    /// Writing an answer failed (stdout closed or full): the server stops with an error.
    out_failed: std::sync::atomic::AtomicBool,
    settings: Arc<Settings>,
    started: Instant,
}

enum Work {
    Call { job: u64, name: String, args: Map<String, Value>, ctx: Ctx },
    Evict(String),
    Shutdown(String),
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

impl Shared {
    fn send(&self, v: &Value) {
        let mut out = lock(&self.out);
        if writeln!(out, "{v}").and_then(|()| out.flush()).is_err() {
            self.out_failed.store(true, Ordering::SeqCst);
        }
    }

    fn reply(&self, id: &Value, result: Value) {
        self.send(&json!({"jsonrpc": "2.0", "id": id, "result": result}));
    }
}

fn job_name(n: u64) -> String {
    format!("job-{n}")
}

fn parse_job(v: &Value) -> Option<u64> {
    match v {
        Value::Number(n) => n.as_u64(),
        Value::String(s) => {
            let t = s.trim().to_ascii_lowercase();
            let t = t.strip_prefix("job").unwrap_or(&t).trim_start_matches(['-', '_', ' ']);
            t.parse().ok()
        }
        _ => None,
    }
}

fn retry_after(elapsed_ms: u64) -> u64 {
    (elapsed_ms / 4).clamp(500, 5000)
}

fn rss_bytes() -> Option<u64> {
    memory_stats::memory_stats().map(|m| m.physical_mem as u64)
}

fn committed_bytes() -> Option<u64> {
    memory_stats::memory_stats().map(|m| m.virtual_mem as u64)
}

fn memory_source() -> &'static str {
    if cfg!(target_os = "linux") {
        "linux /proc/self"
    } else if cfg!(windows) {
        "windows GetProcessMemoryInfo"
    } else if cfg!(target_os = "macos") {
        "macos task_info"
    } else {
        "os"
    }
}

/// A successful `tools/call` result: the JSON as text and `structuredContent`, images after it.
fn ok_result(v: &Value, images: &[Vec<u8>]) -> Value {
    let mut content = vec![json!({"type": "text", "text": v.to_string()})];
    for png in images {
        content.push(json!({"type": "image", "data": base64::engine::general_purpose::STANDARD.encode(png), "mimeType": "image/png"}));
    }
    json!({"content": content, "structuredContent": v})
}

fn progress_fields(j: &Job) -> Value {
    let p = lock(&j.ctx.progress).clone();
    let done = j.status != "running";
    let frac = if done {
        1.0
    } else if p.total > 0 {
        (p.done as f64 / p.total as f64).min(0.99)
    } else {
        0.0
    };
    json!({"progress": frac, "stage": if p.stage.is_empty() { j.tool.clone() } else { p.stage.clone() }, "steps": {"done": p.done, "total": p.total}, "stages": p.stages})
}

fn job_summary(id: u64, j: &Job) -> Value {
    let elapsed = j.finished.unwrap_or_else(Instant::now).duration_since(j.started).as_millis() as u64;
    let mut v = json!({"job_id": job_name(id), "tool": j.tool, "status": j.status, "elapsed_ms": elapsed});
    if let (Some(o), Some(p)) = (v.as_object_mut(), progress_fields(j).as_object()) {
        for (k, x) in p {
            o.insert(k.clone(), x.clone());
        }
    }
    v
}

/// Serve until `input` ends. `make` builds the automation session on the worker thread.
pub fn serve(
    make: impl FnOnce() -> Automation + Send + 'static,
    settings: Settings,
    input: impl BufRead,
    output: impl Write + Send + 'static,
) -> std::io::Result<()> {
    let settings = Arc::new(settings);
    let shared = Arc::new(Shared {
        state: Mutex::new(State {
            running: None,
            jobs: BTreeMap::new(),
            next: 1,
            calls: 0,
            busy_replies: 0,
            last_activity: Instant::now(),
            snapshot: json!({"documents": [], "checkpoints": {}, "evicted": [], "recovered_files": []}),
            shutting_down: false,
            ready: false,
            queue: std::collections::VecDeque::new(),
            next_ticket: 1,
        }),
        cv: Condvar::new(),
        out: Mutex::new(Box::new(output)),
        out_failed: std::sync::atomic::AtomicBool::new(false),
        settings: settings.clone(),
        started: Instant::now(),
    });
    let (tx, rx) = mpsc::channel::<Work>();
    let worker = {
        let shared = shared.clone();
        let settings = settings.clone();
        std::thread::Builder::new().name("pdfcraft-whiq-worker".into()).spawn(move || worker_loop(make(), settings, &shared, &rx))?
    };
    // Requests are read once the worker has looked for recovered files (server_status names them).
    {
        let mut st = lock(&shared.state);
        while !st.ready {
            st = shared.cv.wait_timeout(st, Duration::from_millis(50)).map_or_else(|e| e.into_inner().0, |r| r.0);
        }
    }
    // SIGTERM / SIGINT / SIGHUP (Ctrl-C, Ctrl-Break, console close on Windows): unsaved work is
    // auto-saved, then the process exits with 143.
    {
        let tx = Mutex::new(tx.clone());
        let _ = ctrlc::set_handler(move || {
            let _ = lock(&tx).send(Work::Shutdown("signal".into()));
        });
    }
    // Idle eviction.
    if let Some(secs) = settings.idle_evict_secs.filter(|s| *s > 0) {
        let shared = shared.clone();
        let tx = tx.clone();
        let _ = std::thread::Builder::new().name("pdfcraft-whiq-idle".into()).spawn(move || {
            loop {
                std::thread::sleep(Duration::from_secs(1));
                let st = lock(&shared.state);
                if st.shutting_down {
                    return;
                }
                let has_docs = st.snapshot.get("documents").and_then(Value::as_array).is_some_and(|d| !d.is_empty());
                if st.running.is_none() && has_docs && st.last_activity.elapsed().as_secs() >= secs {
                    drop(st);
                    let _ = tx.send(Work::Evict("idle".into()));
                    let mut st = lock(&shared.state);
                    st.last_activity = Instant::now();
                }
            }
        });
    }
    let mut buf = String::new();
    let mut input = input;
    loop {
        buf.clear();
        match input.read_line(&mut buf) {
            Ok(0) => break,
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::InvalidData => {
                shared.send(&protocol_error(
                    &Value::Null,
                    -32700,
                    WhiqError::new(code::INVALID_REQUEST, "the line is not UTF-8", "send JSON-RPC 2.0 requests, one per line"),
                ));
                continue;
            }
            Err(e) => return Err(e),
        }
        let line = buf.trim();
        if line.is_empty() {
            continue;
        }
        let msg: Value = match serde_json::from_str(line) {
            Ok(v) => v,
            Err(e) => {
                shared.send(&protocol_error(
                    &Value::Null,
                    -32700,
                    WhiqError::new(code::INVALID_REQUEST, format!("the line is not JSON: {e}"), "send JSON-RPC 2.0 requests, one per line"),
                ));
                continue;
            }
        };
        handle_message(&shared, &tx, msg);
        if shared.out_failed.load(Ordering::SeqCst) {
            break;
        }
    }
    // Hang-up: the calls that arrived before it are served, then unsaved work is auto-saved and
    // the worker stops.
    {
        let mut st = lock(&shared.state);
        while st.running.is_some() || !st.queue.is_empty() {
            st = shared.cv.wait_timeout(st, Duration::from_millis(200)).map_or_else(|e| e.into_inner().0, |r| r.0);
        }
        st.shutting_down = true;
    }
    let _ = tx.send(Work::Shutdown("disconnect".into()));
    let _ = worker.join();
    if shared.out_failed.load(Ordering::SeqCst) {
        return Err(std::io::Error::other("an answer could not be written to stdout"));
    }
    Ok(())
}

fn protocol_error(id: &Value, rpc_code: i64, e: WhiqError) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "error": {"code": rpc_code, "message": e.text(&[]), "data": e.object()}})
}

fn handle_message(shared: &Arc<Shared>, tx: &mpsc::Sender<Work>, msg: Value) {
    let id = msg.get("id").cloned();
    let method = msg.get("method").and_then(Value::as_str).unwrap_or("");
    let Some(id) = id else {
        // A notification: nothing to answer. `notifications/cancelled` lets the call carry on as a
        // job (its answer kept for job_status); `cancel` is what stops work.
        return;
    };
    match method {
        "initialize" => {
            let asked = msg.pointer("/params/protocolVersion").and_then(Value::as_str).unwrap_or(PROTOCOL_VERSIONS[0]);
            let version = PROTOCOL_VERSIONS.iter().find(|v| **v == asked).copied().unwrap_or(PROTOCOL_VERSIONS[0]);
            let recovered = lock(&shared.state).snapshot.get("recovered_files").cloned().unwrap_or(json!([]));
            let mut instructions = INSTRUCTIONS.to_string();
            if recovered.as_array().is_some_and(|a| !a.is_empty()) {
                instructions.push_str(&format!(" Recovered files from an earlier process: {recovered}."));
            }
            shared.reply(
                &id,
                json!({"protocolVersion": version, "capabilities": {"tools": {"listChanged": false}}, "serverInfo": {"name": "pdfcraft", "version": env!("CARGO_PKG_VERSION")}, "instructions": instructions}),
            );
        }
        "ping" => shared.reply(&id, json!({})),
        "tools/list" => shared.reply(&id, json!({"tools": defs::definitions()})),
        "resources/list" => shared.reply(&id, json!({"resources": []})),
        "resources/templates/list" => shared.reply(&id, json!({"resourceTemplates": []})),
        "prompts/list" => shared.reply(&id, json!({"prompts": []})),
        "tools/call" => {
            let params = msg.get("params").cloned().unwrap_or(Value::Null);
            let Some(raw) = params.get("name").and_then(Value::as_str) else {
                shared.send(&protocol_error(
                    &id,
                    -32602,
                    WhiqError::new(code::INVALID_REQUEST, "tools/call needs a name", "send params {name, arguments}"),
                ));
                return;
            };
            let token = params.pointer("/_meta/progressToken").cloned();
            let args = match params.get("arguments") {
                None | Some(Value::Null) => Map::new(),
                Some(Value::Object(o)) => o.clone(),
                Some(other) => {
                    shared.reply(
                        &id,
                        WhiqError::invalid(format!("arguments must be an object, not {other}"), "pass arguments as a JSON object").result(&[]),
                    );
                    return;
                }
            };
            call(shared, tx, id, raw, args, token);
        }
        other => shared.send(&protocol_error(
            &id,
            -32601,
            WhiqError::new(code::INVALID_REQUEST, format!("unknown method {other:?}"), "use initialize, ping, tools/list or tools/call"),
        )),
    }
}

fn call(shared: &Arc<Shared>, tx: &mpsc::Sender<Work>, id: Value, raw: &str, args: Map<String, Value>, token: Option<Value>) {
    let names = defs::names();
    let sq = squash(raw);
    let name = if names.contains(&sq) {
        sq
    } else if let Some(a) = tool_alias(&sq) {
        a.to_string()
    } else if shared.settings.test_hooks && sq == "__sleep" {
        sq
    } else {
        let near = closest(raw, names.iter().map(String::as_str), 5);
        shared.reply(
            &id,
            WhiqError::new(code::NOT_FOUND, format!("no tool named {raw:?}"), "tools/list names every tool").with("closest", json!(near)).result(&[]),
        );
        return;
    };
    let (args, warnings) = match defs::find(&name) {
        Some(def) => normalize_args(def.get("inputSchema").unwrap_or(&Value::Null), &args),
        None => (args, Vec::new()),
    };
    lock(&shared.state).calls += 1;
    match name.as_str() {
        "server_status" => return shared.reply(&id, with_warnings(ok_result(&server_status(shared), &[]), &warnings)),
        "list_jobs" => {
            let st = lock(&shared.state);
            let jobs: Vec<Value> = st.jobs.iter().map(|(k, j)| job_summary(*k, j)).collect();
            drop(st);
            return shared.reply(&id, ok_result(&json!({"jobs": jobs}), &[]));
        }
        "job_status" => return shared.reply(&id, job_status(shared, args.get("job_id"))),
        "cancel" => return shared.reply(&id, cancel(shared, args.get("job_id"))),
        _ => {}
    }
    // A ticket in arrival order: pipelined calls run in the order they were sent.
    let ticket = {
        let mut st = lock(&shared.state);
        let t = st.next_ticket;
        st.next_ticket += 1;
        st.queue.push_back(t);
        t
    };
    let shared = shared.clone();
    let tx = tx.clone();
    let spawned = {
        let shared = shared.clone();
        let id = id.clone();
        std::thread::Builder::new()
            .name("pdfcraft-whiq-call".into())
            .spawn(move || run_call(&shared, &tx, CallReq { id, ticket, name, args, warnings, token }))
    };
    if spawned.is_err() {
        lock(&shared.state).queue.retain(|t| *t != ticket);
        shared.cv.notify_all();
        shared.reply(&id, WhiqError::new(code::INTERNAL, "no thread could be started for the call", "retry the call").result(&[]));
    }
}

fn with_warnings(mut r: Value, warnings: &[String]) -> Value {
    if !warnings.is_empty() {
        if let Some(sc) = r.get_mut("structuredContent").and_then(Value::as_object_mut) {
            sc.insert("warnings".into(), json!(warnings));
        }
        if let Some(t) = r.pointer_mut("/content/0/text")
            && let Some(s) = t.as_str()
        {
            *t = json!(format!("{s}\nwarnings: {}", warnings.join("; ")));
        }
    }
    r
}

/// `p` as a path inside the read or write root, by name (no `..`, nothing looked up on disk), or
/// `None`: only such a file is measured before the worker resolves it properly.
fn resolve_inside_roots(settings: &Settings, p: &str) -> Option<PathBuf> {
    let path = std::path::Path::new(p);
    if path.components().any(|c| matches!(c, std::path::Component::ParentDir)) {
        return None;
    }
    let roots = [&settings.read_root, &settings.write_root];
    if path.is_absolute() {
        return roots.into_iter().flatten().any(|r| path.starts_with(r)).then(|| path.to_path_buf());
    }
    settings.read_root.as_ref().map(|r| r.join(path))
}

fn starts_as_job(settings: &Settings, name: &str, args: &Map<String, Value>) -> bool {
    if args.get("async").and_then(Value::as_bool) == Some(true) {
        return true;
    }
    if matches!(name, "open_document" | "read" | "render_preview" | "ocr" | "document_info" | "search" | "list_content")
        && let Some(full) = args.get("path").and_then(Value::as_str).and_then(|p| resolve_inside_roots(settings, p))
        && std::fs::metadata(&full).is_ok_and(|m| m.len() >= settings.open_job_bytes)
    {
        return true;
    }
    false
}

/// A `tools/call` on its way to the worker.
struct CallReq {
    id: Value,
    ticket: u64,
    name: String,
    args: Map<String, Value>,
    warnings: Vec<String>,
    token: Option<Value>,
}

fn run_call(shared: &Arc<Shared>, tx: &mpsc::Sender<Work>, req: CallReq) {
    let CallReq { id, ticket, name, args, warnings, token } = req;
    let id = &id;
    let ctx = Ctx::new();
    let job = {
        let mut st = lock(&shared.state);
        let deadline = Instant::now() + Duration::from_millis(shared.settings.busy_after_ms);
        while st.running.is_some() || st.queue.front() != Some(&ticket) || st.shutting_down {
            let now = Instant::now();
            if now >= deadline || st.shutting_down {
                st.busy_replies += 1;
                st.queue.retain(|t| *t != ticket);
                shared.cv.notify_all();
                let running = st.running.and_then(|r| st.jobs.get(&r).map(|j| (r, j)));
                let (elapsed, detail) = running.map_or((0, Value::Null), |(r, j)| {
                    (
                        j.started.elapsed().as_millis() as u64,
                        json!({"job_id": job_name(r), "tool": j.tool, "elapsed_ms": j.started.elapsed().as_millis() as u64}),
                    )
                });
                let mut e = WhiqError::new(
                    code::BUSY,
                    if st.shutting_down {
                        "the server is shutting down".to_string()
                    } else {
                        format!("another call is running ({})", detail.get("tool").and_then(Value::as_str).unwrap_or("?"))
                    },
                    "retry the same call after retry_after_ms; job_status shows the running call",
                )
                .with("retry_after_ms", retry_after(elapsed))
                .with("running", detail.clone());
                if let Some(j) = detail.get("job_id") {
                    e = e.with("job_id", j.clone());
                }
                drop(st);
                shared.reply(id, e.result(&warnings));
                return;
            }
            st = shared.cv.wait_timeout(st, deadline - now).map_or_else(|e| e.into_inner().0, |r| r.0);
        }
        st.queue.pop_front();
        let job = st.next;
        st.next += 1;
        st.running = Some(job);
        st.last_activity = Instant::now();
        st.jobs.insert(
            job,
            Job { tool: name.clone(), started: Instant::now(), finished: None, status: "running", result: None, handed_off: false, ctx: ctx.clone() },
        );
        // Finished jobs nobody asked about are kept (job_status), but not forever.
        while st.jobs.len() > 500 {
            let Some(oldest) = st.jobs.iter().find(|(_, j)| j.status != "running").map(|(k, _)| *k) else { break };
            st.jobs.remove(&oldest);
        }
        job
    };
    let as_job = starts_as_job(&shared.settings, &name, &args);
    if tx.send(Work::Call { job, name: name.clone(), args, ctx }).is_err() {
        let mut st = lock(&shared.state);
        st.running = None;
        st.jobs.remove(&job);
        drop(st);
        shared.cv.notify_all();
        shared.reply(
            id,
            WhiqError::new(code::INTERNAL, "the engine's worker has stopped", "the platform restarts the engine; call again").result(&warnings),
        );
        return;
    }
    let started = Instant::now();
    let job_after = Duration::from_millis(shared.settings.job_after_ms);
    let mut beats = 0u64;
    let mut st = lock(&shared.state);
    loop {
        if let Some(j) = st.jobs.get_mut(&job)
            && j.status != "running"
        {
            let result = j.result.take().unwrap_or_else(|| ok_result(&json!({}), &[]));
            st.jobs.remove(&job);
            drop(st);
            shared.reply(id, with_warnings(result, &warnings));
            return;
        }
        if as_job || started.elapsed() >= job_after {
            let Some(j) = st.jobs.get_mut(&job) else { return };
            j.handed_off = true;
            let elapsed = j.started.elapsed().as_millis() as u64;
            let mut v = json!({
                "job_id": job_name(job), "status": "running", "tool": name, "elapsed_ms": elapsed, "retry_after_ms": retry_after(elapsed),
                "next_step": format!("the call carries on as {}: call job_status with job_id {} after retry_after_ms for its progress and result", job_name(job), job_name(job)),
            });
            if let (Some(o), Some(p)) = (v.as_object_mut(), progress_fields(j).as_object()) {
                for (k, x) in p {
                    o.insert(k.clone(), x.clone());
                }
            }
            drop(st);
            shared.reply(id, with_warnings(ok_result(&v, &[]), &warnings));
            return;
        }
        let wait = Duration::from_millis(250).min(job_after.saturating_sub(started.elapsed()));
        st = shared.cv.wait_timeout(st, wait).map_or_else(|e| e.into_inner().0, |r| r.0);
        if let Some(tok) = &token
            && started.elapsed().as_secs() > beats
        {
            beats = started.elapsed().as_secs();
            if let Some(j) = st.jobs.get(&job) {
                let p = lock(&j.ctx.progress).clone();
                let progress = p.done.max(beats);
                let mut params = json!({"progressToken": tok, "progress": progress, "message": format!("{} ({} s)", if p.stage.is_empty() { name.as_str() } else { p.stage.as_str() }, beats)});
                if p.total > 0 && p.done >= beats {
                    params["total"] = json!(p.total);
                }
                shared.send(&json!({"jsonrpc": "2.0", "method": "notifications/progress", "params": params}));
            }
        }
    }
}

fn job_status(shared: &Shared, id: Option<&Value>) -> Value {
    let st = lock(&shared.state);
    let Some(v) = id.filter(|v| !v.is_null()) else {
        let jobs: Vec<Value> = st.jobs.iter().map(|(k, j)| job_summary(*k, j)).collect();
        return ok_result(&json!({"jobs": jobs}), &[]);
    };
    let Some(n) = parse_job(v).filter(|n| st.jobs.contains_key(n)) else {
        let ids: Vec<String> = st.jobs.keys().map(|k| job_name(*k)).collect();
        return WhiqError::new(code::NOT_FOUND, format!("no job {v}"), "list_jobs names the jobs of this process")
            .with("closest", json!(ids.iter().rev().take(5).collect::<Vec<_>>()))
            .result(&[]);
    };
    let Some(j) = st.jobs.get(&n) else { return ok_result(&json!({}), &[]) };
    let mut s = job_summary(n, j);
    let mut images: Vec<Vec<u8>> = Vec::new();
    match j.status {
        "running" => {
            let elapsed = j.started.elapsed().as_millis() as u64;
            s["retry_after_ms"] = json!(retry_after(elapsed));
            s["next_step"] = json!("still running: call job_status again after retry_after_ms (cancel stops it)");
        }
        status => {
            let r = j.result.clone().unwrap_or(Value::Null);
            let sc = r.get("structuredContent").cloned().unwrap_or(Value::Null);
            if r.get("isError").and_then(Value::as_bool) == Some(true) {
                s["error"] = sc.get("error").cloned().unwrap_or(sc);
                s["next_step"] = json!(if status == "cancelled" { "the call was cancelled; nothing of it was kept" } else { "read error.next_step" });
            } else {
                s["result"] = sc;
                s["next_step"] = json!("done: the result is here");
                if let Some(content) = r.get("content").and_then(Value::as_array) {
                    for c in content.iter().filter(|c| c.get("type").and_then(Value::as_str) == Some("image")) {
                        if let Some(d) = c.get("data").and_then(Value::as_str)
                            && let Ok(png) = base64::engine::general_purpose::STANDARD.decode(d)
                        {
                            images.push(png);
                        }
                    }
                }
            }
        }
    }
    ok_result(&s, &images)
}

fn cancel(shared: &Shared, id: Option<&Value>) -> Value {
    let st = lock(&shared.state);
    let target = match id.filter(|v| !v.is_null()) {
        Some(v) => match parse_job(v).filter(|n| st.jobs.contains_key(n)) {
            Some(n) => Some(n),
            None => return WhiqError::new(code::NOT_FOUND, format!("no job {v}"), "list_jobs names the jobs of this process").result(&[]),
        },
        None => st.running,
    };
    let Some(n) = target else {
        return ok_result(&json!({"cancelled": false, "status": "not_running", "next_step": "nothing is running"}), &[]);
    };
    let Some(j) = st.jobs.get(&n) else { return ok_result(&json!({"cancelled": false, "status": "not_running"}), &[]) };
    if j.status != "running" {
        return ok_result(
            &json!({"cancelled": false, "status": j.status, "job_id": job_name(n), "tool": j.tool, "next_step": "the job had already finished: job_status has its answer"}),
            &[],
        );
    }
    j.ctx.cancel.store(true, Ordering::SeqCst);
    ok_result(
        &json!({"cancelled": true, "status": "cancelling", "job_id": job_name(n), "tool": j.tool, "retry_after_ms": 200,
                "next_step": "the call stops at its next step and leaves the document as it was before it; job_status shows when"}),
        &[],
    )
}

fn server_status(shared: &Shared) -> Value {
    let s = &shared.settings;
    let st = lock(&shared.state);
    let current = st.running.and_then(|r| st.jobs.get(&r).map(|j| job_summary(r, j)));
    let jobs: Vec<Value> = st.jobs.iter().filter(|(_, j)| j.handed_off || j.status == "running").map(|(k, j)| job_summary(*k, j)).collect();
    let snap = st.snapshot.clone();
    let (calls, busy, idle) = (st.calls, st.busy_replies, st.last_activity.elapsed().as_millis() as u64);
    drop(st);
    let faces = pdfcraft_fonts::CRAFT_FONTS.len();
    let mut families: Vec<&str> = pdfcraft_fonts::CRAFT_FONTS.iter().map(|f| f.family).collect();
    families.dedup();
    json!({
        "server": "pdfcraft",
        "version": env!("CARGO_PKG_VERSION"),
        "protocol": PROTOCOL_VERSIONS[0],
        "pid": std::process::id(),
        "uptime_ms": shared.started.elapsed().as_millis() as u64,
        "backend": "headless",
        "author": s.author,
        "memory": {"rss_bytes": rss_bytes(), "committed_bytes": committed_bytes(), "limit_bytes": s.max_memory_mb.map(|m| m * 1024 * 1024), "measured": rss_bytes().is_some(), "source": memory_source()},
        "fonts": {"source": "craft-fonts", "faces": faces, "families": families, "default": "Helvetica (standard 14)", "arabic": pdfcraft_fonts::document_arabic_font().is_some()},
        "ocr": {"models": pdfcraft_engine::ocr::available(), "languages": pdfcraft_engine::ocr::languages_installed(), "search_dirs": pdfcraft_engine::ocr::Models::search_dirs().iter().map(|d| d.to_string_lossy().into_owned()).collect::<Vec<_>>()},
        "roots": {"read": s.read_root, "write": s.write_root},
        "documents": snap.get("documents").cloned().unwrap_or(json!([])),
        "current_call": current,
        "jobs": jobs,
        "recovered_files": snap.get("recovered_files").cloned().unwrap_or(json!([])),
        "evicted": snap.get("evicted").cloned().unwrap_or(json!([])),
        "checkpoints": snap.get("checkpoints").cloned().unwrap_or(json!({})),
        "idle_ms": idle,
        "calls": calls,
        "busy_replies": busy,
        "limits": {
            "max_result_bytes": s.max_result_bytes, "preview_max_bytes": s.preview_max_bytes, "max_memory_mb": s.max_memory_mb, "busy_after_ms": s.busy_after_ms,
            "job_after_ms": s.job_after_ms, "open_job_bytes": s.open_job_bytes, "idle_evict_secs": s.idle_evict_secs, "old_copies": !s.no_old_copies,
            "checkpoint_edits": s.checkpoint_edits, "checkpoint_secs": s.checkpoint_secs,
        },
    })
}

/// Tools that page their own answers (by pages); every other answer is trimmed here.
const SELF_PAGED: &[&str] = &["read", "render_preview"];

fn skip_from(mut v: Value, from: usize) -> Value {
    if from == 0 {
        return v;
    }
    let Some(o) = v.as_object_mut() else { return v };
    let key = o.iter().filter_map(|(k, x)| x.as_array().map(|a| (k.clone(), a.len()))).max_by_key(|x| x.1).map(|x| x.0);
    if let Some(k) = key
        && let Some(Value::Array(a)) = o.get_mut(&k)
    {
        let n = from.min(a.len());
        a.drain(..n);
    }
    v
}

fn worker_loop(auto: Automation, settings: Arc<Settings>, shared: &Shared, rx: &mpsc::Receiver<Work>) {
    let mut engine = Engine::new(auto, settings.clone());
    publish(&engine, shared);
    lock(&shared.state).ready = true;
    shared.cv.notify_all();
    while let Ok(work) = rx.recv() {
        match work {
            Work::Call { job, name, args, ctx } => {
                let from = args.get("continue_from").and_then(Value::as_u64).map_or(0, |n| usize::try_from(n).unwrap_or(0));
                let max = args.get("max_bytes").and_then(Value::as_u64).and_then(|n| usize::try_from(n).ok()).unwrap_or(settings.max_result_bytes);
                let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| engine.call(&name, &args, &ctx)));
                let (status, result) = match outcome {
                    Ok(Ok(Reply { mut value, images })) => {
                        if !SELF_PAGED.contains(&name.as_str()) {
                            value = trim_result(skip_from(value, from), max, from, &name);
                        }
                        if !engine.recovered_unreported.is_empty()
                            && let Some(o) = value.as_object_mut()
                        {
                            o.insert("recovered_files".into(), json!(std::mem::take(&mut engine.recovered_unreported)));
                        }
                        ("done", ok_result(&value, &images))
                    }
                    Ok(Err(e)) => (if e.code == code::CANCELLED { "cancelled" } else { "failed" }, e.result(&[])),
                    Err(p) => {
                        // A panic is a bug: the call fails, unsaved work is written to recovered files,
                        // and the server carries on.
                        let msg = p
                            .downcast_ref::<&str>()
                            .map(ToString::to_string)
                            .or_else(|| p.downcast_ref::<String>().cloned())
                            .unwrap_or_else(|| "panic".into());
                        let saved = engine.evict_all("panic");
                        (
                            "failed",
                            WhiqError::new(
                                code::INTERNAL,
                                format!("{name} failed inside the engine: {msg}"),
                                "the open documents were auto-saved (recovered_path); reopen and retry once",
                            )
                            .with("evicted", saved)
                            .result(&[]),
                        )
                    }
                };
                // Memory over the budget: released after the call, never during one.
                let mut note = None;
                if let (Some(limit), Some(rss)) = (settings.max_memory_mb, rss_bytes())
                    && rss > limit * 1024 * 1024
                    && !engine.open_ids().is_empty()
                {
                    note = Some(engine.evict_all("memory"));
                }
                let mut result = result;
                if let Some(n) = note
                    && let Some(sc) = result.get_mut("structuredContent").and_then(Value::as_object_mut)
                {
                    sc.insert("closed_after_this_call".into(), n);
                }
                publish(&engine, shared);
                let mut st = lock(&shared.state);
                if let Some(j) = st.jobs.get_mut(&job) {
                    j.status = status;
                    j.finished = Some(Instant::now());
                    j.result = Some(result);
                }
                st.running = None;
                st.last_activity = Instant::now();
                drop(st);
                shared.cv.notify_all();
            }
            Work::Evict(reason) => {
                if lock(&shared.state).running.is_none() {
                    let _ = engine.evict_all(&reason);
                    publish(&engine, shared);
                }
            }
            Work::Shutdown(reason) => {
                let _ = engine.evict_all(&reason);
                publish(&engine, shared);
                if reason == "signal" {
                    std::process::exit(143);
                }
                return;
            }
        }
    }
}

fn publish(engine: &Engine, shared: &Shared) {
    let snap = json!({
        "documents": engine.documents_value(),
        "checkpoints": engine.checkpoints_value(),
        "evicted": engine.evicted_value(),
        "recovered_files": engine.recovered_all,
    });
    lock(&shared.state).snapshot = snap;
}
