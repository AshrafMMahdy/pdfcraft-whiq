//! `read`: pages as structured text, in windows, with OCR for pages that have no text layer.

use std::panic::AssertUnwindSafe;
use std::sync::atomic::Ordering;

use serde_json::{Map, Value, json};

use super::contract::{WhiqError, code, one_based, page_spec, parse_pages};
use super::engine::{Ctx, Engine, Reply, Res, handle};
use pdfcraft_engine::DocId;
use pdfcraft_render::RenderRequest;

/// Pages laid out together: the body size that tells headings from text is measured over them.
const WINDOW: usize = 8;

pub fn read(e: &mut Engine, a: &Map<String, Value>, ctx: &Ctx) -> Res<Reply> {
    let id = e.target(a, Some("path"))?;
    let (count, metadata, name) = {
        let d = e.doc(id)?;
        let i = &d.info;
        (
            i.pages.len(),
            json!({"title": i.title, "author": i.author, "subject": i.subject, "keywords": i.keywords, "creator": i.creator, "producer": i.producer,
                   "pdf_version": i.pdf_version, "encrypted": i.encrypted, "tagged": i.tagged, "file_size": i.file_size,
                   "bookmarks": i.outline.len(), "form_fields": i.fields.len(), "annotations": i.annotations.len()}),
            d.name.clone(),
        )
    };
    let selection = parse_pages(a.get("pages").unwrap_or(&Value::Null), count)?;
    let from = a.get("continue_from").and_then(Value::as_u64).map_or(0, |n| usize::try_from(n).unwrap_or(usize::MAX));
    if from > selection.len() {
        return Err(WhiqError::new(
            code::OUT_OF_RANGE,
            format!("continue_from {from} is past the {} page(s) asked for", selection.len()),
            "start again without continue_from",
        ));
    }
    let markdown = !matches!(a.get("format").and_then(Value::as_str), Some("text"));
    let ocr_mode = a.get("ocr").and_then(Value::as_str).unwrap_or("auto").to_string();
    let ocr_dpi = a.get("ocr_dpi").and_then(Value::as_f64).unwrap_or(300.0).clamp(72.0, 600.0);
    let budget = a.get("max_bytes").and_then(Value::as_u64).and_then(|n| usize::try_from(n).ok()).unwrap_or(e.settings.max_result_bytes).max(1024);

    let rest = selection.get(from..).unwrap_or_default().to_vec();
    ctx.stage("reading pages", rest.len() as u64);
    let mut out_pages: Vec<Value> = Vec::new();
    let mut ocr_pages: Vec<usize> = Vec::new();
    let mut ocr_unavailable: Option<WhiqError> = None;
    let mut used = 600usize;
    let mut done = 0usize;
    // A stop after some pages (a cancel, a failure, a panic) keeps them: they are answered with
    // continue_from, as the byte budget's cut is, and `stopped` says why; a stop before any page
    // fails the call as before.
    let mut stopped: Option<(&str, Option<WhiqError>)> = None;
    let hooks = e.settings.test_hooks;
    let hook = |k: &str| if hooks { a.get(k).and_then(Value::as_u64) } else { None };
    let (stop_after, fail_after, panic_after) = (hook("__stop_after_pages"), hook("__fail_after_pages"), hook("__panic_after_pages"));
    let generation = e.doc(id)?.generation();
    let carry_key = (id, generation, ocr_dpi.to_bits());
    let mut ocr_runs = 0u64;
    for window in rest.chunks(WINDOW) {
        // Whether to go on to the next window (false: the byte budget is spent).
        let ran = std::panic::catch_unwind(AssertUnwindSafe(|| -> Res<bool> {
            ctx.check()?;
            let texts = e.auto.page_texts(DocId(id), window).map_err(|err| super::contract::classify(&err.to_string(), "read"))?;
            let md = if markdown {
                let (pages, _) = e.doc(id)?.export_pages_window(window, false);
                pdfcraft_export::markdown_pages(&pages)
            } else {
                Vec::new()
            };
            for (k, &p) in window.iter().enumerate() {
                ctx.check()?;
                if fail_after == Some(done as u64) {
                    return Err(WhiqError::failed("test hook: a failure in read", "call read again"));
                }
                if panic_after == Some(done as u64) {
                    super::engine::test_hook_panic("read");
                }
                let layer = texts.get(k);
                let has_text = layer.is_some_and(|t| t.glyphs.iter().any(|g| !g.text.trim().is_empty()));
                let want_ocr = match ocr_mode.as_str() {
                    "off" => false,
                    "force" => true,
                    _ => !has_text,
                };
                let mut entry = Map::new();
                entry.insert("page".into(), json!(p + 1));
                let mut text = if markdown { md.get(k).cloned().unwrap_or_default() } else { layer.map(|t| ordered_text(t)).unwrap_or_default() };
                if want_ocr && ocr_unavailable.is_none() {
                    let carried = e.read_carry.take_if(|c| (c.0, c.1, c.3) == carry_key && c.2 == p).map(|c| c.4);
                    let read = match carried {
                        Some(t) => Ok(t),
                        None => {
                            ocr_runs += 1;
                            ocr_page(e, id, p, ocr_dpi)
                        }
                    };
                    match read {
                        Ok(t) => {
                            text = t;
                            entry.insert("ocr".into(), json!(true));
                        }
                        Err(err) if err.code == code::NOT_AVAILABLE => ocr_unavailable = Some(err),
                        Err(err) => {
                            entry.insert("ocr_error".into(), err.object());
                        }
                    }
                }
                if markdown {
                    entry.insert("tables".into(), json!(text.lines().filter(|l| l.starts_with("|") && l.contains(" --- |")).count()));
                }
                entry.insert("chars".into(), json!(text.chars().count()));
                if !has_text && !entry.contains_key("ocr") {
                    entry.insert("no_text_layer".into(), json!(true));
                }
                entry.insert("text".into(), json!(text));
                let ocr = entry.contains_key("ocr");
                let v = Value::Object(entry);
                let n = v.to_string().len() + 1;
                if !out_pages.is_empty() && used + n > budget {
                    // Not in this answer: a page OCR'd is kept for the next call, not read twice.
                    if ocr {
                        e.read_carry = Some((carry_key.0, carry_key.1, p, carry_key.2, text));
                    }
                    return Ok(false);
                }
                if ocr {
                    ocr_pages.push(p + 1);
                }
                used += n;
                out_pages.push(v);
                done += 1;
                ctx.step(done as u64);
                if stop_after == Some(done as u64) {
                    ctx.cancel.store(true, Ordering::SeqCst);
                }
            }
            Ok(true)
        }));
        match ran {
            Ok(Ok(true)) => continue,
            Ok(Ok(false)) => {}
            Ok(Err(err)) if done == 0 => return Err(err),
            Ok(Err(err)) if err.code == code::CANCELLED => stopped = Some(("cancelled", None)),
            Ok(Err(err)) => stopped = Some(("failed", Some(err))),
            // A panic before any page is the call's (the worker releases what it opened).
            Err(p) if done == 0 => std::panic::resume_unwind(p),
            Err(p) => {
                let msg = p.downcast_ref::<&str>().map(ToString::to_string).or_else(|| p.downcast_ref::<String>().cloned()).unwrap_or_else(|| "panic".into());
                stopped = Some(("panic", Some(WhiqError::new(code::INTERNAL, format!("read failed inside the engine: {msg}"), "call read again"))));
            }
        }
        break;
    }
    let next = from + done;
    // After a panic the read's state is not to be trusted: the document is released (auto-saved
    // first if it has unsaved changes) and the OCR engine dropped, so the next read reopens the
    // file clean.
    let mut released = None;
    if let Some(("panic", Some(err))) = &mut stopped {
        e.ocr = None;
        e.read_carry = None;
        let r = e.evict(&[id], "panic");
        let gone = &r["evicted"][0];
        let file = gone["recovered_path"].as_str().or_else(|| gone["original_path"].as_str());
        err.next_step = if gone["kept_open"] == json!(true) {
            format!(
                "{} could not be released ({}); call read again with the same arguments and continue_from: {next} for the rest",
                handle(id),
                gone["error"]["reason"].as_str().unwrap_or("its unsaved changes could not be saved")
            )
        } else if let Some(file) = file {
            format!(
                "{} was closed so the next read starts clean: call read with path: {file:?} (instead of the document), the same pages and other arguments, and continue_from: {next} for the rest",
                handle(id)
            )
        } else {
            format!("{} was closed and has no file to reopen; open it again and call read with continue_from: {next} for the rest", handle(id))
        };
        released = Some(r);
    }
    let mut v = json!({
        "document": handle(id),
        "name": name,
        "page_count": count,
        "metadata": metadata,
        "format": if markdown { "markdown" } else { "text" },
        "pages": out_pages,
        "ocr_pages": ocr_pages,
    });
    if let Some(err) = ocr_unavailable {
        v["ocr_unavailable"] = err.object();
    }
    if let Some(r) = released {
        v["evicted"] = r;
    }
    if hooks && a.contains_key("__count_ocr") {
        v["__ocr_runs"] = json!(ocr_runs);
    }
    if next < selection.len() {
        v["truncated"] = json!(true);
        v["continue_from"] = json!(next);
        v["next_page"] = json!(selection.get(next).map(|p| p + 1));
        v["total"] = json!(selection.len());
        let again = format!("call read again with the same arguments and continue_from: {next} for the rest");
        v["next_step"] = json!(format!("pages {} of {} asked for were read; {again}", next, selection.len()));
        if let Some((why, err)) = stopped {
            let read = page_spec(&one_based(selection.get(from..next).unwrap_or_default()));
            let cause = err.as_ref().map_or_else(|| "the call was cancelled".to_string(), |err| err.reason.clone());
            let then = match &err {
                Some(err) if why == "panic" => err.next_step.clone(),
                Some(_) => format!("fix it or {again}"),
                None => again,
            };
            v["stopped"] = json!(why);
            if let Some(err) = err {
                v["stop_error"] = err.object();
            }
            v["next_step"] = json!(format!("pages {read} were read and are kept here; {cause}; {then}"));
        }
    }
    Ok(v.into())
}

