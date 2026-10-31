//! `edit`: many operations on one document in one call, all or nothing (or keep_going), and
//! `list_operations`, the catalogue of what an operation can be.
//!
//! An operation is a named one (`rotate`, `redact`, `fill_form`…, mapped to the automation tool
//! that does it) or any automation tool by its own name, with that tool's arguments. The
//! document is given once for the batch. Input files are named `file` (or `files`), so the
//! platform knows which arguments to stage; output files and folders resolve in the write root.

use serde_json::{Map, Value, json};

use super::contract::{WhiqError, classify, closest, code, one_based, parse_pages, squash};
use super::engine::{Ctx, Engine, Reply, Res, handle};
use pdfcraft_engine::DocId;

/// A named operation: its name, the automation tool it runs, and what it is for.
const NAMED: &[(&str, &str, &str)] = &[
    ("rotate", "page_rotate", "Rotate pages: pages (default all), degrees (multiple of 90, clockwise)."),
    ("delete_pages", "page_delete", "Delete pages."),
    ("move_pages", "page_move", "Move pages so the first lands at position to."),
    (
        "extract_pages",
        "page_extract",
        "Copy pages to a new PDF at path (delete: true also removes them here; separate: true one file per page into out_dir).",
    ),
    ("split", "doc_split", "Split into files in out_dir (default the write root): every N pages, before pages, at bookmarks, or max_mb."),
    ("insert_blank", "page_insert_blank", "Insert a blank page at position at (default: the end); width/height in points."),
    ("insert_file", "page_insert_file", "Insert the pages of file (pages: which) at position at (default: the end)."),
    ("replace_pages", "page_replace", "Replace pages with pages of file (from_pages)."),
    ("duplicate_pages", "page_duplicate", "Insert copies of pages at at (default: after them)."),
    ("reverse_pages", "page_reverse", "Reverse pages (default: the whole document)."),
    ("crop", "page_set_box", "Set the crop box (or box: trim/bleed/art/media): margins [left, bottom, right, top] or rect."),
    ("fill_form", "form_fill", "Fill form fields: values {field name: value}."),
    ("add_field", "form_add_field", "Create a form field: page, type (text, checkbox, radio, combo, list, button, date, signature), rect, name."),
    ("delete_field", "form_delete_field", "Delete a form field (field; widget for one widget)."),
    (
        "annotate",
        "comment_add",
        "Add a comment: type (note, highlight, underline, strikeout, squiggly, rectangle, oval, line, arrow, textbox, stamp, cloud, callout, ink…), page, rect | at | find, contents, color.",
    ),
    ("highlight", "comment_add", "Highlight text: find (every match) or quads, on page (optional), color."),
    ("delete_comment", "comment_delete", "Delete a comment (id, or page and index from list_content kind comments)."),
    ("redact", "redact_mark", "Redact: find | words | pattern | rect with page; apply (default true) removes the content for good."),
    ("watermark", "doc_watermark", "Watermark pages with text, or a picture from file; opacity, rotation, scale, behind."),
    ("header_footer", "doc_header_footer", "Header/footer text (header_left … footer_right, <<1>> page number tokens) on pages."),
    ("add_text", "page_add_text", "Add text to a page: page, text, at [x, y] or rect, size, bold, italic, color, align."),
    ("add_image", "page_add_image", "Add an image file to a page: page, file, rect (optional)."),
    ("set_metadata", "doc_set_info", "Set title, author, subject, keywords, creator, producer (or custom: {name: value})."),
    ("bookmark", "bookmark_add", "Add a bookmark: title, page (parent, position)."),
    ("delete_bookmark", "bookmark_delete", "Delete a bookmark: path [2, 1] = first child of the second top-level bookmark."),
    ("label_pages", "page_number", "Page labels: from, to, style (decimal, lower-roman…), prefix, start."),
    ("link", "link_add", "Add a link: page, rect, url or page target."),
    ("flatten", "doc_flatten", "Merge comments and/or form fields into the page content."),
    ("protect", "doc_protect", "Encrypt: open_password and/or permissions_password with printing/changes/copy."),
    ("unprotect", "doc_unprotect", "Remove the document's security (needs the permissions password it was opened with)."),
    ("ocr", "ocr_recognize", "Recognise text on pages (searchable invisible layer)."),
    ("attach", "attachment_add", "Attach file to the document (name, description)."),
    ("undo", "edit_undo", "Undo the last change."),
];

