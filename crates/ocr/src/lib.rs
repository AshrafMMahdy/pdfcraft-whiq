//! pdfcraft-ocr — Scan & OCR ▸ Recognize text (L4).
//!
//! Recognition runs the ocrs engine (MIT/Apache-2.0) with its pre-trained models (CC-BY-SA-4.0,
//! fetched by `cargo xtask models`; see ATTRIBUTION.toml). The caller renders a page to pixels;
//! [`Ocr::recognize`] finds the words in them, and [`text_layer`] turns words placed in user space
//! into page content: invisible text (rendering mode 3) over each word, so the page becomes a
//! searchable image (Acrobat's "Searchable Image (Exact)": the image is left untouched).
//!
//! The ocrs models read the Latin alphabet (English and other languages written without
//! accents). When the Arabic model is installed beside them (`models/`, PaddleOCR's Arabic
//! text-line recogniser, Apache-2.0; see [`arabic`]), every line ocrs finds is also read by it,
//! and a line in Arabic script takes that reading: Arabic and Latin both read with no setting,
//! line by line, mixed lines included. Arabic lines come out in logical (typed) order.

#![deny(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unimplemented, clippy::todo, clippy::unreachable)]

use std::path::{Path, PathBuf};

pub use pdfcraft_fonts::helvetica_width;

pub mod arabic;
pub mod bidi;

pub use arabic::{ARABIC_CHARSET, ARABIC_MODEL};
pub use bidi::visual_word;

/// The model files, as named in ATTRIBUTION.toml.
pub const DETECTION_MODEL: &str = "text-detection.onnx";
pub const RECOGNITION_MODEL: &str = "text-recognition.onnx";

/// The recognition model's alphabet, as it was trained (ocrs-models' `DEFAULT_ALPHABET`): its
/// output classes follow this order. The ocrs crate's own copy has an `E` where the training had
/// `€`, which made a scanned "€250" read as "E250".
pub const ALPHABET: &str = " 0123456789!\"#$%&'()*+,-./:;<=>?@[\\]^_`{|}~€ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz";

/// The languages the models read (ISO 639-1): English with the ocrs models, Arabic when the
/// Arabic model is installed too. Recognition tells them apart line by line on its own.
pub const LANGUAGES: &[(&str, &str)] = &[("en", "English"), ("ar", "Arabic")];

#[derive(Debug, thiserror::Error)]
pub enum OcrError {
    /// Shown to end users (release packages ship the models; a source build fetches them with
    /// `cargo xtask models`). The UI catalogs key this exact text.
    #[error("the text recognition models are not installed (reinstall PdfCraft, or set PDFCRAFT_MODELS to the folder that holds them)")]
    NoModels,
    #[error("loading {0}: {1}")]
    Load(String, String),
    #[error("text recognition failed: {0}")]
    Recognize(String),
    #[error("the image is empty")]
    EmptyImage,
}

/// Where the two model files are.
#[derive(Clone, Debug, PartialEq)]
pub struct Models {
    pub detection: PathBuf,
    pub recognition: PathBuf,
}

impl Models {
    /// A complete model pair in `dir`, preferring current ONNX files over legacy RTEN files.
    /// An incomplete update never mixes formats or hides an intact legacy pair.
    pub fn in_dir(dir: &Path) -> Option<Models> {
        [(DETECTION_MODEL, RECOGNITION_MODEL), ("text-detection.rten", "text-recognition.rten")].into_iter().find_map(|(detection, recognition)| {
            let m = Models { detection: dir.join(detection), recognition: dir.join(recognition) };
            (m.detection.is_file() && m.recognition.is_file()).then_some(m)
        })
    }

    /// Look for the models: `$PDFCRAFT_MODELS`, then where the release packages install them
    /// beside the executable ([`Models::dirs_beside_exe`]), then the source tree's `assets/models/`.
    pub fn find() -> Option<Models> {
        Self::search_dirs().iter().find_map(|d| Self::in_dir(d))
    }

    /// The directories [`Models::find`] looks in, in order.
    pub fn search_dirs() -> Vec<PathBuf> {
        let mut dirs = Vec::new();
        if let Some(d) = std::env::var_os("PDFCRAFT_MODELS") {
            dirs.push(PathBuf::from(d));
        }
        if let Some(exe) = std::env::current_exe().ok().and_then(|e| e.parent().map(Path::to_path_buf)) {
            dirs.extend(Self::dirs_beside_exe(&exe));
        }
        dirs.push(Path::new(env!("CARGO_MANIFEST_DIR")).join("../../assets/models"));
        dirs.push(Path::new(env!("CARGO_MANIFEST_DIR")).join("../../models"));
        dirs
    }