/// The text OCR reads on page `p` (0-based), line by line.
pub fn ocr_page(e: &mut Engine, id: u64, p: usize, dpi: f64) -> Res<String> {
    let ocr = e.ocr_engine()?;
    // A scan is read at its own resolution (enlarging it only blurs the letters), at most `dpi`;
    // a page larger than a sheet is measured as if shrunk to one (Recognize Text's rule).
    let scale = {
        let d = e.doc(id)?;
        let wanted = (dpi / 72.0) as f32;
        let (w, h) = d.info.pages.get(p).map_or((612.0, 792.0), |i| (i.width, i.height));
        let fit = (842.0 / w.max(h).max(1e-3)).min(1.0);
        let native = d
            .page_images(p)
            .iter()
            .filter(|i| i.width > 1 && i.height > 1)
            .map(|i| {
                let m = i.matrix;
                let (sx, sy) = (m[0].hypot(m[1]), m[2].hypot(m[3]));
                (i.width as f64 / sx.max(1e-6)).max(i.height as f64 / sy.max(1e-6)) as f32
            })
            .reduce(f32::max);
        match native {
            Some(n) if n.is_finite() => n.clamp(fit, (wanted * fit).max(fit)),
            _ => wanted,
        }
    };
    let r = e.auto.renderer(DocId(id)).map_err(|err| super::contract::classify(&err.to_string(), "read"))?;
    let shot = r.render(RenderRequest { page: p, scale, ..Default::default() });
    if let Some(err) = shot.error {
        return Err(WhiqError::failed(format!("page {} could not be rendered for OCR: {err}", p + 1), "read the other pages; this one is damaged"));
    }
    let lines = ocr.recognize(&shot.rgba, shot.width, shot.height).map_err(|err| super::contract::classify(&err.to_string(), "read"))?;
    // Recognised lines come in the order they were found: put them in reading order (columns one
    // after another) with the same cut the text layer's reading uses. Pixel boxes are y down.
    let h = shot.height as f64;
    let rects: Vec<[f64; 4]> = lines
        .iter()
        .map(|l| {
            l.words.iter().fold([f64::MAX, f64::MAX, f64::MIN, f64::MIN], |a, w| {
                let r = w.rect;
                [a[0].min(f64::from(r[0])), a[1].min(h - f64::from(r[3])), a[2].max(f64::from(r[2])), a[3].max(h - f64::from(r[1]))]
            })
        })
        .collect();
    // A right-to-left (Arabic) page reads its columns and side-by-side lines from the right.
    let rects: Vec<[f64; 4]> =
        if lines.iter().filter(|l| l.rtl).count() * 2 > lines.len() { rects.iter().map(|r| [-r[2], r[1], -r[0], r[3]]).collect() } else { rects };
    let order = pdfcraft_export::reading_order(&rects);
    Ok(order.iter().filter_map(|&i| lines.get(i)).map(|l| l.text()).collect::<Vec<_>>().join("\n"))
}

