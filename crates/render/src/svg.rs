//! Pages as SVG (vector), for a preview pane: hayro-svg draws the page's paths, glyph outlines,
//! images and shadings, and [`text_layer`] lays the page's text over it as invisible, selectable
//! `<text>` (the glyphs themselves are drawn as outlines, which a browser cannot select).

use std::fmt::Write as _;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::Arc;

use hayro::hayro_syntax::Pdf;

use crate::RenderConfig;
use crate::text::PageText;

/// One page as SVG.
#[derive(Clone, Debug, PartialEq)]
pub struct SvgPage {
    /// A complete `<svg>` document, `viewBox` in points of the displayed page (y down).
    pub svg: String,
    /// The displayed page's size in points.
    pub width: f32,
    pub height: f32,
}

/// Renders pages of one document as SVG, parsing the file once.
pub struct SvgRenderer {
    bytes: Arc<Vec<u8>>,
    config: RenderConfig,
    pdf: Option<Pdf>,
}

impl SvgRenderer {
    pub fn new(bytes: Arc<Vec<u8>>, config: RenderConfig) -> Self {
        SvgRenderer { bytes, config, pdf: None }
    }

    /// The bytes this renderer draws (a caller swaps it for a new one when they change).
    pub fn bytes(&self) -> &Arc<Vec<u8>> {
        &self.bytes
    }

    /// Page `page` (0-based) as SVG, with the page's `text` laid over it when given.
    pub fn page(&mut self, page: usize, text: Option<&PageText>) -> Result<SvgPage, String> {
        if self.pdf.is_none() {
            let bytes = self.bytes.clone();
            let password = self.config.password.clone();
            self.pdf = catch_unwind(AssertUnwindSafe(|| Pdf::new_with_password(bytes, password.as_deref().unwrap_or("")).ok())).ok().flatten();
        }
        let pdf = self.pdf.as_ref().ok_or_else(|| "the PDF could not be read".to_string())?;
        let pages = pdf.pages();
        let p = pages.get(page).ok_or_else(|| format!("page {} does not exist (the document has {} pages)", page + 1, pages.len()))?;
        let settings = self.config.settings();
        let drawn = catch_unwind(AssertUnwindSafe(|| {
            let cache = hayro_svg::RenderCache::new();
            let svg = hayro_svg::convert(p, &cache, &settings, &hayro_svg::SvgRenderSettings { bg_color: [255, 255, 255, 255] });
            let (w, h) = p.render_dimensions();
            (svg, w, h)
        }))
        .map_err(|e| format!("page {} could not be drawn: {}", page + 1, crate::raster::panic_message(&e)))?;
        let (mut svg, width, height) = drawn;
        if let Some(t) = text.filter(|t| !t.is_empty()) {
            let layer = text_layer(t);
            match svg.rfind("</svg>") {
                Some(at) => svg.insert_str(at, &layer),
                None => svg.push_str(&layer),
            }
        }
        Ok(SvgPage { svg, width, height })
    }
}

fn esc(s: &str, out: &mut String) {
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            c if (c as u32) < 0x20 => out.push(' '),
            c => out.push(c),
        }
    }
}

fn num(v: f32) -> String {
    if v.is_finite() {
        let r = (v * 100.0).round() / 100.0;
        format!("{r}")
    } else {
        "0".into()
    }
}

/// The page's text as invisible `<text>` elements, one per line, each character at the place
/// its glyph is drawn, with spaces between words, so a browser selects, copies and finds it
/// where it is seen.
pub fn text_layer(t: &PageText) -> String {
    let mut out =
        String::from("<g class=\"pdf-text-layer\" fill=\"transparent\" stroke=\"none\" style=\"white-space:pre\" font-family=\"sans-serif\">");
    let mut i = 0usize;
    let n = t.glyphs.len();
    while i < n {
        let line = t.line_of.get(i).copied().unwrap_or(0);
        let mut j = i;
        while j < n && t.line_of.get(j).copied() == Some(line) {
            j += 1;
        }
        let mut xs: Vec<String> = Vec::new();
        let mut chars = String::new();
        let mut top = f32::MAX;
        let mut bottom = f32::MIN;
        let mut prev_right: Option<f32> = None;
        let mut upright = true;
        for k in i..j {
            let Some(g) = t.glyphs.get(k) else { break };
            let [x0, y0, x1, y1] = g.rect;
            if !(x0.is_finite() && y0.is_finite() && x1.is_finite() && y1.is_finite()) {
                continue;
            }
            upright &= (g.direction[0] - 1.0).abs() < 0.05 && g.direction[1].abs() < 0.05;
            if k > i && t.space_before.get(k).copied().unwrap_or(false) {
                xs.push(num(prev_right.unwrap_or(x0)));
                chars.push(' ');
            }
            let count = g.text.chars().count().max(1);
            let step = (x1 - x0) / count as f32;
            for (c_i, c) in g.text.chars().enumerate() {
                xs.push(num(x0 + step * c_i as f32));
                let mut buf = String::new();
                esc(c.encode_utf8(&mut [0; 4]), &mut buf);
                chars.push_str(&buf);
            }
            top = top.min(y0);
            bottom = bottom.max(y1);
            prev_right = Some(x1);
        }
        if !chars.trim().is_empty() && top <= bottom {
            let size = (bottom - top).max(1.0);
            // A baseline a fifth of the line's height above its bottom: where Latin glyphs sit.
            let base = bottom - size * 0.2;
            if upright {
                let _ = write!(out, "<text x=\"{}\" y=\"{}\" font-size=\"{}\">{}</text>", xs.join(" "), num(base), num(size * 0.95), chars);
            } else {
                let first = xs.first().cloned().unwrap_or_else(|| "0".into());
                let _ = write!(out, "<text x=\"{first}\" y=\"{}\" font-size=\"{}\">{}</text>", num(base), num(size * 0.95), chars);
            }
        }
        i = j.max(i + 1);
    }
    out.push_str("</g>");
    out
}