/// Automation tools whose `path` is a file they read (everywhere else `path` is a file written).
const INPUT_PATH_TOOLS: &[&str] = &[
    "page_insert_file",
    "page_replace",
    "page_add_image",
    "attachment_add",
    "form_set_image",
    "doc_import_data",
    "text_font_import",
    "form_merge_data",
    "accessibility_review_load",
    "doc_compare",
    "sign_document",
    "stamp_custom",
];

/// Arguments that are 1-based page lists in the automation tools.
const PAGE_LISTS: &[&str] = &["pages", "before", "from_pages"];

fn named(op: &str) -> Option<&'static str> {
    NAMED.iter().find(|(n, ..)| *n == op).map(|(_, t, _)| *t)
}

/// Spellings people use for the named operations.
fn op_alias(op: &str) -> &str {
    match op {
        "delete" | "delete_page" | "remove_pages" | "remove_page" => "delete_pages",
        "move" | "move_page" | "reorder" => "move_pages",
        "extract" | "extract_page" => "extract_pages",
        "insert" | "insert_pages" | "insert_pdf" | "merge" | "append" | "append_file" => "insert_file",
        "blank" | "insert_blank_page" | "add_page" | "add_blank_page" => "insert_blank",
        "replace" => "replace_pages",
        "duplicate" => "duplicate_pages",
        "reverse" => "reverse_pages",
        "fill" | "fill_fields" | "set_fields" | "form_fill" => "fill_form",
        "comment" | "add_comment" | "note" | "annotation" | "add_annotation" => "annotate",
        "remove_comment" | "delete_annotation" | "remove_annotation" => "delete_comment",
        "remove_field" => "delete_field",
        "remove_bookmark" => "delete_bookmark",
        "add_bookmark" => "bookmark",
        "redaction" | "redact_text" => "redact",
        "metadata" | "set_info" | "properties" => "set_metadata",
        "text" | "insert_text" => "add_text",
        "image" | "insert_image" | "picture" => "add_image",
        "page_numbers" | "labels" => "label_pages",
        "rotate_pages" | "rotation" => "rotate",
        "headers" | "footer" | "header" => "header_footer",
        "stamp_watermark" | "add_watermark" => "watermark",
        "encrypt" | "password" => "protect",
        "decrypt" => "unprotect",
        "recognize" => "ocr",
        "attachment" => "attach",
        other => other,
    }
}

pub fn edit(e: &mut Engine, a: &Map<String, Value>, ctx: &Ctx) -> Res<Reply> {
    let id = e.target(a, None)?;
    let ops = a.get("ops").and_then(Value::as_array).cloned().unwrap_or_default();
    if ops.is_empty() {
        return Err(WhiqError::invalid(
            "ops is empty",
            "give ops: [{op: \"rotate\", pages: \"1-3\", degrees: 90}, …]; list_operations shows them all",
        ));
    }
    let keep_going = a.get("keep_going").and_then(Value::as_bool).unwrap_or(false);
    let checkpoint = e.auto.session.batch_checkpoint(DocId(id));
    ctx.stage("applying operations", ops.len() as u64);
    let mut results: Vec<Value> = Vec::new();
    let mut applied = 0usize;
    let mut failed = 0usize;
    for (i, op) in ops.iter().enumerate() {
        let name = op.get("op").and_then(Value::as_str).unwrap_or("").to_string();
        if let Err(err) = ctx.check() {
            let rolled = restore(e, id, checkpoint);
            return Err(err.with("stopped_at", i).with("results", json!(results)).with("rolled_back", rolled));
        }
        let outcome = match op.as_object() {
            Some(o) => run_op(e, id, o, ctx),
            None => Err(WhiqError::invalid(format!("ops[{i}] is not an object"), "each op is {op: name, …arguments}")),
        };
        match outcome {
            Ok(v) => {
                applied += 1;
                results.push(json!({"index": i, "op": name, "status": "applied", "result": v}));
            }
            Err(err) if keep_going => {
                failed += 1;
                results.push(json!({"index": i, "op": name, "status": "failed", "error": err.object()}));
            }
            Err(err) => {
                let rolled = restore(e, id, checkpoint);
                let reason =
                    format!("op {i} ({name}) failed: {}{}", err.reason, if rolled { "; the document is as it was before this call" } else { "" });
                let mut out = WhiqError::new(err.code, reason, err.next_step.clone());
                out.details = err.details.clone();
                return Err(out.with("failed_at", i).with("results", json!(results)).with("rolled_back", rolled));
            }
        }
        ctx.step((i + 1) as u64);
    }
    let mut v = json!({"document": handle(id), "applied": applied, "failed": failed, "results": results});
    if let Some(p) = a.get("save_to").and_then(Value::as_str).filter(|s| !s.trim().is_empty()) {
        let mut sa = Map::new();
        sa.insert("document".into(), json!(handle(id)));
        sa.insert("path".into(), json!(p));
        sa.insert("overwrite".into(), a.get("overwrite").cloned().unwrap_or(json!(false)));
        let saved = e.call("save_document", &sa, ctx)?;
        v["saved"] = saved.value;
    }
    let s = e.summary(id);
    v["pages"] = s.get("pages").cloned().unwrap_or(Value::Null);
    v["dirty"] = s.get("dirty").cloned().unwrap_or(Value::Null);
    v["summary"] = s;
    Ok(v.into())
}

