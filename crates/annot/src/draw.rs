use super::create::{finite, num_array};
use super::lookup::{annot_dict, annots, page_refs};
use super::modify::refuse_foreign_text;
use super::types::NOTE_SIZE;
use super::{AnnotError, Dict, Document, ObjRef, Object, Stream, appearance};

/// (Re)generate `/AP /N` for the annotation `r` from its dictionary, for an edit that keeps its
/// text (a restyle, a new size or colour).
///
/// A comment whose text neither the standard fonts nor the bundled Inter face can draw, with an
/// appearance another program drew (with a font that has those characters), keeps that appearance:
/// redrawing it would put `?` in their place, so the edit is refused (`AnnotError::Undrawable`).
/// One PdfCraft drew with `?` (marked `pdfcraft_fonts::UNDRAWN_MARK`) is redrawn.
pub fn set_appearance(doc: &mut Document, r: ObjRef) -> Result<(), AnnotError> {
    refuse_foreign_text(doc, &annot_dict(doc, r))?;
    draw_appearance(doc, r)
}

/// Put `before` back when redrawing fails, so a refused edit does not keep the new rectangle or style.
pub(crate) fn redraw_or_restore(doc: &mut Document, r: ObjRef, before: Dict) -> Result<(), AnnotError> {
    if let Err(e) = set_appearance(doc, r) {
        doc.set(r, Object::Dict(before));
        return Err(e);
    }
    Ok(())
}

/// Draw `r`'s normal appearance from its dictionary, whatever its text: text the standard fonts
/// can't draw is written in the bundled Inter face, embedded in the document once (WHIQ: it came
/// out as `?`); characters neither can draw come out as `?`, and the annotation is marked (`pdfcraft_fonts::UNDRAWN_MARK`,
/// cleared when all of it can be drawn) so the edit is reported and flattening won't make that
/// `?` permanent. For new text (a new comment, changed contents), as Acrobat keeps text its font
/// can't show, and for display-only copies.
pub(crate) fn draw_appearance(doc: &mut Document, r: ObjRef) -> Result<(), AnnotError> {
    let d = annot_dict(doc, r);
    let subtype = String::from_utf8_lossy(d.name(b"Subtype").unwrap_or_default()).into_owned();
    let face = unicode_face(doc, &d);
    let Some(stream) = build_appearance(doc, &d, face.as_ref()) else { return Err(AnnotError::Unsupported(subtype)) };
    // What still came out as `?`: with the face, what neither font has; without it (it could not be embedded), what the standard fonts lack.
    let missing = if face.is_some() { appearance::undrawn_text(&d) } else { appearance::undrawable_text(&d) }.map(|(_, c)| c);
    apply_appearance(doc, r, &d, stream, missing)
}

/// The catalog's private entry naming the Inter face embedded for comment text, so every comment of
/// the document that needs it shares one font (its maps and widths are written once).
const FACE_KEY: &[u8] = b"PCAnnotFont";

/// The embedded Inter face for `d`'s text, when that text calls for it ([`appearance::needs_face`]):
/// the one the document already has, else embedded now. `None` when the text needs none, for a
/// redaction (its overlay text is drawn in the standard fonts when applied), or when the face
/// could not be embedded (the text is then drawn as before, with `?`).
fn unicode_face(doc: &mut Document, d: &Dict) -> Option<appearance::UnicodeFace> {
    if d.name(b"Subtype") == Some(b"Redact") {
        return None;
    }
    let (_, text) = appearance::drawn_text(d)?;
    if !appearance::needs_face(&text) {
        return None;
    }
    let root = doc.root()?;
    let known = doc.dict(&Object::Ref(root))?.get(FACE_KEY).filter(|o| matches!(o, Object::Ref(_))).cloned();
    // `embed` reuses a resource of this dictionary that is the same embedding (same program and
    // maps), so the document's face is offered to it under the name it gives a first resource.
    let mut fonts = Dict::new();
    if let Some(font) = &known {
        fonts.set(b"PCFull0".to_vec(), font.clone());
    }
    let tag = pdfcraft_fonts::embedding::embed(doc, &mut fonts, pdfcraft_fonts::INTER_REGULAR, "Inter").ok()?;
    let font = fonts.get(tag.as_bytes())?.clone();
    if known.as_ref() != Some(&font) {
        doc.update_dict(root, |c| c.set(FACE_KEY.to_vec(), font.clone())).ok()?;
    }
    let metrics = pdfcraft_fonts::pdf::Metrics::from_dict(doc, &doc.dict(&font)?);
    Some(appearance::UnicodeFace { tag, font, metrics })
}

