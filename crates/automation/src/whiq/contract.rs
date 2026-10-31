//! The shared "no dead stops" shape of the White Hat IQ craft engines (CONTRACT-PARITY.md in the
//! WordCraft, GridCraft and DeckCraft forks): one error object, eighteen codes, forgiving
//! argument names, and results trimmed at their largest list with a `continue_from`.

use serde_json::{Map, Value, json};

/// The eighteen error codes, and nothing else (`contract::code::ALL` in the other forks).
pub mod code {
    pub const BUSY: &str = "busy";
    pub const CANCELLED: &str = "cancelled";
    pub const DISK_FULL: &str = "disk_full";
    pub const EVICTED: &str = "evicted";
    pub const FAILED: &str = "failed";
    pub const INTERNAL: &str = "internal";
    pub const INVALID_ARGUMENT: &str = "invalid_argument";
    pub const INVALID_REQUEST: &str = "invalid_request";
    pub const LOCKED: &str = "locked";
    pub const NO_DOCUMENT: &str = "no_document";
    pub const NOT_AVAILABLE: &str = "not_available";
    pub const NOT_FOUND: &str = "not_found";
    pub const OUT_OF_RANGE: &str = "out_of_range";
    pub const OUTSIDE_ROOT: &str = "outside_root";
    pub const PASSWORD_REQUIRED: &str = "password_required";
    pub const TOO_LARGE: &str = "too_large";
    pub const UNSUPPORTED: &str = "unsupported";
    pub const WRONG_PASSWORD: &str = "wrong_password";
    pub const ALL: [&str; 18] = [
        BUSY,
        CANCELLED,
        DISK_FULL,
        EVICTED,
        FAILED,
        INTERNAL,
        INVALID_ARGUMENT,
        INVALID_REQUEST,
        LOCKED,
        NO_DOCUMENT,
        NOT_AVAILABLE,
        NOT_FOUND,
        OUT_OF_RANGE,
        OUTSIDE_ROOT,
        PASSWORD_REQUIRED,
        TOO_LARGE,
        UNSUPPORTED,
        WRONG_PASSWORD,
    ];
}

/// A failed call: `{code, reason, next_step, …details}`.
#[derive(Clone, Debug, PartialEq)]
pub struct WhiqError {
    pub code: &'static str,
    pub reason: String,
    pub next_step: String,
    pub details: Map<String, Value>,
}

impl WhiqError {
    pub fn new(code: &'static str, reason: impl Into<String>, next_step: impl Into<String>) -> Self {
        WhiqError { code, reason: one_line(&reason.into()), next_step: one_line(&next_step.into()), details: Map::new() }
    }

    pub fn with(mut self, key: &str, value: impl Into<Value>) -> Self {
        self.details.insert(key.to_string(), value.into());
        self
    }

    pub fn invalid(reason: impl Into<String>, next_step: impl Into<String>) -> Self {
        Self::new(code::INVALID_ARGUMENT, reason, next_step)
    }

    pub fn failed(reason: impl Into<String>, next_step: impl Into<String>) -> Self {
        Self::new(code::FAILED, reason, next_step)
    }

    /// The error object (`structuredContent.error`).
    pub fn object(&self) -> Value {
        let mut o = Map::new();
        o.insert("code".into(), json!(self.code));
        o.insert("reason".into(), json!(self.reason));
        o.insert("next_step".into(), json!(self.next_step));
        for (k, v) in &self.details {
            o.insert(k.clone(), v.clone());
        }
        Value::Object(o)
    }

    /// The text block: `[code] reason` / `next_step: …`.
    pub fn text(&self, warnings: &[String]) -> String {
        let mut s = format!("[{}] {}\nnext_step: {}", self.code, self.reason, self.next_step);
        if !warnings.is_empty() {
            s.push_str("\nwarnings: ");
            s.push_str(&warnings.join("; "));
        }
        s
    }

