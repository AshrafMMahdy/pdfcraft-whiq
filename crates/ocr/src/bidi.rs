//! Right-to-left lines read from a scan.
//!
//! A recognition model reads a line as it is drawn, left to right (*visual* order). Arabic is
//! typed right to left, with Latin words and numbers inside it left to right, so the text a
//! person would type (*logical* order, what `read` and the text layer's Unicode hold) is the
//! visual line with its right-to-left stretches turned around. [`logical_order`] gives that
//! order as a permutation of the characters read, so each character keeps its place on the page.
//! [`visual_word`] goes the other way for one word, for drawing it into the text layer.
//!
//! The rules follow UAX #9 as the display of a single line applies it, and mirror
//! `pdfcraft-edit`'s `bidi::to_logical` (which works on whole strings): in a right-to-left line
//! everything reads right to left except Latin words and numbers; in a left-to-right line only
//! the Arabic stretches turn, numbers inside them still reading left to right. Brackets in a
//! turned stretch are mirrored. A model reads the vowel marks above and below a letter in the
//! order it sees them, before the letter in visual order, so turning a stretch puts them after
//! it, as typed.

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Class {
    Rtl,
    Ltr,
    Number,
    Neutral,
}

/// Letters, marks and punctuation of the right-to-left scripts' blocks (digits excepted).
pub(crate) fn is_rtl(c: char) -> bool {
    !is_digit(c) && matches!(u32::from(c), 0x0590..=0x08FF | 0xFB1D..=0xFDFF | 0xFE70..=0xFEFC)
}

fn is_digit(c: char) -> bool {
    c.is_ascii_digit() || matches!(c, '\u{0660}'..='\u{0669}' | '\u{06F0}'..='\u{06F9}')
}

/// Combining marks (Arabic harakat, Latin accents).
fn is_mark(c: char) -> bool {
    matches!(u32::from(c), 0x0300..=0x036F | 0x0610..=0x061A | 0x064B..=0x065F | 0x0670 | 0x06D6..=0x06DC | 0x06DF..=0x06E4 | 0x06E7..=0x06E8 | 0x06EA..=0x06ED)
}

pub(crate) fn class_of(text: &str) -> Class {
    match text.chars().next() {
        Some(c) if is_digit(c) => Class::Number,
        Some(c) if is_rtl(c) => Class::Rtl,
        Some(c) if c.is_alphabetic() => Class::Ltr,
        _ => Class::Neutral,
    }
}

pub(crate) fn mirrored(c: char) -> char {
    match c {
        '(' => ')',
        ')' => '(',
        '[' => ']',
        ']' => '[',
        '{' => '}',
        '}' => '{',
        '<' => '>',
        '>' => '<',
        '«' => '»',
        '»' => '«',
        other => other,
    }
}

/// Whether a line reads right to left as a whole. A line with far more letters of one script
/// (four times as many) reads that way. A mixed line ("Subject: تقديم الرسومات", "باستخدام
/// Primavera P6") belongs to its paragraph: `aligned` says which margin the line keeps to, when
/// it keeps to one (a paragraph's lines start at its margin), else the page's way (`page_rtl`).
pub(crate) fn rtl_base(classes: &[Class], aligned: Option<bool>, page_rtl: bool) -> bool {
    let r = classes.iter().filter(|c| **c == Class::Rtl).count();
    let l = classes.iter().filter(|c| **c == Class::Ltr).count();
    if r > l.saturating_mul(4) {
        true
    } else if l > r.saturating_mul(4) {
        false
    } else {
        aligned.unwrap_or(page_rtl)
    }
}

/// Maximal ranges that start and end with a `member` and hold only members and `bridge`s.
fn runs(cl: &[Class], member: impl Fn(Class) -> bool, bridge: impl Fn(&[Class], usize) -> bool) -> Vec<std::ops::Range<usize>> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < cl.len() {
        if !cl.get(i).is_some_and(|c| member(*c)) {
            i += 1;
            continue;
        }
        let mut end = i;
        let mut j = i + 1;
        while let Some(c) = cl.get(j) {
            if member(*c) {
                end = j;
            } else if !bridge(cl, j) {
                break;
            }
            j += 1;
        }
        out.push(i..end + 1);
        i = end + 1;
    }
    out
}