/// Draw the appearance for an annotation dictionary: the expensive half of [`set_appearance`].
/// Pure from the dictionary and the objects it points at, so [`AppearanceCache`] can reuse it.
fn build_appearance(doc: &Document, d: &Dict, face: Option<&appearance::UnicodeFace>) -> Option<Stream> {
    let mut stream = appearance::build_with(d, face)?;
    // Picture-stamp restyling must retain the placement's page-axis correction.
    if d.get(b"PCPicture").is_some() {
        let matrix = d.get(b"AP").and_then(|ap| {
            let ap = doc.resolve(ap);
            let normal = doc.resolve(ap.as_dict()?.get(b"N")?);
            let Object::Stream(normal) = normal.as_ref() else { return None };
            normal.dict.get(b"Matrix").cloned()
        });
        if let Some(matrix) = matrix {
            stream.dict.set(b"Matrix".to_vec(), matrix);
        }
    }
    Some(stream)
}

/// Store a built appearance on the annotation: the cheap half of [`set_appearance`]. `missing`: the
/// first character that came out as `?` in it, which the annotation is marked with.
fn apply_appearance(doc: &mut Document, r: ObjRef, d: &Dict, stream: Stream, missing: Option<char>) -> Result<(), AnnotError> {
    let ap = doc.add(Object::Stream(stream));
    // A copy (a shared /AP is left alone) that keeps unknown entries; the old down and rollover
    // appearances would show the previous look on press or hover, so they go with the old /N.
    let mut apd = d.get(b"AP").map(|a| doc.resolve(a)).and_then(|a| a.as_dict().cloned()).unwrap_or_default();
    apd.remove(b"D");
    apd.remove(b"R");
    apd.set(b"N".to_vec(), Object::Ref(ap));
    // A note icon has a fixed size (`NOTE_SIZE`); viewers draw it at the upper-left corner of `/Rect`.
    // The appearance's box is mapped onto `/Rect`, so any other `/Rect` (a large one, or one with
    // no width, as LaTeX `pdfcomment` writes) would stretch the icon or shrink it to nothing.
    let icon_rect = matches!(d.name(b"Subtype"), Some(b"Text" | b"FileAttachment"))
        .then(|| d.get(b"Rect")?.as_array()?.iter().map(|o| o.as_f64()).collect::<Option<Vec<f64>>>())
        .flatten()
        .filter(|q| q.len() == 4 && finite(q))
        .map(|q| {
            let (x, top) = (q[0].min(q[2]), q[1].max(q[3]));
            [x, top - NOTE_SIZE, x + NOTE_SIZE, top]
        });
    doc.update_dict(r, |d| {
        d.set(b"AP".to_vec(), Object::Dict(apd));
        if let Some(rect) = icon_rect {
            d.set(b"Rect".to_vec(), num_array(&rect));
        }
        d.remove(b"AS");
        pdfcraft_fonts::set_undrawn_mark(d, missing);
    })?;
    Ok(())
}

/// Display appearances built for annotations without any, reused while the annotation
/// dictionaries they were drawn from are unchanged.
///
/// A content edit (moving a paragraph, retyping text, …) leaves every annotation dictionary
/// alone, but the display copy is rebuilt from the edited document on every refresh: without
/// this cache each refresh redrew every appearance (27k on one CAD file, ~0.7 s per edit
/// commit). Entries are keyed by annotation reference; object numbers are never reused within
/// a document lineage, so an entry stays valid until an annotation edit runs. Callers clear
/// the cache on any refresh that may change annotations (comments, structural and full
/// refreshes) and keep it for content-only ones.
#[derive(Clone, Debug, Default)]
pub struct AppearanceCache {
    entries: std::collections::HashMap<ObjRef, Stream>,
    bytes: usize,
    hits: u64,
    builds: u64,
}

