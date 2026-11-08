//! The WHIQ craft-engine contract, end to end: `pdfcraft-cli mcp` driven over stdio as the White
//! Hat IQ platform's supervisor drives it (read and write roots, author, busy-after-ms). Every
//! tool is called on PDFs these tests make themselves (Markdown, HTML, images, blank pages): a
//! text PDF, a two-column page, a table-heavy PDF, an image-only scan and a 600-page file.
//!
//! `PDFCRAFT_CLI=path` runs the suite against another binary (the release workflow's).

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc;
use std::time::Duration;

use serde_json::{Value, json};

const CODES: [&str; 18] = [
    "busy",
    "cancelled",
    "disk_full",
    "evicted",
    "failed",
    "internal",
    "invalid_argument",
    "invalid_request",
    "locked",
    "no_document",
    "not_available",
    "not_found",
    "out_of_range",
    "outside_root",
    "password_required",
    "too_large",
    "unsupported",
    "wrong_password",
];

fn bin() -> PathBuf {
    std::env::var_os("PDFCRAFT_CLI").map_or_else(|| PathBuf::from(env!("CARGO_BIN_EXE_pdfcraft-cli")), PathBuf::from)
}

/// The OCR models, for the tests that need them: beside the binary, `PDFCRAFT_MODELS`, or the
/// source tree's assets/models (fetched by `cargo xtask models`).
fn models_present() -> bool {
    let beside = bin().parent().map(|d| d.join("models").join("text-recognition.onnx").exists()).unwrap_or(false);
    let env = std::env::var_os("PDFCRAFT_MODELS").is_some_and(|d| Path::new(&d).join("text-recognition.onnx").exists());
    let tree = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../assets/models/text-recognition.onnx").exists();
    beside || env || tree
}

struct Server {
    child: Child,
    stdin: Option<ChildStdin>,
    lines: mpsc::Receiver<String>,
    next: u64,
    pub dir: tempfile::TempDir,
}

impl Server {
    fn start(extra: &[&str]) -> Server {
        Self::start_env(extra, &[])
    }

