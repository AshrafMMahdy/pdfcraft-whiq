//! Lines in Arabic script, with the Latin words and numbers inside them.
//!
//! Every line ocrs finds is read by the Arabic model too. A line where it reads Arabic letters is
//! an Arabic (or mixed) line and takes the Arabic reading; any other line keeps ocrs's reading.
//!
//! Inside an Arabic line the Arabic model reads Latin words and numbers poorly (it expects Arabic
//! around them, and drops or garbles them), while ocrs reads them well. So the line is checked
//! word box by word box (the boxes ocrs's detector found). A box read as more Latin letters and
//! ASCII digits than Arabic letters, as nothing, or as far fewer characters than its width holds
//! is *doubtful*, and so is an Arabic word read unsurely, or any Arabic word in a line mostly in
//! Latin letters (the model reads Arabic less well there). Neighbouring doubtful boxes of a kind
//! form a run, read again on its own by both models. Read alone, the Arabic model tells an Arabic
//! word from a Latin one: an Arabic word takes that reading when it is surer than the line's (and
//! never where the line's reading had nothing: a piece of a word already read), a Latin word
//! takes ocrs's, and a number the Arabic model's unless ocrs sees letters in it.

use rten_imageproc::RotatedRect;

use crate::arabic::{ArabicModel, Frame, Glyph, Grey, TALL, is_arabic_letter};
use crate::bidi::{self, mirrored};
use crate::{Line, OcrError, Word};

/// Arabic letters a line needs (read with at least [`SURE`] confidence), and their least share of
/// its letters, to count as an Arabic line; or one, in a line read with [`SURE_SHORT`] mean
/// confidence.
const MIN_ARABIC_LETTERS: usize = 2;
const MIN_ARABIC_SHARE: f32 = 0.15;
const SURE: f32 = 0.6;
const SURE_SHORT: f32 = 0.85;
/// A line read with less mean confidence than this is read again with more room.
const RETRY_BELOW: f32 = 0.85;
/// A box the line's reading had nothing in is filled only when the model reads it alone this
/// surely.
const FILL_SURE: f32 = 0.75;
/// A line that is not Arabic, which the Arabic model reads this unsurely and ocrs as a few
/// characters, is noise on the scan.
const NOISE_BELOW: f32 = 0.5;
pub(crate) const NOISE_CHARS: usize = 6;
/// An Arabic word box read with less mean confidence than this is doubtful.
const DOUBTFUL_BELOW: f32 = 0.6;

/// The mean confidence of the characters read (spaces aside); 0 for none.
fn mean_prob<'a>(glyphs: impl Iterator<Item = &'a Glyph>) -> f32 {
    let (n, sum) = glyphs.filter(|g| g.text != " ").fold((0usize, 0.0f32), |(n, s), g| (n + 1, s + g.prob));
    if n == 0 { 0.0 } else { sum / n as f32 }
}

/// What the Arabic model makes of one of ocrs's lines.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum Verdict {
    /// Not Arabic: ocrs's reading stands.
    Latin,
    /// Not Arabic and read very unsurely: noise, if ocrs reads only a few characters there.
    Unsure,
    /// An Arabic line, read.
    Arabic(Line),
}

