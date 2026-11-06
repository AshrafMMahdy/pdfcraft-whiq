//! The tools `pdfcraft-cli mcp` lists in this fork: the shared craft-engine tools
//! (`server_status`, `job_status`, `list_jobs`, `cancel`, `evict`), the document tools
//! (`open_document`, `new_document`, `save_document`, `close_document`, `document_info`) and the
//! PDF tools (`read`, `render_preview`, `edit`, `list_operations`, `convert`, `combine`, `ocr`).
//! Every tool takes the common arguments `caller`, `async`, `continue_from` and `max_bytes`.

use serde_json::{Value, json};

struct Hints {
    read_only: bool,
    destructive: bool,
    idempotent: bool,
}

const RO: Hints = Hints { read_only: true, destructive: false, idempotent: true };
const WRITE: Hints = Hints { read_only: false, destructive: false, idempotent: false };
const DESTRUCTIVE: Hints = Hints { read_only: false, destructive: true, idempotent: false };
const CONTROL: Hints = Hints { read_only: false, destructive: false, idempotent: true };

fn obj(props: Value, required: &[&str]) -> Value {
    let mut p = props;
    if let Some(o) = p.as_object_mut() {
        o.insert("caller".into(), json!({"type":"string","description":"Who asks (comments and the last modifier carry it)."}));
        o.insert(
            "async".into(),
            json!({"type":"boolean","description":"Run as a job at once: the answer is {job_id, status: running}; poll job_status."}),
        );
        o.insert("continue_from".into(), json!({"type":"integer","minimum":0,"description":"Resume a trimmed answer: the continue_from it gave."}));
        o.insert("max_bytes".into(), json!({"type":"integer","minimum":1024,"description":"Largest answer in bytes (default: the server's --max-result-bytes); a longer one is cut with continue_from."}));
    }
    json!({ "type": "object", "properties": p, "required": required, "additionalProperties": false })
}

fn tool(name: &str, title: &str, desc: &str, schema: Value, h: Hints, files: Option<Value>) -> Value {
    let mut t = json!({
        "name": name,
        "title": title,
        "description": desc,
        "inputSchema": schema,
        "annotations": { "title": title, "readOnlyHint": h.read_only, "destructiveHint": h.destructive, "idempotentHint": h.idempotent, "openWorldHint": false },
    });
    if let Some(f) = files {
        t["_meta"] = json!({ "whiq/files": f });
    }
    t
}

const DOCUMENT: &str = "The document handle (doc-N, or N) from open_document / new_document / convert; default the active document.";
const PAGES: &str = "Pages: \"4\", \"2-9\", \"1,3,5-9\", \"10-\" (to the end), \"all\", \"last\", or a list of numbers. 1-based.";

fn document() -> Value {
    json!({"type":["string","integer"],"description":DOCUMENT})
}
fn pages(extra: &str) -> Value {
    json!({"type":["string","integer","array"],"items":{"type":["integer","string"]},"description":format!("{PAGES} {extra}")})
}
fn read_path(what: &str) -> Value {
    json!({"type":"string","description":format!("{what} (inside the read root; a relative path resolves there, then in the write root). Opens it as the active document, or reuses it when already open.")})
}
fn out_path(what: &str) -> Value {
    json!({"type":"string","description":format!("{what} (inside the write root; relative paths resolve there). An existing file is renamed OLD_<name> unless overwrite is true or the server runs with --automation-no-old-copies.")})
}