/// Put the document back as it was before the batch; whether that happened.
fn restore(e: &mut Engine, id: u64, checkpoint: Option<pdfcraft_engine::BatchCheckpoint>) -> bool {
    match checkpoint {
        Some(c) => e.auto.session.batch_restore(DocId(id), c).is_ok(),
        None => false,
    }
}

/// Page selections given as text or numbers made the 1-based lists the tools take.
fn fix_pages(args: &mut Map<String, Value>, count: usize, schema_props: Option<&Map<String, Value>>) -> Res<()> {
    for key in PAGE_LISTS {
        let Some(v) = args.get(*key).cloned() else { continue };
        let wants_list = schema_props.and_then(|p| p.get(*key)).and_then(|s| s.get("type")).and_then(Value::as_str) == Some("array");
        if !wants_list {
            continue;
        }
        // `from_pages` counts pages of another file: any page number is let through to the tool.
        let n = if *key == "from_pages" { usize::MAX / 2 } else { count };
        let list = parse_pages(&v, n)?;
        args.insert((*key).to_string(), json!(one_based(&list)));
    }
    // A single `page` given as text.
    if let Some(Value::String(s)) = args.get("page")
        && let Ok(n) = s.trim().parse::<u64>()
    {
        args.insert("page".into(), json!(n));
    }
    Ok(())
}

fn run_op(e: &mut Engine, id: u64, op: &Map<String, Value>, ctx: &Ctx) -> Res<Value> {
    let raw =
        op.get("op").and_then(Value::as_str).ok_or_else(|| WhiqError::invalid("an op has no `op` name", "each op is {op: name, …arguments}"))?;
    let name = squash(raw);
    let name = op_alias(&name).to_string();
    let count = e.doc(id)?.info.pages.len();
    let mut args: Map<String, Value> =
        op.iter().filter(|(k, _)| k.as_str() != "op" && k.as_str() != "caller").map(|(k, v)| (k.clone(), v.clone())).collect();
    match name.as_str() {
        "set_metadata" => return set_metadata(e, id, &args),
        "redact" => {
            let apply = args.remove("apply").and_then(|v| v.as_bool()).unwrap_or(true);
            let mut marked = run_tool(e, id, "redact_mark", args, count, ctx)?;
            if apply {
                let applied = run_tool(e, id, "redact_apply", Map::new(), count, ctx)?;
                marked["applied"] = applied;
            }
            return Ok(marked);
        }
        "highlight" => {
            args.entry("type").or_insert(json!("highlight"));
        }
        _ => {}
    }
    // A text markup found by `find` without a page: every page the text is on.
    let markup = matches!(name.as_str(), "highlight" | "annotate" | "comment_add");
    if markup && !args.contains_key("page") && args.get("find").and_then(Value::as_str).is_some() {
        let find = args.get("find").cloned().unwrap_or(Value::Null);
        let found = e.tool("text_find", json!({"doc": id, "query": find, "limit": 100_000}))?;
        let mut pages: Vec<u64> = found
            .get("matches")
            .and_then(Value::as_array)
            .map(|m| m.iter().filter_map(|x| x.get("page").and_then(Value::as_u64)).collect())
            .unwrap_or_default();
        pages.dedup();
        if pages.is_empty() {
            return Err(WhiqError::new(
                code::NOT_FOUND,
                format!("the text {find} is not in the document"),
                "search for it first (search) and pass the words exactly as they are on the page",
            ));
        }
        let mut done = Vec::new();
        for p in pages {
            let mut a = args.clone();
            a.insert("page".into(), json!(p));
            done.push(run_tool(e, id, "comment_add", a, count, ctx)?);
        }
        return Ok(json!({"pages_marked": done.len(), "results": done}));
    }
    match name.as_str() {
        "rotate" => {
            if let Some(v) = args.remove("angle").or_else(|| args.remove("rotation")) {
                args.entry("degrees").or_insert(v);
            }
        }
        "insert_file" | "insert_blank" => {
            args.entry("at").or_insert(json!(count + 1));
        }
        "extract_pages" => {
            if let Some(p) = args.remove("path") {
                args.insert("out".into(), p);
            }
        }
        "split" if !args.contains_key("out_dir") => {
            let dir = e.settings.write_root.as_ref().map_or_else(|| ".".to_string(), |r| r.to_string_lossy().into_owned());
            args.insert("out_dir".into(), json!(dir));
        }
        _ => {}
    }
    let tool = named(&name).map(str::to_string).unwrap_or(name);
    run_tool(e, id, &tool, args, count, ctx)
}