/// The Arabic model's verdict on each of `lines` (ocrs's lines of word boxes).
pub(crate) fn read(
    model: &ArabicModel,
    engine: &ocrs::OcrEngine,
    input: &ocrs::OcrInput,
    img: &Grey,
    lines: &[Vec<RotatedRect>],
) -> Result<Vec<Verdict>, OcrError> {
    let mut framed: Vec<(usize, Frame)> = lines.iter().enumerate().filter_map(|(i, l)| Frame::of_line(l).map(|f| (i, f))).collect();
    let mut reads = model.read(img, &framed.iter().map(|(_, f)| *f).collect::<Vec<_>>())?;
    // Lines read unsurely (vowel marks cut off, often) are read again with more room above and
    // below; the surer reading stays.
    let unsure: Vec<(usize, Frame)> = framed
        .iter()
        .zip(&reads)
        .enumerate()
        .filter(|(_, (_, glyphs))| {
            mean_prob(glyphs.iter()) < RETRY_BELOW && glyphs.iter().any(|g| g.text.chars().next().is_some_and(is_arabic_letter))
        })
        .filter_map(|(k, ((i, _), _))| lines.get(*i).and_then(|l| Frame::padded(l, TALL)).map(|f| (k, f)))
        .collect();
    if !unsure.is_empty() {
        let again = model.read(img, &unsure.iter().map(|(_, f)| *f).collect::<Vec<_>>())?;
        for ((k, frame), glyphs) in unsure.into_iter().zip(again) {
            if let (Some(old), Some(slot)) = (reads.get_mut(k), framed.get_mut(k))
                && mean_prob(glyphs.iter()) > mean_prob(old.iter())
            {
                *old = glyphs;
                slot.1 = frame;
            }
        }
    }
    // How the page reads, for lines with about as many letters of each script.
    let count = |f: fn(char) -> bool| reads.iter().flatten().filter(|g| g.prob >= SURE && g.text.chars().next().is_some_and(f)).count();
    let page_rtl = count(is_arabic_letter) >= count(|c| c.is_ascii_alphabetic());
    let mut plans: Vec<Plan> = Vec::new();
    let mut out: Vec<Verdict> = vec![Verdict::Latin; lines.len()];
    for ((i, frame), glyphs) in framed.into_iter().zip(reads) {
        if !is_arabic_line(&glyphs) {
            if mean_prob(glyphs.iter()) < NOISE_BELOW
                && let Some(v) = out.get_mut(i)
            {
                *v = Verdict::Unsure;
            }
            continue;
        }
        let Some(boxes) = lines.get(i) else { continue };
        plans.push(Plan::new(i, frame, glyphs, boxes));
    }
    // Read every doubtful box again, alone, with both models (one batch each).
    let doubts: Vec<(usize, usize, Frame)> = plans
        .iter()
        .enumerate()
        .flat_map(|(p, plan)| plan.doubts.iter().enumerate().filter_map(move |(d, doubt)| Frame::of_line(&doubt.words).map(|f| (p, d, f))))
        .collect();
    if !doubts.is_empty() {
        let alone = model.read(img, &doubts.iter().map(|(_, _, f)| *f).collect::<Vec<_>>())?;
        let boxes: Vec<Vec<RotatedRect>> =
            doubts.iter().filter_map(|(p, d, _)| plans.get(*p).and_then(|plan| plan.doubts.get(*d)).map(|doubt| doubt.words.clone())).collect();
        let latin = engine.recognize_text(input, &boxes).map_err(|e| OcrError::Recognize(e.to_string()))?;
        for (((p, d, frame), alone), latin) in doubts.iter().zip(alone).zip(latin) {
            let Some(plan) = plans.get_mut(*p) else { continue };
            let line = plan.frame;
            let glyphs = &plan.glyphs;
            if let Some(doubt) = plan.doubts.get_mut(*d) {
                doubt.fill = choose(&line, frame, if doubt.empty { 2.0 } else { doubt.sure }, &alone, latin.as_ref());
                if let Some(fill) = doubt.fill.as_mut() {
                    let context: Vec<&Glyph> = doubt.owned.iter().filter_map(|i| glyphs.get(*i)).collect();
                    keep_end_punctuation(&context, fill);
                }
            }
        }
    }
    let margins = Margins::of(lines);
    for plan in plans {
        let index = plan.index;
        let aligned = lines.get(index).and_then(|l| margins.as_ref().and_then(|m| m.side(l)));
        if let Some(slot) = out.get_mut(index) {
            *slot = Verdict::Arabic(plan.assemble(aligned, page_rtl));
        }
    }
    Ok(out)
}

/// Whether the Arabic model's reading of a line is Arabic: enough sure Arabic letters, or one in
/// a short, surely read line (a unit such as "م3" in a table).
fn is_arabic_line(glyphs: &[Glyph]) -> bool {
    let letters = |f: fn(char) -> bool| glyphs.iter().filter(|g| g.prob >= SURE && g.text.chars().next().is_some_and(f)).count();
    let arabic = letters(is_arabic_letter);
    let latin = letters(|c| c.is_ascii_alphabetic());
    let share = arabic as f32 >= MIN_ARABIC_SHARE * (arabic + latin) as f32;
    (arabic >= MIN_ARABIC_LETTERS && share) || (arabic >= 1 && arabic >= latin && mean_prob(glyphs.iter()) >= SURE_SHORT)
}

