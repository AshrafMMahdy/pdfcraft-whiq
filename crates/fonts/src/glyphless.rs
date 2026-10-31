//! A font for invisible text in any script: OCR text layers over Arabic (and any other text the
//! standard fonts' WinAnsiEncoding can't hold).
//!
//! The text is drawn invisibly (rendering mode 3), so the font needs no shapes: a Type0 font,
//! Identity-H, whose character codes are the text's UTF-16 code units, each 1 em wide. Every code
//! shows the program's one empty glyph, and `/ToUnicode` maps every code back to itself, so
//! search, selection and copying give the text. The program is a minimal TrueType generated here
//! (two empty glyphs, no outlines): contributor-original, no font file is shipped.

use pdfcraft_cos::{Dict, Document, ObjRef, Object, PdfString, Stream};

/// The resource name content asks for the font by (`pdfcraft_edit::stamp` adds it to the page,
/// under another name if a page already uses this one for something else).
pub const GLYPHLESS_RESOURCE: &str = "PCOcrU";
/// The font's name, and the key that marks a font dictionary as this one (for reuse).
pub const GLYPHLESS_NAME: &str = "PdfCraftOCR";
pub const GLYPHLESS_MARK: &[u8] = b"PCGlyphless";
/// Units per em, and the em box's top and bottom (a word box spans it).
const EM: i16 = 1000;
pub const GLYPHLESS_ASCENT: f64 = 0.75;
pub const GLYPHLESS_DESCENT: f64 = -0.25;

/// The character codes for `text` in this font: UTF-16BE, characters beyond the Basic
/// Multilingual Plane as U+FFFD (a code is one glyph, one em wide).
pub fn glyphless_codes(text: &str) -> Vec<u8> {
    text.chars().flat_map(|c| u16::try_from(u32::from(c)).ok().filter(|u| !(0xD800..=0xDFFF).contains(u)).unwrap_or(0xFFFD).to_be_bytes()).collect()
}

/// The TrueType program: two empty glyphs (.notdef and the one every code shows), 1 em wide.
pub fn glyphless_truetype() -> Vec<u8> {
    let be16 = |v: i32| (v as u16).to_be_bytes();
    let mut head = Vec::new();
    for part in [&0x0001_0000u32.to_be_bytes()[..], &0x0001_0000u32.to_be_bytes(), &[0; 4], &0x5F0F_3CF5u32.to_be_bytes()] {
        head.extend_from_slice(part);
    }
    head.extend_from_slice(&be16(0x000B)); // flags: baseline at 0, integer scaling
    head.extend_from_slice(&be16(i32::from(EM)));
    head.extend_from_slice(&[0; 16]); // created, modified
    for v in [0, -250, 1000, 750] {
        head.extend_from_slice(&be16(v)); // xMin yMin xMax yMax
    }
    for v in [0, 8, 2, 0, 0] {
        head.extend_from_slice(&be16(v)); // macStyle lowestRecPPEM fontDirectionHint indexToLocFormat glyphDataFormat
    }
    let mut hhea = 0x0001_0000u32.to_be_bytes().to_vec();
    for v in [750, -250, 0, 1000, 0, 0, 1000, 1, 0, 0, 0, 0, 0, 0, 0, 2] {
        hhea.extend_from_slice(&be16(v));
    }
    let mut maxp = 0x0001_0000u32.to_be_bytes().to_vec();
    for v in [2, 0, 0, 0, 0, 2, 0, 0, 0, 0, 0, 0, 0, 0] {
        maxp.extend_from_slice(&be16(v));
    }
    let hmtx: Vec<u8> = [1000, 0, 1000, 0].iter().flat_map(|v| be16(*v)).collect();
    let loca: Vec<u8> = [0, 0, 0].iter().flat_map(|v| be16(*v)).collect();
    let glyf: Vec<u8> = Vec::new();
    // cmap: one empty format 4 subtable (Windows Unicode); the PDF maps codes to the glyph.
    let mut cmap = Vec::new();
    for v in [0, 1, 3, 1] {
        cmap.extend_from_slice(&be16(v));
    }
    cmap.extend_from_slice(&12u32.to_be_bytes());
    for v in [4, 24, 0, 2, 2, 0, 0, 0xFFFF, 0, 0xFFFF, 1, 0] {
        cmap.extend_from_slice(&be16(v));
    }
    // name: the family (1) and PostScript (6) names.
    let utf16: Vec<u8> = GLYPHLESS_NAME.encode_utf16().flat_map(u16::to_be_bytes).collect();
    let len = utf16.len() as i32;
    let mut name = Vec::new();
    for v in [0, 2, 6 + 2 * 12] {
        name.extend_from_slice(&be16(v));
    }
    for id in [1, 6] {
        for v in [3, 1, 0x409, id, len, 0] {
            name.extend_from_slice(&be16(v));
        }
    }
    name.extend_from_slice(&utf16);
    let mut post = 0x0003_0000u32.to_be_bytes().to_vec();
    post.extend_from_slice(&[0; 4]); // italicAngle
    for v in [-100, 50] {
        post.extend_from_slice(&be16(v));
    }
    post.extend_from_slice(&1u32.to_be_bytes()); // isFixedPitch
    post.extend_from_slice(&[0; 16]);
    let tables: [(&[u8; 4], Vec<u8>); 9] = [
        (b"cmap", cmap),
        (b"glyf", glyf),
        (b"head", head),
        (b"hhea", hhea),
        (b"hmtx", hmtx),
        (b"loca", loca),
        (b"maxp", maxp),
        (b"name", name),
        (b"post", post),
    ];
    let checksum = |bytes: &[u8]| -> u32 {
        bytes.chunks(4).map(|c| c.iter().enumerate().fold(0u32, |a, (i, b)| a | (u32::from(*b) << (24 - 8 * i)))).fold(0u32, u32::wrapping_add)
    };
    let n = tables.len() as u16;
    let mut out = 0x0001_0000u32.to_be_bytes().to_vec();
    // numTables, searchRange, entrySelector, rangeShift for 9 tables.
    for v in [n, 128, 3, n * 16 - 128] {
        out.extend_from_slice(&v.to_be_bytes());
    }
    let mut offset = 12 + 16 * tables.len();
    let mut body = Vec::new();
    let mut head_at = 0;
    for (tag, data) in &tables {
        out.extend_from_slice(*tag);
        out.extend_from_slice(&checksum(data).to_be_bytes());
        out.extend_from_slice(&(offset as u32).to_be_bytes());
        out.extend_from_slice(&(data.len() as u32).to_be_bytes());
        if *tag == b"head" {
            head_at = 12 + 16 * tables.len() + body.len();
        }
        body.extend_from_slice(data);
        while body.len() % 4 != 0 {
            body.push(0);
        }
        offset = 12 + 16 * tables.len() + body.len();
    }
    out.extend_from_slice(&body);
    let adjust = 0xB1B0_AFBAu32.wrapping_sub(checksum(&out));
    if let Some(slot) = out.get_mut(head_at + 8..head_at + 12) {
        slot.copy_from_slice(&adjust.to_be_bytes());
    }
    out
}