    /// A `tools/call` result carrying this error.
    pub fn result(&self, warnings: &[String]) -> Value {
        let mut sc = json!({ "error": self.object() });
        if !warnings.is_empty() {
            sc["warnings"] = json!(warnings);
        }
        json!({ "isError": true, "content": [{ "type": "text", "text": self.text(warnings) }], "structuredContent": sc })
    }
}

/// Newlines and runs of spaces folded, so an error reads as one line.
fn one_line(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// The automation layer's message classified into a contract code, with the next step that
/// helps an agent most. `tool` names the call, for the advice.
pub fn classify(msg: &str, tool: &str) -> WhiqError {
    let lower = msg.to_ascii_lowercase();
    let has = |s: &str| lower.contains(s);
    if has("outside the read root") || has("outside the write root") || has("outside the allowed directory") || has("automation path rejected") {
        return WhiqError::new(
            code::OUTSIDE_ROOT,
            msg,
            "use a path inside the read root (files to read) or the write root (files to write); a relative path resolves inside them",
        );
    }
    if has("wrong password") || has("incorrect password") || has("password is not correct") {
        return WhiqError::new(code::WRONG_PASSWORD, msg, "pass the document's correct password in `password`");
    }
    if has("password") && (has("needs a password") || has("encrypted") || has("requires a password") || has("password required")) {
        return WhiqError::new(code::PASSWORD_REQUIRED, msg, "open it again with `password`");
    }
    if has("no space left") || has("disk full") || has("not enough space") {
        return WhiqError::new(code::DISK_FULL, msg, "free disk space in the write root, then retry the same call");
    }
    if has("being used by another process") || has("resource busy") || has("locked") && has("file") {
        return WhiqError::new(code::LOCKED, msg, "the file is open in another program: close it there, then retry").with("retry_after_ms", 2000);
    }
    if has("no such file") || has("not found") || has("cannot find") || has("does not exist") {
        return WhiqError::new(code::NOT_FOUND, msg, "check the name: list the folder or use the path the platform gave you");
    }
    if has("out of range") || has("must be between") || has("the document has") && has("page") {
        return WhiqError::new(code::OUT_OF_RANGE, msg, "use page numbers from 1 to the document's page count (read or document_info says it)");
    }
    if has("models are not installed") || has("recognition models") {
        return WhiqError::new(
            code::NOT_AVAILABLE,
            msg,
            "this installation has no OCR models beside the engine; an administrator reinstalls the engine release (it ships models/)",
        );
    }
    if has("unsaved changes") {
        return WhiqError::new(code::FAILED, msg, "save the document with save_document, or pass discard: true to drop the changes");
    }
    WhiqError::failed(
        msg,
        format!("read the reason, fix the arguments of {tool} and call it again; list_operations shows every operation's arguments"),
    )
}

/// Lower-case, `-` and spaces as `_`, camelCase split: `renderPreview` → `render_preview`.
pub fn squash(name: &str) -> String {
    let mut out = String::with_capacity(name.len() + 4);
    let mut prev_lower = false;
    for c in name.trim().chars() {
        if c.is_ascii_uppercase() {
            if prev_lower {
                out.push('_');
            }
            out.push(c.to_ascii_lowercase());
            prev_lower = false;
        } else if c == '-' || c == ' ' || c == '.' {
            out.push('_');
            prev_lower = false;
        } else {
            out.push(c);
            prev_lower = c.is_ascii_lowercase() || c.is_ascii_digit();
        }
    }
    out
}

/// Levenshtein distance, for `closest` suggestions (inputs are short names).
pub fn distance(a: &str, b: &str) -> usize {
    let b: Vec<char> = b.chars().collect();
    let mut row: Vec<usize> = (0..=b.len()).collect();
    for (i, ca) in a.chars().enumerate() {
        let mut prev = row.first().copied().unwrap_or(0);
        if let Some(first) = row.first_mut() {
            *first = i + 1;
        }
        for (j, cb) in b.iter().enumerate() {
            let cur = row.get(j + 1).copied().unwrap_or(0);
            let sub = prev + usize::from(ca != *cb);
            let del = cur + 1;
            let ins = row.get(j).copied().unwrap_or(0) + 1;
            if let Some(cell) = row.get_mut(j + 1) {
                *cell = sub.min(del).min(ins);
            }
            prev = cur;
        }
    }
    row.last().copied().unwrap_or(0)
}

/// Up to `n` candidates nearest to `wanted` (a prefix or substring counts as near).
pub fn closest<'a>(wanted: &str, candidates: impl IntoIterator<Item = &'a str>, n: usize) -> Vec<String> {
    let w = squash(wanted);
    let mut scored: Vec<(usize, String)> = candidates
        .into_iter()
        .map(|c| {
            let d = if c.contains(&w) || w.contains(c) { 0 } else { distance(&w, c) };
            (d, c.to_string())
        })
        .filter(|(d, c)| *d <= c.len().max(w.len()) / 2 + 1)
        .collect();
    scored.sort();
    scored.into_iter().take(n).map(|(_, c)| c).collect()
}