/// The page's text margins: the left and right ends of its long lines.
struct Margins {
    left: f32,
    right: f32,
}

impl Margins {
    fn of(lines: &[Vec<RotatedRect>]) -> Option<Margins> {
        let ends: Vec<(f32, f32)> = lines.iter().filter_map(|l| ends(l)).collect();
        let widest = ends.iter().map(|(a, b)| b - a).fold(0.0f32, f32::max);
        // Long lines only: a speck of noise near the edge is not a margin.
        let long: Vec<&(f32, f32)> = ends.iter().filter(|(a, b)| b - a >= 0.5 * widest).collect();
        if long.len() < 2 {
            return None;
        }
        let left = long.iter().map(|(a, _)| *a).fold(f32::MAX, f32::min);
        let right = long.iter().map(|(_, b)| *b).fold(f32::MIN, f32::max);
        (right > left).then_some(Margins { left, right })
    }

    /// Which margin a line keeps to: `Some(true)` the right one only (a right-to-left
    /// paragraph's line), `Some(false)` the left one only, `None` both or neither.
    fn side(&self, line: &[RotatedRect]) -> Option<bool> {
        let (a, b) = ends(line)?;
        let tol = 0.03 * (self.right - self.left);
        match (a - self.left <= tol, self.right - b <= tol) {
            (false, true) => Some(true),
            (true, false) => Some(false),
            _ => None,
        }
    }
}

/// The left and right ends of a line of word boxes on the page.
fn ends(line: &[RotatedRect]) -> Option<(f32, f32)> {
    let xs = line.iter().flat_map(|w| w.corners()).map(|p| p.x);
    let (a, b) = xs.fold((f32::MAX, f32::MIN), |(a, b), x| (a.min(x), b.max(x)));
    (a.is_finite() && b.is_finite() && b > a).then_some((a, b))
}

/// More Latin letters and ASCII digits than Arabic letters: what the Arabic model reads poorly
/// inside an Arabic line. (A stray digit or two in an Arabic word box is usually the number
/// beside it, placed a column off.)
fn latinish(glyphs: &[&Glyph]) -> bool {
    let count = |f: fn(char) -> bool| glyphs.iter().filter(|g| g.text.chars().next().is_some_and(f)).count();
    count(|c| c.is_ascii_alphanumeric()) > count(is_arabic_letter)
}

/// A run of neighbouring doubtful word boxes (a Latin phrase, a number): its boxes, its span along
/// the line, whether the line's reading had nothing in it, the characters of that reading it owns
/// (replaced when it is filled), and its fill.
struct Doubt {
    words: Vec<RotatedRect>,
    start: f32,
    end: f32,
    empty: bool,
    /// The mean confidence of the line's reading in the run.
    sure: f32,
    owned: Vec<usize>,
    fill: Option<Vec<Glyph>>,
}

/// An Arabic line being read: the Arabic model's characters, and the doubtful boxes.
struct Plan {
    index: usize,
    frame: Frame,
    glyphs: Vec<Glyph>,
    /// The word boxes' spans along the line, left to right.
    spans: Vec<(f32, f32)>,
    doubts: Vec<Doubt>,
}