/// `/ToUnicode` for the font: every code is the UTF-16 code unit it names.
fn to_unicode() -> Vec<u8> {
    let mut s = String::from(
        "/CIDInit /ProcSet findresource begin\n12 dict begin\nbegincmap\n/CIDSystemInfo << /Registry (Adobe) /Ordering (UCS) /Supplement 0 >> def\n/CMapName /Adobe-Identity-UCS def\n/CMapType 2 def\n1 begincodespacerange\n<0000> <FFFF>\nendcodespacerange\n",
    );
    // One range per high byte (a range may only vary in its last byte); no surrogates.
    let highs: Vec<u32> = (0u32..=0xFF).filter(|h| !(0xD8..=0xDF).contains(h)).collect();
    for chunk in highs.chunks(100) {
        s.push_str(&format!("{} beginbfrange\n", chunk.len()));
        for h in chunk {
            s.push_str(&format!("<{h:02X}00> <{h:02X}FF> <{h:02X}00>\n"));
        }
        s.push_str("endbfrange\n");
    }
    s.push_str("endcmap\nCMapName currentdict /CMap defineresource pop\nend\nend\n");
    s.into_bytes()
}

/// Add the font's objects to `doc` and return the Type0 font dictionary's reference.
pub fn add_glyphless_font(doc: &mut Document) -> ObjRef {
    let program = glyphless_truetype();
    let mut file = Dict::new();
    file.set(b"Length1".to_vec(), Object::Int(program.len() as i64));
    let file = doc.add(Object::Stream(Stream::flate(file, &program)));
    let mut descriptor = Dict::new();
    descriptor.set(b"Type".to_vec(), Object::name("FontDescriptor"));
    descriptor.set(b"FontName".to_vec(), Object::name(GLYPHLESS_NAME));
    descriptor.set(b"Flags".to_vec(), Object::Int(4));
    descriptor.set(b"FontBBox".to_vec(), Object::Array([0, -250, 1000, 750].map(Object::Int).to_vec()));
    descriptor.set(b"ItalicAngle".to_vec(), Object::Int(0));
    descriptor.set(b"Ascent".to_vec(), Object::Int(750));
    descriptor.set(b"Descent".to_vec(), Object::Int(-250));
    descriptor.set(b"CapHeight".to_vec(), Object::Int(750));
    descriptor.set(b"StemV".to_vec(), Object::Int(80));
    descriptor.set(b"FontFile2".to_vec(), Object::Ref(file));
    let descriptor = doc.add(Object::Dict(descriptor));
    // Every code shows glyph 1.
    let gids: Vec<u8> = std::iter::repeat_n([0u8, 1u8], 0x1_0000).flatten().collect();
    let gids = doc.add(Object::Stream(Stream::flate(Dict::new(), &gids)));
    let mut system = Dict::new();
    system.set(b"Registry".to_vec(), Object::String(PdfString::literal(b"Adobe".to_vec())));
    system.set(b"Ordering".to_vec(), Object::String(PdfString::literal(b"Identity".to_vec())));
    system.set(b"Supplement".to_vec(), Object::Int(0));
    let mut cid = Dict::new();
    cid.set(b"Type".to_vec(), Object::name("Font"));
    cid.set(b"Subtype".to_vec(), Object::name("CIDFontType2"));
    cid.set(b"BaseFont".to_vec(), Object::name(GLYPHLESS_NAME));
    cid.set(b"CIDSystemInfo".to_vec(), Object::Dict(system));
    cid.set(b"FontDescriptor".to_vec(), Object::Ref(descriptor));
    cid.set(b"DW".to_vec(), Object::Int(i64::from(EM)));
    cid.set(b"CIDToGIDMap".to_vec(), Object::Ref(gids));
    let cid = doc.add(Object::Dict(cid));
    let cmap = doc.add(Object::Stream(Stream::flate(Dict::new(), &to_unicode())));
    let mut top = Dict::new();
    top.set(b"Type".to_vec(), Object::name("Font"));
    top.set(b"Subtype".to_vec(), Object::name("Type0"));
    top.set(b"BaseFont".to_vec(), Object::name(GLYPHLESS_NAME));
    top.set(b"Encoding".to_vec(), Object::name("Identity-H"));
    top.set(b"DescendantFonts".to_vec(), Object::Array(vec![Object::Ref(cid)]));
    top.set(b"ToUnicode".to_vec(), Object::Ref(cmap));
    top.set(GLYPHLESS_MARK.to_vec(), Object::Bool(true));
    doc.add(Object::Dict(top))
}

