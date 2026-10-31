//! Arabic text lines: a second recognition model that reads the Arabic script (and the Latin
//! letters, digits and signs written among it), run on the lines ocrs's detector finds.
//!
//! The model is PaddleOCR's Arabic text-line recogniser (Apache-2.0), exported to ONNX and run
//! by rten like the ocrs models. It takes a line image 48 pixels high and gives, per column of
//! eight pixels, a probability for each of its characters and a blank (CTC). It reads the line
//! as it is drawn, left to right, so its output is in visual order; [`crate::bidi`] turns it
//! into the order the text is typed in.

use std::path::Path;

use rten_imageproc::{PointF, RotatedRect};
use rten_tensor::NdTensor;
use rten_tensor::prelude::*;

use crate::OcrError;

/// The model file and its character list (one per line, in the model's class order after the
/// blank; the space comes last and is not listed).
pub const ARABIC_MODEL: &str = "arabic-recognition.onnx";
pub const ARABIC_CHARSET: &str = "arabic-recognition.txt";

/// The model's input height and its widest exported input.
const HEIGHT: usize = 48;
const MAX_WIDTH: usize = 3200;
const MIN_WIDTH: usize = 16;
/// Lines whose inputs are about as wide run as one batch (they are padded to the widest).
const BATCH_STEP: usize = 64;
const BATCH_SPREAD: f32 = 1.3;
const MAX_BATCH: usize = 16;
/// The most characters a class name may have (the list is untrusted input).
const MAX_CLASSES: usize = 4096;

/// Space added around a line before it is read, in line heights: above and below for the dots
/// the detector's word boxes leave out, at the ends for the first and last letters' tails. The
/// model reads best with little room above and below (more shrinks the letters in its 48
/// pixels); a line it reads unsurely is read again with [`TALL`] room, for vowel marks.
const PAD_ABOVE: f32 = 0.1;
const PAD_BELOW: f32 = 0.1;
const PAD_ENDS: f32 = 0.2;
pub(crate) const TALL: (f32, f32) = (0.4, 0.3);

pub(crate) struct ArabicModel {
    model: rten::Model,
    /// Class `i + 1` is `classes[i]`; class 0 is the CTC blank.
    classes: Vec<String>,
}

/// A line's frame on the page: a strip along the line, `length` × `height` image pixels, from
/// its top-left corner `origin`, with unit vectors `across` (as the line is drawn, left to
/// right) and `down`. The line's own ink lies between `ink_top` and `ink_bottom` (distances
/// from the top edge).
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct Frame {
    pub origin: [f32; 2],
    pub across: [f32; 2],
    pub down: [f32; 2],
    pub length: f32,
    pub height: f32,
    pub ink_top: f32,
    pub ink_bottom: f32,
}

impl Frame {
    /// The frame of a line of word boxes, padded for the marks above and below.
    ///
    /// The line runs through the words' centres (a skewed scan's lines slope); a single word
    /// keeps its own slope. Single boxes around Arabic words often lean a little (their tails and
    /// dots pull a tight box round), so their own axes are not trusted beyond that.
    pub fn of_line(words: &[RotatedRect]) -> Option<Frame> {
        Self::padded(words, (PAD_ABOVE, PAD_BELOW))
    }