/// Short aliases every craft engine answers to.
pub fn tool_alias(name: &str) -> Option<&'static str> {
    Some(match name {
        "status" => "server_status",
        "jobs" => "list_jobs",
        "job" => "job_status",
        "stop" => "cancel",
        "open" | "open_pdf" | "doc_open" => "open_document",
        "save" | "save_pdf" | "doc_save" => "save_document",
        "close" | "close_pdf" | "doc_close" => "close_document",
        "new" | "new_pdf" | "create" => "new_document",
        "info" | "metadata" | "doc_info" => "document_info",
        "preview" | "render" | "render_page" | "render_pages" => "render_preview",
        "extract_text" | "read_text" | "text" => "read",
        "batch" | "edit_batch" | "edits" => "edit",
        "operations" | "ops" => "list_operations",
        "recognize" | "recognize_text" => "ocr",
        "merge" => "combine",
        _ => return None,
    })
}

/// Argument synonyms (`file` → `path`, `job` → `job_id`, `offset` ↔ `continue_from`), applied
/// only where the tool has the target argument and not the given one.
const SYNONYMS: &[(&str, &str)] = &[
    ("file", "path"),
    ("filename", "path"),
    ("input", "path"),
    ("source", "path"),
    ("output", "path"),
    ("out", "path"),
    ("output_path", "path"),
    ("job", "job_id"),
    ("id", "job_id"),
    ("doc", "document"),
    ("handle", "document"),
    ("offset", "continue_from"),
    ("page", "pages"),
    ("page_range", "pages"),
    ("range", "pages"),
    ("operations", "ops"),
    ("steps", "ops"),
    ("edits", "ops"),
    ("sources", "inputs"),
    ("files", "inputs"),
    ("paths", "inputs"),
    ("md", "markdown"),
];

/// The arguments with names fixed to the schema's (any case or spelling, synonyms), numbers and
/// booleans given as strings coerced, enum values matched in any case, and a list given as
/// `"a, b"` split. Unknown arguments are kept out and named in the warnings.
pub fn normalize_args(schema: &Value, args: &Map<String, Value>) -> (Map<String, Value>, Vec<String>) {
    let props = schema.get("properties").and_then(Value::as_object);
    let open = schema.get("additionalProperties").and_then(Value::as_bool).unwrap_or(false);
    let mut out = Map::new();
    let mut warnings = Vec::new();
    let Some(props) = props else { return (args.clone(), warnings) };
    for (k, v) in args {
        let sq = squash(k);
        let target = if props.contains_key(k.as_str()) {
            Some(k.clone())
        } else if props.contains_key(&sq) {
            Some(sq.clone())
        } else {
            SYNONYMS.iter().find(|(from, to)| *from == sq && props.contains_key(*to) && !args.contains_key(*to)).map(|(_, to)| (*to).to_string())
        };
        match target {
            Some(t) => {
                let v = props.get(&t).map_or_else(|| v.clone(), |p| coerce(p, v));
                out.insert(t, v);
            }
            None if open || k == "caller" || k == "async" || k == "max_bytes" || k == "continue_from" || k == "overwrite" => {
                out.insert(k.clone(), v.clone());
            }
            None => {
                warnings.push(format!("unknown argument `{k}` ignored (this tool takes: {})", props.keys().cloned().collect::<Vec<_>>().join(", ")))
            }
        }
    }
    (out, warnings)
}