/// Run automation tool `tool` on document `id` with `args`: the document filled in, page lists
/// made lists, `file` turned into the input path, output files and folders put in the write root.
fn run_tool(e: &mut Engine, id: u64, tool: &str, mut args: Map<String, Value>, count: usize, ctx: &Ctx) -> Res<Value> {
    ctx.check()?;
    let Some(def) = crate::tools::find(tool) else {
        let mut names: Vec<&str> = NAMED.iter().map(|(n, ..)| *n).collect();
        names.extend(crate::tools::tools().iter().map(|d| d.name));
        return Err(WhiqError::new(code::NOT_FOUND, format!("no operation named {tool:?}"), "list_operations shows every operation")
            .with("closest", json!(closest(tool, names, 5))));
    };
    let props = def.input_schema.get("properties").and_then(Value::as_object).cloned();
    if props.as_ref().is_some_and(|p| p.contains_key("doc")) {
        args.insert("doc".into(), json!(id));
    }
    // Comments, measurements and stamps carry the caller (or --author) unless the op names one.
    if props.as_ref().is_some_and(|p| p.contains_key("author"))
        && !args.contains_key("author")
        && let Some(who) = e.caller.clone()
    {
        args.insert("author".into(), json!(who));
    }
    fix_pages(&mut args, count, props.as_ref())?;
    // Input files.
    if let Some(f) = args.remove("file") {
        let key = if props.as_ref().is_some_and(|p| p.contains_key("file")) { "file" } else { "path" };
        let p = f.as_str().ok_or_else(|| WhiqError::invalid("file must be a path", "name the input file"))?;
        args.insert(key.into(), json!(e.resolve_input(p)?.to_string_lossy()));
    } else if INPUT_PATH_TOOLS.contains(&tool)
        && let Some(p) = args.get("path").and_then(Value::as_str).map(str::to_string)
    {
        args.insert("path".into(), json!(e.resolve_input(&p)?.to_string_lossy()));
    }
    if let Some(Value::Array(files)) = args.remove("files") {
        let mut out = Vec::new();
        for f in files {
            if let Some(p) = f.as_str() {
                out.push(json!(e.resolve_input(p)?.to_string_lossy()));
            }
        }
        args.insert("paths".into(), json!(out));
    }
    // Output files and folders, in the write root (an existing file renamed OLD_ first).
    let overwrite = args.remove("overwrite").and_then(|v| v.as_bool()).unwrap_or(false);
    let mut renamed = Vec::new();
    let out_keys: Vec<&str> = if INPUT_PATH_TOOLS.contains(&tool) { vec!["out"] } else { vec!["out", "path"] };
    let mut written = Vec::new();
    for key in out_keys {
        if let Some(p) = args.get(key).and_then(Value::as_str).map(str::to_string)
            && props.as_ref().is_some_and(|pp| pp.contains_key(key))
        {
            let (target, r) = e.resolve_output(&p, overwrite)?;
            renamed.extend(r);
            written.push(target.clone());
            args.insert(key.into(), json!(target.to_string_lossy()));
        }
    }
    for key in ["out_dir", "folder"] {
        if let Some(p) = args.get(key).and_then(Value::as_str).map(str::to_string)
            && props.as_ref().is_some_and(|pp| pp.contains_key(key))
        {
            let (target, _) = e.resolve_output(&p, true)?;
            std::fs::create_dir_all(&target).map_err(|err| classify(&format!("{}: {err}", target.display()), tool))?;
            args.insert(key.into(), json!(target.to_string_lossy()));
        }
    }
    let mut v = e.tool(tool, Value::Object(args))?;
    for w in &written {
        e.wrote(w);
    }
    if !renamed.is_empty()
        && let Some(o) = v.as_object_mut()
    {
        o.insert("previous_files_renamed".into(), json!(renamed));
    }
    Ok(v)
}