    /// The same with `(above, below)` room, in line heights.
    pub fn padded(words: &[RotatedRect], (pad_above, pad_below): (f32, f32)) -> Option<Frame> {
        let first = words.first()?;
        // A box's extent across the line: its height when it stands upright, else its width.
        let tall = |w: &RotatedRect| if w.up_axis().y.abs() >= w.up_axis().x.abs() { w.height() } else { w.width() };
        // The slope: a least-squares line through the centres, weighted by word width.
        let (mut sw, mut sx, mut sy) = (0.0f32, 0.0f32, 0.0f32);
        for w in words {
            let (c, k) = (w.center(), w.width().max(w.height()).max(1.0));
            sw += k;
            sx += k * c.x;
            sy += k * c.y;
        }
        let (mx, my) = (sx / sw, sy / sw);
        let (mut sxx, mut sxy) = (0.0f32, 0.0f32);
        for w in words {
            let (c, k) = (w.center(), w.width().max(w.height()).max(1.0));
            sxx += k * (c.x - mx) * (c.x - mx);
            sxy += k * (c.x - mx) * (c.y - my);
        }
        let spread = words.iter().map(|w| w.center().x).fold(f32::MIN, f32::max) - words.iter().map(|w| w.center().x).fold(f32::MAX, f32::min);
        let height = words.iter().map(tall).fold(0.0f32, f32::max);
        let slope = if words.len() > 1 && spread > 2.0 * height && sxx > 0.0 {
            sxy / sxx
        } else {
            let u = first.up_axis();
            if u.y.abs() > 1e-6 { -u.x / u.y } else { 0.0 }
        };
        // Pages are turned upright before they are read; more than this is not a line's slope.
        let slope = if slope.is_finite() { slope.clamp(-0.27, 0.27) } else { 0.0 };
        let n = (1.0 + slope * slope).sqrt();
        let across = [1.0 / n, slope / n];
        let down = [-across[1], across[0]];
        let (mut umin, mut umax, mut vmin, mut vmax) = (f32::MAX, f32::MIN, f32::MAX, f32::MIN);
        for w in words {
            for p in w.corners() {
                let u = dot([p.x, p.y], across);
                umin = umin.min(u);
                umax = umax.max(u);
            }
            // Height from the box's centre and its own height, not its corners: a leaning box's
            // corners would make the line look taller than it is.
            let (c, h) = (w.center(), tall(w) / 2.0);
            let v = dot([c.x, c.y], down);
            vmin = vmin.min(v - h);
            vmax = vmax.max(v + h);
        }
        let h = vmax - vmin;
        if !(h.is_finite() && h > 1.0 && (umax - umin).is_finite() && umax > umin) {
            return None;
        }
        let (above, below, ends) = (h * pad_above, h * pad_below, h * PAD_ENDS);
        let (u0, v0) = (umin - ends, vmin - above);
        Some(Frame {
            origin: [across[0] * u0 + down[0] * v0, across[1] * u0 + down[1] * v0],
            across,
            down,
            length: umax - umin + 2.0 * ends,
            height: h + above + below,
            ink_top: above,
            ink_bottom: above + h,
        })
    }

    /// The image point `u` along and `v` down from the frame's corner.
    pub fn point(&self, u: f32, v: f32) -> [f32; 2] {
        [self.origin[0] + self.across[0] * u + self.down[0] * v, self.origin[1] + self.across[1] * u + self.down[1] * v]
    }

    /// How far along the frame the image point `p` is.
    pub fn along(&self, p: [f32; 2]) -> f32 {
        dot([p[0] - self.origin[0], p[1] - self.origin[1]], self.across)
    }

    /// The span along the frame that a word box covers.
    pub fn span(&self, word: &RotatedRect) -> (f32, f32) {
        word.corners().iter().map(|p: &PointF| self.along([p.x, p.y])).fold((f32::MAX, f32::MIN), |(lo, hi), u| (lo.min(u), hi.max(u)))
    }

    /// The axis-aligned image box `[left, top, right, bottom]` of the ink between `u0` and `u1`.
    pub fn ink_box(&self, u0: f32, u1: f32) -> [f32; 4] {
        let pts = [self.point(u0, self.ink_top), self.point(u1, self.ink_top), self.point(u0, self.ink_bottom), self.point(u1, self.ink_bottom)];
        pts.iter().fold([f32::MAX, f32::MAX, f32::MIN, f32::MIN], |b, p| [b[0].min(p[0]), b[1].min(p[1]), b[2].max(p[0]), b[3].max(p[1])])
    }
}

fn dot(a: [f32; 2], b: [f32; 2]) -> f32 {
    a[0] * b[0] + a[1] * b[1]
}