    /// Where the release packages put the models, relative to the directory holding the
    /// executable (packaging/*):
    /// - `models/` beside it: the Windows MSI and portable zip;
    /// - `../Resources/models`: the macOS app bundle (`Contents/MacOS` → `Contents/Resources`);
    /// - `../share/pdfcraft/models`: the FHS-style trees (`bin/` → `share/`) of the Linux
    ///   deb/rpm/tar.gz/AppImage/Flatpak and the FreeBSD tarball, wherever they are installed.
    pub fn dirs_beside_exe(exe_dir: &Path) -> Vec<PathBuf> {
        vec![exe_dir.join("models"), exe_dir.join("../Resources/models"), exe_dir.join("../share/pdfcraft/models")]
    }
}

/// Where the Arabic model and its character list are.
#[derive(Clone, Debug, PartialEq)]
pub struct ArabicFiles {
    pub model: PathBuf,
    pub charset: PathBuf,
}

impl ArabicFiles {
    pub fn in_dir(dir: &Path) -> Option<ArabicFiles> {
        let f = ArabicFiles { model: dir.join(ARABIC_MODEL), charset: dir.join(ARABIC_CHARSET) };
        (f.model.is_file() && f.charset.is_file()).then_some(f)
    }

    /// Look in the same places as [`Models::find`]. `PDFCRAFT_OCR_ARABIC=0` turns Arabic off.
    pub fn find() -> Option<ArabicFiles> {
        if std::env::var_os("PDFCRAFT_OCR_ARABIC").is_some_and(|v| v == "0") {
            return None;
        }
        Models::search_dirs().iter().find_map(|d| Self::in_dir(d))
    }
}

/// What the installed models read, for status reports.
pub fn languages_installed() -> &'static str {
    match (Models::find().is_some(), ArabicFiles::find().is_some()) {
        (true, true) => "Latin alphabet (en) and Arabic (ar), detected per line",
        (true, false) => "Latin alphabet (en)",
        _ => "none",
    }
}

/// A recognised word: its text and its box in image pixels `[left, top, right, bottom]`.
#[derive(Clone, Debug, PartialEq)]
pub struct Word {
    pub text: String,
    pub rect: [f32; 4],
}

/// A recognised line of words, in reading (logical) order: right to left for an Arabic line,
/// each word's text in typed order.
#[derive(Clone, Debug, PartialEq, Default)]
pub struct Line {
    pub words: Vec<Word>,
    /// Whether the line reads right to left (an Arabic line).
    pub rtl: bool,
}

impl Line {
    pub fn text(&self) -> String {
        self.words.iter().map(|w| w.text.as_str()).collect::<Vec<_>>().join(" ")
    }

    /// The words left to right as they stand on the page (a line with Arabic in it is in typed
    /// order, which runs right to left through the Arabic).
    pub fn words_left_to_right(&self) -> Vec<&Word> {
        let mut words: Vec<&Word> = self.words.iter().collect();
        if self.rtl || words.iter().any(|w| w.text.chars().any(bidi::is_rtl)) {
            words.sort_by(|a, b| a.rect[0].total_cmp(&b.rect[0]));
        }
        words
    }
}

/// A loaded recogniser. Loading takes a moment; keep one and reuse it.
pub struct Ocr {
    engine: ocrs::OcrEngine,
    arabic: Option<arabic::ArabicModel>,
}

impl Ocr {
    /// Load the ocrs models, and the Arabic model when it is installed (beside them first)
    /// and not turned off ([`ArabicFiles::find`]).
    pub fn load(models: &Models) -> Result<Ocr, OcrError> {
        let arabic = ArabicFiles::find().map(|found| models.detection.parent().and_then(ArabicFiles::in_dir).unwrap_or(found));
        Self::load_with(models, arabic.as_ref())
    }

    /// Load the ocrs models and, if given, the Arabic model.
    pub fn load_with(models: &Models, arabic: Option<&ArabicFiles>) -> Result<Ocr, OcrError> {
        let load = |p: &Path| rten::Model::load_file(p).map_err(|e| OcrError::Load(p.display().to_string(), e.to_string()));
        let params = ocrs::OcrEngineParams {
            detection_model: Some(load(&models.detection)?),
            recognition_model: Some(load(&models.recognition)?),
            alphabet: Some(ALPHABET.into()),
            ..Default::default()
        };
        let engine = ocrs::OcrEngine::new(params).map_err(|e| OcrError::Load("ocr engine".into(), e.to_string()))?;
        let arabic = arabic.map(|f| arabic::ArabicModel::load(&f.model, &f.charset)).transpose()?;
        Ok(Ocr { engine, arabic })
    }

    /// Whether this recogniser reads Arabic as well as Latin.
    pub fn reads_arabic(&self) -> bool {
        self.arabic.is_some()
    }