fn types_of(p: &Value) -> Vec<String> {
    match p.get("type") {
        Some(Value::String(s)) => vec![s.clone()],
        Some(Value::Array(a)) => a.iter().filter_map(|t| t.as_str().map(str::to_string)).collect(),
        _ => Vec::new(),
    }
}

/// `v` made to fit `p` where the intent is plain: "3" for an integer, "true" for a boolean, an
/// enum value in another case, "a, b" for a list of strings.
fn coerce(p: &Value, v: &Value) -> Value {
    let types = types_of(p);
    let wants = |t: &str| types.iter().any(|x| x == t);
    match v {
        Value::String(s) => {
            let t = s.trim();
            if let Some(values) = p.get("enum").and_then(Value::as_array)
                && let Some(hit) = values.iter().find(|e| e.as_str().is_some_and(|e| e.eq_ignore_ascii_case(t)))
            {
                return hit.clone();
            }
            if !wants("string") {
                if wants("integer")
                    && let Ok(n) = t.parse::<i64>()
                {
                    return json!(n);
                }
                if wants("number")
                    && let Ok(n) = t.parse::<f64>()
                    && n.is_finite()
                {
                    return json!(n);
                }
                if wants("boolean") {
                    match t.to_ascii_lowercase().as_str() {
                        "true" | "yes" | "1" => return json!(true),
                        "false" | "no" | "0" => return json!(false),
                        _ => {}
                    }
                }
                if wants("array") && p.get("items").is_some_and(|i| types_of(i).iter().any(|x| x == "string")) {
                    return Value::Array(t.split(',').map(|x| json!(x.trim())).filter(|x| x.as_str().is_some_and(|s| !s.is_empty())).collect());
                }
            }
            v.clone()
        }
        _ => v.clone(),
    }
}

/// The largest array in `v` (top level or one level down), as (key path, length).
fn largest_list(v: &Value) -> Option<(Vec<String>, usize)> {
    let o = v.as_object()?;
    let mut best: Option<(Vec<String>, usize)> = None;
    for (k, x) in o {
        let cand = match x {
            Value::Array(a) => Some((vec![k.clone()], a.len())),
            Value::Object(inner) => {
                inner.iter().filter_map(|(k2, y)| y.as_array().map(|a| (vec![k.clone(), k2.clone()], a.len()))).max_by_key(|c| c.1)
            }
            _ => None,
        };
        if let Some(c) = cand
            && best.as_ref().is_none_or(|b| c.1 > b.1)
        {
            best = Some(c);
        }
    }
    best
}

