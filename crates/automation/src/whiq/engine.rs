//! The worker side of the WHIQ server: one [`Engine`] owns the [`Automation`] session and runs
//! one call at a time. Documents are named by handles `doc-N` (N is the automation id).

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use serde_json::{Map, Value, json};

use super::Settings;
use super::contract::{WhiqError, classify, closest, code, one_based, page_spec};
use super::recovery::{DocSnap, Recovery};
use crate::{Automation, Content, ToolError};

/// What a running call reports: progress 0..1 that never goes back, the stage, steps.
#[derive(Clone, Debug, Default)]
pub struct Progress {
    pub done: u64,
    pub total: u64,
    pub stage: String,
    pub stages: Vec<String>,
    /// What the steps count (`pages`), when the progress message names it.
    pub unit: String,
}

/// Handed to each call: the cancel flag and the progress the status tools read.
#[derive(Clone)]
pub struct Ctx {
    pub cancel: Arc<AtomicBool>,
    pub progress: Arc<Mutex<Progress>>,
}

impl Ctx {
    pub fn new() -> Self {
        Ctx { cancel: Arc::new(AtomicBool::new(false)), progress: Arc::new(Mutex::new(Progress::default())) }
    }

    pub fn cancelled(&self) -> bool {
        self.cancel.load(Ordering::SeqCst)
    }

    /// Fail with `cancelled` when the call was cancelled.
    pub fn check(&self) -> Result<(), WhiqError> {
        if self.cancelled() {
            return Err(WhiqError::new(code::CANCELLED, "the call was cancelled", "nothing of it was kept; call it again when wanted"));
        }
        Ok(())
    }

    pub fn stage(&self, stage: &str, total: u64) {
        self.stage_counting(stage, total, "");
    }

    /// [`Self::stage`] whose steps are `unit`s (`pages`): the progress message says
    /// "<stage>: 3 of 10 pages done".
    pub fn stage_counting(&self, stage: &str, total: u64, unit: &str) {
        let mut p = self.progress.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        if p.stage != stage {
            p.stage = stage.to_string();
            p.stages.push(stage.to_string());
        }
        p.total = total;
        p.done = 0;
        p.unit = unit.to_string();
    }

    pub fn step(&self, done: u64) {
        let mut p = self.progress.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        p.done = done.max(p.done);
    }
}

impl Default for Ctx {
    fn default() -> Self {
        Self::new()
    }
}

/// A call's answer: the JSON and any PNG images (MCP image blocks).
#[derive(Debug, Default)]
pub struct Reply {
    pub value: Value,
    pub images: Vec<Vec<u8>>,
}

impl From<Value> for Reply {
    fn from(value: Value) -> Self {
        Reply { value, images: Vec::new() }
    }
}

pub type Res<T> = Result<T, WhiqError>;

/// A document the server released (evict, idle, memory): calls naming it answer `evicted`.
#[derive(Clone, Debug)]
pub struct Evicted {
    pub handle: String,
    pub name: String,
    pub recovered_path: Option<String>,
    pub original_path: Option<String>,
    pub reason: String,
}

/// An `ocr` call under way: the pages done (their text added) and still to read, so a cancel,
/// a failure or a panic says exactly what was kept and what remains.
#[derive(Debug, Default)]
pub(crate) struct OcrRun {
    pub(crate) doc: u64,
    /// 0-based, in the order read.
    pub(crate) done: Vec<usize>,
    pub(crate) remaining: Vec<usize>,
    pub(crate) words: usize,
    /// Per page: words found, or why it was skipped.
    pub(crate) results: Vec<Value>,
    /// A page of this run added words already (later pages join its undo step).
    pub(crate) joined: bool,
    /// Why adding a page's text failed (the run stops there).
    pub(crate) failed: Option<WhiqError>,
}

pub struct Engine {
    pub(crate) auto: Automation,
    pub(crate) settings: Arc<Settings>,
    active: Option<u64>,
    /// Files this process wrote: writing them again replaces them (no OLD_ copy of our own output).
    written: HashSet<PathBuf>,
    /// What the checkpoint timer, SIGTERM and hang-up save from, published after each call.
    pub(crate) recovery: Arc<Recovery>,
    pub(crate) evicted: Vec<Evicted>,
    /// Recovered files not yet named in an answer.
    pub(crate) recovered_unreported: Vec<Value>,
    pub(crate) recovered_all: Vec<Value>,
    pub(crate) ocr: Option<Arc<pdfcraft_engine::ocr::Ocr>>,
    /// SVG renderers per document, rebuilt when its displayed bytes change.
    pub(crate) svg: HashMap<u64, pdfcraft_render::SvgRenderer>,
    /// Who the running call is for: its `caller`, else `--author`. Comments carry it.
    pub(crate) caller: Option<String>,
    /// The documents open when the running call started, with their working files: a panic
    /// releases only those the call opened or changed.
    call_start: Vec<(u64, Arc<Vec<u8>>)>,
    /// The running `edit`'s document as it was before the batch: put back if the call panics.
    pub(crate) batch: Option<(u64, pdfcraft_engine::BatchCheckpoint)>,
    /// The running `ocr`'s pages done and remaining.
    pub(crate) ocr_run: Option<OcrRun>,
}

pub fn handle(id: u64) -> String {
    format!("doc-{id}")
}

/// `doc-3`, `3`, `"3"`, `DOC_3` → 3.
pub fn parse_handle(v: &Value) -> Option<u64> {
    match v {
        Value::Number(n) => n.as_u64(),
        Value::String(s) => {
            let t = s.trim().to_ascii_lowercase();
            let t = t.strip_prefix("doc").unwrap_or(&t).trim_start_matches(['-', '_', ' ']);
            t.parse().ok()
        }
        _ => None,
    }
}

/// UTC stamp for recovered files: `20261030-221501`.
pub fn stamp() -> String {
    now_utc().format("%Y%m%d-%H%M%S").to_string()
}

