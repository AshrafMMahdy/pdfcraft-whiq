//! `render_preview`: a window of pages for the platform's preview pane — vector SVG with an
//! invisible selectable text layer (default), or PNG. Only the pages asked for are drawn.

use std::sync::Arc;

use serde_json::{Map, Value, json};

use super::contract::{WhiqError, classify, code, parse_pages};
use super::engine::{Ctx, Engine, Reply, Res, handle};
use pdfcraft_engine::DocId;
use pdfcraft_render::{RenderConfig, RenderRequest, SvgRenderer};

/// PNG resolution when none is asked for.
const DEFAULT_DPI: f64 = 110.0;

pub fn render_preview(e: &mut Engine, a: &Map<String, Value>, ctx: &Ctx) -> Res<Reply> {
    let id = e.target(a, Some("path"))?;
    let (count, name, display, config, sizes) = {
        let d = e.doc(id)?;
        let config = e
            .auto
            .session
            .render_config(d.id)
            .unwrap_or_else(|| RenderConfig { password: d.password.as_deref().map(Arc::from), ..Default::default() });
        let sizes: Vec<(f32, f32, u16)> = d.info.pages.iter().map(|p| (p.width, p.height, p.rotation)).collect();
        (d.info.pages.len(), d.name.clone(), d.display.clone(), config, sizes)
    };
    let selection = parse_pages(a.get("pages").unwrap_or(&json!(1)), count)?;
    let from = a.get("continue_from").and_then(Value::as_u64).map_or(0, |n| usize::try_from(n).unwrap_or(usize::MAX));
    if from > selection.len() {
        return Err(WhiqError::new(
            code::OUT_OF_RANGE,
            format!("continue_from {from} is past the {} page(s) asked for", selection.len()),
            "start again without continue_from",
        ));
    }
    let png = matches!(a.get("format").and_then(Value::as_str), Some("png"));
    let text_layer = a.get("text_layer").and_then(Value::as_bool).unwrap_or(true);
    let dpi = a.get("dpi").and_then(Value::as_f64).unwrap_or(DEFAULT_DPI).clamp(9.0, 1200.0);
    let budget = a.get("max_bytes").and_then(Value::as_u64).and_then(|n| usize::try_from(n).ok()).unwrap_or(e.settings.preview_max_bytes).max(4096);
    let out_dir = match a.get("out_dir").and_then(Value::as_str).filter(|s| !s.trim().is_empty()) {
        Some(d) => {
            let (dir, _) = e.resolve_output(d, true)?;
            std::fs::create_dir_all(&dir).map_err(|err| classify(&format!("{}: {err}", dir.display()), "render_preview"))?;
            Some(dir)
        }
        None => None,
    };
    let stem = std::path::Path::new(&name).file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_else(|| "page".into());
    let rest = selection.get(from..).unwrap_or_default().to_vec();
    ctx.stage("rendering pages", rest.len() as u64);
    let mut pages: Vec<Value> = Vec::new();
    let mut images: Vec<Vec<u8>> = Vec::new();
    let mut used = 400usize;
    let mut done = 0usize;
    for &p in &rest {
        ctx.check()?;
        let (w, h, rot) = sizes.get(p).copied().unwrap_or((0.0, 0.0, 0));
        let mut entry = json!({"page": p + 1, "width": w, "height": h, "rotation": rot});
        let (bytes, ext): (Vec<u8>, &str) = if png {
            let r = e.auto.renderer(DocId(id)).map_err(|err| classify(&err.to_string(), "render_preview"))?;
            let shot = r.render(RenderRequest { page: p, scale: (dpi / 72.0) as f32, ..Default::default() });
            if let Some(err) = shot.error {
                entry["error"] = json!(err);
                pages.push(entry);
                done += 1;
                continue;
            }
            let data = crate::encode_png(shot.width, shot.height, &shot.rgba).map_err(|err| classify(&err.to_string(), "render_preview"))?;
            entry["pixels"] = json!([shot.width, shot.height]);
            entry["dpi"] = json!(dpi);
            (data, "png")
        } else {
            let text = if text_layer { e.auto.page_texts(DocId(id), &[p]).ok().and_then(|mut v| v.pop()) } else { None };
            let renderer = e.svg.entry(id).or_insert_with(|| SvgRenderer::new(display.clone(), config.clone()));
            if !Arc::ptr_eq(renderer.bytes(), &display) {
                *renderer = SvgRenderer::new(display.clone(), config.clone());
            }
            match renderer.page(p, text.as_deref()) {
                Ok(svg) => (svg.svg.into_bytes(), "svg"),
                Err(err) => {
                    entry["error"] = json!(err);
                    pages.push(entry);
                    done += 1;
                    continue;
                }
            }
        };
        let n = if out_dir.is_some() {
            200
        } else if png {
            bytes.len() * 4 / 3 + 200
        } else {
            bytes.len() + 200
        };
        if !pages.is_empty() && used + n > budget {
            break;
        }
        used += n;
        match &out_dir {
            Some(dir) => {
                let file = dir.join(format!("{stem}_p{:04}.{ext}", p + 1));
                crate::paths::write_atomic(&file, &bytes).map_err(|err| classify(&err.to_string(), "render_preview"))?;
                e.wrote(&file);
                entry["path"] = json!(file.to_string_lossy());
                entry["bytes"] = json!(bytes.len());
            }
            None if png => {
                entry["image"] = json!(images.len());
                images.push(bytes);
            }
            None => {
                entry["svg"] = json!(String::from_utf8_lossy(&bytes));
            }
        }
        pages.push(entry);
        done += 1;
        ctx.step(done as u64);
    }
    let mut v = json!({"document": handle(id), "name": name, "page_count": count, "format": if png { "png" } else { "svg" }, "text_layer": !png && text_layer, "pages": pages});
    let next = from + done;
    if next < selection.len() {
        v["truncated"] = json!(true);
        v["continue_from"] = json!(next);
        v["next_page"] = json!(selection.get(next).map(|p| p + 1));
        v["total"] = json!(selection.len());
        v["next_step"] = json!(format!(
            "{} of {} pages fit in this answer; call render_preview again with the same arguments and continue_from: {next}",
            next,
            selection.len()
        ));
    }
    Ok(Reply { value: v, images })
}