impl AppearanceCache {
    /// Cached appearance data restarts past this instead of growing without bound.
    const MAX_BYTES: usize = 64 << 20;

    /// Forget every entry (an annotation edit may have changed the dictionaries).
    pub fn clear(&mut self) {
        *self = Self::default();
    }

    fn get(&mut self, r: ObjRef) -> Option<Stream> {
        let stream = self.entries.get(&r)?.clone();
        self.hits += 1;
        Some(stream)
    }

    fn insert(&mut self, r: ObjRef, stream: &Stream) {
        self.builds += 1;
        if self.entries.len() >= 1_000_000 {
            self.clear();
        }
        if self.bytes.saturating_add(stream.raw.len()) <= Self::MAX_BYTES {
            self.bytes += stream.raw.len();
            self.entries.insert(r, stream.clone());
        }
        // Else over budget: keep serving the entries already cached instead of growing.
    }

    /// Cache hits so far (reused appearances); for tests.
    pub fn hits(&self) -> u64 {
        self.hits
    }

    /// Appearances built so far; for tests.
    pub fn builds(&self) -> u64 {
        self.builds
    }
}

/// A copy of `doc` in which every annotation without a normal appearance (`/AP /N`) has the one
/// [`appearance::build`] draws from its dictionary, for displaying the document (`None` when no
/// annotation needs one). Many files carry comments without appearances (FreeText, Ink, notes,
/// stamps, …) that viewers draw from the dictionary. `doc` is not changed: saving writes the file
/// as it was. Links, form fields and pop-ups keep their own handling.
pub fn with_missing_appearances(doc: &Document) -> Option<Document> {
    with_missing_appearances_cached(doc, &mut AppearanceCache::default())
}

/// A copy of `doc` with [`AppearanceCache`] reuse across refreshes: annotations whose
/// dictionary is unchanged since the cached build keep their stored appearance instead of
/// redrawing it. See [`AppearanceCache`] for when entries stay valid.
pub fn with_missing_appearances_cached(doc: &Document, cache: &mut AppearanceCache) -> Option<Document> {
    let mut copy: Option<Document> = None;
    // An annotation listed more than once, on one page or several, is drawn once. `has_normal`
    // reads `doc`, which never gets the appearance, so every repeat used to add another stream to
    // the copy, and the display bytes grew with the number of listings.
    let mut seen = std::collections::HashSet::new();
    for page in page_refs(doc).ok()? {
        for r in annots(doc, page).iter().filter_map(Object::as_ref) {
            if !seen.insert(r) {
                continue;
            }
            let d = annot_dict(doc, r);
            if matches!(d.name(b"Subtype"), None | Some(b"Link" | b"Widget" | b"Popup")) {
                continue;
            }
            let has_normal = d.get(b"AP").is_some_and(|ap| doc.resolve(ap).as_dict().is_some_and(|ap| ap.contains(b"N")));
            if has_normal {
                continue;
            }
            if let Some(stream) = cache.get(r) {
                // Fails only when `r` isn't a dictionary, which the cached build ruled out.
                apply_appearance(copy.get_or_insert_with(|| doc.clone()), r, &d, stream, appearance::undrawable_text(&d).map(|(_, c)| c)).ok();
                continue;
            }
            // Another program's stamp gets a drawing for display only (ISO 32000-2 Table 184). Its
            // `/AP`, if any, has no `/N` to keep here.
            let other_stamp = || {
                let mut bare = d.clone();
                bare.remove(b"AP");
                appearance::display_stamp(&bare)
            };
            // (A display copy draws in the standard fonts only: nothing is embedded in a copy that is never saved.)
            let Some(stream) = build_appearance(doc, &d, None).or_else(other_stamp) else { continue };
            cache.insert(r, &stream);
            // Fails only when `r` isn't a dictionary, which the build just ruled out.
            apply_appearance(copy.get_or_insert_with(|| doc.clone()), r, &d, stream, appearance::undrawable_text(&d).map(|(_, c)| c)).ok();
        }
    }
    copy
}