/// Whether the neutral at `j` sits between a Latin word on one side and a number on the other
/// (UAX #9 resolves such a space to the line's direction, so it splits the run).
fn neutral_between_word_and_number(cl: &[Class], j: usize) -> bool {
    let strong = |mut k: usize, step: isize| -> Option<Class> {
        loop {
            k = k.checked_add_signed(step)?;
            match *cl.get(k)? {
                Class::Neutral => continue,
                other => return Some(other),
            }
        }
    };
    matches!((strong(j, -1), strong(j, 1)), (Some(Class::Ltr), Some(Class::Number)) | (Some(Class::Number), Some(Class::Ltr)))
}

/// Separators that keep a number whole between two digits (`1,250`, `2026/10/18`, `12:30`).
fn number_separator(texts: &[&str], cl: &[Class], j: usize) -> bool {
    let digit = |k: Option<usize>| k.and_then(|k| cl.get(k)).is_some_and(|c| *c == Class::Number);
    texts.get(j).is_some_and(|t| matches!(*t, "." | "," | ":" | "/" | "-" | "+" | "٫" | "٬")) && digit(j.checked_sub(1)) && digit(Some(j + 1))
}

/// The logical order of a line read in visual order: indices into `texts` (one character, or a
/// ligature, each), with whether that character is drawn mirrored (a bracket in a turned
/// stretch, which reads as its partner). `rtl`: the line reads right to left ([`rtl_base`]).
pub(crate) fn logical_order(texts: &[&str], rtl: bool) -> Vec<(usize, bool)> {
    let classes: Vec<Class> = texts.iter().map(|t| class_of(t)).collect();
    let mut order: Vec<(usize, bool)> = (0..texts.len()).map(|i| (i, false)).collect();
    if !classes.contains(&Class::Rtl) {
        return order;
    }
    let turn = |o: &mut [(usize, bool)]| {
        o.reverse();
        for e in o.iter_mut() {
            e.1 = !e.1;
        }
    };
    if rtl {
        // Everything reads right to left, except Latin words and numbers.
        turn(&mut order);
        let cl: Vec<Class> = order.iter().map(|(i, _)| classes.get(*i).copied().unwrap_or(Class::Neutral)).collect();
        let tx: Vec<&str> = order.iter().map(|(i, _)| texts.get(*i).copied().unwrap_or("")).collect();
        let ltr = |c: Class| matches!(c, Class::Ltr | Class::Number);
        let bridge = |cl: &[Class], j: usize| {
            cl.get(j) == Some(&Class::Neutral)
                && (number_separator(&tx, cl, j) || (!neutral_between_word_and_number(cl, j) && !tx.get(j).is_some_and(|t| is_trailing_sign(t))))
        };
        for r in runs(&cl, ltr, bridge) {
            if let Some(run) = order.get_mut(r) {
                turn(run);
            }
        }
    } else {
        // Left to right, except the Arabic stretches (numbers inside them still left to right).
        let bridge = |cl: &[Class], j: usize| matches!(cl.get(j), Some(Class::Neutral | Class::Number));
        for r in runs(&classes, |c| c == Class::Rtl, bridge) {
            if let Some(run) = order.get_mut(r) {
                turn(run);
                let cl: Vec<Class> = run.iter().map(|(i, _)| classes.get(*i).copied().unwrap_or(Class::Neutral)).collect();
                let tx: Vec<&str> = run.iter().map(|(i, _)| texts.get(*i).copied().unwrap_or("")).collect();
                for n in runs(&cl, |c| c == Class::Number, |cl, j| number_separator(&tx, cl, j)) {
                    if let Some(num) = run.get_mut(n) {
                        turn(num);
                    }
                }
            }
        }
    }
    // A model may read a bracket in a right-to-left stretch as the character it stands for, not
    // the shape drawn; then mirroring it again turns the pair inside out: keep whichever reading
    // pairs up.
    let text = |o: &[(usize, bool)], flip: bool| -> String {
        o.iter().flat_map(|(i, m)| texts.get(*i).copied().unwrap_or("").chars().map(move |c| if *m != flip { mirrored(c) } else { c })).collect()
    };
    if !balanced(&text(&order, false)) && balanced(&text(&order, true)) {
        for e in order.iter_mut() {
            e.1 = !e.1;
        }
    }
    order
}