/// The page's text line by line, the lines in reading order (columns one after another, by the
/// same cut the Markdown reading uses), each line's words as the text layer joins them.
fn ordered_text(t: &pdfcraft_render::PageText) -> String {
    let n = t.glyphs.len();
    let mut ranges: Vec<(usize, usize)> = Vec::new();
    let mut i = 0usize;
    while i < n {
        let line = t.line_of.get(i).copied();
        let mut j = i + 1;
        while j < n && t.line_of.get(j).copied() == line {
            j += 1;
        }
        ranges.push((i, j));
        i = j;
    }
    // Glyph boxes are y down; the cut wants y up.
    let rects: Vec<[f64; 4]> = ranges
        .iter()
        .map(|&(a, b)| {
            t.glyphs.get(a..b).unwrap_or_default().iter().fold([f64::MAX, f64::MAX, f64::MIN, f64::MIN], |acc, g| {
                let r = g.rect;
                [acc[0].min(f64::from(r[0])), acc[1].min(-f64::from(r[3])), acc[2].max(f64::from(r[2])), acc[3].max(-f64::from(r[1]))]
            })
        })
        .collect();
    // A right-to-left (Arabic, Hebrew) page reads its columns from the right.
    let rtl = |s: &str| {
        let r = s.chars().filter(|c| matches!(u32::from(*c), 0x0590..=0x08FF | 0xFB1D..=0xFDFF | 0xFE70..=0xFEFC) && c.is_alphabetic()).count();
        r > s.chars().filter(|c| c.is_ascii_alphabetic()).count()
    };
    let rtl_lines = ranges.iter().filter(|&&(a, b)| rtl(&t.text_of(a..b))).count();
    let rects: Vec<[f64; 4]> = if rtl_lines * 2 > ranges.len() { rects.iter().map(|r| [-r[2], r[1], -r[0], r[3]]).collect() } else { rects };
    let order = pdfcraft_export::reading_order(&rects);
    order.iter().filter_map(|&k| ranges.get(k)).map(|&(a, b)| t.text_of(a..b)).collect::<Vec<_>>().join("\n")
}