/// Now, in UTC (the system clock; chrono only formats it).
pub fn now_utc() -> chrono::DateTime<chrono::Utc> {
    let secs = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs());
    chrono::DateTime::from_timestamp(i64::try_from(secs).unwrap_or(0), 0).unwrap_or_default()
}

impl Engine {
    pub fn new(auto: Automation, settings: Arc<Settings>, recovery: Arc<Recovery>) -> Self {
        let mut e = Engine {
            auto,
            settings,
            active: None,
            written: HashSet::new(),
            recovery,
            evicted: Vec::new(),
            recovered_unreported: Vec::new(),
            recovered_all: Vec::new(),
            ocr: None,
            svg: HashMap::new(),
            caller: None,
            call_start: Vec::new(),
            batch: None,
            ocr_run: None,
        };
        e.scan_recovered();
        e
    }

    /// Recovered files an earlier process left in the write root (reason `found`).
    fn scan_recovered(&mut self) {
        let Some(root) = self.settings.write_root.clone() else { return };
        let Ok(root) = self.auto.resolve(&root.to_string_lossy(), false) else { return };
        let Ok(rd) = std::fs::read_dir(&root) else { return };
        for entry in rd.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            if name.contains(".recovered-") && name.to_ascii_lowercase().ends_with(".pdf") {
                let v = json!({"path": entry.path().to_string_lossy(), "reason": "found", "original": Value::Null, "at": now_utc().to_rfc3339()});
                self.recovered_unreported.push(v.clone());
                self.recovered_all.push(v);
            }
        }
    }

    // ---- automation bridge -------------------------------------------------------------

    /// Run an automation tool; the JSON part of its answer, errors in the contract's shape.
    pub fn tool(&mut self, name: &str, args: Value) -> Res<Value> {
        Ok(self.tool_all(name, args)?.0)
    }

    /// [`Self::tool`] with the PNG images of the answer.
    pub fn tool_all(&mut self, name: &str, args: Value) -> Res<(Value, Vec<Vec<u8>>)> {
        match self.auto.call(name, &args) {
            Ok(parts) => {
                let mut json_part = Value::Null;
                let mut images = Vec::new();
                for p in parts {
                    match p {
                        Content::Json(v) if json_part.is_null() => json_part = v,
                        Content::Json(v) => {
                            if let Some(o) = json_part.as_object_mut() {
                                o.insert("render_warnings".into(), v);
                            }
                        }
                        Content::Png { data, .. } => images.push(data),
                    }
                }
                Ok((json_part, images))
            }
            Err(e) => Err(self.tool_error(name, e)),
        }
    }

    fn tool_error(&self, name: &str, e: ToolError) -> WhiqError {
        match e {
            ToolError::UnknownTool(t) => {
                let names: Vec<&str> = crate::tools::tools().iter().map(|d| d.name).collect::<Vec<_>>();
                let near = closest(&t, names.iter().copied(), 5);
                WhiqError::new(code::NOT_FOUND, format!("no operation or tool named {t:?}"), "use list_operations to see every operation")
                    .with("closest", json!(near))
            }
            ToolError::InvalidArgs(m) => WhiqError::invalid(
                format!("{name}: {m}"),
                format!("list_operations with name: \"{name}\" gives its arguments; fix them and call again"),
            ),
            ToolError::Failed(m) => classify(&m, name),
        }
    }

    // ---- documents ---------------------------------------------------------------------

    pub fn open_ids(&self) -> Vec<u64> {
        self.auto.session.docs().iter().map(|d| d.id.0).collect()
    }

    fn doc_exists(&self, id: u64) -> bool {
        self.auto.session.get(pdfcraft_engine::DocId(id)).is_some()
    }

    pub fn doc(&self, id: u64) -> Res<&pdfcraft_engine::Document> {
        self.auto.session.get(pdfcraft_engine::DocId(id)).ok_or_else(|| {
            WhiqError::new(code::NOT_FOUND, format!("no open document {}", handle(id)), "open it with open_document")
                .with("open_documents", self.handles_value())
        })
    }

    fn handles_value(&self) -> Value {
        json!(self.open_ids().into_iter().map(handle).collect::<Vec<_>>())
    }

    /// The document a call names (`document`), the one it opens (`path`, with `path_key`), or the
    /// active one.
    pub fn target(&mut self, a: &Map<String, Value>, path_key: Option<&str>) -> Res<u64> {
        if let Some(v) = a.get("document").filter(|v| !v.is_null()) {
            let id = parse_handle(v).ok_or_else(|| {
                WhiqError::invalid(format!("{v} is not a document handle"), "pass a handle such as doc-1 (server_status lists them)")
            })?;
            if self.doc_exists(id) {
                self.active = Some(id);
                return Ok(id);
            }
            return Err(self.missing_handle(&handle(id)));
        }
        if let Some(k) = path_key
            && let Some(p) = a.get(k).and_then(Value::as_str).filter(|s| !s.trim().is_empty())
        {
            let id = self.open_path(p, a.get("password").and_then(Value::as_str))?;
            return Ok(id);
        }
        if let Some(id) = self.active.filter(|id| self.doc_exists(*id)) {
            return Ok(id);
        }
        let ids = self.open_ids();
        if let [only] = ids.as_slice() {
            self.active = Some(*only);
            return Ok(*only);
        }
        if let Some(ev) = self.evicted.last() {
            return Err(self.evicted_error(ev));
        }
        Err(WhiqError::new(
            code::NO_DOCUMENT,
            if ids.is_empty() { "no document is open".to_string() } else { "several documents are open and none was named".to_string() },
            "open one with open_document (or pass path), or name it with document",
        )
        .with("open_documents", self.handles_value()))
    }

    fn missing_handle(&self, h: &str) -> WhiqError {
        if let Some(ev) = self.evicted.iter().find(|e| e.handle == h) {
            return self.evicted_error(ev);
        }
        WhiqError::new(code::NOT_FOUND, format!("no open document {h}"), "use one of open_documents, or open the file again with open_document")
            .with("open_documents", self.handles_value())
    }

    fn evicted_error(&self, ev: &Evicted) -> WhiqError {
        let next = match &ev.recovered_path {
            Some(r) => format!(
                "its unsaved work was auto-saved: open_document with path {r} (or the original {}) and carry on",
                ev.original_path.as_deref().unwrap_or("file")
            ),
            None => format!("it had no unsaved work: open_document with path {} again", ev.original_path.as_deref().unwrap_or(&ev.name)),
        };
        WhiqError::new(code::EVICTED, format!("{} ({}) was released by the server ({})", ev.handle, ev.name, ev.reason), next)
            .with("recovered_path", json!(ev.recovered_path))
            .with("original_path", json!(ev.original_path))
    }

    /// Where a file to read is: absolute as given; relative inside the read root, then the write
    /// root (a file this engine wrote earlier).
    pub fn read_path(&self, p: &str) -> PathBuf {
        let path = Path::new(p);
        if path.is_absolute() {
            return path.to_path_buf();
        }
        for root in [&self.settings.read_root, &self.settings.write_root].into_iter().flatten() {
            let cand = root.join(path);
            if cand.exists() {
                return cand;
            }
        }
        match (&self.settings.read_root, &self.settings.write_root) {
            (Some(r), _) | (None, Some(r)) => r.join(path),
            (None, None) => path.to_path_buf(),
        }
    }

    /// A file to read, resolved and checked against the roots (an error an agent can act on).
    pub fn resolve_input(&self, p: &str) -> Res<PathBuf> {
        let path = self.read_path(p);
        let checked = self.auto.resolve(&path.to_string_lossy(), false).map_err(|e| self.tool_error("open", e))?;
        if !checked.exists() {
            let near = nearby_resolved_files(&checked);
            return Err(WhiqError::new(code::NOT_FOUND, format!("{p}: no such file"), "check the name (closest lists files next to it)")
                .with("closest", json!(near)));
        }
        Ok(checked)
    }

    /// Open `p` (or find it already open) and make it active.
    pub fn open_path(&mut self, p: &str, password: Option<&str>) -> Res<u64> {
        let path = self.resolve_input(p)?;
        let shown = path.to_string_lossy().into_owned();
        let canon = path.canonicalize().ok();
        if let Some(d) = self.auto.session.docs().iter().find(|d| {
            d.path
                .as_deref()
                .is_some_and(|dp| dp == shown || canon.as_deref().is_some_and(|c| Path::new(dp).canonicalize().ok().as_deref() == Some(c)))
        }) {
            let id = d.id.0;
            self.active = Some(id);
            return Ok(id);
        }
        let mut args = json!({ "path": shown });
        if let Some(pw) = password {
            args["password"] = json!(pw);
        }
        let v = self.tool("doc_open", args)?;
        let id = v
            .get("doc")
            .and_then(Value::as_u64)
            .ok_or_else(|| WhiqError::new(code::INTERNAL, "the document did not open", "call open_document again"))?;
        self.evicted.retain(|e| e.original_path.as_deref() != Some(shown.as_str()) && e.recovered_path.as_deref() != Some(shown.as_str()));
        self.active = Some(id);
        Ok(id)
    }

    /// A file to write: relative inside the write root; an existing file renamed `OLD_<name>`
    /// unless `overwrite`, the server keeps no OLD_ copies, or this engine wrote it. Returns the
    /// path and the renames made.
    pub fn resolve_output(&mut self, p: &str, overwrite: bool) -> Res<(PathBuf, Vec<Value>)> {
        let path = Path::new(p);
        let path = if path.is_absolute() {
            path.to_path_buf()
        } else {
            match &self.settings.write_root {
                Some(r) => r.join(path),
                None => path.to_path_buf(),
            }
        };
        if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty())
            && !parent.exists()
        {
            // The parent must itself resolve inside the write root before anything is made.
            self.auto.resolve(&parent.to_string_lossy(), true).map_err(|e| self.tool_error("write", e))?;
            std::fs::create_dir_all(parent).map_err(|e| classify(&format!("{}: {e}", parent.display()), "write"))?;
        }
        let target = self.auto.resolve(&path.to_string_lossy(), true).map_err(|e| self.tool_error("write", e))?;
        let mut renamed = Vec::new();
        if target.exists() && !overwrite && !self.settings.no_old_copies && !self.written.contains(&target) && target.is_file() {
            let dir = target.parent().map(Path::to_path_buf).unwrap_or_default();
            let name = target.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
            let mut n = 1u32;
            let old = loop {
                let cand = if n == 1 { dir.join(format!("OLD_{name}")) } else { dir.join(format!("OLD_{n}_{name}")) };
                if !cand.exists() {
                    break cand;
                }
                n = n.saturating_add(1);
                if n > 10_000 {
                    return Err(WhiqError::failed("too many OLD_ copies beside the file", "pass overwrite: true or another path"));
                }
            };
            std::fs::rename(&target, &old).map_err(|e| classify(&format!("{}: {e}", target.display()), "write"))?;
            renamed.push(json!({"path": target.to_string_lossy(), "previous": old.to_string_lossy()}));
        }
        Ok((target, renamed))
    }

    pub fn wrote(&mut self, p: &Path) {
        self.written.insert(p.to_path_buf());
    }

    /// The document summary every document tool answers with.
    pub fn summary(&self, id: u64) -> Value {
        let Ok(d) = self.doc(id) else { return json!({"document": handle(id)}) };
        json!({
            "document": handle(id),
            "name": d.name,
            "path": d.path,
            "pages": d.info.pages.len(),
            "dirty": d.dirty,
            "editable": d.read_only_reason.is_none(),
            "read_only_reason": d.read_only_reason,
            "encrypted": d.info.encrypted,
            "title": d.info.title,
            "author": d.info.author,
            "undo": d.can_undo(),
        })
    }

    /// What server_status lists for each open document.
    pub fn documents_value(&self) -> Value {
        let docs: Vec<Value> = self
            .auto
            .session
            .docs()
            .iter()
            .map(|d| {
                json!({
                    "document": handle(d.id.0), "name": d.name, "path": d.path, "dirty": d.dirty,
                    "active": self.active == Some(d.id.0), "pages": d.info.pages.len(), "bytes": d.bytes.len(),
                })
            })
            .collect();
        json!(docs)
    }

    pub fn checkpoints_value(&self) -> Value {
        checkpoints_value(&self.recovery, &self.settings)
    }

    pub fn evicted_value(&self) -> Value {
        json!(self.evicted.iter().map(|e| json!({"document": e.handle, "name": e.name, "reason": e.reason, "recovered_path": e.recovered_path, "original_path": e.original_path})).collect::<Vec<_>>())
    }

    /// Publish the open documents to [`Recovery`] (the checkpoint timer, SIGTERM and hang-up
    /// save from it): each one's working file when it has unsaved work, and where a new
    /// recovered file of it goes. A document saved since is told to it (its checkpoint goes).
    pub fn publish_recovery(&mut self) {
        let mut docs = Vec::new();
        for d in self.auto.session.docs() {
            let id = d.id.0;
            let generation = d.generation();
            let prev = self.recovery.doc(id);
            if !d.dirty && (prev.as_ref().is_some_and(|p| p.bytes.is_some()) || self.recovery.file_of(id).is_some()) {
                self.recovery.saved(id, generation);
            }
            // A document with no place for its recovered file inside the write root is not
            // saved from here (writing it would fail the same way on the worker).
            let place = self.recovered_place(d);
            let bytes = match prev.as_ref() {
                _ if !d.dirty || place.is_err() => None,
                Some(p) if p.generation == generation && p.bytes.is_some() => p.bytes.clone(),
                _ => self.auto.session.recovery_snapshot(d.id).map(|r| r.bytes),
            };
            let (dir, stem) = place.unwrap_or_default();
            docs.push(DocSnap { id, generation, bytes, dir, stem, original: d.path.clone(), since: None, edits: 0, seen: 0 });
        }
        self.recovery.publish(docs);
        for (_, p, _) in self.recovery.files() {
            self.written.insert(p);
        }
    }

    /// Where a new recovered file of `d` goes (the write root, else beside the document, else
    /// the working folder; resolved inside the write root) and its stem.
    fn recovered_place(&self, d: &pdfcraft_engine::Document) -> Res<(PathBuf, String)> {
        let stem = Path::new(&d.name).file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_else(|| "document".into());
        let dir = self
            .settings
            .write_root
            .clone()
            .or_else(|| d.path.as_deref().and_then(|p| Path::new(p).parent().map(Path::to_path_buf)))
            .unwrap_or_else(|| PathBuf::from("."));
        // Resolved as a file in it is, so the folder is held to the write root.
        let probe = dir.join(format!("{stem}.recovered.pdf"));
        let resolved = self.auto.resolve(&probe.to_string_lossy(), true).map_err(|e| self.tool_error("evict", e))?;
        Ok((resolved.parent().map(Path::to_path_buf).unwrap_or(dir), stem))
    }

    /// After a call: publish the documents, count the call as an edit of each document it
    /// changed, and write a checkpoint when `--checkpoint-edits` is reached (the timer thread
    /// writes the ones `--checkpoint-secs` makes due, during a call too).
    pub fn after_call(&mut self) {
        self.publish_recovery();
        let ids: Vec<u64> = self.open_ids();
        self.svg.retain(|id, _| ids.contains(id));
        for id in ids {
            let Ok(generation) = self.doc(id).map(pdfcraft_engine::Document::generation) else { continue };
            let edits = self.recovery.count_edit(id, generation);
            if self.settings.checkpoint_edits > 0 && edits >= self.settings.checkpoint_edits {
                let _ = self.write_recovered(id);
            }
        }
    }

    /// Write document `id`'s current state to its recovered file (the one it has, else a new
    /// `<stem>.recovered-<stamp>.pdf` in the write root, else beside the document).
    pub fn write_recovered(&mut self, id: u64) -> Res<PathBuf> {
        let d = self.doc(id)?;
        let generation = d.generation();
        let (dir, stem) = self.recovered_place(d)?;
        let bytes =
            self.auto.session.save_bytes(d.id).or_else(|_| self.auto.session.save_full_bytes(d.id)).map_err(|e| classify(&e.to_string(), "evict"))?;
        let target = self
            .recovery
            .write(id, generation, &bytes, &dir, &stem, false)
            .map_err(|e| classify(&e, "evict"))?
            .ok_or_else(|| WhiqError::new(code::INTERNAL, format!("{} was not written", handle(id)), "call evict again"))?;
        self.written.insert(target.clone());
        Ok(target)
    }

    /// Release every open document: unsaved work auto-saved to a recovered file first. A save that
    /// fails leaves that document open (nothing is lost).
    pub fn evict_all(&mut self, reason: &str) -> Value {
        let ids = self.open_ids();
        self.evict(&ids, reason)
    }

    /// Release documents `ids` (as [`Self::evict_all`] releases every one).
    pub fn evict(&mut self, ids: &[u64], reason: &str) -> Value {
        let mut out = Vec::new();
        for &id in ids {
            let (name, path, dirty) = match self.doc(id) {
                Ok(d) => (d.name.clone(), d.path.clone(), d.dirty),
                Err(_) => continue,
            };
            let mut recovered = None;
            if dirty {
                match self.write_recovered(id) {
                    Ok(p) => recovered = Some(p.to_string_lossy().into_owned()),
                    Err(e) => {
                        out.push(json!({"document": handle(id), "name": name, "kept_open": true, "error": e.object()}));
                        continue;
                    }
                }
            }
            let _ = self.tool("doc_close", json!({"doc": id, "discard_changes": true}));
            self.recovery.forget(id, false);
            if let Some(r) = &recovered {
                let v = json!({"path": r, "reason": reason, "original": path, "at": now_utc().to_rfc3339()});
                self.recovered_unreported.push(v.clone());
                self.recovered_all.push(v);
            }
            let ev = Evicted {
                handle: handle(id),
                name: name.clone(),
                recovered_path: recovered.clone(),
                original_path: path.clone(),
                reason: reason.to_string(),
            };
            out.push(json!({"document": ev.handle, "name": name, "dirty": dirty, "recovered_path": recovered, "original_path": path}));
            self.evicted.push(ev);
        }
        if self.active.is_some_and(|a| !self.doc_exists(a)) {
            self.active = None;
        }
        self.publish_recovery();
        json!({"evicted": out, "next_step": "every call that needs a released document answers evicted with the file to reopen"})
    }

    /// Before the worker runs a call: what is open, so a panic releases only what the call
    /// opened or changed.
    pub fn begin_call(&mut self) {
        self.call_start = self.auto.session.docs().iter().map(|d| (d.id.0, d.bytes.clone())).collect();
        self.batch = None;
        self.ocr_run = None;
    }

    /// A call panicked (a bug): put back the batch it was applying, auto-save and release only
    /// the documents it opened or changed (the others stay open as they are), and say what of
    /// the call was kept.
    pub fn after_panic(&mut self, name: &str, msg: &str) -> WhiqError {
        let restored = self.batch.take().map(|(id, c)| (id, self.auto.session.batch_restore(pdfcraft_engine::DocId(id), c).is_ok()));
        let run = self.ocr_run.take();
        let mut ids: Vec<u64> = self
            .auto
            .session
            .docs()
            .iter()
            .filter(|d| !self.call_start.iter().any(|(id, b)| *id == d.id.0 && Arc::ptr_eq(b, &d.bytes)))
            .map(|d| d.id.0)
            .collect();
        for id in restored.iter().map(|r| r.0).chain(run.iter().map(|r| r.doc)) {
            if !ids.contains(&id) && self.doc_exists(id) {
                ids.push(id);
            }
        }
        self.call_start.clear();
        let released = self.evict(&ids, "panic");
        let handles = ids.iter().map(|id| handle(*id)).collect::<Vec<_>>().join(", ");
        let reopen = if ids.is_empty() {
            "no document was changed".to_string()
        } else {
            format!(
                "only {handles} released, its unsaved work auto-saved (evicted lists each recovered_path; with none, its original file has all of it); the other documents stay open"
            )
        };
        let e = match (restored, run) {
            (Some((id, true)), _) => WhiqError::new(
                code::INTERNAL,
                format!("{name} failed inside the engine: {msg}. Nothing of the call was applied: {} was put back as it was before it", handle(id)),
                format!("{reopen}: open_document with that file and send the call again (once)"),
            )
            .with("applied", false),
            (Some((id, false)), _) => WhiqError::new(
                code::INTERNAL,
                format!(
                    "{name} failed inside the engine: {msg}. {} could not be put back as it was before the call, so part of the batch may be in it",
                    handle(id)
                ),
                format!(
                    "{reopen}: open_document with that file, check which operations are in it (read, list_content), and send only the missing ones"
                ),
            )
            .with("applied", "unknown"),
            (None, Some(run)) => {
                let mut e = WhiqError::new(
                    code::INTERNAL,
                    format!("{name} failed inside the engine: {msg}. {}", ocr_kept(&run)),
                    format!("{reopen}: open_document with that file and {}", ocr_rest(&run)),
                );
                e.details = ocr_details(&run);
                e
            }
            (None, None) => WhiqError::new(
                code::INTERNAL,
                format!("{name} failed inside the engine: {msg}"),
                format!("{reopen}; call again once (open_document first if it was released)"),
            ),
        };
        e.with("evicted", released)
    }

    /// The OCR engine, loaded once.
    pub fn ocr_engine(&mut self) -> Res<Arc<pdfcraft_engine::ocr::Ocr>> {
        if let Some(o) = &self.ocr {
            return Ok(o.clone());
        }
        let o = pdfcraft_engine::ocr::engine().map_err(|m| classify(&m, "ocr"))?;
        self.ocr = Some(o.clone());
        Ok(o)
    }

    // ---- the tools ---------------------------------------------------------------------

    pub fn call(&mut self, name: &str, a: &Map<String, Value>, ctx: &Ctx) -> Res<Reply> {
        ctx.check()?;
        self.caller =
            a.get("caller").and_then(Value::as_str).filter(|s| !s.trim().is_empty()).map(str::to_string).or_else(|| self.settings.author.clone());
        let r = match name {
            "open_document" => {
                let p = a.get("path").and_then(Value::as_str).ok_or_else(|| WhiqError::invalid("path is required", "pass the PDF's path"))?;
                ctx.stage("opening", 1);
                let id = self.open_path(p, a.get("password").and_then(Value::as_str))?;
                Ok(self.summary(id).into())
            }
            "new_document" => self.new_document(a),
            "save_document" => self.save_document(a),
            "close_document" => {
                let id = self.target(a, None)?;
                let discard = a.get("discard").and_then(Value::as_bool).unwrap_or(false);
                let d = self.doc(id)?;
                if d.dirty && !discard {
                    return Err(WhiqError::new(
                        code::FAILED,
                        format!("{} has unsaved changes", handle(id)),
                        "save it with save_document, or pass discard: true to drop the changes",
                    )
                    .with("document", handle(id)));
                }
                self.tool("doc_close", json!({"doc": id, "discard_changes": true}))?;
                self.recovery.forget(id, true);
                if self.active == Some(id) {
                    self.active = None;
                }
                Ok(json!({"closed": handle(id), "discarded": discard, "open_documents": self.handles_value()}).into())
            }
            "document_info" => {
                let id = self.target(a, Some("path"))?;
                let mut v = self.tool("doc_info", json!({"doc": id}))?;
                if let Some(o) = v.as_object_mut() {
                    o.insert("document".into(), self.summary(id));
                }
                Ok(v.into())
            }
            "search" => {
                let id = self.target(a, Some("path"))?;
                let mut args = json!({"doc": id, "query": a.get("query").cloned().unwrap_or(Value::Null)});
                for k in ["limit", "start_page", "skip"] {
                    if let Some(v) = a.get(k) {
                        args[k] = v.clone();
                    }
                }
                ctx.stage("searching", 1);
                let mut v = self.tool("text_find", args)?;
                if let Some(o) = v.as_object_mut() {
                    o.insert("document".into(), json!(handle(id)));
                }
                Ok(v.into())
            }
            "list_content" => self.list_content(a, ctx),
            "read" => super::read::read(self, a, ctx),
            "render_preview" => super::preview::render_preview(self, a, ctx),
            "edit" => super::edit::edit(self, a, ctx),
            "list_operations" => Ok(super::edit::list_operations(a).into()),
            "convert" => super::convert::convert(self, a, ctx),
            "combine" => self.combine(a),
            "ocr" => self.ocr(a, ctx),
            "evict" => Ok(self.evict_all("evict").into()),
            "__sleep" if self.settings.test_hooks => {
                let ms = a.get("ms").and_then(Value::as_u64).unwrap_or(1000);
                let start = Instant::now();
                while start.elapsed().as_millis() < u128::from(ms) {
                    ctx.check()?;
                    std::thread::sleep(std::time::Duration::from_millis(10));
                }
                Ok(json!({"slept_ms": ms}).into())
            }
            other => Err(WhiqError::new(code::NOT_FOUND, format!("no tool named {other:?}"), "tools/list names every tool")
                .with("closest", json!(closest(other, super::defs::names().iter().map(String::as_str), 5)))),
        };
        self.after_call();
        r
    }

    fn new_document(&mut self, a: &Map<String, Value>) -> Res<Reply> {
        let (w, h) = super::convert::page_size(a.get("size"), a.get("landscape").and_then(Value::as_bool).unwrap_or(false))?;
        let n = a.get("pages").and_then(Value::as_u64).unwrap_or(1).max(1);
        let n = usize::try_from(n).map_err(|_| WhiqError::invalid("pages is too large", "ask for fewer pages"))?;
        let bytes = self.auto.session.create_blank(w, h, n).map_err(|e| classify(&e.to_string(), "new_document"))?;
        let name = a.get("name").and_then(Value::as_str).filter(|s| !s.trim().is_empty()).unwrap_or("untitled.pdf").to_string();
        let id = self.auto.session.open_new(name, bytes).map_err(|e| classify(&e.to_string(), "new_document"))?;
        self.active = Some(id.0);
        Ok(self.summary(id.0).into())
    }

    fn save_document(&mut self, a: &Map<String, Value>) -> Res<Reply> {
        let id = self.target(a, None)?;
        let overwrite = a.get("overwrite").and_then(Value::as_bool).unwrap_or(false);
        let mut args = json!({"doc": id});
        let mut renamed = Vec::new();
        let given = a.get("path").and_then(Value::as_str).filter(|s| !s.trim().is_empty()).map(str::to_string);
        let own = self.doc(id)?.path.clone();
        let path = match (given, own) {
            (Some(p), _) => Some(p),
            (None, Some(_)) => None,
            (None, None) => Some(self.doc(id)?.name.clone()),
        };
        if let Some(p) = path {
            let same = self.doc(id)?.path.as_deref().is_some_and(|own| Path::new(own) == self.read_path(&p) || own == p);
            let (target, r) = if same { (PathBuf::from(&p), Vec::new()) } else { self.resolve_output(&p, overwrite)? };
            renamed = r;
            args["path"] = json!(target.to_string_lossy());
        }
        if let Some(full) = a.get("full").and_then(Value::as_bool) {
            args["full"] = json!(full);
        }
        let mut v = self.tool("doc_save", args)?;
        if let Some(p) = v.get("path").and_then(Value::as_str) {
            let p = PathBuf::from(p);
            self.wrote(&p);
        }
        if let Some(o) = v.as_object_mut() {
            o.insert("document".into(), self.summary(id));
            if !renamed.is_empty() {
                o.insert("previous_files_renamed".into(), json!(renamed));
                o.insert("notice".into(), json!("an existing file was renamed OLD_… before the save"));
            }
        }
        Ok(v.into())
    }

    fn combine(&mut self, a: &Map<String, Value>) -> Res<Reply> {
        let inputs: Vec<String> =
            a.get("inputs").and_then(Value::as_array).map(|v| v.iter().filter_map(|x| x.as_str().map(str::to_string)).collect()).unwrap_or_default();
        if inputs.is_empty() {
            return Err(WhiqError::invalid("inputs is empty", "list the files to combine, in order"));
        }
        let mut paths = Vec::new();
        for p in &inputs {
            paths.push(self.resolve_input(p)?.to_string_lossy().into_owned());
        }
        let out = a.get("path").and_then(Value::as_str).ok_or_else(|| WhiqError::invalid("path is required", "name the combined PDF"))?;
        let (target, renamed) = self.resolve_output(out, a.get("overwrite").and_then(Value::as_bool).unwrap_or(false))?;
        let mut args = json!({"paths": paths, "mode": "combine", "out": target.to_string_lossy(), "open": true});
        if let Some(p) = a.get("pages").filter(|v| v.is_array()) {
            args["pages"] = p.clone();
        }
        let mut v = self.tool("doc_create_multiple", args)?;
        self.wrote(&target);
        let opened = v.get("doc").and_then(Value::as_u64).or_else(|| v.get("document").and_then(|d| d.get("doc")).and_then(Value::as_u64));
        let bytes = self.auto.resolve(&target.to_string_lossy(), false).ok().and_then(|t| std::fs::metadata(t).ok()).map_or(0, |m| m.len());
        let mut out = json!({"path": target.to_string_lossy(), "bytes": bytes, "sources": inputs, "detail": v.take()});
        if let Some(id) = opened.or_else(|| self.find_by_path(&target)) {
            self.active = Some(id);
            out["document"] = json!(handle(id));
            out["pages"] = json!(self.doc(id).map(|d| d.info.pages.len()).unwrap_or(0));
        }
        if !renamed.is_empty() {
            out["previous_files_renamed"] = json!(renamed);
        }
        Ok(out.into())
    }

    fn list_content(&mut self, a: &Map<String, Value>, ctx: &Ctx) -> Res<Reply> {
        let id = self.target(a, Some("path"))?;
        let kind = a.get("kind").and_then(Value::as_str).unwrap_or("").to_string();
        let count = self.doc(id)?.info.pages.len();
        let pages = super::contract::parse_pages(a.get("pages").unwrap_or(&Value::Null), count)?;
        let all_pages = a.get("pages").is_none_or(Value::is_null);
        let mut out = match kind.as_str() {
            "comments" => {
                let mut v = self.tool("comment_list", json!({"doc": id}))?;
                if !all_pages && let Some(list) = v.get_mut("comments").and_then(Value::as_array_mut) {
                    list.retain(|c| c.get("page").and_then(Value::as_u64).is_some_and(|p| pages.contains(&(p as usize).saturating_sub(1))));
                }
                v
            }
            "fields" => self.tool("form_fields", json!({"doc": id}))?,
            "bookmarks" => {
                let mut all = Vec::new();
                let mut offset = 0u64;
                loop {
                    ctx.check()?;
                    let v = self.tool("bookmark_list", json!({"doc": id, "offset": offset, "limit": 1000}))?;
                    let rows = v.get("bookmarks").and_then(Value::as_array).cloned().unwrap_or_default();
                    all.extend(rows);
                    match v.get("next").and_then(Value::as_u64) {
                        Some(n) if n > offset => offset = n,
                        _ => break,
                    }
                }
                json!({"bookmarks": all})
            }
            "images" => {
                let mut list = Vec::new();
                ctx.stage("listing images", pages.len() as u64);
                for (k, p) in pages.iter().enumerate() {
                    ctx.check()?;
                    let v = self.tool("page_images", json!({"doc": id, "page": p + 1}))?;
                    for im in v.get("images").and_then(Value::as_array).cloned().unwrap_or_default() {
                        let mut im = im;
                        im["page"] = json!(p + 1);
                        list.push(im);
                    }
                    ctx.step((k + 1) as u64);
                }
                json!({"images": list})
            }
            "links" => self.tool("link_list", json!({"doc": id}))?,
            "attachments" | "fonts" | "layers" | "pages" => {
                let info = self.tool("doc_info", json!({"doc": id}))?;
                json!({ kind.clone(): info.get(kind.as_str()).cloned().unwrap_or(json!([])) })
            }
            other => {
                return Err(WhiqError::invalid(
                    format!("unknown kind {other:?}"),
                    "kind is one of comments, fields, bookmarks, images, links, attachments, fonts, layers, pages",
                ));
            }
        };
        if let Some(o) = out.as_object_mut() {
            o.insert("document".into(), json!(handle(id)));
            o.insert("kind".into(), json!(kind));
        }
        Ok(out.into())
    }

    pub fn find_by_path(&self, p: &Path) -> Option<u64> {
        let s = p.to_string_lossy();
        self.auto.session.docs().iter().rev().find(|d| d.path.as_deref() == Some(&*s)).map(|d| d.id.0)
    }

    /// `ocr`: page by page. Each page's text is added as soon as it is read (and published, so
    /// a checkpoint, SIGTERM or hang-up keeps it), progress counts the pages, and a cancel or a
    /// failure keeps the pages done and names the ones left.
    fn ocr(&mut self, a: &Map<String, Value>, ctx: &Ctx) -> Res<Reply> {
        let id = self.target(a, Some("path"))?;
        let count = self.doc(id)?.info.pages.len();
        let pages = super::contract::parse_pages(a.get("pages").unwrap_or(&Value::Null), count)?;
        let mut args = json!({"doc": id});
        if let Some(d) = a.get("dpi") {
            args["dpi"] = d.clone();
        }
        if let Some(s) = a.get("skip_text_pages") {
            args["skip_text_pages"] = s.clone();
        }
        let settings = self.auto.ocr_settings(&crate::json::Args(&args)).map_err(|e| self.tool_error("ocr_recognize", e))?;
        let doc = self.doc(id)?;
        if let Some(why) = doc.read_only_reason.clone() {
            return Err(classify(&why, "ocr"));
        }
        // The recognised words are listed only when the document allows copying.
        let copy = doc.allows_copying();
        let ocr = self.ocr_engine()?;
        let job = self
            .auto
            .session
            .ocr_job(pdfcraft_engine::DocId(id), &pages, settings)
            .ok_or_else(|| WhiqError::new(code::NOT_FOUND, format!("no open document {}", handle(id)), "open it with open_document"))?;
        let hook = |k: &str| if self.settings.test_hooks { a.get(k).and_then(Value::as_u64) } else { None };
        let (stop_after, panic_after) = (hook("__stop_after_pages"), hook("__panic_after_pages"));
        let total = job.pages.len();
        self.ocr_run = Some(OcrRun { doc: id, remaining: job.pages.clone(), ..OcrRun::default() });
        self.publish_recovery();
        ctx.stage_counting("recognising text", total as u64, "pages");
        job.run_each(
            &ocr,
            |done, _| {
                ctx.step(done as u64);
                !ctx.cancelled()
            },
            |found| {
                let kept = self.ocr_keep(id, found, copy);
                let done = self.ocr_run.as_ref().map_or(0, |r| r.done.len() as u64);
                if stop_after == Some(done) {
                    ctx.cancel.store(true, Ordering::SeqCst);
                }
                if panic_after == Some(done) {
                    test_hook_panic("ocr");
                }
                kept
            },
        );
        let run = self.ocr_run.take().unwrap_or_default();
        if let Some(err) = run.failed.clone() {
            let mut e = WhiqError::new(err.code, format!("{} {}", err.reason, ocr_kept(&run)), format!("{}; then {}", err.next_step, ocr_rest(&run)));
            e.details = ocr_details(&run);
            return Err(e.with("page_error", err.object()));
        }
        if ctx.cancelled() && !run.remaining.is_empty() {
            // Stopped by cancel: what was read stays.
            let mut e = WhiqError::new(
                code::CANCELLED,
                format!("the call was cancelled. {}", ocr_kept(&run)),
                format!("the text added stays in {} (save_document keeps it); to go on, {}", handle(id), ocr_rest(&run)),
            );
            e.details = ocr_details(&run);
            return Err(e);
        }
        let mut v = json!({"words": run.words, "pages": run.results});
        if let Some(out) = a.get("save_to").and_then(Value::as_str).filter(|s| !s.trim().is_empty()) {
            let saved = self.save_document(
                &json!({"document": handle(id), "path": out, "overwrite": a.get("overwrite").cloned().unwrap_or(json!(false))})
                    .as_object()
                    .cloned()
                    .unwrap_or_default(),
            )?;
            v["saved"] = saved.value;
        }
        if let Some(o) = v.as_object_mut() {
            o.insert("document".into(), self.summary(id));
        }
        Ok(v.into())
    }

    /// Add one page's recognised text to document `id` and record it in the run; whether to go
    /// on to the next page. A page that could not be read is reported as skipped (the run goes
    /// on); text that could not be added stops the run.
    fn ocr_keep(&mut self, id: u64, found: pdfcraft_engine::ocr::OcrPage, copy: bool) -> bool {
        let joins = self.ocr_run.as_ref().is_some_and(|r| r.joined);
        let added = self.auto.session.apply_ocr_page(pdfcraft_engine::DocId(id), &found, joins);
        let Some(run) = self.ocr_run.as_mut() else { return false };
        match added {
            Ok(words) => {
                run.joined |= words > 0;
                run.words += words;
                run.done.push(found.page);
                run.remaining.retain(|p| *p != found.page);
                run.results.push(match &found.skipped {
                    Some(why) => json!({ "page": found.page + 1, "skipped": why }),
                    None if copy => json!({ "page": found.page + 1, "words": words, "already_on_page": found.already_on_page, "text": found.text() }),
                    None => json!({ "page": found.page + 1, "words": words, "already_on_page": found.already_on_page }),
                });
                if words > 0 {
                    self.publish_recovery();
                }
                true
            }
            Err(e) => {
                run.failed = Some(classify(&e.to_string(), "ocr").with("page", found.page + 1));
                false
            }
        }
    }
}