    /// Load the models found by [`Models::find`].
    pub fn find() -> Result<Ocr, OcrError> {
        Self::load(&Models::find().ok_or(OcrError::NoModels)?)
    }

    /// Recognise the text in an RGBA (or RGB, or grey) image, `width` × `height` pixels.
    pub fn recognize(&self, pixels: &[u8], width: u32, height: u32) -> Result<Vec<Line>, OcrError> {
        if width == 0 || height == 0 || pixels.is_empty() {
            return Err(OcrError::EmptyImage);
        }
        // ocrs wants 1 or 3 channels.
        let rgb: std::borrow::Cow<[u8]> = if pixels.len() == (width * height * 4) as usize {
            pixels.as_chunks::<4>().0.iter().flat_map(|p| [p[0], p[1], p[2]]).collect::<Vec<u8>>().into()
        } else {
            pixels.into()
        };
        let err = |e: &dyn std::fmt::Display| OcrError::Recognize(e.to_string());
        let source = ocrs::ImageSource::from_bytes(&rgb, (width, height)).map_err(|e| err(&e))?;
        let input = self.engine.prepare_input(source).map_err(|e| err(&e))?;
        let found = self.engine.detect_words(&input).map_err(|e| err(&e))?;
        let lines = self.engine.find_text_lines(&input, &found);
        let read = self.engine.recognize_text(&input, &lines).map_err(|e| err(&e))?;
        let mut out: Vec<Line> = read.iter().map(|line| line.as_ref().map(latin_line).unwrap_or_default()).collect();
        let Some(model) = &self.arabic else {
            return Ok(out.into_iter().filter(|l| !l.words.is_empty()).collect());
        };
        let grey = luma(&rgb, width, height);
        let img = arabic::Grey { px: &grey, width: width as usize, height: height as usize };
        for (slot, verdict) in out.iter_mut().zip(mixed::read(model, &self.engine, &input, &img, &lines)?) {
            match verdict {
                mixed::Verdict::Arabic(line) => *slot = line,
                // Read unsurely by both models and only a few characters: noise.
                mixed::Verdict::Unsure if slot.words.iter().map(|w| w.text.chars().count()).sum::<usize>() <= mixed::NOISE_CHARS => {
                    slot.words.clear()
                }
                _ => {}
            }
        }
        let mut out: Vec<Line> = drop_specks(out.into_iter().filter(|l| !l.words.is_empty()).collect());
        if out.iter().filter(|l| l.rtl).count() * 2 > out.len() {
            out = rtl_reading_order(out);
        }
        Ok(out)
    }
}

/// Lines no taller than this share of the page's usual line are specks (noise on a scan read
/// as a letter or two), not text.
const SPECK: f32 = 0.45;

/// The page's lines without the ones made of specks: every word in them much shorter than the
/// page's usual word and only a few characters long.
fn drop_specks(lines: Vec<Line>) -> Vec<Line> {
    let mut heights: Vec<f32> = lines.iter().flat_map(|l| &l.words).map(|w| w.rect[3] - w.rect[1]).filter(|h| h.is_finite() && *h > 0.0).collect();
    if heights.len() < 8 {
        return lines;
    }
    heights.sort_by(f32::total_cmp);
    let usual = heights.get(heights.len() / 2).copied().unwrap_or(0.0);
    lines
        .into_iter()
        .filter(|l| {
            let tallest = l.words.iter().map(|w| w.rect[3] - w.rect[1]).fold(0.0f32, f32::max);
            let chars: usize = l.words.iter().map(|w| w.text.chars().count()).sum();
            tallest >= SPECK * usual || chars > 3
        })
        .collect()
}

/// A right-to-left page's lines in its reading order: rows top to bottom, and the lines side by
/// side in a row (table cells, a label and its value) right to left. ocrs orders lines for
/// left-to-right pages, by columns from the left, which puts the short right-aligned last line
/// of an Arabic paragraph in a column of its own.
fn rtl_reading_order(lines: Vec<Line>) -> Vec<Line> {
    let bounds = |l: &Line| {
        l.words.iter().fold([f32::MAX, f32::MAX, f32::MIN, f32::MIN], |b, w| {
            [b[0].min(w.rect[0]), b[1].min(w.rect[1]), b[2].max(w.rect[2]), b[3].max(w.rect[3])]
        })
    };
    // A line's height is its words' (on a skewed scan a long line's box is much taller).
    let height = |l: &Line| {
        let mut h: Vec<f32> = l.words.iter().map(|w| w.rect[3] - w.rect[1]).collect();
        h.sort_by(f32::total_cmp);
        h.get(h.len() / 2).copied().unwrap_or(0.0)
    };
    let mut items: Vec<([f32; 4], f32, Line)> = lines.into_iter().map(|l| (bounds(&l), height(&l), l)).collect();
    let mid = |b: &[f32; 4]| (b[1] + b[3]) / 2.0;
    items.sort_by(|a, b| mid(&a.0).total_cmp(&mid(&b.0)));
    let mut rows: Vec<Vec<([f32; 4], f32, Line)>> = Vec::new();
    for item in items {
        // A line whose middle is within half a line of the row's first line's is beside it.
        match rows.last_mut() {
            Some(row) if row.first().is_some_and(|(b, h, _)| (mid(&item.0) - mid(b)).abs() < 0.5 * h.max(item.1)) => row.push(item),
            _ => rows.push(vec![item]),
        }
    }
    rows.into_iter()
        .flat_map(|mut row| {
            row.sort_by(|a, b| b.0[2].total_cmp(&a.0[2]));
            row.into_iter().map(|(_, _, l)| l)
        })
        .collect()
}