/// Every tool definition, in `tools/list` order.
pub fn definitions() -> Vec<Value> {
    let opens = |keys: &[&str]| Some(json!({ "read": keys, "opens": true }));
    vec![
        tool(
            "server_status",
            "Server status",
            "The engine's state: version, memory, fonts, OCR models, roots, open documents (handle, name, path, pages, dirty, active), the running call, jobs, recovered files, evicted documents, checkpoints and limits. Answered even while a call runs.",
            obj(json!({}), &[]),
            RO,
            None,
        ),
        tool(
            "job_status",
            "Job status",
            "A job's state: {job_id, tool, status: running|done|failed|cancelled, elapsed_ms, progress 0..1, stage, steps {done,total}, retry_after_ms (running), next_step, result (done) | error (failed, cancelled)}. Without job_id: every job ({jobs}).",
            obj(json!({"job_id":{"type":["string","integer"],"description":"job-N (or N) from a running answer."}}), &[]),
            RO,
            None,
        ),
        tool(
            "list_jobs",
            "List jobs",
            "Every job this process ran or runs: {jobs: [{job_id, tool, status, elapsed_ms, progress}]}.",
            obj(json!({}), &[]),
            RO,
            None,
        ),
        tool(
            "cancel",
            "Cancel a call or job",
            "Stop the running call (or job job_id). A cancelled edit leaves the document as it was before the call. Answers {cancelled, status, job_id, tool, next_step}.",
            obj(json!({"job_id":{"type":["string","integer"]}}), &[]),
            CONTROL,
            None,
        ),
        tool(
            "evict",
            "Release the documents",
            "Auto-save every document with unsaved changes to <name>.recovered-<stamp>.pdf in the write root, then close them all (the platform's memory release). Answers {evicted: [{document, name, recovered_path, original_path, dirty}]}.",
            obj(json!({}), &[]),
            CONTROL,
            None,
        ),
        tool(
            "open_document",
            "Open a PDF",
            "Open a PDF and make it the active document. Answers {document (the handle), name, path, pages, dirty, editable, encrypted, title, author}. A file of --open-job-mb or more opens as a job.",
            obj(
                json!({"path": read_path("The PDF"), "password": {"type":"string","description":"User or owner password of an encrypted PDF."}}),
                &["path"],
            ),
            WRITE,
            opens(&["path"]),
        ),
        tool(
            "new_document",
            "New blank PDF",
            "Start a new PDF of blank pages (size A4 default, Letter, A3, A5, Legal, or [width, height] in points; landscape: true turns it). Build it with edit (add_text, add_image, insert_file…) and save_document with a path.",
            obj(
                json!({"pages":{"type":"integer","minimum":1,"description":"Number of pages (default 1)."},"size":{"type":["string","array"],"items":{"type":"number"},"description":"A4 (default), Letter, Legal, A3, A5, or [width, height] in points."},"landscape":{"type":"boolean"},"name":{"type":"string","description":"File name it is saved under by default (default untitled.pdf)."}}),
                &[],
            ),
            WRITE,
            Some(json!({"read": [], "opens": true})),
        ),
        tool(
            "save_document",
            "Save the PDF",
            "Save a document: in place (an incremental update that keeps signatures valid) or to path (a full rewrite). The write is atomic. Answers {path, bytes, incremental, document}.",
            obj(
                json!({"document": document(), "path": out_path("Save as this file"), "full":{"type":"boolean","description":"Force a full rewrite (false: an incremental update)."}, "overwrite":{"type":"boolean"}}),
                &[],
            ),
            DESTRUCTIVE,
            None,
        ),
        tool(
            "close_document",
            "Close the PDF",
            "Close a document. Refused while it has unsaved changes unless discard: true (the changes are then lost).",
            obj(json!({"document": document(), "discard":{"type":"boolean","description":"Drop unsaved changes."}}), &[]),
            DESTRUCTIVE,
            None,
        ),
        tool(
            "document_info",
            "Document properties",
            "Metadata (title, author, subject, keywords, creator, producer, dates), PDF version, file size, page count and sizes, bookmarks, form fields, annotations, attachments, fonts, layers, security and repair notes of a document (or of path, opened read-only for this).",
            obj(json!({"document": document(), "path": read_path("A PDF to inspect")}), &[]),
            RO,
            opens(&["path"]),
        ),
        tool(
            "read",
            "Read a PDF",
            "Read pages as structured text, page by page in reading order: headings as #, tables as Markdown pipe tables, paragraphs (format markdown, the default), or plain lines (format text). Pages with no text layer are read by OCR automatically (ocr: auto, the default; off never; force every page) and listed in ocr_pages; OCR reads the Latin alphabet, and Arabic (mixed Arabic and Latin lines included, in typed order) when the Arabic model is installed. Every answer carries page_count and the metadata. Windowed: only the pages asked for are laid out; a long answer is cut at --max-result-bytes with continue_from (the next page number) — call again with it. Answers {document, name, page_count, metadata, pages: [{page, text, chars, tables, ocr, ocr_confidence?}], ocr_pages, continue_from?, next_page?}.",
            obj(
                json!({
                    "document": document(),
                    "path": read_path("The PDF to read"),
                    "pages": pages("Default: all, delivered in windows."),
                    "format": {"type":"string","enum":["markdown","text"],"description":"markdown (default): headings, tables and paragraphs; text: the plain reading-order lines."},
                    "ocr": {"type":"string","enum":["auto","off","force"],"description":"auto (default): OCR the pages that have no text layer; off: never; force: OCR every page asked for."},
                    "ocr_dpi": {"type":"number","minimum":72,"maximum":600,"description":"Resolution pages are recognised at (default 300)."}
                }),
                &[],
            ),
            RO,
            opens(&["path"]),
        ),
        tool(
            "search",
            "Find text",
            "Find a phrase (case-insensitive, whitespace-normalised) and get every match in page order with its page and rectangles in points from the top-left of the displayed page (as render_preview draws it), plus the line it is on. Pages without a text layer are found once ocr has made them searchable. Answers {document, query, count, matches: [{page, rects, text}], complete, next?}; pass next.start_page and next.skip back to continue. To redact the matches use edit with {op: \"redact\", find}.",
            obj(
                json!({
                    "document": document(),
                    "path": read_path("The PDF to search"),
                    "query": {"type":"string","minLength":1},
                    "limit": {"type":"integer","minimum":1,"description":"Most matches in one answer (default 500)."},
                    "start_page": {"type":"integer","minimum":1},
                    "skip": {"type":"integer","minimum":0,"description":"Matches to pass over on start_page (from next)."}
                }),
                &["query"],
            ),
            RO,
            opens(&["path"]),
        ),
        tool(
            "list_content",
            "List what a PDF holds",
            "List one kind of object of a document: comments (annotations with their author, contents, page, rect, replies, status), fields (form fields with type, value, options, page, rect), bookmarks (titles, pages, nesting), images (per page: rect, pixel size, colour space), links (page, rect, target), attachments, fonts, layers, pages (size, rotation, label). pages limits comments and images to those pages. Long lists are cut with continue_from.",
            obj(
                json!({
                    "document": document(),
                    "path": read_path("The PDF to inspect"),
                    "kind": {"type":"string","enum":["comments","fields","bookmarks","images","links","attachments","fonts","layers","pages"]},
                    "pages": pages("comments and images: only these pages (default all).")
                }),
                &["kind"],
            ),
            RO,
            opens(&["path"]),
        ),
        tool(
            "render_preview",
            "Render pages for the preview pane",
            "Render a window of pages for a preview pane, never the whole file: format svg (default) is vector — each page one self-contained SVG in points (viewBox = the displayed page) with the page's text as an invisible, selectable and searchable text layer (text_layer, default true); format png gives PNG images at dpi (default 110) as image blocks. Answers {document, page_count, format, pages: [{page, width, height, rotation, svg? | path?}], continue_from?}; a long answer is cut at max_bytes with continue_from (the next page). With out_dir the pages are written there as files (<stem>_p0001.svg / .png) and only their paths come back.",
            obj(
                json!({
                    "document": document(),
                    "path": read_path("The PDF to render"),
                    "pages": pages("Default: 1."),
                    "format": {"type":"string","enum":["svg","png"]},
                    "text_layer": {"type":"boolean","description":"SVG: add the invisible selectable text layer (default true)."},
                    "dpi": {"type":"number","minimum":9,"maximum":1200,"description":"PNG resolution (default 110)."},
                    "out_dir": {"type":"string","description":"Write each page as a file in this folder of the write root instead of answering it inline."}
                }),
                &[],
            ),
            RO,
            opens(&["path"]),
        ),
        tool(
            "edit",
            "Edit a PDF (batch)",
            "Apply many operations to one document in one call, in order: ops = [{op, …arguments}]. Operations: rotate {pages, degrees}, delete_pages {pages}, move_pages {pages, to}, extract_pages {pages, path} (a new PDF; delete: true also removes them), split {every | before | bookmarks | max_mb, out_dir}, insert_blank {at?, width?, height?}, insert_file {file, at?, pages?}, replace_pages {pages, file, from_pages?}, duplicate_pages {pages, at?}, reverse_pages {pages?}, crop {pages?, margins | rect, box?}, fill_form {values: {field: value}}, annotate {type: note|highlight|underline|strikeout|rectangle|oval|line|arrow|textbox|stamp|…, page, rect|at|find, contents, color}, redact {find | words | rect+page | pattern, apply?: true}, watermark {text | file, pages?, opacity, rotation, …}, header_footer {header_left…footer_right, pages?}, add_text {page, text, at|rect, size, bold, color, align}, add_image {page, file, rect?}, set_metadata {title?, author?, subject?, keywords?}, bookmark {title, page}, label_pages {from, to, style, prefix}, flatten {comments?, fields?}, protect {open_password?, permissions_password?, …}, ocr {pages?}, attach {file}. Any PdfCraft automation tool name is an operation too, with that tool's arguments (list_operations lists them all with their schemas); input files are always named by `file`/`files`. Atomic by default: if one op fails the document is restored as it was before the call (rolled_back) and the answer names failed_at; keep_going: true applies what it can and reports each failure. save_to saves once at the end. Answers {document, applied, results: [{index, op, status, …}], pages, dirty, failed_at?, rolled_back?, saved?}.",
            obj(
                json!({
                    "document": document(),
                    "ops": {"type":"array","minItems":1,"items":{"type":"object","properties":{"op":{"type":"string"}},"required":["op"]},"description":"The operations, in order."},
                    "keep_going": {"type":"boolean","description":"Apply the ops that work and report the failures (default false: all or nothing)."},
                    "save_to": out_path("Save the document here after the ops"),
                    "overwrite": {"type":"boolean"}
                }),
                &["ops"],
            ),
            DESTRUCTIVE,
            Some(json!({"read": ["ops[].file", "ops[].files[]"], "opens": false})),
        ),
        tool(
            "list_operations",
            "List edit operations",
            "The operations edit accepts: the named ones with the PdfCraft tool each runs, and every automation tool with its input schema (query filters by name or words of the description; name gives one operation's full schema).",
            obj(json!({"query":{"type":"string"},"name":{"type":"string"}}), &[]),
            RO,
            None,
        ),
        tool(
            "convert",
            "Make a PDF (images, HTML, Markdown, text)",
            "Make a PDF from images (PNG, JPEG, TIFF, GIF, BMP, JPEG 2000: one page each, in order), plain-text files, HTML (a file in inputs, or html) or Markdown (a .md file, or markdown): headings, paragraphs with bold/italic/code, lists, block quotes, code blocks, rules, tables with a ruled grid and a shaded header, images, links, page breaks (\\pagebreak or <div style=\"page-break-before:always\">). Arabic, Hebrew and Thai text is drawn with the embedded craft-fonts faces, right to left where it is. Several inputs are combined in order. The PDF is written to path and opened as the active document. Answers {path, pages, bytes, document, sources, notes}.",
            obj(
                json!({
                    "inputs": {"type":"array","items":{"type":"string"},"description":"Files to convert, in order (inside the read root)."},
                    "markdown": {"type":"string","description":"Markdown text to typeset."},
                    "html": {"type":"string","description":"HTML text to typeset."},
                    "path": out_path("The PDF to write"),
                    "page_size": {"type":["string","array"],"items":{"type":"number"},"description":"A4 (default), Letter, Legal, A3, A5, or [width, height] in points (HTML/Markdown/text)."},
                    "landscape": {"type":"boolean"},
                    "margin": {"type":"number","minimum":0,"maximum":288,"description":"Page margin in points (default 56)."},
                    "font_size": {"type":"number","minimum":4,"maximum":72,"description":"Body text size in points (default 10.5)."},
                    "title": {"type":"string","description":"The PDF's title (default: the first heading or the file name)."},
                    "overwrite": {"type":"boolean"}
                }),
                &["path"],
            ),
            WRITE,
            Some(json!({"read": ["inputs[]"], "opens": false})),
        ),
        tool(
            "combine",
            "Combine PDFs",
            "Combine files, in order, into one PDF at path (images and .txt are made into pages first; pages chooses each file's pages, in step with inputs: \"1-3\" or null for all). Bookmarks are kept under one per file. Opens the result as the active document. Answers {path, pages, bytes, document}.",
            obj(
                json!({
                    "inputs": {"type":"array","items":{"type":"string"},"minItems":1,"description":"The files, in order."},
                    "pages": {"type":"array","items":{"type":["string","null"]}},
                    "path": out_path("The combined PDF"),
                    "overwrite": {"type":"boolean"}
                }),
                &["inputs", "path"],
            ),
            WRITE,
            Some(json!({"read": ["inputs[]"], "opens": false})),
        ),
        tool(
            "ocr",
            "OCR to a searchable PDF",
            "Recognise the text of pages (default: every page without a text layer; skip_text_pages: false reads all) and add it as an invisible text layer over the page image, so a scanned PDF becomes searchable and selectable; the picture is unchanged. With save_to the searchable PDF is saved there (else save_document later). Answers {document, words, pages: [{page, words, text} or {page, skipped}], saved?}. Pages are read one at a time: progress counts them, and each page's text is added as soon as it is read, so a cancel or a failure keeps the pages done and answers pages_done, pages_remaining and continue_with {pages} (call ocr with those pages to read only the rest). Reads the Latin alphabet, and Arabic (with the Latin words and numbers among it) when the Arabic model is installed beside the binary, telling them apart line by line with no setting; Arabic is stored in typed order.",
            obj(
                json!({
                    "document": document(),
                    "path": read_path("The PDF to recognise"),
                    "pages": pages("Default: all."),
                    "dpi": {"type":"number","minimum":72,"maximum":600,"description":"Resolution pages are read at (default 300)."},
                    "skip_text_pages": {"type":"boolean","description":"Leave pages that already have text alone (default true)."},
                    "save_to": out_path("Save the searchable PDF here"),
                    "overwrite": {"type":"boolean"}
                }),
                &[],
            ),
            WRITE,
            opens(&["path"]),
        ),
    ]
}

/// The tool names, for `closest`.
pub fn names() -> Vec<String> {
    definitions().iter().filter_map(|d| d.get("name").and_then(Value::as_str).map(str::to_string)).collect()
}

/// One tool's definition.
pub fn find(name: &str) -> Option<Value> {
    definitions().into_iter().find(|d| d.get("name").and_then(Value::as_str) == Some(name))
}