/// One character the model read: its text, where it is along the frame (image pixels from the
/// frame's left end) and how sure the model is (0–1).
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Glyph {
    pub text: String,
    pub start: f32,
    pub end: f32,
    pub prob: f32,
}

impl Glyph {
    pub fn centre(&self) -> f32 {
        (self.start + self.end) / 2.0
    }
}

/// A greyscale page image, one byte per pixel.
pub(crate) struct Grey<'a> {
    pub px: &'a [u8],
    pub width: usize,
    pub height: usize,
}

impl Grey<'_> {
    /// The grey level at `(x, y)` (pixel centres at `.5`), bilinear; white outside the image.
    fn sample(&self, x: f32, y: f32) -> f32 {
        let (x, y) = (x - 0.5, y - 0.5);
        let (x0, y0) = (x.floor(), y.floor());
        let (fx, fy) = (x - x0, y - y0);
        let at = |xi: f32, yi: f32| -> f32 {
            if xi < 0.0 || yi < 0.0 || xi >= self.width as f32 || yi >= self.height as f32 {
                return 255.0;
            }
            self.px.get(yi as usize * self.width + xi as usize).map_or(255.0, |v| f32::from(*v))
        };
        let top = at(x0, y0) * (1.0 - fx) + at(x0 + 1.0, y0) * fx;
        let bottom = at(x0, y0 + 1.0) * (1.0 - fx) + at(x0 + 1.0, y0 + 1.0) * fx;
        top * (1.0 - fy) + bottom * fy
    }
}

/// A line prepared for the model: its pixels (`HEIGHT` rows of `width`, normalised to -1…1) and
/// the image pixels per input column along the frame.
struct Strip {
    px: Vec<f32>,
    width: usize,
    step: f32,
}

fn strip(img: &Grey, f: &Frame) -> Strip {
    let s = f.height / HEIGHT as f32;
    let width = ((f.length / s).round() as usize).clamp(MIN_WIDTH, MAX_WIDTH);
    let step = f.length / width as f32;
    // Average a few samples per input pixel when the line is shrunk, so thin strokes survive.
    let (nu, nv) = ((step.ceil() as usize).clamp(1, 4), (s.ceil() as usize).clamp(1, 4));
    let mut px = vec![0.0f32; HEIGHT * width];
    for (i, row) in px.chunks_mut(width).enumerate() {
        for (j, out) in row.iter_mut().enumerate() {
            let mut sum = 0.0;
            for a in 0..nv {
                for b in 0..nu {
                    let v = (i as f32 + (a as f32 + 0.5) / nv as f32) * s;
                    let u = (j as f32 + (b as f32 + 0.5) / nu as f32) * step;
                    let [x, y] = f.point(u, v);
                    sum += img.sample(x, y);
                }
            }
            *out = sum / (nu * nv) as f32 / 127.5 - 1.0;
        }
    }
    Strip { px, width, step }
}

impl ArabicModel {
    pub fn load(model: &Path, charset: &Path) -> Result<ArabicModel, OcrError> {
        let list = std::fs::read_to_string(charset).map_err(|e| OcrError::Load(charset.display().to_string(), e.to_string()))?;
        let mut classes: Vec<String> = list.lines().map(normalise_class).collect();
        if classes.is_empty() || classes.len() > MAX_CLASSES {
            return Err(OcrError::Load(charset.display().to_string(), "not a character list".into()));
        }
        classes.push(" ".into());
        let model = rten::Model::load_file(model).map_err(|e| OcrError::Load(model.display().to_string(), e.to_string()))?;
        Ok(ArabicModel { model, classes })
    }