impl Plan {
    fn new(index: usize, frame: Frame, glyphs: Vec<Glyph>, boxes: &[RotatedRect]) -> Plan {
        let mut boxes: Vec<(RotatedRect, (f32, f32))> = boxes.iter().map(|b| (*b, frame.span(b))).collect();
        boxes.sort_by(|a, b| a.1.0.total_cmp(&b.1.0));
        // Each character belongs to the nearest box: the model's columns are coarse (a few
        // pixels of the page each), so a character's centre may fall just outside its word.
        let reach = (frame.ink_bottom - frame.ink_top) * 0.5;
        let owner = |g: &Glyph| -> Option<usize> {
            let c = g.centre();
            let gap = |(a, b): (f32, f32)| {
                if c < a {
                    a - c
                } else if c > b {
                    c - b
                } else {
                    0.0
                }
            };
            boxes.iter().enumerate().map(|(k, (_, s))| (k, gap(*s))).filter(|(_, d)| *d <= reach).min_by(|x, y| x.1.total_cmp(&y.1)).map(|(k, _)| k)
        };
        let owners: Vec<Option<usize>> = glyphs.iter().map(|g| if g.text == " " { None } else { owner(g) }).collect();
        // In a line mostly in Latin letters the model reads the Arabic in it less well: its
        // Arabic words are read again alone too.
        let count = |f: fn(char) -> bool| glyphs.iter().filter(|g| g.text.chars().next().is_some_and(f)).count();
        let latin_line = count(|c| c.is_ascii_alphabetic()) > count(is_arabic_letter);
        let mut doubts: Vec<Doubt> = Vec::new();
        let mut last: Option<(usize, bool)> = None;
        for (k, (word, (start, end))) in boxes.iter().enumerate() {
            let owned: Vec<usize> = owners.iter().enumerate().filter(|(_, o)| **o == Some(k)).map(|(i, _)| i).collect();
            let inside: Vec<&Glyph> = owned.iter().filter_map(|i| glyphs.get(*i)).collect();
            // Far fewer characters than the box is wide also means something was dropped (a
            // number read as a comma).
            let sparse = (inside.iter().filter(|g| g.text != " ").count() as f32) * 2.2 * reach < end - start;
            let latin = inside.is_empty() || sparse || latinish(&inside);
            let sure = mean_prob(inside.iter().copied());
            let arabic = !latin && (sure < DOUBTFUL_BELOW || latin_line);
            if !(latin || arabic) {
                continue;
            }
            // How sure the line's reading is: a minority Arabic word's reading is not trusted.
            let sure = if latin_line && arabic { 0.0 } else { sure };
            // A doubtful box right after another of its kind, a word space away, joins its run.
            match doubts.last_mut() {
                Some(d) if last.is_some_and(|(j, l)| j + 1 == k && l == latin) && *start - d.end <= 3.0 * reach => {
                    d.words.push(*word);
                    d.end = d.end.max(*end);
                    d.empty &= inside.is_empty();
                    d.owned.extend(owned);
                    d.sure = d.sure.min(sure);
                }
                _ => doubts.push(Doubt { words: vec![*word], start: *start, end: *end, empty: inside.is_empty(), sure, owned, fill: None }),
            }
            last = Some((k, latin));
        }
        Plan { index, frame, glyphs, spans: boxes.iter().map(|(_, s)| *s).collect(), doubts }
    }

    /// The line: the Arabic reading with each filled doubtful box's characters replaced by its
    /// fill, in logical order.
    fn assemble(self, aligned: Option<bool>, page_rtl: bool) -> Line {
        let filled: Vec<&Doubt> =
            self.doubts.iter().filter(|d| d.fill.as_ref().is_some_and(|f| f.iter().any(|g| !g.text.trim().is_empty()))).collect();
        let replaced: std::collections::HashSet<usize> = filled.iter().flat_map(|d| d.owned.iter().copied()).collect();
        // Characters left to right along the line, the fills in their places.
        let mut seq: Vec<Glyph> = self.glyphs.iter().enumerate().filter(|(i, _)| !replaced.contains(i)).map(|(_, g)| g.clone()).collect();
        for d in &filled {
            let space = |at: f32| Glyph { text: " ".into(), start: at, end: at, prob: 1.0 };
            seq.retain(|g| !(g.text == " " && (d.start..=d.end).contains(&g.centre())));
            seq.push(space(d.start - 0.01));
            seq.extend(d.fill.iter().flatten().cloned().map(|mut g| {
                // Keep the fill inside its box so it can't swap places with its neighbours.
                // (min/max, not clamp: a degenerate span must not panic.)
                g.start = g.start.max(d.start).min(d.end);
                g.end = g.end.max(g.start).min(d.end);
                g
            }));
            seq.push(space(d.end + 0.01));
        }
        seq.sort_by(|a, b| a.centre().total_cmp(&b.centre()));
        // One space between words, none at the ends.
        let mut chars: Vec<Glyph> = Vec::with_capacity(seq.len());
        for g in seq {
            let blank = g.text.trim().is_empty();
            if blank && chars.last().is_none_or(|l: &Glyph| l.text == " ") {
                continue;
            }
            chars.push(if blank { Glyph { text: " ".into(), ..g } } else { g });
        }
        while chars.last().is_some_and(|g| g.text == " ") {
            chars.pop();
        }
        let texts: Vec<&str> = chars.iter().map(|g| g.text.as_str()).collect();
        let classes: Vec<bidi::Class> = texts.iter().map(|t| bidi::class_of(t)).collect();
        let rtl = bidi::rtl_base(&classes, aligned, page_rtl);
        let order = bidi::logical_order(&texts, rtl);
        // Words in logical order, each with its span of characters along the line.
        let mut spans: Vec<(String, f32, f32)> = Vec::new();
        let mut text = String::new();
        let (mut lo, mut hi) = (f32::MAX, f32::MIN);
        for (i, m) in order {
            let Some(g) = chars.get(i) else { continue };
            if g.text == " " {
                if !text.is_empty() {
                    spans.push((std::mem::take(&mut text), lo, hi));
                }
                (lo, hi) = (f32::MAX, f32::MIN);
                continue;
            }
            text.extend(g.text.chars().map(|c| if m { mirrored(c) } else { c }));
            lo = lo.min(g.centre());
            hi = hi.max(g.centre());
        }
        if !text.is_empty() {
            spans.push((text, lo, hi));
        }
        let words = self.word_boxes(spans);
        Line { words, rtl }
    }