    fn start_env(extra: &[&str], env: &[(&str, &str)]) -> Server {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("in")).unwrap();
        std::fs::create_dir_all(dir.path().join("out")).unwrap();
        let mut cmd = Command::new(bin());
        cmd.arg("mcp")
            .arg("--automation-read-root")
            .arg(dir.path().join("in"))
            .arg("--automation-write-root")
            .arg(dir.path().join("out"))
            .args(["--author", "Test Author", "--busy-after-ms", "300"])
            .args(extra)
            .current_dir(dir.path().join("out"))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        for (k, v) in env {
            cmd.env(k, v);
        }
        let mut child = cmd.spawn().expect("start pdfcraft-cli mcp");
        let stdout = child.stdout.take().unwrap();
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines().map_while(Result::ok) {
                if tx.send(line).is_err() {
                    break;
                }
            }
        });
        let stdin = child.stdin.take();
        let mut s = Server { child, stdin, lines: rx, next: 1, dir };
        let init =
            s.request("initialize", json!({"protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": {"name": "test", "version": "1"}}));
        assert_eq!(init["result"]["serverInfo"]["name"], "pdfcraft", "{init}");
        s
    }

    fn inp(&self) -> PathBuf {
        self.dir.path().join("in")
    }

    fn out(&self) -> PathBuf {
        self.dir.path().join("out")
    }

    fn send(&mut self, v: &Value) {
        let stdin = self.stdin.as_mut().unwrap();
        writeln!(stdin, "{v}").unwrap();
        stdin.flush().unwrap();
    }

    fn wait_id(&mut self, id: u64) -> Value {
        loop {
            let line = self.lines.recv_timeout(Duration::from_secs(600)).expect("an answer from the server");
            let v: Value = serde_json::from_str(&line).unwrap();
            if v.get("id") == Some(&json!(id)) {
                return v;
            }
        }
    }

    fn request(&mut self, method: &str, params: Value) -> Value {
        let id = self.next;
        self.next += 1;
        self.send(&json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}));
        self.wait_id(id)
    }

    /// A tools/call's raw result (`content`, `structuredContent`, `isError`), waiting out jobs.
    fn raw(&mut self, name: &str, args: Value) -> Value {
        let r = self.request("tools/call", json!({"name": name, "arguments": args}));
        let res = r.get("result").cloned().unwrap_or_else(|| panic!("{name}: no result: {r}"));
        check_shape(&res);
        let sc = &res["structuredContent"];
        if sc["status"] == "running" && sc["job_id"].is_string() && name != "job_status" {
            let job = sc["job_id"].as_str().unwrap().to_string();
            loop {
                std::thread::sleep(Duration::from_millis(200));
                let st = self.raw("job_status", json!({"job_id": job}));
                let s = &st["structuredContent"];
                match s["status"].as_str() {
                    Some("running") => continue,
                    Some("done") => return json!({"content": st["content"], "structuredContent": s["result"]}),
                    _ => return json!({"isError": true, "structuredContent": {"error": s["error"]}}),
                }
            }
        }
        res
    }

    /// A successful call's structured answer.
    fn ok(&mut self, name: &str, args: Value) -> Value {
        let r = self.raw(name, args.clone());
        assert_ne!(r["isError"], json!(true), "{name} {args} failed: {}", r["structuredContent"]);
        r["structuredContent"].clone()
    }

    /// A failed call's error object.
    fn err(&mut self, name: &str, args: Value) -> Value {
        let r = self.raw(name, args.clone());
        assert_eq!(r["isError"], json!(true), "{name} {args} should fail: {}", r["structuredContent"]);
        r["structuredContent"]["error"].clone()
    }

    fn close_stdin(&mut self) {
        self.stdin.take();
    }

    fn wait_exit(&mut self) {
        for _ in 0..600 {
            if self.child.try_wait().unwrap().is_some() {
                return;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        panic!("the server did not exit after stdin closed");
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        self.stdin.take();
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Every answer has the contract's shape; every error one of the eighteen codes with a next step.
fn check_shape(res: &Value) {
    assert!(res["content"].is_array(), "{res}");
    if res["isError"] == json!(true) {
        let e = &res["structuredContent"]["error"];
        let code = e["code"].as_str().unwrap_or_default();
        assert!(CODES.contains(&code), "unknown code {code}: {res}");
        assert!(e["reason"].is_string() && e["next_step"].as_str().is_some_and(|s| !s.is_empty()), "{res}");
        let text = res["content"][0]["text"].as_str().unwrap_or_default();
        assert!(text.starts_with(&format!("[{code}]")) && text.contains("next_step:"), "{text}");
    }
}

const REPORT_MD: &str = "# Site Progress Report\n\n\
Prepared for the **project board**. This report covers *concrete* works.\n\n\
## Summary of quantities\n\n\
| Item | Description | Unit | Qty |\n|---|---|---|---|\n| 1.1 | Slab concrete | m3 | 523 |\n| 1.2 | Column formwork | m2 | 752 |\n| 1.3 | Reinforcement steel | kg | 9100 |\n\n\
## Next steps\n\n- Pour level 3 slab\n- Inspect formwork\n\n1. First ordered\n2. Second ordered\n\n> A quoted remark.\n\n```\ncode line 1\n```\n\n\
Visit [the site](https://example.com/site) for photos.\n";

fn make_report(s: &mut Server) -> Value {
    s.ok("convert", json!({"markdown": REPORT_MD, "path": "report.pdf"}))
}

// ---- protocol and contract --------------------------------------------------------------

#[test]
fn tools_are_listed_with_annotations_and_file_arguments() {
    let mut s = Server::start(&[]);
    let r = s.request("tools/list", json!({}));
    let tools = r["result"]["tools"].as_array().unwrap();
    let names: Vec<&str> = tools.iter().map(|t| t["name"].as_str().unwrap()).collect();
    for want in [
        "server_status",
        "job_status",
        "list_jobs",
        "cancel",
        "evict",
        "open_document",
        "new_document",
        "save_document",
        "close_document",
        "document_info",
        "read",
        "search",
        "list_content",
        "render_preview",
        "edit",
        "list_operations",
        "convert",
        "combine",
        "ocr",
    ] {
        assert!(names.contains(&want), "{want} missing from {names:?}");
    }
    for t in tools {
        assert!(t["annotations"]["readOnlyHint"].is_boolean() && t["annotations"]["destructiveHint"].is_boolean(), "{t}");
        assert_eq!(t["annotations"]["openWorldHint"], json!(false));
        for common in ["caller", "async", "continue_from", "max_bytes"] {
            assert!(t["inputSchema"]["properties"][common].is_object(), "{} lacks {common}", t["name"]);
        }
    }
    let open = tools.iter().find(|t| t["name"] == "open_document").unwrap();
    assert_eq!(open["_meta"]["whiq/files"], json!({"read": ["path"], "opens": true}));
    let edit = tools.iter().find(|t| t["name"] == "edit").unwrap();
    assert_eq!(edit["_meta"]["whiq/files"]["read"], json!(["ops[].file", "ops[].files[]"]));
    let st = s.ok("server_status", json!({}));
    assert_eq!(st["limits"]["old_copies"], json!(true));
    assert_eq!(st["limits"]["busy_after_ms"], json!(300));
    assert_eq!(st["author"], json!("Test Author"));
    assert!(st["memory"]["rss_bytes"].as_u64().unwrap() > 0);
    // Protocol errors carry the error object.
    let bad = s.request("no/such", json!({}));
    assert!(CODES.contains(&bad["error"]["data"]["code"].as_str().unwrap()), "{bad}");
}

#[test]
fn errors_name_a_code_and_a_next_step() {
    let mut s = Server::start(&[]);
    let e = s.err("rendr_preveiw", json!({}));
    assert_eq!(e["code"], "not_found");
    assert!(e["closest"].as_array().unwrap().iter().any(|c| c == "render_preview"), "{e}");
    assert_eq!(s.err("read", json!({}))["code"], "no_document");
    let e = s.err("open_document", json!({"path": "nope.pdf"}));
    assert_eq!(e["code"], "not_found");
    let e = s.err("open_document", json!({"path": "/etc/hostname"}));
    assert_eq!(e["code"], "outside_root", "{e}");
    make_report(&mut s);
    let e = s.err("read", json!({"pages": "40"}));
    assert_eq!(e["code"], "out_of_range");
    let e = s.err("read", json!({"pages": "x-y"}));
    assert_eq!(e["code"], "invalid_argument");
    let e = s.err("edit", json!({"ops": [{"op": "rotaet", "pages": 1, "degrees": 90}]}));
    assert_eq!(e["code"], "not_found");
    assert!(e["closest"].as_array().unwrap().iter().any(|c| c == "rotate"), "{e}");
    let e = s.err("job_status", json!({"job_id": "job-999"}));
    assert_eq!(e["code"], "not_found");
    // Forgiving names: camelCase tool, argument synonyms, numbers as strings, an alias.
    let r = s.ok("renderPreview", json!({"page": "1", "Format": "PNG", "dpi": "40"}));
    assert_eq!(r["format"], "png");
    let r = s.ok("status", json!({"bogus": 1}));
    assert!(r["warnings"].as_array().is_some_and(|w| !w.is_empty()), "{r}");
}

// ---- convert -------------------------------------------------------------------------------

#[test]
fn markdown_html_images_and_text_become_pdfs() {
    let mut s = Server::start(&[]);
    let r = make_report(&mut s);
    assert_eq!(r["pages"], json!(1), "{r}");
    assert!(s.out().join("report.pdf").exists());
    let text = s.ok("read", json!({"document": r["document"]}));
    let t = text["pages"][0]["text"].as_str().unwrap();
    assert!(t.contains("# Site Progress Report"), "{t}");
    assert!(t.contains("| 1.2 | Column formwork | m2 | 752 |"), "{t}");
    assert_eq!(text["metadata"]["title"], "Site Progress Report");
    // The link is a real link annotation.
    let links = s.ok("list_content", json!({"kind": "links"}));
    assert!(links.to_string().contains("example.com/site"), "{links}");

    let html = "<html><head><title>Inspection</title></head><body><h1>Inspection Log</h1><p>All <b>clear</b>.</p>\
                <table><tr><th>Zone</th><th>Result</th></tr><tr><td>A</td><td>Pass</td></tr><tr><td>B</td><td>Fail</td></tr><tr><td>C</td><td>Pass</td></tr></table>\
                <div style=\"page-break-before: always\"></div><h2>Annex</h2><p>Second page.</p></body></html>";
    let h = s.ok("convert", json!({"html": html, "path": "log.pdf", "page_size": "Letter"}));
    assert_eq!(h["pages"], json!(2), "{h}");
    assert_eq!(h["title"], "Inspection");
    let read = s.ok("read", json!({"document": h["document"], "pages": "1-2"}));
    let t0 = read["pages"][0]["text"].as_str().unwrap();
    assert!(t0.contains("Zone") && t0.contains("| A | Pass |") && t0.contains("| C | Pass |"), "{read}");
    assert!(read["pages"][1]["text"].as_str().unwrap().contains("Annex"), "{read}");
    // An HTML file and a Markdown file as inputs, with a picture beside the Markdown.
    std::fs::write(s.inp().join("notes.html"), html).unwrap();
    let png = s.ok("render_preview", json!({"document": r["document"], "format": "png", "dpi": 30, "out_dir": "shots"}));
    let shot = PathBuf::from(png["pages"][0]["path"].as_str().unwrap());
    std::fs::copy(&shot, s.inp().join("pic.png")).unwrap();
    std::fs::write(s.inp().join("doc.md"), "# With a picture\n\n![site photo](pic.png)\n\nAfter the picture.\n").unwrap();
    std::fs::write(s.inp().join("plain.txt"), "Plain text file\nline two\n").unwrap();
    let all = s.ok("convert", json!({"inputs": [s.inp().join("doc.md"), s.inp().join("notes.html"), s.inp().join("pic.png"), s.inp().join("plain.txt")], "path": "bundle.pdf"}));
    assert_eq!(all["pages"], json!(5), "{all}");
    let imgs = s.ok("list_content", json!({"document": all["document"], "kind": "images", "pages": "1"}));
    assert_eq!(imgs["images"].as_array().unwrap().len(), 1, "{imgs}");
    // Image → PDF alone.
    let one = s.ok("convert", json!({"inputs": ["pic.png"], "path": "pic.pdf"}));
    assert_eq!(one["pages"], json!(1));
}

#[test]
fn arabic_text_is_drawn_with_the_embedded_face() {
    let mut s = Server::start(&[]);
    let st = s.ok("server_status", json!({}));
    if st["fonts"]["arabic"] != json!(true) {
        eprintln!("skipped: this build has no craft-fonts Arabic face (CRAFT_FONTS_DIR)");
        return;
    }
    let r = s.ok(
        "convert",
        json!({"markdown": "# تقرير الموقع\n\nتم صب الخرسانة للطابق الثالث.\n\n| البند | الكمية |\n|---|---|\n| خرسانة | 523 |\n", "path": "ar.pdf"}),
    );
    assert!(r["notes"].as_array().unwrap().is_empty(), "{r}");
    let t = s.ok("read", json!({"document": r["document"], "format": "text"}));
    let text = t["pages"][0]["text"].as_str().unwrap();
    assert!(text.contains("الخرسانة"), "{text}");
}

// ---- read ----------------------------------------------------------------------------------

#[test]
fn two_columns_are_read_column_by_column() {
    let mut s = Server::start(&[]);
    let d = s.ok("new_document", json!({"size": "A4"}));
    let left = "Left column opening paragraph about concrete works and the pour sequence of the slab.";
    let left2 = "Left column closing paragraph on curing times and inspections.";
    let right = "Right column opening paragraph about steel reinforcement deliveries.";
    let right2 = "Right column closing paragraph on the crane schedule.";
    s.ok(
        "edit",
        json!({"document": d["document"], "ops": [
            {"op": "add_text", "page": 1, "text": left, "rect": [50, 80, 280, 140], "size": 11},
            {"op": "add_text", "page": 1, "text": right, "rect": [315, 84, 545, 144], "size": 11},
            {"op": "add_text", "page": 1, "text": left2, "rect": [50, 200, 280, 260], "size": 11},
            {"op": "add_text", "page": 1, "text": right2, "rect": [315, 204, 545, 264], "size": 11},
        ]}),
    );
    let r = s.ok("read", json!({"format": "text"}));
    let t = r["pages"][0]["text"].as_str().unwrap().replace('\n', " ");
    let pos = |needle: &str| t.find(needle).unwrap_or_else(|| panic!("{needle} missing in {t}"));
    assert!(pos("Left column opening") < pos("Left column closing"), "{t}");
    assert!(pos("Left column closing") < pos("Right column opening"), "{t}");
    assert!(pos("Right column opening") < pos("Right column closing"), "{t}");
    let md = s.ok("read", json!({}));
    let m = md["pages"][0]["text"].as_str().unwrap().replace('\n', " ");
    let p = |needle: &str| m.find(needle).unwrap_or_else(|| panic!("{needle} missing in {m}"));
    assert!(p("Left column closing") < p("Right column opening"), "markdown order: {m}");
}

#[test]
fn read_answers_in_windows_with_continue_from() {
    let mut s = Server::start(&["--max-result-bytes", "4096"]);
    let md: String = (1..=12).map(|i| format!("# Chapter {i}\n\n{}\n\n\\pagebreak\n\n", "Words of the chapter body. ".repeat(40))).collect();
    let r = s.ok("convert", json!({"markdown": md, "path": "chapters.pdf"}));
    assert_eq!(r["pages"], json!(12));
    let mut seen = Vec::new();
    let mut from = json!(null);
    for _ in 0..20 {
        let mut args = json!({"pages": "all"});
        if !from.is_null() {
            args["continue_from"] = from.clone();
        }
        let a = s.ok("read", args);
        assert_eq!(a["page_count"], json!(12));
        for p in a["pages"].as_array().unwrap() {
            seen.push(p["page"].as_u64().unwrap());
        }
        if a["truncated"] != json!(true) {
            break;
        }
        from = a["continue_from"].clone();
    }
    assert_eq!(seen, (1..=12).collect::<Vec<u64>>());
}

/// A `read` cancelled after some pages answers them with continue_from (never an error that drops
/// them), at a window's start or inside one, as a direct answer or as a job's result; the next
/// call continues from it.
#[test]
fn a_cancelled_read_keeps_the_pages_read_and_continues_from_the_next() {
    let mut s = Server::start_env(&[], &HOOKS);
    let md: String = (1..=12).map(|i| format!("# Chapter {i}\n\nThe body of chapter {i}.\n\n\\pagebreak\n\n")).collect();
    let r = s.ok("convert", json!({"markdown": md, "path": "chapters.pdf"}));
    assert_eq!(r["pages"], json!(12));
    let doc = r["document"].clone();
    let pages = |a: &Value| a["pages"].as_array().unwrap().iter().map(|p| p["page"].as_u64().unwrap()).collect::<Vec<_>>();

    // Cancelled after 8 pages: the cancel is seen at the next window's start.
    let a = s.ok("read", json!({"document": doc, "__stop_after_pages": 8}));
    assert_eq!(pages(&a), (1..=8).collect::<Vec<u64>>(), "{a}");
    assert!(a["pages"][7]["text"].as_str().unwrap().contains("The body of chapter 8."), "{a}");
    assert_eq!(a["cancelled"], json!(true), "{a}");
    assert_eq!(a["continue_from"], json!(8), "{a}");
    assert_eq!(a["next_page"], json!(9), "{a}");
    assert!(a["next_step"].as_str().unwrap().contains("cancelled") && a["next_step"].as_str().unwrap().contains("continue_from: 8"), "{a}");

    // Cancelled after 2 more pages, inside a window, asked as a job: a job is done and its result
    // is the pages read with continue_from (what the platform's reader reads); a job that ends
    // before the server answers comes back as that result directly.
    let r = s.request("tools/call", json!({"name": "read", "arguments": {"document": doc, "continue_from": 8, "__stop_after_pages": 2, "async": true}}));
    let a = match r["result"]["structuredContent"]["job_id"].as_str() {
        Some(job) => {
            let job = job.to_string();
            let j = loop {
                let j = s.ok("job_status", json!({"job_id": job}));
                if j["status"] != "running" {
                    break j;
                }
                std::thread::sleep(Duration::from_millis(50));
            };
            assert_eq!(j["status"], "done", "{j}");
            j["result"].clone()
        }
        None => {
            assert_ne!(r["result"]["isError"], json!(true), "{r}");
            r["result"]["structuredContent"].clone()
        }
    };
    assert_eq!(pages(&a), [9, 10], "{a}");
    assert_eq!(a["cancelled"], json!(true), "{a}");
    assert_eq!(a["continue_from"], json!(10), "{a}");
    assert_eq!(a["next_page"], json!(11), "{a}");

    // The next call reads the rest.
    let a = s.ok("read", json!({"document": doc, "continue_from": 10}));
    assert_eq!(pages(&a), [11, 12], "{a}");
    assert!(a.get("continue_from").is_none() && a.get("cancelled").is_none(), "{a}");
}

#[test]
fn scanned_pages_are_read_by_ocr_and_ocr_makes_them_searchable() {
    if !models_present() {
        eprintln!("skipped: no OCR models (cargo xtask models)");
        return;
    }
    let mut s = Server::start(&[]);
    let r = s.ok("convert", json!({"markdown": "# Concrete Pour Record\n\nThe slab on level three was poured on Monday morning.\n\nInspection passed without remarks.\n", "path": "src.pdf", "font_size": 16}));
    let shot = s.ok("render_preview", json!({"document": r["document"], "format": "png", "dpi": 200, "out_dir": "scan"}));
    let png = PathBuf::from(shot["pages"][0]["path"].as_str().unwrap());
    std::fs::copy(&png, s.inp().join("scan.png")).unwrap();
    let scan = s.ok("convert", json!({"inputs": ["scan.png"], "path": "scan.pdf"}));
    let read = s.ok("read", json!({"document": scan["document"], "format": "text"}));
    assert_eq!(read["ocr_pages"], json!([1]), "{read}");
    let text = read["pages"][0]["text"].as_str().unwrap().to_lowercase();
    let words = [
        "concrete",
        "pour",
        "record",
        "the",
        "slab",
        "on",
        "level",
        "three",
        "was",
        "poured",
        "monday",
        "morning",
        "inspection",
        "passed",
        "without",
        "remarks",
    ];
    let got = text.split(|c: char| !c.is_alphanumeric()).collect::<Vec<_>>();
    let hits = words.iter().filter(|w| got.contains(w)).count();
    eprintln!("OCR word accuracy {hits}/{}: {text}", words.len());
    assert!(hits * 100 >= words.len() * 85, "OCR read {hits} of {} words: {text}", words.len());
    // ocr off: no text, said so.
    let off = s.ok("read", json!({"document": scan["document"], "ocr": "off"}));
    assert_eq!(off["pages"][0]["no_text_layer"], json!(true));
    // The ocr tool writes a searchable PDF.
    let o = s.ok("ocr", json!({"document": scan["document"], "save_to": "scan-searchable.pdf"}));
    assert!(o["saved"]["path"].as_str().unwrap().ends_with("scan-searchable.pdf"), "{o}");
    let found = s.ok("search", json!({"path": s.out().join("scan-searchable.pdf"), "query": "inspection passed"}));
    assert_eq!(found["count"], json!(1), "{found}");
}

/// The Arabic model too: beside the binary, `PDFCRAFT_MODELS`, or the source tree's models/.
fn arabic_model_present() -> bool {
    let at = |d: &Path| d.join("arabic-recognition.onnx").exists() && d.join("arabic-recognition.txt").exists();
    let beside = bin().parent().is_some_and(|d| at(&d.join("models")));
    let env = std::env::var_os("PDFCRAFT_MODELS").is_some_and(|d| at(Path::new(&d)));
    beside || env || at(&Path::new(env!("CARGO_MANIFEST_DIR")).join("../../models"))
}

/// An Arabic scan with Latin words and numbers in it: read by OCR in typed order (right to left,
/// the Latin and the numbers left to right where they stand), and `ocr` makes it searchable in
/// Arabic.
#[test]
fn arabic_scans_are_read_in_typed_order_and_ocr_makes_them_searchable() {
    if !models_present() || !arabic_model_present() {
        eprintln!("skipped: no OCR models with the Arabic model (cargo xtask models; models/)");
        return;
    }
    let mut s = Server::start(&[]);
    let st = s.ok("server_status", json!({}));
    if st["fonts"]["arabic"] != json!(true) {
        eprintln!("skipped: this build has no craft-fonts Arabic face to draw the scan with (CRAFT_FONTS_DIR)");
        return;
    }
    assert!(st["ocr"]["languages"].as_str().unwrap().contains("Arabic"), "{st}");
    let md =
        "# تقرير الموقع اليومي\n\nتم صب الخرسانة للطابق الثالث بمقاومة C30 بعد فحص الشدات.\n\nبلغت نسبة الإنجاز 47 بالمائة حسب البرنامج الزمني.\n";
    let r = s.ok("convert", json!({"markdown": md, "path": "src.pdf", "font_size": 16}));
    let shot = s.ok("render_preview", json!({"document": r["document"], "format": "png", "dpi": 200, "out_dir": "scan"}));
    let png = PathBuf::from(shot["pages"][0]["path"].as_str().unwrap());
    std::fs::copy(&png, s.inp().join("scan.png")).unwrap();
    let scan = s.ok("convert", json!({"inputs": ["scan.png"], "path": "scan.pdf"}));
    let read = s.ok("read", json!({"document": scan["document"], "format": "text"}));
    assert_eq!(read["ocr_pages"], json!([1]), "{read}");
    let text = read["pages"][0]["text"].as_str().unwrap();
    let words = [
        "تقرير",
        "الموقع",
        "اليومي",
        "تم",
        "صب",
        "الخرسانة",
        "للطابق",
        "الثالث",
        "بمقاومة",
        "C30",
        "بعد",
        "فحص",
        "بلغت",
        "نسبة",
        "الإنجاز",
        "47",
        "حسب",
        "البرنامج",
        "الزمني",
    ];
    let got: Vec<&str> = text.split(|c: char| !c.is_alphanumeric()).collect();
    let hits = words.iter().filter(|w| got.contains(w)).count();
    eprintln!("Arabic OCR word accuracy {hits}/{}: {text}", words.len());
    assert!(hits * 100 >= words.len() * 85, "OCR read {hits} of {} words: {text}", words.len());
    // Typed order: the first words come first, the Latin and the number where they belong.
    let pos = |w: &str| text.find(w).unwrap_or_else(|| panic!("{w} missing in {text}"));
    assert!(pos("تقرير") < pos("اليومي") && pos("بمقاومة") < pos("C30") && pos("C30") < pos("فحص"), "{text}");
    assert!(pos("نسبة") < pos("47") && pos("47") < pos("البرنامج"), "{text}");
    // The searchable PDF holds the Arabic in its text layer, in typed order.
    let o = s.ok("ocr", json!({"document": scan["document"], "save_to": "scan-searchable.pdf"}));
    assert!(o["words"].as_u64().unwrap() >= 15, "{o}");
    let found = s.ok("search", json!({"path": s.out().join("scan-searchable.pdf"), "query": "الخرسانة"}));
    assert_eq!(found["count"], json!(1), "{found}");
    let back = s.ok("read", json!({"path": s.out().join("scan-searchable.pdf"), "format": "text", "ocr": "off"}));
    let layer = back["pages"][0]["text"].as_str().unwrap();
    assert!(layer.contains("الخرسانة") && layer.contains("C30"), "{layer}");
    let at = |w: &str| layer.find(w).unwrap_or_else(|| panic!("{w} missing in {layer}"));
    assert!(at("تم") < at("الخرسانة") && at("الخرسانة") < at("الثالث"), "{layer}");
}

// ---- search, list, preview -------------------------------------------------------------------

#[test]
fn search_finds_positions_and_redact_removes_the_matches() {
    let mut s = Server::start(&[]);
    make_report(&mut s);
    let f = s.ok("search", json!({"query": "formwork"}));
    assert_eq!(f["count"], json!(2), "{f}");
    let m = &f["matches"][0];
    assert_eq!(m["page"], json!(1));
    assert!(m.to_string().contains("rect"), "{m}");
    s.ok("edit", json!({"ops": [{"op": "redact", "find": "formwork"}]}));
    let after = s.ok("search", json!({"query": "formwork"}));
    assert_eq!(after["count"], json!(0), "{after}");
    let saved = s.ok("save_document", json!({"path": "redacted.pdf"}));
    let raw = std::fs::read(saved["path"].as_str().unwrap()).unwrap();
    let reopened = s.ok("read", json!({"path": s.out().join("redacted.pdf"), "format": "text"}));
    assert!(!reopened["pages"][0]["text"].as_str().unwrap().to_lowercase().contains("formwork"));
    assert!(!raw.is_empty());
}

#[test]
fn render_preview_gives_vector_svg_with_a_selectable_text_layer_and_pngs() {
    let mut s = Server::start(&[]);
    let md: String = (1..=6).map(|i| format!("# Page {i}\n\nBody text on page {i}.\n\n\\pagebreak\n\n")).collect();
    let r = s.ok("convert", json!({"markdown": md, "path": "six.pdf"}));
    let p = s.ok("render_preview", json!({"document": r["document"], "pages": "2-4"}));
    assert_eq!(p["format"], "svg");
    let pages = p["pages"].as_array().unwrap();
    assert_eq!(pages.len(), 3);
    let svg = pages[0]["svg"].as_str().unwrap();
    assert!(svg.starts_with("<svg") || svg.contains("<svg"), "{}", &svg[..200.min(svg.len())]);
    assert!(svg.contains("viewBox"), "vector page");
    assert!(svg.contains("pdf-text-layer") && svg.contains(">Body text on page 2.<") || svg.contains("Body text on page 2"), "text layer");
    assert!(!svg.contains("<image"), "text pages are vector, not a picture");
    assert_eq!(pages[0]["page"], json!(2));
    // A small budget pages the answer.
    let small = s.ok("render_preview", json!({"pages": "all", "max_bytes": 4096}));
    assert_eq!(small["truncated"], json!(true));
    assert!(small["continue_from"].as_u64().unwrap() >= 1);
    // PNG as image blocks.
    let raw = s.raw("render_preview", json!({"pages": "1", "format": "png", "dpi": 50}));
    assert_eq!(raw["content"][1]["type"], "image");
    assert_eq!(raw["content"][1]["mimeType"], "image/png");
}

#[test]
fn list_content_reads_comments_fields_bookmarks_images_links_and_pages() {
    let mut s = Server::start(&[]);
    let r = make_report(&mut s);
    let png = s.ok("render_preview", json!({"format": "png", "dpi": 20, "out_dir": "p"}));
    std::fs::copy(png["pages"][0]["path"].as_str().unwrap(), s.inp().join("logo.png")).unwrap();
    s.ok(
        "edit",
        json!({"document": r["document"], "ops": [
            {"op": "annotate", "type": "note", "page": 1, "at": [500, 60], "contents": "Check this table"},
            {"op": "highlight", "find": "Slab concrete"},
            {"op": "add_field", "page": 1, "type": "text", "rect": [400, 700, 550, 720], "name": "inspector"},
            {"op": "bookmark", "title": "Quantities", "page": 1},
            {"op": "add_image", "page": 1, "file": "logo.png", "rect": [450, 740, 520, 780]},
        ]}),
    );
    let c = s.ok("list_content", json!({"kind": "comments"}));
    let cs = c.to_string();
    assert!(cs.contains("Check this table") && cs.contains("Test Author"), "{c}");
    let f = s.ok("list_content", json!({"kind": "fields"}));
    assert!(f.to_string().contains("inspector"), "{f}");
    let b = s.ok("list_content", json!({"kind": "bookmarks"}));
    assert!(b.to_string().contains("Quantities"), "{b}");
    let i = s.ok("list_content", json!({"kind": "images"}));
    assert_eq!(i["images"].as_array().unwrap().len(), 1, "{i}");
    let l = s.ok("list_content", json!({"kind": "links"}));
    assert!(l.to_string().contains("example.com"), "{l}");
    let p = s.ok("list_content", json!({"kind": "pages"}));
    assert_eq!(p["pages"].as_array().unwrap().len(), 1, "{p}");
    let info = s.ok("document_info", json!({}));
    assert_eq!(info["document"]["pages"], json!(1));
}

// ---- edit ----------------------------------------------------------------------------------

#[test]
fn fifty_operations_in_one_batch_and_all_or_nothing() {
    let mut s = Server::start(&[]);
    let d = s.ok("new_document", json!({"pages": 10}));
    let mut ops = Vec::new();
    for p in 1..=10 {
        ops.push(json!({"op": "add_text", "page": p, "text": format!("Page {p} heading"), "at": [72, 72], "size": 18, "bold": true}));
        ops.push(json!({"op": "annotate", "type": "note", "page": p, "at": [500, 40], "contents": format!("note {p}")}));
        ops.push(json!({"op": "bookmark", "title": format!("Section {p}"), "page": p}));
    }
    for p in [2, 4, 6, 8, 10] {
        ops.push(json!({"op": "rotate", "pages": p, "degrees": 90}));
    }
    ops.push(json!({"op": "watermark", "text": "DRAFT", "opacity": 0.2}));
    ops.push(json!({"op": "header_footer", "footer_center": "Page <<1>> of <<n>>"}));
    ops.push(json!({"op": "set_metadata", "title": "Batch built", "author": "Site team", "keywords": ["pour", "slab"]}));
    ops.push(json!({"op": "insert_blank"}));
    ops.push(json!({"op": "duplicate_pages", "pages": "1"}));
    ops.push(json!({"op": "move_pages", "pages": "12", "to": 1}));
    ops.push(json!({"op": "label_pages", "from": 1, "to": 1, "style": "lower-roman"}));
    ops.push(json!({"op": "crop", "pages": "3", "margins": [10, 10, 10, 10]}));
    ops.push(json!({"op": "highlight", "find": "Page 3 heading"}));
    ops.push(json!({"op": "add_field", "page": 2, "type": "checkbox", "rect": [72, 200, 90, 218], "name": "approved"}));
    ops.push(json!({"op": "fill_form", "values": {"approved": true}}));
    ops.push(json!({"op": "extract_pages", "pages": "2-3", "path": "extracted.pdf"}));
    ops.push(json!({"op": "page_number", "from": 2, "to": 12, "style": "decimal"}));
    ops.push(json!({"op": "annotate", "type": "rectangle", "page": 5, "rect": [100, 300, 300, 400], "contents": "area"}));
    ops.push(json!({"op": "link", "page": 1, "rect": [72, 72, 200, 90], "url": "https://example.com"}));
    assert_eq!(ops.len(), 50);
    let r = s.ok("edit", json!({"document": d["document"], "ops": ops, "save_to": "batch.pdf"}));
    assert_eq!(r["applied"], json!(50), "{r}");
    assert_eq!(r["pages"], json!(12));
    assert!(s.out().join("extracted.pdf").exists());
    assert!(s.out().join("batch.pdf").exists());
    let meta = s.ok("read", json!({"path": s.out().join("batch.pdf"), "pages": "1"}));
    assert_eq!(meta["metadata"]["title"], "Batch built");
    assert_eq!(meta["metadata"]["author"], "Site team");

    // One op fails: nothing of the batch stays.
    let before = s.ok("list_content", json!({"document": r["document"], "kind": "comments"}));
    let e = s.err(
        "edit",
        json!({"document": r["document"], "ops": [
            {"op": "delete_pages", "pages": "1-3"},
            {"op": "annotate", "type": "note", "page": 1, "at": [10, 10], "contents": "x"},
            {"op": "rotate", "pages": "1", "degrees": 45},
        ]}),
    );
    assert_eq!(e["failed_at"], json!(2), "{e}");
    assert_eq!(e["rolled_back"], json!(true));
    let after = s.ok("document_info", json!({"document": r["document"]}));
    assert_eq!(after["document"]["pages"], json!(12));
    assert_eq!(s.ok("list_content", json!({"document": r["document"], "kind": "comments"}))["comments"], before["comments"]);
    // keep_going applies what works.
    let k = s.ok(
        "edit",
        json!({"document": r["document"], "keep_going": true, "ops": [
            {"op": "rotate", "pages": "1", "degrees": 45},
            {"op": "delete_pages", "pages": "12"},
        ]}),
    );
    assert_eq!(k["applied"], json!(1));
    assert_eq!(k["failed"], json!(1));
    assert_eq!(k["pages"], json!(11));
}

#[test]
fn delete_pages_comments_fields_and_bookmarks() {
    let mut s = Server::start(&[]);
    let d = s.ok("new_document", json!({"pages": 3}));
    s.ok(
        "edit",
        json!({"document": d["document"], "ops": [
            {"op": "annotate", "type": "note", "page": 1, "at": [100, 100], "contents": "remove me"},
            {"op": "add_field", "page": 1, "type": "text", "rect": [100, 200, 300, 220], "name": "gone"},
            {"op": "bookmark", "title": "Old", "page": 2},
        ]}),
    );
    let c = s.ok("list_content", json!({"kind": "comments"}));
    let id = c["comments"][0]["id"].clone();
    let mut del = json!({"op": "delete_comment"});
    if id.is_string() {
        del["id"] = id;
    } else {
        del["page"] = json!(1);
        del["index"] = json!(1);
    }
    s.ok(
        "edit",
        json!({"ops": [del, {"op": "delete_field", "field": "gone"}, {"op": "delete_bookmark", "path": [1]}, {"op": "delete_pages", "pages": "3"}]}),
    );
    assert_eq!(s.ok("list_content", json!({"kind": "comments"}))["comments"], json!([]));
    assert!(!s.ok("list_content", json!({"kind": "fields"})).to_string().contains("gone"));
    assert!(!s.ok("list_content", json!({"kind": "bookmarks"})).to_string().contains("Old"));
    assert_eq!(s.ok("document_info", json!({}))["document"]["pages"], json!(2));
}

#[test]
fn combine_split_extract_and_old_copies() {
    let mut s = Server::start(&[]);
    for (i, n) in [(1, 2), (2, 3)] {
        let md: String = (0..n).map(|k| format!("# File {i} page {}\n\n\\pagebreak\n\n", k + 1)).collect();
        s.ok("convert", json!({"markdown": md, "path": format!("f{i}.pdf")}));
        std::fs::copy(s.out().join(format!("f{i}.pdf")), s.inp().join(format!("f{i}.pdf"))).unwrap();
    }
    let c = s.ok("combine", json!({"inputs": ["f1.pdf", "f2.pdf"], "path": "both.pdf"}));
    assert_eq!(c["pages"], json!(5), "{c}");
    let sp = s.ok("edit", json!({"document": c["document"], "ops": [{"op": "split", "every": 2, "out_dir": "parts"}]}));
    assert_eq!(std::fs::read_dir(s.out().join("parts")).unwrap().count(), 3, "{sp}");
    // An existing file is renamed OLD_ before it is replaced (the customer's file is never lost).
    std::fs::write(s.out().join("keep.pdf"), b"original").unwrap();
    let saved = s.ok("save_document", json!({"path": "keep.pdf"}));
    assert!(saved["previous_files_renamed"].is_array(), "{saved}");
    assert_eq!(std::fs::read(s.out().join("OLD_keep.pdf")).unwrap(), b"original");
    // …but not with --automation-no-old-copies (the platform keeps the OLD_ copies).
    let mut t = Server::start(&["--automation-no-old-copies"]);
    std::fs::write(t.out().join("keep.pdf"), b"original").unwrap();
    t.ok("new_document", json!({}));
    t.ok("save_document", json!({"path": "keep.pdf"}));
    assert!(!t.out().join("OLD_keep.pdf").exists());
    assert_eq!(t.ok("server_status", json!({}))["limits"]["old_copies"], json!(false));
}

#[test]
fn list_operations_names_every_operation_with_its_schema() {
    let mut s = Server::start(&[]);
    let all = s.ok("list_operations", json!({"max_bytes": 10_000_000}));
    let named: Vec<&str> = all["named"].as_array().unwrap().iter().map(|o| o["op"].as_str().unwrap()).collect();
    for want in
        ["rotate", "delete_pages", "extract_pages", "split", "fill_form", "annotate", "redact", "watermark", "add_text", "add_image", "insert_file"]
    {
        assert!(named.contains(&want), "{want}");
    }
    let one = s.ok("list_operations", json!({"name": "watermark"}));
    assert!(one["input_schema"]["properties"]["opacity"].is_object(), "{one}");
}

// ---- jobs, busy, cancel, eviction, recovery ---------------------------------------------------

#[test]
fn a_long_call_becomes_a_job_a_second_is_busy_and_cancel_stops_it() {
    let mut s = Server::start_env(&["--job-after-ms", "200"], &[("PDFCRAFT_MCP_TEST_HOOKS", "1")]);
    let r = s.request("tools/call", json!({"name": "__sleep", "arguments": {"ms": 1500}}));
    let sc = &r["result"]["structuredContent"];
    assert_eq!(sc["status"], "running", "{r}");
    let job = sc["job_id"].as_str().unwrap().to_string();
    let busy = s.request("tools/call", json!({"name": "new_document", "arguments": {}}));
    let e = &busy["result"]["structuredContent"]["error"];
    assert_eq!(e["code"], "busy", "{busy}");
    assert!(e["retry_after_ms"].as_u64().unwrap() > 0);
    assert_eq!(e["job_id"], json!(job));
    let st = s.ok("server_status", json!({}));
    assert_eq!(st["current_call"]["job_id"], json!(job));
    loop {
        let j = s.ok("job_status", json!({"job_id": job}));
        if j["status"] == "done" {
            assert_eq!(j["result"]["slept_ms"], json!(1500));
            break;
        }
        assert_eq!(j["status"], "running");
        std::thread::sleep(Duration::from_millis(100));
    }
    // cancel
    let r = s.request("tools/call", json!({"name": "__sleep", "arguments": {"ms": 60000, "async": true}}));
    let job = r["result"]["structuredContent"]["job_id"].as_str().unwrap().to_string();
    let c = s.ok("cancel", json!({"job_id": job}));
    assert_eq!(c["cancelled"], json!(true));
    for _ in 0..100 {
        let j = s.ok("job_status", json!({"job_id": job}));
        if j["status"] != "running" {
            assert_eq!(j["status"], "cancelled", "{j}");
            assert_eq!(j["error"]["code"], "cancelled");
            return;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    panic!("the cancelled job did not stop");
}

#[test]
fn evict_auto_saves_and_a_released_document_names_its_file() {
    let mut s = Server::start(&[]);
    let r = make_report(&mut s);
    s.ok("edit", json!({"ops": [{"op": "add_text", "page": 1, "text": "Unsaved line", "at": [72, 700]}]}));
    let ev = s.ok("evict", json!({}));
    let rec = ev["evicted"][0]["recovered_path"].as_str().unwrap().to_string();
    assert!(rec.contains(".recovered-") && Path::new(&rec).exists(), "{ev}");
    let e = s.err("read", json!({"document": r["document"]}));
    assert_eq!(e["code"], "evicted");
    assert_eq!(e["recovered_path"], json!(rec));
    let back = s.ok("read", json!({"path": rec, "format": "text"}));
    assert!(back["pages"][0]["text"].as_str().unwrap().contains("Unsaved line"));
}

#[test]
fn hang_up_saves_unsaved_work_and_the_next_process_reports_it() {
    let mut s = Server::start(&[]);
    make_report(&mut s);
    s.ok("edit", json!({"ops": [{"op": "add_text", "page": 1, "text": "Typed before the hang-up", "at": [72, 700]}]}));
    s.close_stdin();
    s.wait_exit();
    let found: Vec<_> = std::fs::read_dir(s.out()).unwrap().flatten().filter(|e| e.file_name().to_string_lossy().contains(".recovered-")).collect();
    assert_eq!(found.len(), 1);
    // A new process on the same write root names it.
    let dir = s.dir.path().to_path_buf();
    let mut child = Command::new(bin())
        .arg("mcp")
        .arg("--automation-write-root")
        .arg(dir.join("out"))
        .arg("--automation-read-root")
        .arg(dir.join("in"))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let mut stdin = child.stdin.take().unwrap();
    writeln!(stdin, "{}", json!({"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"server_status","arguments":{}}})).unwrap();
    drop(stdin);
    let mut out = String::new();
    std::io::Read::read_to_string(child.stdout.as_mut().unwrap(), &mut out).unwrap();
    let _ = child.wait();
    assert!(out.contains(".recovered-") && out.contains("\"found\""), "{out}");
}

#[test]
fn checkpoints_write_the_recovered_file_while_editing() {
    let mut s = Server::start(&["--checkpoint-edits", "2"]);
    make_report(&mut s);
    s.ok("edit", json!({"ops": [{"op": "add_text", "page": 1, "text": "one", "at": [72, 700]}]}));
    s.ok("edit", json!({"ops": [{"op": "add_text", "page": 1, "text": "two", "at": [72, 720]}]}));
    let st = s.ok("server_status", json!({}));
    let files = st["checkpoints"]["files"].as_array().unwrap();
    assert_eq!(files.len(), 1, "{st}");
    assert!(Path::new(files[0]["path"].as_str().unwrap()).exists());
    // Saved: the checkpoint goes.
    s.ok("save_document", json!({"path": "report-final.pdf"}));
    let st = s.ok("server_status", json!({}));
    assert_eq!(st["checkpoints"]["files"], json!([]));
}

const HOOKS: [(&str, &str); 1] = [("PDFCRAFT_MCP_TEST_HOOKS", "1")];

/// The recovered files in the write root.
/// Finished ones only: an atomic write's staging file (`.<name>.<tag>.pdfcraft-tmp`) is not one.
fn recovered_files(s: &Server) -> Vec<PathBuf> {
    std::fs::read_dir(s.out())
        .unwrap()
        .flatten()
        .map(|e| e.path())
        .filter(|p| {
            let name = p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
            name.contains(".recovered-") && name.ends_with(".pdf") && !name.starts_with('.')
        })
        .collect()
}

/// Wait up to `ms` for a recovered file to appear.
fn wait_recovered(s: &Server, ms: u64) -> Option<PathBuf> {
    for _ in 0..ms / 50 {
        if let Some(p) = recovered_files(s).into_iter().next() {
            return Some(p);
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    None
}

/// A text-layer read of a PDF, OCR off.
fn page_texts(s: &mut Server, path: &Path) -> Vec<String> {
    let r = s.ok("read", json!({"path": path, "format": "text", "ocr": "off"}));
    r["pages"].as_array().unwrap().iter().map(|p| p["text"].as_str().unwrap_or_default().trim().to_string()).collect()
}

#[test]
fn a_panic_mid_edit_puts_the_document_back_and_releases_only_it() {
    let mut s = Server::start_env(&[], &HOOKS);
    let r = make_report(&mut s);
    let doc = r["document"].clone();
    s.ok("edit", json!({"document": doc, "ops": [{"op": "add_text", "page": 1, "text": "Typed before the call", "at": [72, 700]}]}));
    let other = s.ok("new_document", json!({"name": "other.pdf"}))["document"].clone();
    s.ok("edit", json!({"document": other, "ops": [{"op": "add_text", "page": 1, "text": "Work in the other document", "at": [72, 700]}]}));
    let e = s.err(
        "edit",
        json!({"document": doc, "ops": [{"op": "add_text", "page": 1, "text": "Half of the batch", "at": [72, 650]}, {"op": "__panic"}]}),
    );
    assert_eq!(e["code"], "internal", "{e}");
    assert_eq!(e["applied"], json!(false), "{e}");
    assert!(e["reason"].as_str().unwrap().contains("Nothing of the call was applied"), "{e}");
    // Only the document the call worked on was released, auto-saved as it was before the call.
    let released = e["evicted"]["evicted"].as_array().unwrap();
    assert_eq!(released.len(), 1, "{e}");
    assert_eq!(released[0]["document"], doc, "{e}");
    let rec = PathBuf::from(released[0]["recovered_path"].as_str().unwrap());
    let text = page_texts(&mut s, &rec).join(" ");
    assert!(text.contains("Typed before the call") && !text.contains("Half of the batch"), "{text}");
    // The other document is still open with its unsaved work.
    let o = s.ok("read", json!({"document": other, "format": "text", "ocr": "off"}));
    assert!(o["pages"][0]["text"].as_str().unwrap().contains("Work in the other document"), "{o}");
    // Retrying once on the reopened file applies the batch once.
    let back = s.ok("open_document", json!({"path": rec}))["document"].clone();
    s.ok("edit", json!({"document": back, "ops": [{"op": "add_text", "page": 1, "text": "Half of the batch", "at": [72, 650]}]}));
    let t = s.ok("read", json!({"document": back, "format": "text", "ocr": "off"}))["pages"][0]["text"].as_str().unwrap().to_string();
    assert_eq!(t.matches("Half of the batch").count(), 1, "{t}");
}

#[test]
fn the_checkpoint_timer_saves_while_a_long_call_runs() {
    let mut s = Server::start_env(&["--checkpoint-secs", "1"], &HOOKS);
    make_report(&mut s);
    s.ok("edit", json!({"ops": [{"op": "add_text", "page": 1, "text": "Typed before the long call", "at": [72, 700]}]}));
    let r = s.request("tools/call", json!({"name": "__sleep", "arguments": {"ms": 6000, "async": true}}));
    let job = r["result"]["structuredContent"]["job_id"].as_str().unwrap().to_string();
    let rec = wait_recovered(&s, 4000).expect("a checkpoint while the call runs");
    assert_eq!(s.ok("job_status", json!({"job_id": job}))["status"], "running");
    let st = s.ok("server_status", json!({}));
    let listed = Path::new(st["checkpoints"]["files"][0]["path"].as_str().unwrap()).file_name().map(|n| n.to_owned());
    assert_eq!(listed.as_deref(), rec.file_name(), "{st}");
    // A whole PDF on disk already, while the call runs (the engine is busy: read from disk).
    let bytes = std::fs::read(&rec).unwrap();
    assert!(bytes.starts_with(b"%PDF-") && bytes.windows(5).any(|w| w == b"%%EOF"), "{} bytes", bytes.len());
    // Once the call is over, the engine reads the unsaved edit back from it.
    while s.ok("job_status", json!({"job_id": job}))["status"] == "running" {
        std::thread::sleep(Duration::from_millis(100));
    }
    let text = page_texts(&mut s, &rec).join(" ");
    assert!(text.contains("Typed before the long call"), "{text}");
}

#[cfg(unix)]
#[test]
fn sigterm_during_a_long_call_saves_unsaved_work_at_once() {
    let mut s = Server::start_env(&[], &HOOKS);
    make_report(&mut s);
    s.ok("edit", json!({"ops": [{"op": "add_text", "page": 1, "text": "Typed before SIGTERM", "at": [72, 700]}]}));
    s.request("tools/call", json!({"name": "__sleep", "arguments": {"ms": 60000, "async": true}}));
    let pid = s.child.id().to_string();
    assert!(Command::new("kill").args(["-TERM", &pid]).status().unwrap().success());
    let mut code = None;
    for _ in 0..200 {
        if let Some(st) = s.child.try_wait().unwrap() {
            code = st.code();
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    assert_eq!(code, Some(143), "the server did not stop at once on SIGTERM during a call");
    let found = recovered_files(&s);
    assert_eq!(found.len(), 1, "{found:?}");
    let mut t = Server::start(&[]);
    let back = t.inp().join("back.pdf");
    std::fs::copy(&found[0], &back).unwrap();
    let text = page_texts(&mut t, &back).join(" ");
    assert!(text.contains("Typed before SIGTERM"), "{text}");
}

#[test]
fn hang_up_during_a_long_call_saves_at_once_and_again_when_it_ends() {
    let mut s = Server::start_env(&[], &HOOKS);
    make_report(&mut s);
    s.ok("edit", json!({"ops": [{"op": "add_text", "page": 1, "text": "Typed before the hang-up", "at": [72, 700]}]}));
    let id = s.next;
    s.next += 1;
    s.send(&json!({"jsonrpc": "2.0", "id": id, "method": "tools/call", "params": {"name": "__sleep", "arguments": {"ms": 6000}}}));
    std::thread::sleep(Duration::from_millis(300));
    s.close_stdin();
    // Saved while the call still runs, not after it.
    let rec = wait_recovered(&s, 4000).expect("auto-saved at the hang-up");
    assert!(s.child.try_wait().unwrap().is_none(), "the running call is let finish");
    s.wait_exit();
    assert_eq!(recovered_files(&s), vec![rec.clone()], "one file per document: the final save goes over it");
    let mut t = Server::start(&[]);
    let back = t.inp().join("back.pdf");
    std::fs::copy(&rec, &back).unwrap();
    let text = page_texts(&mut t, &back).join(" ");
    assert!(text.contains("Typed before the hang-up"), "{text}");
}

/// `ocr` goes page by page: progress counts pages, each page's text is added as it is read, and
/// a cancel or a panic keeps the pages done and names the ones left (`pages` reads only those).
#[test]
fn ocr_keeps_each_page_as_it_is_read_through_cancel_and_panic() {
    if !models_present() {
        eprintln!("skipped: no OCR models (cargo xtask models)");
        return;
    }
    let mut s = Server::start_env(&[], &HOOKS);
    let md = "# Alpha Concrete Record\n\nThe slab on level one was poured on Monday.\n\n\\pagebreak\n\n# Bravo Formwork Record\n\nThe columns on level two were struck on Tuesday.\n\n\\pagebreak\n\n# Charlie Steel Record\n\nThe bars on level three were fixed on Wednesday.\n";
    let r = s.ok("convert", json!({"markdown": md, "path": "src.pdf", "font_size": 16}));
    assert_eq!(r["pages"], json!(3), "{r}");
    let shot = s.ok("render_preview", json!({"document": r["document"], "pages": "1-3", "format": "png", "dpi": 150, "out_dir": "scan"}));
    let mut inputs = Vec::new();
    for (i, p) in shot["pages"].as_array().unwrap().iter().enumerate() {
        let name = format!("scan-{}.png", i + 1);
        std::fs::copy(p["path"].as_str().unwrap(), s.inp().join(&name)).unwrap();
        inputs.push(name);
    }
    let scan = s.ok("convert", json!({"inputs": inputs, "path": "scan.pdf"}));
    let doc = scan["document"].clone();
    let has_text = |s: &mut Server, doc: &Value| -> Vec<bool> {
        let r = s.ok("read", json!({"document": doc, "format": "text", "ocr": "off"}));
        r["pages"].as_array().unwrap().iter().map(|p| !p["text"].as_str().unwrap_or_default().trim().is_empty()).collect()
    };
    assert_eq!(has_text(&mut s, &doc), [false, false, false]);

    // Cancelled after the first page: page 1 keeps its text, pages 2-3 are named as left.
    let e = s.err("ocr", json!({"document": doc, "__stop_after_pages": 1}));
    assert_eq!(e["code"], "cancelled", "{e}");
    assert_eq!(e["pages_done"], json!([1]), "{e}");
    assert_eq!(e["pages_remaining"], json!([2, 3]), "{e}");
    assert_eq!(e["continue_with"], json!({"pages": "2-3"}), "{e}");
    assert!(e["next_step"].as_str().unwrap().contains("pages: \"2-3\""), "{e}");
    assert_eq!(has_text(&mut s, &doc), [true, false, false]);

    // The rest, as a job: progress counts the pages.
    let r = s.request("tools/call", json!({"name": "ocr", "arguments": {"document": doc, "pages": "2-3", "async": true}}));
    let job = r["result"]["structuredContent"]["job_id"].as_str().unwrap().to_string();
    let done = loop {
        let j = s.ok("job_status", json!({"job_id": job}));
        if j["status"] != "running" {
            break j;
        }
        std::thread::sleep(Duration::from_millis(50));
    };
    assert_eq!(done["status"], "done", "{done}");
    assert_eq!(done["stage"], "recognising text", "{done}");
    assert_eq!(done["steps"], json!({"done": 2, "total": 2, "unit": "pages"}), "{done}");
    assert_eq!(done["result"]["pages"].as_array().unwrap().len(), 2, "{done}");
    assert_eq!(has_text(&mut s, &doc), [true, true, true]);
    // The whole recognition is one undo step per call: undo takes back pages 2-3 only.
    s.ok("edit", json!({"document": doc, "ops": [{"op": "undo"}]}));
    assert_eq!(has_text(&mut s, &doc), [true, false, false]);

    // A panic after page 2 of the rest: what was read is in the recovered file.
    let e = s.err("ocr", json!({"document": doc, "pages": "2-3", "__panic_after_pages": 1}));
    assert_eq!(e["code"], "internal", "{e}");
    assert_eq!(e["pages_done"], json!([2]), "{e}");
    assert_eq!(e["pages_remaining"], json!([3]), "{e}");
    let rec = PathBuf::from(e["evicted"]["evicted"][0]["recovered_path"].as_str().unwrap());
    let texts = page_texts(&mut s, &rec);
    assert_eq!(texts.iter().map(|t| !t.is_empty()).collect::<Vec<_>>(), [true, true, false], "{texts:?}");
}

// ---- a large file ----------------------------------------------------------------------------

#[test]
fn a_600_page_file_is_read_and_previewed_in_windows_within_bounded_memory() {
    let mut s = Server::start(&[]);
    let md: String = (1..=600)
        .map(|i| {
            format!(
                "## Clause {i}\n\n{}\n\n| Ref | Value |\n|---|---|\n| R{i} | {} |\n| S{i} | {} |\n| T{i} | {} |\n\n\\pagebreak\n\n",
                "Specification requirement text for the clause. ".repeat(30),
                i * 3,
                i * 7,
                i * 11
            )
        })
        .collect();
    let r = s.ok("convert", json!({"markdown": md, "path": "big.pdf"}));
    assert_eq!(r["pages"], json!(600), "{r}");
    s.ok("close_document", json!({"document": r["document"]}));
    std::fs::copy(s.out().join("big.pdf"), s.inp().join("big.pdf")).unwrap();
    let rss_before = s.ok("server_status", json!({}))["memory"]["rss_bytes"].as_u64().unwrap();
    let w = s.ok("read", json!({"path": "big.pdf", "pages": "300-305"}));
    assert_eq!(w["page_count"], json!(600));
    assert_eq!(w["pages"].as_array().unwrap().len(), 6);
    assert!(w["pages"][0]["text"].as_str().unwrap().contains("Clause 300"), "{}", w["pages"][0]);
    assert_eq!(w["pages"][0]["tables"], json!(1));
    let p = s.ok("render_preview", json!({"pages": "598-600"}));
    assert_eq!(p["pages"].as_array().unwrap().len(), 3);
    let f = s.ok("search", json!({"query": "Clause 599"}));
    assert_eq!(f["matches"][0]["page"], json!(599), "{f}");
    let rss = s.ok("server_status", json!({}))["memory"]["rss_bytes"].as_u64().unwrap();
    eprintln!("600 pages: rss {} MB before, {} MB after read/preview/search", rss_before >> 20, rss >> 20);
    assert!(rss < 1024 << 20, "resident memory {} MB", rss >> 20);
}

#[test]
fn pipelined_calls_run_in_the_order_they_were_sent_and_hang_up_serves_them() {
    let mut s = Server::start(&[]);
    let calls = [
        json!({"name": "new_document", "arguments": {"pages": 2}}),
        json!({"name": "edit", "arguments": {"ops": [{"op": "add_text", "page": 2, "text": "Sent second", "at": [72, 72]}]}}),
        json!({"name": "read", "arguments": {"pages": "2", "format": "text"}}),
    ];
    let first = s.next;
    for (k, c) in calls.iter().enumerate() {
        s.send(&json!({"jsonrpc": "2.0", "id": first + k as u64, "method": "tools/call", "params": c}));
    }
    s.next += calls.len() as u64;
    s.close_stdin();
    let read = s.wait_id(first + 2);
    let text = read["result"]["structuredContent"]["pages"][0]["text"].as_str().unwrap_or_default().to_string();
    assert!(text.contains("Sent second"), "{read}");
    s.wait_exit();
}

#[test]
fn a_batch_longer_than_the_undo_history_still_rolls_back_whole() {
    let mut s = Server::start(&[]);
    s.ok("new_document", json!({}));
    let mut ops: Vec<Value> =
        (0..120).map(|i| json!({"op": "add_text", "page": 1, "text": format!("line {i}"), "at": [72, 40 + (i % 60) * 12]})).collect();
    ops.push(json!({"op": "rotate", "pages": "1", "degrees": 33}));
    let e = s.err("edit", json!({"ops": ops}));
    assert_eq!(e["failed_at"], json!(120));
    assert_eq!(e["rolled_back"], json!(true));
    let r = s.ok("read", json!({"format": "text", "ocr": "off"}));
    assert_eq!(r["pages"][0]["text"], json!(""), "{r}");
    let st = s.ok("document_info", json!({}));
    // Nothing of the batch is left to undo either (a new document stays unsaved by design).
    assert_eq!(st["document"]["undo"], json!(null), "{st}");
}