/// `v` cut to fit `max` bytes at its largest list, keeping whole items (at least one, so a call
/// always moves forward): `truncated: true`, `continue_from` (the next item's index, offset by
/// `from`), `total` and a `next_step`. A result that fits is returned as it is.
pub fn trim_result(mut v: Value, max: usize, from: usize, tool: &str) -> Value {
    let size = v.to_string().len();
    if size <= max {
        return v;
    }
    let Some((path, len)) = largest_list(&v) else { return v };
    if len <= 1 {
        return v;
    }
    let pointer: String = path.iter().map(|k| format!("/{}", k.replace('~', "~0").replace('/', "~1"))).collect();
    let list = match v.pointer_mut(&pointer).and_then(Value::as_array_mut) {
        Some(a) => std::mem::take(a),
        None => return v,
    };
    let overhead = size.saturating_sub(list.iter().map(|x| x.to_string().len() + 1).sum::<usize>());
    let budget = max.saturating_sub(overhead + 200);
    let mut used = 0usize;
    let mut keep = 0usize;
    for item in &list {
        let n = item.to_string().len() + 1;
        if keep > 0 && used + n > budget {
            break;
        }
        used += n;
        keep += 1;
    }
    let kept: Vec<Value> = list.into_iter().take(keep).collect();
    if let Some(slot) = v.pointer_mut(&pointer) {
        *slot = Value::Array(kept);
    }
    if let Some(o) = v.as_object_mut() {
        o.insert("truncated".into(), json!(true));
        o.insert("continue_from".into(), json!(from + keep));
        o.insert("total".into(), json!(from + len));
        o.insert(
            "next_step".into(),
            json!(format!("the answer was cut at {} of {len} `{}` items to stay under {max} bytes: call {tool} again with the same arguments and continue_from: {}", keep, path.join("."), from + keep)),
        );
    }
    v
}

/// Parse a page selection: `"1-3, 7"`, `[1, 2, 5]`, `5`, `"all"`, `"last"`, `"3-"`, `"-5"`.
/// Returns 0-based indices in the order given (duplicates dropped). `count` is the document's page
/// count; every page must exist.
pub fn parse_pages(v: &Value, count: usize) -> Result<Vec<usize>, WhiqError> {
    let bad = |what: &str| {
        WhiqError::invalid(
            format!("cannot read {what} as pages"),
            "give pages as \"4\", \"2-9\", \"1,3,5-9\", \"all\", \"last\", \"10-\" or a list of numbers",
        )
        .with("page_count", count)
    };
    let mut out: Vec<usize> = Vec::new();
    let push = |p: usize, out: &mut Vec<usize>| -> Result<(), WhiqError> {
        if p == 0 || p > count {
            return Err(WhiqError::new(
                code::OUT_OF_RANGE,
                format!("page {p} does not exist: the document has {count} page(s)"),
                format!("use pages 1 to {count}"),
            )
            .with("page_count", count));
        }
        if !out.contains(&(p - 1)) {
            out.push(p - 1);
        }
        Ok(())
    };
    match v {
        Value::Null => return Ok((0..count).collect()),
        Value::Number(n) => push(n.as_u64().and_then(|n| usize::try_from(n).ok()).ok_or_else(|| bad(&n.to_string()))?, &mut out)?,
        Value::Array(a) => {
            for x in a {
                match x {
                    Value::Number(n) => push(n.as_u64().and_then(|n| usize::try_from(n).ok()).ok_or_else(|| bad(&n.to_string()))?, &mut out)?,
                    Value::String(_) => {
                        for p in parse_pages(x, count)? {
                            if !out.contains(&p) {
                                out.push(p);
                            }
                        }
                    }
                    _ => return Err(bad(&x.to_string())),
                }
            }
        }
        Value::String(s) => {
            let s = s.trim();
            if s.is_empty() || s.eq_ignore_ascii_case("all") {
                return Ok((0..count).collect());
            }
            for part in s.split(',').map(str::trim).filter(|p| !p.is_empty()) {
                let num = |t: &str| -> Result<usize, WhiqError> {
                    let t = t.trim();
                    if t.eq_ignore_ascii_case("last") || t.eq_ignore_ascii_case("end") {
                        return Ok(count);
                    }
                    t.parse::<usize>().map_err(|_| bad(part))
                };
                if let Some((a, b)) = part.split_once('-') {
                    let a = if a.trim().is_empty() { 1 } else { num(a)? };
                    let b = if b.trim().is_empty() { count } else { num(b)? };
                    if a > b {
                        for p in (b..=a).rev() {
                            push(p, &mut out)?;
                        }
                    } else {
                        for p in a..=b {
                            push(p, &mut out)?;
                        }
                    }
                } else {
                    push(num(part)?, &mut out)?;
                }
            }
        }
        _ => return Err(bad(&v.to_string())),
    }
    if out.is_empty() {
        return Err(bad("an empty selection"));
    }
    Ok(out)
}