    /// Each word's box: from its characters out to the edges of the detector's boxes they are in,
    /// but never into its neighbours (a detector box holding two words is split between them,
    /// leaving a gap where the space is, so the text layer reads back with the space).
    fn word_boxes(&self, spans: Vec<(String, f32, f32)>) -> Vec<Word> {
        let gap = (self.frame.ink_bottom - self.frame.ink_top) * 0.3;
        let mut by_place: Vec<usize> = (0..spans.len()).collect();
        by_place.sort_by(|a, b| spans.get(*a).map_or(0.0, |s| s.1).total_cmp(&spans.get(*b).map_or(0.0, |s| s.1)));
        let mut extent: Vec<(f32, f32)> = spans.iter().map(|(_, lo, hi)| (*lo, *hi)).collect();
        for (k, &i) in by_place.iter().enumerate() {
            let Some((lo, hi)) = spans.get(i).map(|(_, lo, hi)| (*lo, *hi)) else { continue };
            let (mut a, mut b) = (lo, hi);
            for (s, e) in &self.spans {
                if *s <= hi && *e >= lo {
                    a = a.min(*s);
                    b = b.max(*e);
                }
            }
            // Not past the middle of the gap to the neighbouring words' characters.
            if let Some(prev) = k.checked_sub(1).and_then(|j| by_place.get(j)).and_then(|j| spans.get(*j)) {
                a = a.max((prev.2 + lo) / 2.0 + gap / 2.0).min(lo);
            }
            if let Some(next) = by_place.get(k + 1).and_then(|j| spans.get(*j)) {
                b = b.min((hi + next.1) / 2.0 - gap / 2.0).max(hi);
            }
            if let Some(slot) = extent.get_mut(i) {
                *slot = (a, b);
            }
        }
        spans.into_iter().zip(extent).map(|((text, _, _), (a, b))| Word { text, rect: self.frame.ink_box(a, b) }).collect()
    }
}

/// Read alone, the Arabic model takes Latin punctuation at the edge of a crop for a letter (a
/// comma after an Arabic name for "و"): the line's reading of punctuation at either end of the
/// run is kept.
fn keep_end_punctuation(context: &[&Glyph], fill: &mut Vec<Glyph>) {
    let punct = |g: &Glyph| g.text.chars().all(|c| c.is_ascii_punctuation());
    let arabic_fill = fill.iter().any(|g| g.text.chars().next().is_some_and(is_arabic_letter));
    if !arabic_fill || fill.is_empty() {
        return;
    }
    let mut ctx: Vec<&Glyph> = context.iter().copied().filter(|g| g.text != " ").collect();
    ctx.sort_by(|a, b| a.centre().total_cmp(&b.centre()));
    for left in [true, false] {
        let Some(c) = (if left { ctx.first() } else { ctx.last() }) else { continue };
        if !punct(c) {
            continue;
        }
        let end = if left { fill.first() } else { fill.last() };
        match end {
            Some(e) if e.text == c.text => {}
            // A lone letter where the line read punctuation is that punctuation, misread.
            Some(e) if e.text == "و" || punct(e) => {
                let i = if left { 0 } else { fill.len() - 1 };
                if let Some(slot) = fill.get_mut(i) {
                    *slot = Glyph { text: c.text.clone(), ..slot.clone() };
                }
            }
            _ => {
                let g = Glyph { text: c.text.clone(), start: c.start, end: c.end, prob: c.prob };
                if left { fill.insert(0, g) } else { fill.push(g) }
            }
        }
    }
}