/// Signs written after a number in Arabic that read with the line, not the number (`47%` is
/// drawn `%47` in a right-to-left line).
fn is_trailing_sign(t: &str) -> bool {
    matches!(t, "%" | "٪" | "‰")
}

/// Whether every closing bracket closes an open one.
pub(crate) fn balanced(text: &str) -> bool {
    let mut open: Vec<char> = Vec::new();
    for c in text.chars() {
        match c {
            '(' | '[' | '{' => open.push(c),
            ')' | ']' | '}' if open.pop() != Some(mirrored(c)) => return false,
            _ => {}
        }
    }
    open.is_empty()
}

/// One word in logical order as it is drawn (left to right): its right-to-left letters turned,
/// each with its marks after it, digits and Latin letters inside it left to right, brackets in
/// the turned part mirrored. A word without right-to-left letters is returned unchanged.
pub fn visual_word(word: &str) -> String {
    if !word.chars().any(is_rtl) {
        return word.to_owned();
    }
    // Clusters: a base character and the marks typed after it.
    let mut clusters: Vec<String> = Vec::new();
    for c in word.chars() {
        match clusters.last_mut() {
            Some(last) if is_mark(c) => last.push(c),
            _ => clusters.push(c.to_string()),
        }
    }
    clusters.reverse();
    // Runs of digits and Latin letters read left to right again.
    let keep = |s: &str| matches!(class_of(s), Class::Ltr | Class::Number);
    let mut out = String::with_capacity(word.len());
    let mut i = 0;
    while i < clusters.len() {
        if clusters.get(i).is_some_and(|c| keep(c)) {
            let mut j = i;
            while clusters
                .get(j + 1)
                .is_some_and(|c| keep(c) || (matches!(c.as_str(), "." | "," | "/" | ":" | "-") && clusters.get(j + 2).is_some_and(|d| keep(d))))
            {
                j += 1;
            }
            if let Some(run) = clusters.get(i..=j) {
                for c in run.iter().rev() {
                    out.push_str(c);
                }
            }
            i = j + 1;
        } else {
            if let Some(c) = clusters.get(i) {
                let mut chars = c.chars();
                if let Some(first) = chars.next() {
                    out.push(mirrored(first));
                }
                out.extend(chars);
            }
            i += 1;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The logical text of a visual line, characters one by one, on a page of `page_rtl` lines.
    fn logical_on(visual: &str, page_rtl: bool) -> String {
        let chars: Vec<String> = visual.chars().map(String::from).collect();
        let texts: Vec<&str> = chars.iter().map(String::as_str).collect();
        let classes: Vec<Class> = texts.iter().map(|t| class_of(t)).collect();
        logical_order(&texts, rtl_base(&classes, None, page_rtl))
            .into_iter()
            .map(|(i, m)| texts[i].chars().map(|c| if m { mirrored(c) } else { c }).collect::<String>())
            .collect()
    }

    /// The same on an Arabic page.
    fn logical(visual: &str) -> String {
        logical_on(visual, true)
    }

    fn rev(s: &str) -> String {
        s.chars().rev().collect()
    }

    #[test]
    fn latin_lines_are_unchanged() {
        assert_eq!(logical("Section 03 30 00 (Concrete)"), "Section 03 30 00 (Concrete)");
    }

    #[test]
    fn an_arabic_line_turns_around() {
        let typed = "مشروع برج الأعمال";
        assert_eq!(logical(&rev(typed)), typed);
    }

    #[test]
    fn latin_words_and_numbers_in_an_arabic_line_keep_their_order() {
        // As drawn: the Arabic turned, "Tower B" and "12" left to right where they stand.
        let visual = format!("Tower B {} 12 {}", rev("مشروع برج الأعمال"), rev("محضر اجتماع رقم"));
        assert_eq!(logical(&visual), "محضر اجتماع رقم 12 مشروع برج الأعمال Tower B");
        let visual = format!("{} 2026/10/18 {}", rev("بمكتب الاستشاري"), rev("الأحد الموافق"));
        assert_eq!(logical(&visual), "الأحد الموافق 2026/10/18 بمكتب الاستشاري");
        // A full stop at the end of the line is drawn at its left end.
        let visual = format!(".MEP {}", rev("الكهروميكانيكية"));
        assert_eq!(logical(&visual), "الكهروميكانيكية MEP.");
    }

    #[test]
    fn a_percent_sign_reads_with_the_line() {
        let visual = format!("%52 {} %47 {}", rev("مقابل"), rev("بلغت"));
        assert_eq!(logical(&visual), "بلغت 47% مقابل 52%");
    }

    #[test]
    fn arabic_inside_a_latin_line_turns_alone() {
        let visual = format!("Project: Al Noor Residential Compound ({}), Phase 2", rev("مجمع النور السكني"));
        assert_eq!(logical_on(&visual, false), "Project: Al Noor Residential Compound (مجمع النور السكني), Phase 2");
        // A number between Arabic words stays inside their stretch, left to right.
        let visual = format!("Item {} 15 {}", rev("البند"), rev("رقم"));
        assert_eq!(logical_on(&visual, false), "Item رقم 15 البند");
    }

    /// A line with about as many letters of each script reads as the page does, or as the
    /// margin it keeps to says.
    #[test]
    fn a_balanced_line_takes_the_page_direction() {
        let visual = format!("Primavera P6 {}", rev("باستخدام"));
        assert_eq!(logical_on(&visual, true), "باستخدام Primavera P6");
        assert_eq!(logical_on(&visual, false), format!("Primavera P6 {}", "باستخدام"));
        let typed = "Subject in Arabic: تقديم الرسومات التنفيذية المعدلة";
        let visual = format!("Subject in Arabic: {}", rev("تقديم الرسومات التنفيذية المعدلة"));
        let chars: Vec<String> = visual.chars().map(String::from).collect();
        let texts: Vec<&str> = chars.iter().map(String::as_str).collect();
        let classes: Vec<Class> = texts.iter().map(|t| class_of(t)).collect();
        assert!(rtl_base(&classes, None, true), "more Arabic letters than Latin");
        assert!(!rtl_base(&classes, Some(false), true), "but the line keeps to the left margin");
        let order = logical_order(&texts, false);
        assert_eq!(order.iter().map(|(i, _)| texts[*i]).collect::<String>(), typed);
    }

    #[test]
    fn brackets_in_an_arabic_line_pair_up() {
        // Drawn shapes: the opening bracket of "(مثال)" shows as ")" at the right end.
        let visual = format!("({}) {}", rev("مثال"), rev("انظر"));
        assert_eq!(logical(&visual), "انظر (مثال)");
        // A model that reads the character a bracket stands for gives the same text.
        let visual = format!("){}( {}", rev("مثال"), rev("انظر"));
        assert_eq!(logical(&visual), "انظر (مثال)");
    }

    #[test]
    fn marks_read_before_their_letter_follow_it() {
        // "بَ" (beh, fatha) read right to left by the model: fatha, then beh.
        assert_eq!(logical("\u{064E}\u{0628}"), "\u{0628}\u{064E}");
    }

    #[test]
    fn ligatures_stay_whole() {
        let texts = ["الله", " ", "و"];
        let order: Vec<usize> = logical_order(&texts, true).into_iter().map(|(i, _)| i).collect();
        assert_eq!(order, vec![2, 1, 0]);
    }

    #[test]
    fn words_are_drawn_in_visual_order() {
        assert_eq!(visual_word("Tower"), "Tower");
        assert_eq!(visual_word("برج"), "جرب");
        assert_eq!(visual_word("م15"), "15م");
        assert_eq!(visual_word("(مثال)"), "(لاثم)");
        // Marks stay after their letter.
        assert_eq!(visual_word("بَت"), "تبَ");
        assert_eq!(visual_word("الرقم2026/10/18"), "2026/10/18مقرلا");
    }
}