/// 0-based indices as 1-based numbers.
pub fn one_based(pages: &[usize]) -> Vec<usize> {
    pages.iter().map(|p| p + 1).collect()
}

/// `[1,2,3,7]` → `"1-3,7"`.
pub fn page_spec(pages: &[usize]) -> String {
    let mut out: Vec<String> = Vec::new();
    let mut i = 0;
    while let Some(&start) = pages.get(i) {
        let mut j = i;
        while pages.get(j + 1).is_some_and(|n| *n == pages.get(j).copied().unwrap_or(0) + 1) {
            j += 1;
        }
        let end = pages.get(j).copied().unwrap_or(start);
        out.push(if j > i { format!("{start}-{end}") } else { start.to_string() });
        i = j + 1;
    }
    out.join(",")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pages_parse_every_form() {
        assert_eq!(parse_pages(&json!("1-3, 7"), 10).unwrap(), vec![0, 1, 2, 6]);
        assert_eq!(parse_pages(&json!([2, "4-5"]), 10).unwrap(), vec![1, 3, 4]);
        assert_eq!(parse_pages(&json!("all"), 3).unwrap(), vec![0, 1, 2]);
        assert_eq!(parse_pages(&json!("9-"), 10).unwrap(), vec![8, 9]);
        assert_eq!(parse_pages(&json!("last"), 4).unwrap(), vec![3]);
        assert_eq!(parse_pages(&json!(3), 4).unwrap(), vec![2]);
        assert_eq!(parse_pages(&json!("12"), 4).unwrap_err().code, code::OUT_OF_RANGE);
        assert_eq!(parse_pages(&json!("x"), 4).unwrap_err().code, code::INVALID_ARGUMENT);
        assert_eq!(page_spec(&[1, 2, 3, 7, 9, 10]), "1-3,7,9-10");
    }

    #[test]
    fn names_are_forgiven() {
        assert_eq!(squash("renderPreview"), "render_preview");
        assert_eq!(squash("render-preview"), "render_preview");
        let schema = json!({"type":"object","properties":{"path":{"type":"string"},"dpi":{"type":"number"},"text_layer":{"type":"boolean"},"format":{"type":"string","enum":["svg","png"]}}});
        let args = json!({"file":"a.pdf","DPI":"150","textLayer":"false","format":"SVG","bogus":1});
        let (out, warnings) = normalize_args(&schema, args.as_object().unwrap());
        assert_eq!(out.get("path"), Some(&json!("a.pdf")));
        assert_eq!(out.get("dpi"), Some(&json!(150.0)));
        assert_eq!(out.get("text_layer"), Some(&json!(false)));
        assert_eq!(out.get("format"), Some(&json!("svg")));
        assert_eq!(warnings.len(), 1);
        assert_eq!(closest("rendr_preview", ["render_preview", "read", "edit"], 2).first().map(String::as_str), Some("render_preview"));
    }

    #[test]
    fn a_long_result_is_cut_at_its_largest_list_and_moves_forward() {
        let v = json!({"document":"doc-1","pages": (0..50).map(|i| json!({"page": i + 1, "text": "x".repeat(100)})).collect::<Vec<_>>() });
        let t = trim_result(v, 1000, 10, "read");
        assert_eq!(t["truncated"], json!(true));
        let kept = t["pages"].as_array().unwrap().len();
        assert!((1..50).contains(&kept));
        assert_eq!(t["continue_from"], json!(10 + kept));
        assert_eq!(t["total"], json!(60));
    }

    #[test]
    fn every_error_code_is_one_of_the_eighteen() {
        for msg in [
            "x is outside the read root /a",
            "no such file",
            "page 9 is out of range",
            "something else",
            "wrong password",
            "the text recognition models are not installed",
        ] {
            assert!(code::ALL.contains(&classify(msg, "read").code), "{msg}");
        }
    }
}