    /// Read each line (a frame on `img`); `None` for a line the model found nothing in.
    pub fn read(&self, img: &Grey, frames: &[Frame]) -> Result<Vec<Vec<Glyph>>, OcrError> {
        let strips: Vec<Strip> = frames.iter().map(|f| strip(img, f)).collect();
        let mut order: Vec<usize> = (0..strips.len()).collect();
        order.sort_by_key(|&i| strips.get(i).map_or(0, |s| s.width));
        let mut out: Vec<Vec<Glyph>> = vec![Vec::new(); strips.len()];
        let mut k = 0;
        while k < order.len() {
            // A batch: up to MAX_BATCH lines (shortest first) no wider than BATCH_SPREAD times
            // the first, padded to the widest.
            let width_of = |i: Option<&usize>| i.and_then(|i| strips.get(*i)).map_or(0, |s| s.width);
            let limit = (width_of(order.get(k)) as f32 * BATCH_SPREAD) as usize + BATCH_STEP;
            let mut end = k;
            while end < order.len() && end - k < MAX_BATCH && width_of(order.get(end)) <= limit {
                end += 1;
            }
            let batch: Vec<usize> = order.get(k..end).map(<[usize]>::to_vec).unwrap_or_default();
            k = end.max(k + 1);
            let width = batch.iter().filter_map(|&i| strips.get(i)).map(|s| s.width).max().unwrap_or(MIN_WIDTH);
            // NCHW, the grey strip in all three channels, each row padded to the batch's width
            // with 0 (mid grey, as the model was trained).
            let mut data = vec![0.0f32; batch.len() * 3 * HEIGHT * width];
            let mut rows = data.chunks_mut(width);
            for &i in &batch {
                let strip_rows = strips.get(i).map(|s| s.px.chunks(s.width.max(1)).collect::<Vec<_>>()).unwrap_or_default();
                for _ in 0..3 {
                    for y in 0..HEIGHT {
                        if let (Some(out), Some(src)) = (rows.next(), strip_rows.get(y)) {
                            let n = src.len().min(out.len());
                            if let (Some(o), Some(s)) = (out.get_mut(..n), src.get(..n)) {
                                o.copy_from_slice(s);
                            }
                        }
                    }
                }
            }
            let input = NdTensor::<f32, 4>::try_from_data([batch.len(), 3, HEIGHT, width], data).map_err(|e| OcrError::Recognize(e.to_string()))?;
            let value = self.model.run_one(input.into(), None).map_err(|e| OcrError::Recognize(e.to_string()))?;
            let probs: NdTensor<f32, 3> = value.try_into().map_err(|_| OcrError::Recognize("unexpected Arabic model output".into()))?;
            let [n, steps, classes] = probs.shape();
            if n != batch.len() || classes != self.classes.len() + 1 || steps == 0 {
                return Err(OcrError::Recognize(format!(
                    "the Arabic model gives {classes} classes; its character list has {}",
                    self.classes.len() + 1
                )));
            }
            let stride = width as f32 / steps as f32;
            for (b, &i) in batch.iter().enumerate() {
                let Some(s) = strips.get(i) else { continue };
                // Columns past the line's own width are padding.
                let valid = ((s.width as f32 / stride).ceil() as usize).min(steps);
                let line = probs.slice(b);
                let mut glyphs: Vec<Glyph> = Vec::new();
                let mut prev = 0usize;
                for t in 0..valid {
                    let row = line.slice(t);
                    let (best, p) = row.iter().enumerate().fold((0usize, f32::MIN), |acc, (c, &p)| if p > acc.1 { (c, p) } else { acc });
                    let (start, end) = (t as f32 * stride * s.step, (t + 1) as f32 * stride * s.step);
                    if best != 0 && best == prev {
                        if let Some(g) = glyphs.last_mut() {
                            g.end = end;
                            g.prob = g.prob.max(p);
                        }
                    } else if best != 0
                        && let Some(text) = self.classes.get(best - 1)
                    {
                        glyphs.push(Glyph { text: text.clone(), start, end, prob: p });
                    }
                    prev = best;
                }
                if let Some(slot) = out.get_mut(i) {
                    *slot = glyphs;
                }
            }
        }
        Ok(out)
    }
}