/// An ocrs line as words.
fn latin_line(line: &ocrs::TextLine) -> Line {
    use ocrs::TextItem;
    Line {
        words: line
            .words()
            .map(|w| {
                let r = w.bounding_rect();
                Word { text: w.to_string(), rect: [r.left() as f32, r.top() as f32, r.right() as f32, r.bottom() as f32] }
            })
            .filter(|w| !w.text.trim().is_empty())
            .collect(),
        rtl: false,
    }
}

/// One grey byte per pixel from RGB (or grey) pixels.
fn luma(px: &[u8], width: u32, height: u32) -> Vec<u8> {
    let n = (width as usize).saturating_mul(height as usize);
    if px.len() >= n.saturating_mul(3) {
        px.as_chunks::<3>().0.iter().take(n).map(|p| ((u32::from(p[0]) * 299 + u32::from(p[1]) * 587 + u32::from(p[2]) * 114) / 1000) as u8).collect()
    } else {
        let mut g = px.get(..n).map(<[u8]>::to_vec).unwrap_or_default();
        g.resize(n, 255);
        g
    }
}

mod mixed;

/// A word placed on a page, in PDF user space: the bottom-left corner of its box, and the
/// vectors along its bottom edge (`across`, left → right as read) and its left edge (`up`).
#[derive(Clone, Debug, PartialEq)]
pub struct PlacedWord {
    pub text: String,
    pub origin: [f64; 2],
    pub across: [f64; 2],
    pub up: [f64; 2],
}

impl PlacedWord {
    /// Place a pixel-space word with `to_user`, which maps an image pixel (x right, y down) to
    /// user space.
    pub fn place(word: &Word, to_user: impl Fn(f32, f32) -> [f64; 2]) -> PlacedWord {
        let [l, t, r, b] = word.rect;
        let o = to_user(l, b);
        let br = to_user(r, b);
        let tl = to_user(l, t);
        PlacedWord { text: word.text.clone(), origin: o, across: [br[0] - o[0], br[1] - o[1]], up: [tl[0] - o[0], tl[1] - o[1]] }
    }
}

/// Helvetica's ascender and descender (em fractions): a word box spans about this much.
const BOX_EM: f64 = 0.93;
const DESCENT: f64 = 0.21;
/// Helvetica's cap height and x-height (em fractions).
const CAP_HEIGHT: f64 = 0.718;
const X_HEIGHT: f64 = 0.523;

/// How far `text`'s ink reaches above and below the baseline, in em fractions: a word box is
/// tight around the ink, so "DT" spans the cap height only, "sun" the x-height, and "gap" reaches
/// down to the descender. Text without letters or digits keeps the full ascender-descender span.
fn ink_extent(text: &str) -> (f64, f64) {
    if !text.chars().any(char::is_alphanumeric) {
        return (BOX_EM - DESCENT, -DESCENT);
    }
    let tall = text
        .chars()
        .any(|c| c.is_uppercase() || c.is_ascii_digit() || "bdfhijklt!?/\\()[]{}|'\"#$%&@".contains(c) || (c.is_alphabetic() && !c.is_ascii()));
    let low = text.chars().any(|c| "gjpqy()[]{}|_".contains(c));
    (if tall { CAP_HEIGHT } else { X_HEIGHT }, if low { -DESCENT } else { 0.0 })
}

fn num(v: f64) -> String {
    let s = format!("{:.4}", if v.abs() < 1e-9 { 0.0 } else { v });
    let s = s.trim_end_matches('0').trim_end_matches('.');
    if s == "-0" { "0".into() } else { s.into() }
}

/// The resource name the text layer gives the font for words Helvetica can't hold (Arabic):
/// [`pdfcraft_fonts::glyphless`]. Whoever adds the layer to a page adds that font under it.
pub const UNICODE_FONT: &str = pdfcraft_fonts::glyphless::GLYPHLESS_RESOURCE;