/// Whether `font` is a dictionary [`add_glyphless_font`] wrote.
pub fn is_glyphless_font(doc: &Document, font: &Object) -> bool {
    doc.resolve(font).as_dict().is_some_and(|d| {
        d.name(b"Subtype") == Some(b"Type0") && d.name(b"BaseFont") == Some(GLYPHLESS_NAME.as_bytes()) && d.get(GLYPHLESS_MARK).is_some()
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use skrifa::raw::TableProvider;

    #[test]
    fn the_program_is_a_valid_truetype_font() {
        let bytes = glyphless_truetype();
        let font = skrifa::FontRef::new(&bytes).unwrap();
        assert_eq!(font.maxp().unwrap().num_glyphs(), 2);
        assert_eq!(font.head().unwrap().units_per_em(), 1000);
        assert_eq!(font.hmtx().unwrap().h_metrics().len(), 2);
        // The whole font sums to the magic number (checkSumAdjustment is right).
        let sum =
            bytes.chunks(4).map(|c| c.iter().enumerate().fold(0u32, |a, (i, b)| a | (u32::from(*b) << (24 - 8 * i)))).fold(0u32, u32::wrapping_add);
        assert_eq!(sum, 0xB1B0_AFBA);
    }

    #[test]
    fn codes_are_utf16_code_units() {
        assert_eq!(glyphless_codes("بA"), vec![0x06, 0x28, 0x00, 0x41]);
        assert_eq!(glyphless_codes("😀"), vec![0xFF, 0xFD]);
    }

    #[test]
    fn to_unicode_maps_every_code_but_surrogates() {
        let cmap = String::from_utf8(to_unicode()).unwrap();
        assert!(cmap.contains("<0600> <06FF> <0600>"), "Arabic");
        assert!(cmap.contains("<0000> <00FF> <0000>"));
        assert!(!cmap.contains("<D800>"));
        assert_eq!(cmap.matches("beginbfrange").count(), 3);
    }

    #[test]
    fn the_font_is_recognised_for_reuse() {
        let mut doc = Document::new_empty();
        let font = add_glyphless_font(&mut doc);
        assert!(is_glyphless_font(&doc, &Object::Ref(font)));
        assert!(!is_glyphless_font(&doc, &Object::Int(1)));
    }
}