/// A class name as text: the Arabic presentation forms (one shaped letter, or a ligature such as
/// ﷲ) become the letters they stand for, so the text layer and `read` hold plain letters.
fn normalise_class(name: &str) -> String {
    use unicode_normalization::UnicodeNormalization;
    name.chars()
        .flat_map(|c| {
            let presentation = matches!(u32::from(c), 0xFB50..=0xFDFF | 0xFE70..=0xFEFF);
            if presentation { c.nfkc().collect::<Vec<_>>() } else { vec![c] }
        })
        .collect()
}

/// Arabic letters (not digits, marks or punctuation): what tells an Arabic line from a Latin one.
pub(crate) fn is_arabic_letter(c: char) -> bool {
    matches!(u32::from(c), 0x0620..=0x063F | 0x0641..=0x064A | 0x066E..=0x066F | 0x0671..=0x06D3 | 0x06D5 | 0x06EE..=0x06EF | 0x06FA..=0x06FF | 0x0750..=0x077F | 0x08A0..=0x08C9 | 0xFB50..=0xFDFF | 0xFE70..=0xFEFC)
}

#[cfg(test)]
mod tests {
    use super::*;
    use rten_imageproc::Vec2;

    #[test]
    fn presentation_forms_become_letters() {
        assert_eq!(normalise_class("ﷲ"), "الله");
        assert_eq!(normalise_class("ﭙ"), "\u{067E}");
        assert_eq!(normalise_class("ب"), "ب");
        // Other compatibility characters keep their own form.
        assert_eq!(normalise_class("²"), "²");
    }

    #[test]
    fn a_frame_pads_the_line_and_maps_back_to_the_page() {
        // Two upright word boxes on one line: 100..200 and 220..300 across, 50..80 down.
        let w = |cx: f32, width: f32| RotatedRect::new(PointF::from_yx(65.0, cx), Vec2::from_yx(-1.0, 0.0), width, 30.0);
        let f = Frame::of_line(&[w(150.0, 100.0), w(260.0, 80.0)]).unwrap();
        assert!((f.across[0] - 1.0).abs() < 1e-5 && f.across[1].abs() < 1e-5, "{f:?}");
        assert!((f.down[1] - 1.0).abs() < 1e-5, "{f:?}");
        assert!((f.length - (200.0 + 2.0 * 30.0 * PAD_ENDS)).abs() < 1e-3, "{f:?}");
        assert!((f.height - 30.0 * (1.0 + PAD_ABOVE + PAD_BELOW)).abs() < 1e-3, "{f:?}");
        // The ink box of the whole line is the two words' union.
        let b = f.ink_box(30.0 * PAD_ENDS, 30.0 * PAD_ENDS + 200.0);
        for (got, want) in b.iter().zip([100.0, 50.0, 300.0, 80.0]) {
            assert!((got - want).abs() < 1e-3, "{b:?}");
        }
        assert!((f.along([100.0, 70.0]) - 30.0 * PAD_ENDS).abs() < 1e-3);
    }

    #[test]
    fn a_degenerate_line_has_no_frame() {
        assert_eq!(Frame::of_line(&[]), None);
        let flat = RotatedRect::new(PointF::from_yx(10.0, 10.0), Vec2::from_yx(-1.0, 0.0), 20.0, 0.5);
        assert_eq!(Frame::of_line(&[flat]), None);
    }

    #[test]
    fn samples_outside_the_image_are_white() {
        let px = [0u8; 4];
        let g = Grey { px: &px, width: 2, height: 2 };
        assert_eq!(g.sample(1.0, 1.0), 0.0);
        assert_eq!(g.sample(-5.0, 1.0), 255.0);
        assert_eq!(g.sample(1.0, 50.0), 255.0);
    }

    #[test]
    fn arabic_letters_are_told_from_digits_and_marks() {
        assert!("مشروع".chars().all(is_arabic_letter));
        assert!(!"٠١٢٣".chars().any(is_arabic_letter));
        assert!(!"\u{064E}\u{0651}،؟".chars().any(is_arabic_letter));
        assert!(!"Tower 12".chars().any(is_arabic_letter));
    }
}