/// The checkpoint settings and files (written by the worker or the timer), as server_status
/// lists them.
pub fn checkpoints_value(recovery: &Recovery, settings: &Settings) -> Value {
    let files: Vec<Value> = recovery
        .files()
        .into_iter()
        .map(|(id, p, edits)| json!({"path": p.to_string_lossy(), "document": handle(id), "pending_edits": edits}))
        .collect();
    json!({"every_edits": settings.checkpoint_edits, "every_secs": settings.checkpoint_secs, "files": files})
}

/// What an `ocr` run kept: "The text of pages 1-3 was added and kept (12 words)…".
fn ocr_kept(run: &OcrRun) -> String {
    let mut done = one_based(&run.done);
    done.sort_unstable();
    if done.is_empty() {
        return format!("No page was finished, so nothing was added to {}.", handle(run.doc));
    }
    let left = if run.remaining.is_empty() { String::new() } else { format!("; pages {} were not read", ocr_left(run)) };
    format!("Pages {} of {} were read and their text added ({} words){left}.", page_spec(&done), handle(run.doc), run.words)
}

/// The pages an `ocr` run did not read, as a page selection.
fn ocr_left(run: &OcrRun) -> String {
    let mut left = one_based(&run.remaining);
    left.sort_unstable();
    page_spec(&left)
}