/// Page content that writes `words` as invisible text: in `/PCHelv` (standard Helvetica,
/// WinAnsiEncoding) when Helvetica can hold the word, else in `/PCOcrU` ([`UNICODE_FONT`], the
/// glyphless Type0 font: Arabic and any other script). Each word is stretched to its box so
/// selection and search highlight the right place. Marked content `/OCR` so it can be told apart
/// from the page's own text. An Arabic word is drawn in visual order ([`visual_word`]), as PDF
/// producers draw right-to-left text, so text extraction (which reorders right-to-left runs)
/// gives it back in typed order.
pub fn text_layer(words: &[PlacedWord]) -> Vec<u8> {
    // `3 Tr` and the font are graphics state, which `ET` doesn't reset: without `q … Q` they
    // would stay set for every later content stream on the page, so text added after OCR (a
    // header or footer) drew invisibly (#3029).
    let mut out = b"/OCR BMC\nq\nBT\n3 Tr\n/PCHelv 1 Tf\n".to_vec();
    // Words in the order given (each line left to right), switching fonts as needed: extraction
    // follows the drawing order within a line.
    let mut helvetica = true;
    for w in words {
        if w.up[0].hypot(w.up[1]) <= 0.0 {
            continue;
        }
        if pdfcraft_fonts::first_undrawable(&w.text).is_none() {
            let width = helvetica_width(&w.text, 1.0);
            if width <= 0.0 {
                continue;
            }
            if !helvetica {
                out.extend_from_slice(b"/PCHelv 1 Tf\n");
                helvetica = true;
            }
            // The em square: the word's own ink (cap height or x-height, descenders or not)
            // spans the box, so the text is as tall as the scanned word; its width is stretched
            // to fit the word.
            let (top, bottom) = ink_extent(&w.text);
            write_word(&mut out, w, width, top, bottom, &pdfcraft_fonts::literal(&pdfcraft_fonts::win_ansi(&w.text)));
        } else {
            let codes = pdfcraft_fonts::glyphless::glyphless_codes(&visual_word(&w.text));
            // Every code is one glyph 1 em wide; the box spans the em from its descent to its
            // ascent.
            let width = (codes.len() / 2) as f64;
            if width <= 0.0 {
                continue;
            }
            if helvetica {
                out.extend_from_slice(format!("/{UNICODE_FONT} 1 Tf\n").as_bytes());
                helvetica = false;
            }
            let hex: String = codes.iter().map(|b| format!("{b:02X}")).collect();
            write_word(
                &mut out,
                w,
                width,
                pdfcraft_fonts::glyphless::GLYPHLESS_ASCENT,
                pdfcraft_fonts::glyphless::GLYPHLESS_DESCENT,
                format!("<{hex}>").as_bytes(),
            );
        }
    }
    out.extend_from_slice(b"ET\nQ\nEMC\n");
    out
}