/// The fill of a doubtful run, in the line's frame, or `None` to keep the line's reading. `sure`
/// is the line's reading's confidence in the run (above 1 when it read nothing there: a piece of
/// a word the line's reading put in the box beside it).
/// - read alone as Arabic: that reading, when it is surer than the line's;
/// - a Latin word (ocrs sees letters): ocrs's reading;
/// - a number (digits and signs): the Arabic model's.
fn choose(line: &Frame, frame: &Frame, sure: f32, alone: &[Glyph], latin: Option<&ocrs::TextLine>) -> Option<Vec<Glyph>> {
    use ocrs::TextItem;
    let first = |g: &Glyph, f: fn(char) -> bool| g.text.chars().next().is_some_and(f);
    let arabic = alone.iter().filter(|g| first(g, is_arabic_letter)).count();
    let other = alone.iter().filter(|g| first(g, |c| c.is_ascii_alphanumeric())).count();
    let mid = (frame.ink_top + frame.ink_bottom) / 2.0;
    let alone_fill = || -> Vec<Glyph> {
        alone
            .iter()
            .map(|g| {
                let (a, b) = (line.along(frame.point(g.start, mid)), line.along(frame.point(g.end, mid)));
                Glyph { text: g.text.clone(), start: a.min(b), end: a.max(b), prob: g.prob }
            })
            .collect()
    };
    if arabic > 0 && arabic >= other {
        return (mean_prob(alone.iter()) > sure).then(alone_fill);
    }
    // Where the line's reading had nothing, only something the model reads surely alone (a
    // number, a word) is put in: not a speck of noise.
    if sure > 1.0 && mean_prob(alone.iter()) < FILL_SURE {
        return None;
    }
    let latin: Vec<Glyph> = latin
        .map(|l| {
            l.chars()
                .iter()
                .map(|c| {
                    let r = c.rect;
                    let (a, b) = (line.along([r.left() as f32, r.top() as f32]), line.along([r.right() as f32, r.bottom() as f32]));
                    Glyph { text: c.char.to_string(), start: a.min(b), end: a.max(b), prob: 1.0 }
                })
                .collect()
        })
        .unwrap_or_default();
    let letters = latin.iter().any(|g| first(g, |c| c.is_ascii_alphabetic()));
    let number = alone.iter().any(|g| first(g, |c| c.is_ascii_digit())) && !alone.iter().any(|g| first(g, |c| c.is_ascii_alphabetic()));
    if number && !letters {
        return Some(alone_fill());
    }
    if latin.is_empty() { Some(alone_fill()) } else { Some(latin) }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rten_imageproc::{PointF, Vec2};

    fn g(text: &str, start: f32, prob: f32) -> Glyph {
        Glyph { text: text.into(), start, end: start + 4.0, prob }
    }

    #[test]
    fn a_line_is_arabic_when_it_reads_enough_arabic_letters() {
        let ar: Vec<Glyph> = "مشروع".chars().enumerate().map(|(i, c)| g(&c.to_string(), i as f32 * 5.0, 0.9)).collect();
        assert!(is_arabic_line(&ar));
        // A stray letter on a Latin line is not enough, nor unsure ones.
        let stray: Vec<Glyph> = "Concreteب".chars().enumerate().map(|(i, c)| g(&c.to_string(), i as f32 * 5.0, 0.9)).collect();
        assert!(!is_arabic_line(&stray));
        let unsure: Vec<Glyph> = "مشروع".chars().enumerate().map(|(i, c)| g(&c.to_string(), i as f32 * 5.0, 0.3)).collect();
        assert!(!is_arabic_line(&unsure));
    }

    #[test]
    fn latin_letters_and_digits_are_latinish() {
        let (a, b, d) = (g("م", 0.0, 0.95), g("T", 0.0, 0.99), g("1", 0.0, 0.99));
        assert!(!latinish(&[&a]));
        assert!(latinish(&[&b]));
        assert!(latinish(&[&d]));
        // A stray digit in an Arabic word is not enough; Latin outnumbering Arabic is.
        assert!(!latinish(&[&a, &a, &d]));
        assert!(latinish(&[&a, &b, &d]));
        assert!(!latinish(&[&g("٢", 0.0, 0.9)]));
    }

    /// An Arabic line with a number the model dropped: the number's box is filled from its own
    /// reading, in its place, and the line reads in typed order.
    #[test]
    fn a_dropped_number_is_filled_in_its_place() {
        let up = Vec2::from_yx(-1.0, 0.0);
        let bx = |x0: f32, x1: f32| RotatedRect::new(PointF::from_yx(20.0, (x0 + x1) / 2.0), up, x1 - x0, 20.0);
        // As drawn, left to right: "يوما" (turned), "28", "بعد" (turned).
        let boxes = [bx(10.0, 50.0), bx(60.0, 80.0), bx(90.0, 120.0)];
        let frame = Frame::of_line(&boxes).unwrap();
        let x = |page: f32| frame.along([page, 20.0]);
        let mut glyphs: Vec<Glyph> = "اموي".chars().enumerate().map(|(i, c)| g(&c.to_string(), x(12.0 + i as f32 * 9.0), 0.95)).collect();
        glyphs.push(g(" ", x(55.0), 0.9));
        glyphs.push(g(" ", x(85.0), 0.9));
        glyphs.extend("دعب".chars().enumerate().map(|(i, c)| g(&c.to_string(), x(92.0 + i as f32 * 9.0), 0.95)));
        let mut plan = Plan::new(0, frame, glyphs, &boxes);
        assert_eq!(plan.doubts.len(), 1, "the empty box is the one doubtful run");
        assert!(plan.doubts[0].empty);
        // Read alone, the box is a number: the Arabic model's reading fills it.
        let alone = [g("2", 2.0, 0.99), g("8", 12.0, 0.99)];
        let word = Frame::of_line(&[boxes[1]]).unwrap();
        plan.doubts[0].fill = choose(&frame, &word, 2.0, &alone, None);
        let line = plan.assemble(None, true);
        assert!(line.rtl);
        assert_eq!(line.text(), "بعد 28 يوما");
        // Each word keeps its own box on the page.
        let num = line.words.iter().find(|w| w.text == "28").unwrap();
        assert!(num.rect[0] >= 59.0 && num.rect[2] <= 81.0, "{num:?}");
    }

    /// A comma the line read after an Arabic name, which the model read alone as "و", stays a comma.
    #[test]
    fn punctuation_at_the_ends_of_a_run_is_kept() {
        let ctx = [g("ن", 10.0, 0.9), g("م", 20.0, 0.9), g(",", 40.0, 0.9)];
        let ctx: Vec<&Glyph> = ctx.iter().collect();
        let mut fill = vec![g("ن", 10.0, 0.9), g("م", 20.0, 0.9), g("و", 40.0, 0.9)];
        keep_end_punctuation(&ctx, &mut fill);
        assert_eq!(fill.iter().map(|g| g.text.as_str()).collect::<String>(), "نم,");
        // A bracket the alone reading dropped is put back.
        let ctx = [g("(", 0.0, 0.9), g("ن", 10.0, 0.9)];
        let ctx: Vec<&Glyph> = ctx.iter().collect();
        let mut fill = vec![g("ن", 10.0, 0.9)];
        keep_end_punctuation(&ctx, &mut fill);
        assert_eq!(fill.iter().map(|g| g.text.as_str()).collect::<String>(), "(ن");
    }

    /// An empty box that reads as Arabic alone is a piece of a word already read: no fill.
    #[test]
    fn an_empty_box_read_as_arabic_is_left_alone() {
        let up = Vec2::from_yx(-1.0, 0.0);
        let b = RotatedRect::new(PointF::from_yx(20.0, 50.0), up, 40.0, 20.0);
        let f = Frame::of_line(&[b]).unwrap();
        let alone = [g("ب", 2.0, 0.99), g("ة", 12.0, 0.99)];
        assert_eq!(choose(&f, &f, 2.0, &alone, None), None);
        assert!(choose(&f, &f, 0.5, &alone, None).is_some());
        // Nor is a box the line's reading was surer of.
        assert_eq!(choose(&f, &f, 0.995, &alone, None), None);
    }
}