/// How to read only what an `ocr` run left: the same call with `pages`.
fn ocr_rest(run: &OcrRun) -> String {
    if run.remaining.is_empty() {
        return "every page was read".into();
    }
    format!("call ocr with pages: \"{}\" to read only the pages left", ocr_left(run))
}

/// The pages done and left, for the error object.
fn ocr_details(run: &OcrRun) -> Map<String, Value> {
    let mut done = one_based(&run.done);
    done.sort_unstable();
    let mut left = one_based(&run.remaining);
    left.sort_unstable();
    let mut m = Map::new();
    m.insert("document".into(), json!(handle(run.doc)));
    m.insert("pages_done".into(), json!(done));
    m.insert("pages_remaining".into(), json!(left));
    m.insert("continue_with".into(), if left.is_empty() { Value::Null } else { json!({"pages": page_spec(&left)}) });
    m.insert("words".into(), json!(run.words));
    // The text itself is in the document (read gives it); only the pages skipped are listed.
    let skipped: Vec<&Value> = run.results.iter().filter(|r| r.get("skipped").is_some()).collect();
    m.insert("pages_skipped".into(), json!(skipped));
    m
}

/// A test hook (`PDFCRAFT_MCP_TEST_HOOKS=1` only): a bug inside a call, to prove nothing is lost.
#[allow(clippy::panic)]
pub(crate) fn test_hook_panic(at: &str) -> ! {
    panic!("test hook: panic in {at}")
}

/// Up to five files next to a missing one, for `closest`.
/// `path` was resolved inside a root by the caller ([`Engine::resolve_input`]).
pub fn nearby_resolved_files(path: &Path) -> Vec<String> {
    let Some(dir) = path.parent() else { return Vec::new() };
    let want = path.file_name().map(|n| n.to_string_lossy().to_lowercase()).unwrap_or_default();
    let names: Vec<String> =
        std::fs::read_dir(dir).map(|rd| rd.flatten().map(|e| e.file_name().to_string_lossy().into_owned()).take(2000).collect()).unwrap_or_default();
    let lowered: Vec<String> = names.iter().map(|n| n.to_lowercase()).collect();
    closest(&want, lowered.iter().map(String::as_str), 5)
        .into_iter()
        .filter_map(|l| lowered.iter().position(|x| *x == l).and_then(|i| names.get(i)).map(|n| dir.join(n).to_string_lossy().into_owned()))
        .collect()
}