/// One word: a text matrix that fits `width` (text space units at size 1) across the word's box
/// and the span from `bottom` to `top` (em fractions) up it, then the string.
fn write_word(out: &mut Vec<u8>, w: &PlacedWord, width: f64, top: f64, bottom: f64, string: &[u8]) {
    let em = [w.up[0] / (top - bottom), w.up[1] / (top - bottom)];
    let ax = [w.across[0] / width, w.across[1] / width];
    let o = [w.origin[0] - em[0] * bottom, w.origin[1] - em[1] * bottom];
    let m = [ax[0], ax[1], em[0], em[1], o[0], o[1]];
    let nums: Vec<String> = m.iter().map(|v| num(*v)).collect();
    out.extend_from_slice(nums.join(" ").as_bytes());
    out.extend_from_slice(b" Tm ");
    out.extend_from_slice(string);
    out.extend_from_slice(b" Tj\n");
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An Arabic stretch in a Latin line is typed right to left: drawn, its words go left to right.
    #[test]
    fn words_are_drawn_left_to_right_whatever_the_typed_order() {
        let w = |t: &str, x: f32| Word { text: t.into(), rect: [x, 0.0, x + 10.0, 10.0] };
        let line = Line { words: vec![w("Compound", 0.0), w("مجمع", 60.0), w("النور", 40.0), w("Phase", 80.0)], rtl: false };
        let order: Vec<&str> = line.words_left_to_right().iter().map(|w| w.text.as_str()).collect();
        assert_eq!(order, ["Compound", "النور", "مجمع", "Phase"]);
        let latin = Line { words: vec![w("b", 20.0), w("a", 0.0)], rtl: false };
        assert_eq!(latin.words_left_to_right()[0].text, "b", "a Latin line keeps ocrs's order");
    }

    #[test]
    fn places_words_through_the_mapping() {
        // 2 pixels per point, page 612 × 792, y flipped.
        let to_user = |x: f32, y: f32| [x as f64 / 2.0, 792.0 - y as f64 / 2.0];
        let w = PlacedWord::place(&Word { text: "Hello".into(), rect: [100.0, 200.0, 300.0, 240.0] }, to_user);
        assert_eq!(w.origin, [50.0, 672.0]);
        assert_eq!(w.across, [100.0, 0.0]);
        assert_eq!(w.up, [0.0, 20.0]);
    }

    /// The text layer is Helvetica in WinAnsiEncoding: every character the model can recognise
    /// must be one it can hold, or recognised text would be saved as `?`. A model with a wider
    /// alphabet (Chinese, for example) needs a text layer in another font first.
    #[test]
    fn the_model_alphabet_fits_the_text_layer_font() {
        assert_eq!(pdfcraft_fonts::first_undrawable(ALPHABET), None);
    }

    /// Arabic goes into the glyphless Unicode font, drawn in visual order, one em per character;
    /// Latin words on the same page stay in Helvetica.
    #[test]
    fn arabic_words_go_into_the_unicode_font() {
        let ar = PlacedWord { text: "برج".into(), origin: [100.0, 20.0], across: [30.0, 0.0], up: [0.0, 10.0] };
        let en = PlacedWord { text: "Tower".into(), origin: [10.0, 20.0], across: [40.0, 0.0], up: [0.0, 9.3] };
        let s = String::from_utf8(text_layer(&[ar, en])).unwrap();
        // "برج" drawn as "جرب": jeem, reh, beh.
        assert!(s.contains("/PCOcrU 1 Tf\n10 0 0 10 100 22.5 Tm <062C06310628> Tj"), "{s}");
        assert_eq!(s.matches("/PCHelv 1 Tf").count(), 2, "Helvetica again after the Arabic word: {s}");
        assert!(s.contains("(Tower) Tj"), "{s}");
        assert!(s.find("/PCOcrU").unwrap() < s.find("(Tower)").unwrap(), "words keep their order: {s}");
    }

    #[test]
    fn text_layer_is_invisible_text_fitted_to_each_box() {
        let w = PlacedWord { text: "Hi".into(), origin: [10.0, 20.0], across: [30.0, 0.0], up: [0.0, 9.3] };
        let s = String::from_utf8(text_layer(&[w])).unwrap();
        assert!(s.contains("3 Tr") && s.contains("/PCHelv 1 Tf"), "{s}");
        let width = helvetica_width("Hi", 1.0);
        // "Hi" has no descender: its box spans the cap height, from the baseline up.
        assert!(s.contains(&format!("{} 0 0 {} 10 20 Tm (Hi) Tj", num(30.0 / width), num(9.3 / CAP_HEIGHT))), "{s}");
        assert!(s.starts_with("/OCR BMC") && s.ends_with("EMC\n"));
    }

    /// #3029: the layer's invisible rendering mode and font end with the layer. Every operator
    /// that sets them sits inside a `q … Q` pair that the layer closes itself, so content drawn
    /// after it on the page starts from the graphics state it had before.
    #[test]
    fn the_layer_restores_the_graphics_state_it_changes() {
        let w = PlacedWord { text: "Hi".into(), origin: [10.0, 20.0], across: [30.0, 0.0], up: [0.0, 9.3] };
        let s = String::from_utf8(text_layer(&[w])).unwrap();
        let mut depth = 0_i32;
        for token in s.split_ascii_whitespace() {
            match token {
                "q" => depth += 1,
                "Q" => depth -= 1,
                "Tr" | "Tf" | "Tm" => assert!(depth > 0, "`{token}` outside q … Q in {s:?}"),
                _ => {}
            }
            assert!(depth >= 0, "unbalanced Q in {s:?}");
        }
        assert_eq!(depth, 0, "the layer leaves a q open: {s:?}");
    }

    /// A word box is tight around the ink, so the text's size and baseline come from the letters:
    /// capitals, lowercase without ascenders, descenders.
    #[test]
    fn the_text_is_as_tall_as_the_scanned_word() {
        assert_eq!(ink_extent("DT"), (CAP_HEIGHT, 0.0));
        assert_eq!(ink_extent("402337"), (CAP_HEIGHT, 0.0));
        assert_eq!(ink_extent("sun"), (X_HEIGHT, 0.0));
        assert_eq!(ink_extent("gap"), (X_HEIGHT, -DESCENT));
        assert_eq!(ink_extent("Gallus.Ze@camtel.cm"), (CAP_HEIGHT, 0.0));
        assert_eq!(ink_extent("Corporation"), (CAP_HEIGHT, -DESCENT));
        assert_eq!(ink_extent("--"), (BOX_EM - DESCENT, -DESCENT));
        let w = PlacedWord { text: "gap".into(), origin: [0.0, 100.0], across: [20.0, 0.0], up: [0.0, 7.3] };
        let s = String::from_utf8(text_layer(&[w])).unwrap();
        let em = 7.3 / (X_HEIGHT + DESCENT);
        assert!(s.contains(&format!(" 0 0 {} 0 {} Tm (gap) Tj", num(em), num(100.0 + em * DESCENT))), "{s}");
    }

    #[test]
    fn rotated_pages_keep_the_reading_direction() {
        // A page turned 90°: image x runs up the page.
        let to_user = |x: f32, y: f32| [y as f64, x as f64];
        let w = PlacedWord::place(&Word { text: "Up".into(), rect: [0.0, 0.0, 50.0, 10.0] }, to_user);
        assert_eq!(w.across, [0.0, 50.0]);
        assert_eq!(w.up, [-10.0, 0.0]);
    }

    /// The current ONNX pair wins when an installation also retains both legacy models.
    #[test]
    fn model_discovery_prefers_the_complete_onnx_pair() {
        let dir = tempfile::tempdir().unwrap();
        for file in ["text-detection.onnx", "text-recognition.onnx"] {
            std::fs::write(dir.path().join(file), b"model").unwrap();
        }
        let expected = Some(Models { detection: dir.path().join("text-detection.onnx"), recognition: dir.path().join("text-recognition.onnx") });
        assert_eq!(Models::in_dir(dir.path()), expected, "a complete ONNX pair is sufficient");
        for file in ["text-detection.rten", "text-recognition.rten"] {
            std::fs::write(dir.path().join(file), b"model").unwrap();
        }
        assert_eq!(Models::in_dir(dir.path()), expected, "the complete ONNX pair takes precedence over legacy models");
    }

    /// An interrupted ONNX update still permits an intact legacy pair in the same directory.
    #[test]
    fn model_discovery_falls_back_to_legacy_when_the_onnx_pair_is_incomplete() {
        for current in ["text-detection.onnx", "text-recognition.onnx"] {
            let dir = tempfile::tempdir().unwrap();
            for file in [current, "text-detection.rten", "text-recognition.rten"] {
                std::fs::write(dir.path().join(file), b"model").unwrap();
            }
            assert_eq!(
                Models::in_dir(dir.path()),
                Some(Models { detection: dir.path().join("text-detection.rten"), recognition: dir.path().join("text-recognition.rten") }),
                "{current} alone must not hide a complete legacy pair"
            );
        }
    }

    /// Detection and recognition must come from one complete format pair, never a mixed update.
    #[test]
    fn model_discovery_refuses_mixed_format_pairs() {
        for pair in [["text-detection.onnx", "text-recognition.rten"], ["text-detection.rten", "text-recognition.onnx"]] {
            let dir = tempfile::tempdir().unwrap();
            for file in pair {
                std::fs::write(dir.path().join(file), b"model").unwrap();
            }
            assert_eq!(Models::in_dir(dir.path()), None, "{pair:?} is not a complete model pair");
        }
    }

    /// Existing source builds and PDFCRAFT_MODELS overrides retain their complete RTEN pair.
    #[test]
    fn model_discovery_keeps_legacy_only_installations() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(Models::in_dir(dir.path()), None);
        std::fs::write(dir.path().join("text-detection.rten"), b"model").unwrap();
        assert_eq!(Models::in_dir(dir.path()), None, "one legacy model is incomplete");
        std::fs::write(dir.path().join("text-recognition.rten"), b"model").unwrap();
        assert_eq!(
            Models::in_dir(dir.path()),
            Some(Models { detection: dir.path().join("text-detection.rten"), recognition: dir.path().join("text-recognition.rten") })
        );
    }

    /// #103: every release package's layout puts the models where the installed app looks.
    /// Mirrors packaging/{windows/package.ps1,macos/package.sh,linux/package.sh,freebsd/package.sh}.
    #[test]
    fn finds_the_models_in_every_release_package_layout() {
        let base = std::env::temp_dir().join(format!("pdfcraft-ocr-layouts-{}", std::process::id()));
        let layouts = [
            ("windows", "PdfCraft", "PdfCraft/models"),
            ("macos", "PdfCraft.app/Contents/MacOS", "PdfCraft.app/Contents/Resources/models"),
            ("linux-deb-rpm", "usr/bin", "usr/share/pdfcraft/models"),
            ("linux-tar", "pdfcraft-1.0.0-linux-x86_64/bin", "pdfcraft-1.0.0-linux-x86_64/share/pdfcraft/models"),
            ("appimage", "PdfCraft.AppDir/usr/bin", "PdfCraft.AppDir/usr/share/pdfcraft/models"),
            ("flatpak", "app/bin", "app/share/pdfcraft/models"),
            ("freebsd", "usr/local/bin", "usr/local/share/pdfcraft/models"),
        ];
        for (name, exe_dir, models_dir) in layouts {
            let root = base.join(name);
            let (exe_dir, models_dir) = (root.join(exe_dir), root.join(models_dir));
            std::fs::create_dir_all(&exe_dir).unwrap();
            std::fs::create_dir_all(&models_dir).unwrap();
            let found = || Models::dirs_beside_exe(&exe_dir).iter().find_map(|d| Models::in_dir(d));
            assert_eq!(found(), None, "{name}: no models yet");
            for file in [DETECTION_MODEL, RECOGNITION_MODEL] {
                std::fs::write(models_dir.join(file), b"model").unwrap();
            }
            let models = found().unwrap_or_else(|| panic!("{name}: models in {} not found", models_dir.display()));
            assert_eq!(models.detection.canonicalize().unwrap(), models_dir.join(DETECTION_MODEL).canonicalize().unwrap(), "{name}");
        }
        let _ = std::fs::remove_dir_all(&base);
    }

    /// The message tells end users what to do; release builds have no cargo.
    #[test]
    fn the_alphabet_is_the_trained_one() {
        let chars: Vec<char> = ALPHABET.chars().collect();
        assert_eq!(chars.len(), 96);
        // The euro sign follows the ASCII punctuation (class 44; 0 is the CTC blank), not a second E.
        assert_eq!(chars.iter().position(|&c| c == '€'), Some(43));
        assert_eq!(chars.iter().filter(|&&c| c == 'E').count(), 1);
    }

    #[test]
    fn missing_models_message_is_for_end_users() {
        let message = OcrError::NoModels.to_string();
        assert!(!message.contains("cargo") && message.contains("PDFCRAFT_MODELS"), "{message}");
    }

    /// End to end on rendered text, when the models are installed (`cargo xtask models`).
    #[test]
    fn reads_rendered_text() {
        let Some(models) = Models::find() else {
            eprintln!("skipped: OCR models not installed");
            return;
        };
        let ocr = Ocr::load(&models).unwrap();
        let (w, h, px) = test_image::hello();
        let lines = ocr.recognize(&px, w, h).unwrap();
        let text: Vec<String> = lines.iter().map(Line::text).collect();
        assert!(text.iter().any(|t| t.contains("HELL") && t.contains("WOR")), "{text:?}");
        let word = lines.iter().flat_map(|l| &l.words).find(|w| w.text.contains("HELL")).unwrap();
        assert!(word.rect[0] >= 10.0 && word.rect[0] < 60.0, "{word:?}");
    }

    mod test_image {
        /// "HELLO WORLD" drawn with blocky 5×7 letters, 4 pixels per dot, black on white.
        pub fn hello() -> (u32, u32, Vec<u8>) {
            const GLYPHS: &[(char, [&str; 7])] = &[
                ('H', ["X...X", "X...X", "X...X", "XXXXX", "X...X", "X...X", "X...X"]),
                ('E', ["XXXXX", "X....", "X....", "XXXX.", "X....", "X....", "XXXXX"]),
                ('L', ["X....", "X....", "X....", "X....", "X....", "X....", "XXXXX"]),
                ('O', [".XXX.", "X...X", "X...X", "X...X", "X...X", "X...X", ".XXX."]),
                ('W', ["X...X", "X...X", "X...X", "X.X.X", "X.X.X", "XX.XX", "X...X"]),
                ('R', ["XXXX.", "X...X", "X...X", "XXXX.", "X.X..", "X..X.", "X...X"]),
                ('D', ["XXXX.", "X...X", "X...X", "X...X", "X...X", "X...X", "XXXX."]),
            ];
            let text = "HELLO WORLD";
            let (dot, x0, y0) = (4u32, 40u32, 40u32);
            let (w, h) = (x0 * 2 + text.len() as u32 * 6 * dot, y0 * 2 + 7 * dot);
            let mut px = vec![255u8; (w * h * 4) as usize];
            for (i, c) in text.chars().enumerate() {
                let Some((_, rows)) = GLYPHS.iter().find(|g| g.0 == c) else { continue };
                for (ry, row) in rows.iter().enumerate() {
                    for (rx, b) in row.bytes().enumerate() {
                        if b != b'X' {
                            continue;
                        }
                        for dy in 0..dot {
                            for dx in 0..dot {
                                let x = x0 + (i as u32 * 6 + rx as u32) * dot + dx;
                                let y = y0 + ry as u32 * dot + dy;
                                let o = ((y * w + x) * 4) as usize;
                                px[o..o + 3].copy_from_slice(&[0, 0, 0]);
                            }
                        }
                    }
                }
            }
            (w, h, px)
        }
    }
}