fn set_metadata(e: &mut Engine, id: u64, args: &Map<String, Value>) -> Res<Value> {
    let mut set = Vec::new();
    for (k, key) in
        [("title", "Title"), ("author", "Author"), ("subject", "Subject"), ("keywords", "Keywords"), ("creator", "Creator"), ("producer", "Producer")]
    {
        if let Some(v) = args.get(k) {
            let value = match v {
                Value::String(s) => s.clone(),
                Value::Array(a) => a.iter().filter_map(Value::as_str).collect::<Vec<_>>().join(", "),
                other => other.to_string(),
            };
            e.tool("doc_set_info", json!({"doc": id, "key": key, "value": value}))?;
            set.push(key.to_string());
        }
    }
    if let Some(Value::Object(custom)) = args.get("custom") {
        for (k, v) in custom {
            e.tool("doc_set_info", json!({"doc": id, "key": k, "value": v.as_str().map_or_else(|| v.to_string(), str::to_string)}))?;
            set.push(k.clone());
        }
    }
    if set.is_empty() {
        return Err(WhiqError::invalid(
            "set_metadata names no field",
            "give title, author, subject, keywords, creator, producer or custom: {name: value}",
        ));
    }
    Ok(json!({"set": set}))
}

pub fn list_operations(a: &Map<String, Value>) -> Value {
    let all = crate::tools::tools();
    if let Some(n) = a.get("name").and_then(Value::as_str) {
        let n = squash(n);
        let n = op_alias(&n);
        let tool = named(n).unwrap_or(n);
        if let Some(d) = all.iter().find(|d| d.name == tool) {
            return json!({"op": n, "tool": d.name, "title": d.title, "description": d.description, "input_schema": d.input_schema, "read_only": d.read_only, "destructive": d.destructive,
                          "note": "pass the arguments beside op; doc is filled in by edit; input files go in `file`"});
        }
        let mut names: Vec<&str> = NAMED.iter().map(|(x, ..)| *x).collect();
        names.extend(all.iter().map(|d| d.name));
        return json!({"error": format!("no operation named {n:?}"), "closest": closest(n, names, 5)});
    }
    let q = a.get("query").and_then(Value::as_str).map(str::to_lowercase);
    let hit = |name: &str, desc: &str| q.as_ref().is_none_or(|q| q.split_whitespace().all(|w| name.contains(w) || desc.to_lowercase().contains(w)));
    let named_ops: Vec<Value> = NAMED.iter().filter(|(n, _, d)| hit(n, d)).map(|(n, t, d)| json!({"op": n, "tool": t, "description": d})).collect();
    let tools: Vec<Value> = all
        .iter()
        .filter(|d| !d.read_only && hit(d.name, d.description))
        .map(|d| json!({"op": d.name, "title": d.title, "description": d.description, "arguments": d.input_schema.get("properties").and_then(Value::as_object).map(|p| p.keys().filter(|k| *k != "doc").cloned().collect::<Vec<_>>()).unwrap_or_default()}))
        .collect();
    json!({"named": named_ops, "tools": tools, "next_step": "list_operations with name: <op> gives one operation's full input schema"})
}
