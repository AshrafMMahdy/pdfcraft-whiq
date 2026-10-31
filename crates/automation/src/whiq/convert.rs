//! `convert`: PDFs from images, text, HTML and Markdown.
//!
//! Images, plain text and PDFs go through Create (`Session::convert_to_pdf`). HTML and Markdown
//! are parsed into one block model ([`Block`]) and typeset here: the Latin text in the standard
//! Helvetica and Courier faces (exact metrics, WinAnsi), with headings, bold and italic runs,
//! lists, quotes, code blocks, rules, ruled tables with a shaded header row repeated on each
//! page, images and links. A block or table cell holding characters WinAnsi cannot draw
//! (Arabic, Hebrew, Thai, …) is measured with the editor's text layout and placed afterwards as
//! added text (`page_add_text`), which shapes it with the embedded craft-fonts face and runs it
//! right to left where it is.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use pdfcraft_cos::{Dict, Document, Object, PdfString};
use pdfcraft_fonts::{Std14, literal, win_ansi, win_ansi_encodable};
use serde_json::{Map, Value, json};

use super::contract::{WhiqError, classify};
use super::engine::{Ctx, Engine, Reply, Res, handle};

/// Page sizes by name, in points.
pub fn page_size(v: Option<&Value>, landscape: bool) -> Res<(f64, f64)> {
    let (w, h) = match v {
        None | Some(Value::Null) => (595.276, 841.89),
        Some(Value::String(s)) => match s.trim().to_ascii_lowercase().as_str() {
            "a4" | "" => (595.276, 841.89),
            "letter" | "us letter" => (612.0, 792.0),
            "legal" => (612.0, 1008.0),
            "a3" => (841.89, 1190.551),
            "a5" => (419.528, 595.276),
            "tabloid" | "ledger" => (792.0, 1224.0),
            other => {
                return Err(WhiqError::invalid(
                    format!("unknown page size {other:?}"),
                    "use A4, Letter, Legal, A3, A5, Tabloid or [width, height] in points",
                ));
            }
        },
        Some(Value::Array(a)) => match (a.first().and_then(Value::as_f64), a.get(1).and_then(Value::as_f64)) {
            (Some(w), Some(h)) if w.is_finite() && h.is_finite() && w >= 36.0 && h >= 36.0 && w <= 14_400.0 && h <= 14_400.0 => (w, h),
            _ => return Err(WhiqError::invalid("a page size is [width, height] in points, 36 to 14400", "e.g. [595, 842]")),
        },
        Some(other) => return Err(WhiqError::invalid(format!("page size {other} is not a name or [width, height]"), "use A4 or [595, 842]")),
    };
    Ok(if landscape { (w.max(h), w.min(h)) } else { (w, h) })
}

// ---- the block model ----------------------------------------------------------------------

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Run {
    pub text: String,
    pub bold: bool,
    pub italic: bool,
    pub code: bool,
    pub link: Option<String>,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Block {
    Heading(u8, Vec<Run>),
    Para(Vec<Run>),
    /// A list item: its marker (`•` or `3.`), depth from 0, and text.
    Item(String, usize, Vec<Run>),
    Quote(Vec<Run>),
    Code(String),
    Rule,
    /// Rows of cells (each cell its runs); the first row is the header when `header`.
    Table {
        rows: Vec<Vec<Vec<Run>>>,
        header: bool,
    },
    Image {
        src: String,
        alt: String,
    },
    PageBreak,
}

fn plain(runs: &[Run]) -> String {
    runs.iter().map(|r| r.text.as_str()).collect()
}

fn trim_runs(mut runs: Vec<Run>) -> Vec<Run> {
    while runs.first().is_some_and(|r| r.text.trim().is_empty()) {
        runs.remove(0);
    }
    while runs.last().is_some_and(|r| r.text.trim().is_empty()) {
        runs.pop();
    }
    if let Some(f) = runs.first_mut() {
        f.text = f.text.trim_start().to_string();
    }
    if let Some(l) = runs.last_mut() {
        l.text = l.text.trim_end().to_string();
    }
    runs
}

// ---- Markdown ---------------------------------------------------------------------------

pub fn markdown_blocks(md: &str) -> Vec<Block> {
    use pulldown_cmark::{Event, HeadingLevel, Options, Parser, Tag, TagEnd};
    let mut blocks = Vec::new();
    let opts = Options::ENABLE_TABLES | Options::ENABLE_STRIKETHROUGH | Options::ENABLE_TASKLISTS;
    let mut runs: Vec<Run> = Vec::new();
    let (mut bold, mut italic) = (0u32, 0u32);
    let mut link: Option<String> = None;
    let mut lists: Vec<Option<u64>> = Vec::new();
    let mut item_marker: Option<String> = None;
    let mut heading: Option<u8> = None;
    let mut quote = 0u32;
    let mut code: Option<String> = None;
    let mut table: Option<(Vec<Vec<Vec<Run>>>, bool)> = None;
    let mut row: Vec<Vec<Run>> = Vec::new();
    let mut in_head = false;
    let mut image: Option<(String, String)> = None;
    let flush = |runs: &mut Vec<Run>, blocks: &mut Vec<Block>, heading: Option<u8>, marker: &mut Option<String>, depth: usize, quote: u32| {
        let r = trim_runs(std::mem::take(runs));
        if r.is_empty() && marker.is_none() {
            return;
        }
        if plain(&r).trim() == "\\pagebreak" || plain(&r).trim() == "\\newpage" {
            blocks.push(Block::PageBreak);
            return;
        }
        if let Some(l) = heading {
            blocks.push(Block::Heading(l, r));
        } else if let Some(m) = marker.take() {
            blocks.push(Block::Item(m, depth, r));
        } else if quote > 0 {
            blocks.push(Block::Quote(r));
        } else {
            blocks.push(Block::Para(r));
        }
    };
    for ev in Parser::new_ext(md, opts) {
        if let Some((_, alt)) = image.as_mut() {
            match ev {
                Event::Text(t) | Event::Code(t) => {
                    alt.push_str(&t);
                    continue;
                }
                Event::End(TagEnd::Image) => {
                    if let Some((src, alt)) = image.take() {
                        flush(&mut runs, &mut blocks, heading, &mut item_marker, lists.len().saturating_sub(1), quote);
                        blocks.push(Block::Image { src, alt });
                    }
                    continue;
                }
                _ => continue,
            }
        }
        match ev {
            Event::Start(Tag::Heading { level, .. }) => {
                heading = Some(match level {
                    HeadingLevel::H1 => 1,
                    HeadingLevel::H2 => 2,
                    HeadingLevel::H3 => 3,
                    HeadingLevel::H4 => 4,
                    HeadingLevel::H5 => 5,
                    HeadingLevel::H6 => 6,
                });
            }
            Event::End(TagEnd::Heading(_)) => {
                flush(&mut runs, &mut blocks, heading, &mut item_marker, 0, quote);
                heading = None;
            }
            Event::Start(Tag::Paragraph) => {}
            Event::End(TagEnd::Paragraph) => {
                if table.is_none() {
                    flush(&mut runs, &mut blocks, None, &mut item_marker, lists.len().saturating_sub(1), quote);
                }
            }
            Event::Start(Tag::BlockQuote(_)) => quote += 1,
            Event::End(TagEnd::BlockQuote(_)) => {
                flush(&mut runs, &mut blocks, None, &mut item_marker, 0, quote);
                quote = quote.saturating_sub(1);
            }
            Event::Start(Tag::CodeBlock(_)) => code = Some(String::new()),
            Event::End(TagEnd::CodeBlock) => {
                if let Some(c) = code.take() {
                    blocks.push(Block::Code(c.trim_end_matches('\n').to_string()));
                }
            }
            Event::Start(Tag::List(start)) => {
                flush(&mut runs, &mut blocks, None, &mut item_marker, lists.len().saturating_sub(1), quote);
                lists.push(start);
            }
            Event::End(TagEnd::List(_)) => {
                flush(&mut runs, &mut blocks, None, &mut item_marker, lists.len().saturating_sub(1), quote);
                lists.pop();
            }
            Event::Start(Tag::Item) => {
                flush(&mut runs, &mut blocks, None, &mut item_marker, lists.len().saturating_sub(1), quote);
                let marker = match lists.last_mut() {
                    Some(Some(n)) => {
                        let m = format!("{n}.");
                        *n = n.saturating_add(1);
                        m
                    }
                    _ => "\u{2022}".to_string(),
                };
                item_marker = Some(marker);
            }
            Event::End(TagEnd::Item) => flush(&mut runs, &mut blocks, None, &mut item_marker, lists.len().saturating_sub(1), quote),
            Event::TaskListMarker(done) => runs.push(Run { text: if done { "[x] ".into() } else { "[ ] ".into() }, ..Default::default() }),
            Event::Start(Tag::Table(_)) => table = Some((Vec::new(), true)),
            Event::End(TagEnd::Table) => {
                if let Some((rows, header)) = table.take() {
                    blocks.push(Block::Table { rows, header });
                }
            }
            Event::Start(Tag::TableHead) => in_head = true,
            Event::End(TagEnd::TableHead) => {
                in_head = false;
                if let Some((rows, _)) = table.as_mut() {
                    rows.push(std::mem::take(&mut row));
                }
            }
            Event::Start(Tag::TableRow) => row.clear(),
            Event::End(TagEnd::TableRow) => {
                if let Some((rows, _)) = table.as_mut() {
                    rows.push(std::mem::take(&mut row));
                }
            }
            Event::Start(Tag::TableCell) => {}
            Event::End(TagEnd::TableCell) => {
                let mut cell = trim_runs(std::mem::take(&mut runs));
                if in_head {
                    for r in &mut cell {
                        r.bold = true;
                    }
                }
                row.push(cell);
            }
            Event::Start(Tag::Strong) => bold += 1,
            Event::End(TagEnd::Strong) => bold = bold.saturating_sub(1),
            Event::Start(Tag::Emphasis) => italic += 1,
            Event::End(TagEnd::Emphasis) => italic = italic.saturating_sub(1),
            Event::Start(Tag::Link { dest_url, .. }) => link = Some(dest_url.to_string()),
            Event::End(TagEnd::Link) => link = None,
            Event::Start(Tag::Image { dest_url, .. }) => image = Some((dest_url.to_string(), String::new())),
            Event::Text(t) => match code.as_mut() {
                Some(c) => c.push_str(&t),
                None => {
                    runs.push(Run { text: t.to_string(), bold: bold > 0 || heading.is_some(), italic: italic > 0, code: false, link: link.clone() })
                }
            },
            Event::Code(t) => runs.push(Run { text: t.to_string(), bold: bold > 0, italic: italic > 0, code: true, link: link.clone() }),
            Event::SoftBreak => runs.push(Run { text: " ".into(), ..Default::default() }),
            Event::HardBreak => runs.push(Run { text: "\n".into(), ..Default::default() }),
            Event::Rule => blocks.push(Block::Rule),
            Event::Html(h) | Event::InlineHtml(h) => {
                let lower = h.to_ascii_lowercase();
                if lower.contains("page-break-before")
                    || lower.contains("page-break-after")
                    || lower.contains("break-before: page")
                    || lower.contains("<pagebreak")
                {
                    flush(&mut runs, &mut blocks, heading, &mut item_marker, 0, quote);
                    blocks.push(Block::PageBreak);
                } else if lower.trim_start().starts_with("<br") {
                    runs.push(Run { text: "\n".into(), ..Default::default() });
                }
            }
            _ => {}
        }
    }
    flush(&mut runs, &mut blocks, heading, &mut item_marker, 0, quote);
    blocks
}

// ---- HTML -------------------------------------------------------------------------------

fn decode_entities(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(at) = rest.find('&') {
        out.push_str(rest.get(..at).unwrap_or_default());
        let tail = rest.get(at..).unwrap_or_default();
        let end = tail.find(';').filter(|e| *e <= 12);
        let Some(end) = end else {
            out.push('&');
            rest = tail.get(1..).unwrap_or_default();
            continue;
        };
        let name = tail.get(1..end).unwrap_or_default();
        let c = match name {
            "amp" => Some('&'),
            "lt" => Some('<'),
            "gt" => Some('>'),
            "quot" => Some('"'),
            "apos" => Some('\''),
            "nbsp" => Some('\u{a0}'),
            "ndash" => Some('\u{2013}'),
            "mdash" => Some('\u{2014}'),
            "hellip" => Some('\u{2026}'),
            "copy" => Some('\u{a9}'),
            "reg" => Some('\u{ae}'),
            "deg" => Some('\u{b0}'),
            "times" => Some('\u{d7}'),
            "euro" => Some('\u{20ac}'),
            "bull" => Some('\u{2022}'),
            n if n.starts_with("#x") || n.starts_with("#X") => u32::from_str_radix(n.get(2..).unwrap_or_default(), 16).ok().and_then(char::from_u32),
            n if n.starts_with('#') => n.get(1..).unwrap_or_default().parse::<u32>().ok().and_then(char::from_u32),
            _ => None,
        };
        match c {
            Some(c) => {
                out.push(c);
                rest = tail.get(end + 1..).unwrap_or_default();
            }
            None => {
                out.push('&');
                rest = tail.get(1..).unwrap_or_default();
            }
        }
    }
    out.push_str(rest);
    out
}

enum Tok {
    Open(String, String),
    Close(String),
    Text(String),
}

fn tokens(html: &str) -> Vec<Tok> {
    let mut out = Vec::new();
    let mut rest = html;
    while !rest.is_empty() {
        match rest.find('<') {
            Some(0) => {
                if rest.starts_with("<!--") {
                    rest = rest.find("-->").and_then(|e| rest.get(e + 3..)).unwrap_or("");
                    continue;
                }
                let Some(end) = rest.find('>') else {
                    out.push(Tok::Text(rest.to_string()));
                    break;
                };
                let inner = rest.get(1..end).unwrap_or_default();
                rest = rest.get(end + 1..).unwrap_or_default();
                if inner.starts_with('!') || inner.starts_with('?') {
                    continue;
                }
                if let Some(name) = inner.strip_prefix('/') {
                    out.push(Tok::Close(name.trim().to_ascii_lowercase()));
                    continue;
                }
                let inner = inner.trim_end_matches('/');
                let (name, attrs) = inner.split_once(char::is_whitespace).unwrap_or((inner, ""));
                let name = name.to_ascii_lowercase();
                if matches!(name.as_str(), "script" | "style" | "head" | "title") {
                    let close = format!("</{name}");
                    let pos = rest.to_ascii_lowercase().find(&close);
                    let body = pos.and_then(|p| rest.get(..p)).unwrap_or(rest).to_string();
                    if name == "title" {
                        out.push(Tok::Open("title".into(), body));
                    } else if name == "head" {
                        // The title inside the head is kept; the rest of it is not shown.
                        let lower = body.to_ascii_lowercase();
                        if let (Some(a), Some(b)) = (lower.find("<title>"), lower.find("</title>")) {
                            out.push(Tok::Open("title".into(), body.get(a + 7..b).unwrap_or_default().to_string()));
                        }
                    }
                    rest = pos.and_then(|p| rest.get(p..)).and_then(|r| r.find('>').and_then(|e| r.get(e + 1..))).unwrap_or("");
                    continue;
                }
                out.push(Tok::Open(name, attrs.to_string()));
            }
            Some(n) => {
                out.push(Tok::Text(decode_entities(rest.get(..n).unwrap_or_default())));
                rest = rest.get(n..).unwrap_or_default();
            }
            None => {
                out.push(Tok::Text(decode_entities(rest)));
                break;
            }
        }
    }
    out
}

fn attr(attrs: &str, name: &str) -> Option<String> {
    let lower = attrs.to_ascii_lowercase();
    let mut from = 0;
    while let Some(i) = lower.get(from..).and_then(|s| s.find(name)).map(|i| i + from) {
        let before_ok = i == 0 || lower.as_bytes().get(i - 1).is_some_and(|b| b.is_ascii_whitespace());
        let after = lower.get(i + name.len()..).unwrap_or_default().trim_start();
        if before_ok && after.starts_with('=') {
            let start = attrs.len() - after.len() + 1;
            let v = attrs.get(start..).unwrap_or_default().trim_start();
            let (q, body) = match v.chars().next() {
                Some(q @ ('"' | '\'')) => (Some(q), v.get(1..).unwrap_or_default()),
                _ => (None, v),
            };
            let end = match q {
                Some(q) => body.find(q).unwrap_or(body.len()),
                None => body.find(char::is_whitespace).unwrap_or(body.len()),
            };
            return Some(decode_entities(body.get(..end).unwrap_or_default()));
        }
        from = i + name.len();
    }
    None
}

/// HTML as blocks, and its `<title>`.
pub fn html_blocks(html: &str) -> (Vec<Block>, Option<String>) {
    let mut blocks = Vec::new();
    let mut title = None;
    let mut runs: Vec<Run> = Vec::new();
    let (mut bold, mut italic, mut code_inline) = (0u32, 0u32, 0u32);
    let mut link: Option<String> = None;
    let mut lists: Vec<Option<u64>> = Vec::new();
    let mut marker: Option<String> = None;
    let mut heading: Option<u8> = None;
    let mut quote = 0u32;
    let mut pre: Option<String> = None;
    let mut table: Option<Vec<Vec<Vec<Run>>>> = None;
    let mut table_header = false;
    let mut row: Option<Vec<Vec<Run>>> = None;
    let mut cell: Option<bool> = None;
    let flush = |runs: &mut Vec<Run>, blocks: &mut Vec<Block>, heading: Option<u8>, marker: &mut Option<String>, depth: usize, quote: u32| {
        let r = trim_runs(std::mem::take(runs));
        let r: Vec<Run> = r.into_iter().filter(|x| !x.text.is_empty()).collect();
        if r.is_empty() && marker.is_none() {
            return;
        }
        if let Some(l) = heading {
            blocks.push(Block::Heading(l, r));
        } else if let Some(m) = marker.take() {
            blocks.push(Block::Item(m, depth, r));
        } else if quote > 0 {
            blocks.push(Block::Quote(r));
        } else {
            blocks.push(Block::Para(r));
        }
    };
    for t in tokens(html) {
        let depth = lists.len().saturating_sub(1);
        match t {
            Tok::Text(s) => {
                if let Some(p) = pre.as_mut() {
                    p.push_str(&s);
                    continue;
                }
                let collapsed: String = {
                    let mut o = String::with_capacity(s.len());
                    let mut space = false;
                    for c in s.chars() {
                        if c.is_whitespace() && c != '\u{a0}' {
                            if !space {
                                o.push(' ');
                            }
                            space = true;
                        } else {
                            o.push(c);
                            space = false;
                        }
                    }
                    o
                };
                if collapsed.trim().is_empty() && runs.is_empty() {
                    continue;
                }
                runs.push(Run {
                    text: collapsed,
                    bold: bold > 0 || heading.is_some() || cell == Some(true),
                    italic: italic > 0,
                    code: code_inline > 0,
                    link: link.clone(),
                });
            }
            Tok::Open(name, attrs) => {
                let style = attr(&attrs, "style").unwrap_or_default().to_ascii_lowercase().replace(' ', "");
                if style.contains("page-break-before:always") || style.contains("break-before:page") {
                    flush(&mut runs, &mut blocks, heading, &mut marker, depth, quote);
                    blocks.push(Block::PageBreak);
                }
                match name.as_str() {
                    "title" => title = Some(decode_entities(attrs.trim())),
                    "h1" | "h2" | "h3" | "h4" | "h5" | "h6" => {
                        flush(&mut runs, &mut blocks, heading, &mut marker, depth, quote);
                        heading = name.get(1..).and_then(|n| n.parse().ok());
                    }
                    "p" | "div" | "section" | "article" | "header" | "footer" | "main" | "figure" | "figcaption" | "dl" | "dt" | "dd" => {
                        if cell.is_none() {
                            flush(&mut runs, &mut blocks, heading, &mut marker, depth, quote);
                        }
                    }
                    "br" => runs.push(Run { text: "\n".into(), ..Default::default() }),
                    "hr" => {
                        flush(&mut runs, &mut blocks, heading, &mut marker, depth, quote);
                        blocks.push(Block::Rule);
                    }
                    "b" | "strong" | "th" if name != "th" => bold += 1,
                    "i" | "em" | "cite" => italic += 1,
                    "code" | "kbd" | "samp" | "tt" => code_inline += 1,
                    "a" => link = attr(&attrs, "href"),
                    "ul" | "ol" => {
                        flush(&mut runs, &mut blocks, heading, &mut marker, depth, quote);
                        lists.push(if name == "ol" { Some(attr(&attrs, "start").and_then(|s| s.parse().ok()).unwrap_or(1)) } else { None });
                    }
                    "li" => {
                        flush(&mut runs, &mut blocks, heading, &mut marker, depth, quote);
                        marker = Some(match lists.last_mut() {
                            Some(Some(n)) => {
                                let m = format!("{n}.");
                                *n = n.saturating_add(1);
                                m
                            }
                            _ => "\u{2022}".into(),
                        });
                    }
                    "blockquote" => {
                        flush(&mut runs, &mut blocks, heading, &mut marker, depth, quote);
                        quote += 1;
                    }
                    "pre" => {
                        flush(&mut runs, &mut blocks, heading, &mut marker, depth, quote);
                        pre = Some(String::new());
                    }
                    "img" => {
                        if let Some(src) = attr(&attrs, "src") {
                            if cell.is_none() {
                                flush(&mut runs, &mut blocks, heading, &mut marker, depth, quote);
                            }
                            blocks.push(Block::Image { src, alt: attr(&attrs, "alt").unwrap_or_default() });
                        }
                    }
                    "table" => {
                        flush(&mut runs, &mut blocks, heading, &mut marker, depth, quote);
                        table = Some(Vec::new());
                        table_header = false;
                    }
                    "tr" => row = Some(Vec::new()),
                    "td" | "th" => {
                        runs.clear();
                        cell = Some(name == "th");
                        if name == "th" && table.as_ref().is_some_and(Vec::is_empty) {
                            table_header = true;
                        }
                    }
                    _ => {}
                }
            }
            Tok::Close(name) => match name.as_str() {
                "h1" | "h2" | "h3" | "h4" | "h5" | "h6" => {
                    flush(&mut runs, &mut blocks, heading, &mut marker, depth, quote);
                    heading = None;
                }
                "p" | "div" | "section" | "article" | "header" | "footer" | "main" | "figure" | "figcaption" | "dt" | "dd" => {
                    if cell.is_none() {
                        flush(&mut runs, &mut blocks, heading, &mut marker, depth, quote);
                    }
                }
                "li" => flush(&mut runs, &mut blocks, heading, &mut marker, depth, quote),
                "ul" | "ol" => {
                    flush(&mut runs, &mut blocks, heading, &mut marker, depth, quote);
                    lists.pop();
                }
                "b" | "strong" => bold = bold.saturating_sub(1),
                "i" | "em" | "cite" => italic = italic.saturating_sub(1),
                "code" | "kbd" | "samp" | "tt" => code_inline = code_inline.saturating_sub(1),
                "a" => link = None,
                "blockquote" => {
                    flush(&mut runs, &mut blocks, heading, &mut marker, depth, quote);
                    quote = quote.saturating_sub(1);
                }
                "pre" => {
                    if let Some(p) = pre.take() {
                        blocks.push(Block::Code(p.trim_matches('\n').to_string()));
                    }
                }
                "td" | "th" => {
                    if let Some(r) = row.as_mut() {
                        r.push(trim_runs(std::mem::take(&mut runs)));
                    }
                    cell = None;
                }
                "tr" => {
                    if let (Some(t), Some(r)) = (table.as_mut(), row.take())
                        && !r.is_empty()
                    {
                        t.push(r);
                    }
                }
                "table" => {
                    if let Some(rows) = table.take()
                        && !rows.is_empty()
                    {
                        blocks.push(Block::Table { rows, header: table_header });
                    }
                }
                _ => {}
            },
        }
    }
    flush(&mut runs, &mut blocks, heading, &mut marker, 0, quote);
    (blocks, title.map(|t| t.trim().to_string()).filter(|t| !t.is_empty()))
}

// ---- typesetting ------------------------------------------------------------------------

#[derive(Clone, Copy)]
pub struct Layout {
    pub width: f64,
    pub height: f64,
    pub margin: f64,
    pub size: f64,
}

/// Text placed afterwards with the editor (scripts WinAnsi cannot draw).
#[derive(Clone, Debug)]
pub struct Foreign {
    pub page: usize,
    /// [x0, top, x1, bottom] in points from the top-left of the page.
    pub rect: [f64; 4],
    pub text: String,
    pub size: f64,
    pub bold: bool,
    pub rtl: bool,
}

/// An image placed afterwards: its file and box (points from the top-left).
#[derive(Clone, Debug)]
struct PlacedImage {
    page: usize,
    rect: [f64; 4],
    /// The image file, already checked against the read roots.
    path: PathBuf,
    name: String,
}

struct PageOut {
    content: Vec<u8>,
    links: Vec<([f64; 4], String)>,
}

pub struct Typeset {
    l: Layout,
    pages: Vec<PageOut>,
    /// Distance from the top of the content area, in points.
    y: f64,
    pub foreign: Vec<Foreign>,
    images: Vec<PlacedImage>,
    pub notes: Vec<String>,
    /// A page break asked for and not yet made: made by the next block, so a break at the very
    /// end of the text adds no blank page.
    pending_break: bool,
}

const LINE: f64 = 1.32;

/// Finds an image a block names: its checked path and bytes, or `None`.
pub type ImageSource<'a> = dyn Fn(&str) -> Option<(PathBuf, Vec<u8>)> + 'a;

fn font_of(bold: bool, italic: bool, code: bool) -> (&'static str, Std14) {
    match (code, bold, italic) {
        (true, true, _) => ("F6", Std14::CourierBold),
        (true, false, _) => ("F5", Std14::Courier),
        (false, false, false) => ("F1", Std14::Helvetica),
        (false, true, false) => ("F2", Std14::HelveticaBold),
        (false, false, true) => ("F3", Std14::HelveticaOblique),
        (false, true, true) => ("F4", Std14::HelveticaBoldOblique),
    }
}

/// A piece of a line: text in one style at an x offset.
struct Piece {
    x: f64,
    text: String,
    run: Run,
}

fn is_rtl(s: &str) -> bool {
    s.chars().find(|c| c.is_alphabetic()).is_some_and(|c| matches!(u32::from(c), 0x0590..=0x08FF | 0xFB1D..=0xFDFF | 0xFE70..=0xFEFF))
}

fn foreign(s: &str) -> bool {
    s.chars().any(|c| c != '\n' && !win_ansi_encodable(c))
}

impl Typeset {
    pub fn new(l: Layout) -> Self {
        let mut t = Typeset { l, pages: Vec::new(), y: 0.0, foreign: Vec::new(), images: Vec::new(), notes: Vec::new(), pending_break: false };
        t.new_page();
        t
    }

    fn content_w(&self) -> f64 {
        (self.l.width - 2.0 * self.l.margin).max(36.0)
    }

    fn content_h(&self) -> f64 {
        (self.l.height - 2.0 * self.l.margin).max(36.0)
    }

    fn new_page(&mut self) {
        self.pages.push(PageOut { content: Vec::new(), links: Vec::new() });
        self.y = 0.0;
    }

    fn page_i(&self) -> usize {
        self.pages.len().saturating_sub(1)
    }

    /// Room for `h` points on this page, or a new page (unless the page is still empty).
    fn need(&mut self, h: f64) {
        if self.y > 0.0 && self.y + h > self.content_h() {
            self.new_page();
        }
    }

    fn out(&mut self, s: &str) {
        if let Some(p) = self.pages.last_mut() {
            p.content.extend_from_slice(s.as_bytes());
        }
    }

    fn out_bytes(&mut self, b: &[u8]) {
        if let Some(p) = self.pages.last_mut() {
            p.content.extend_from_slice(b);
        }
    }

    /// PDF y (up) of a distance `d` below the top of the content area.
    fn py(&self, d: f64) -> f64 {
        self.l.height - self.l.margin - d
    }

    /// Break runs into lines of at most `width` points.
    fn lines(runs: &[Run], size: f64, width: f64) -> Vec<Vec<Piece>> {
        let mut lines: Vec<Vec<Piece>> = vec![Vec::new()];
        let mut x = 0.0f64;
        for run in runs {
            let (_, face) = font_of(run.bold, run.italic, run.code);
            let size = if run.code { size * 0.92 } else { size };
            for (k, seg) in run.text.split('\n').enumerate() {
                if k > 0 {
                    lines.push(Vec::new());
                    x = 0.0;
                }
                // Words with the space before them.
                let mut word = String::new();
                let mut chunks: Vec<String> = Vec::new();
                for c in seg.chars() {
                    if c == ' ' {
                        if !word.is_empty() {
                            chunks.push(std::mem::take(&mut word));
                        }
                        chunks.push(" ".into());
                    } else {
                        word.push(c);
                    }
                }
                if !word.is_empty() {
                    chunks.push(word);
                }
                for chunk in chunks {
                    let w = face.text_width(&chunk, size);
                    if chunk == " " {
                        if x > 0.0 {
                            x += w;
                        }
                        continue;
                    }
                    if x > 0.0 && x + w > width {
                        lines.push(Vec::new());
                        x = 0.0;
                    }
                    if w > width {
                        // A word longer than the line: broken where it reaches the edge.
                        let mut part = String::new();
                        for c in chunk.chars() {
                            let mut next = part.clone();
                            next.push(c);
                            if !part.is_empty() && x + face.text_width(&next, size) > width {
                                let pw = face.text_width(&part, size);
                                if let Some(l) = lines.last_mut() {
                                    l.push(Piece { x, text: std::mem::take(&mut part), run: run.clone() });
                                }
                                let _ = pw;
                                lines.push(Vec::new());
                                x = 0.0;
                                part.push(c);
                            } else {
                                part = next;
                            }
                        }
                        let pw = face.text_width(&part, size);
                        if let Some(l) = lines.last_mut() {
                            l.push(Piece { x, text: part, run: run.clone() });
                        }
                        x += pw;
                        continue;
                    }
                    if let Some(l) = lines.last_mut() {
                        l.push(Piece { x, text: chunk, run: run.clone() });
                    }
                    x += w;
                }
            }
        }
        if lines.last().is_some_and(Vec::is_empty) && lines.len() > 1 {
            lines.pop();
        }
        lines
    }

    fn draw_line(&mut self, pieces: &[Piece], x0: f64, baseline: f64, size: f64, gray: Option<f64>) {
        for p in pieces {
            let (font, face) = font_of(p.run.bold, p.run.italic, p.run.code);
            let s = if p.run.code { size * 0.92 } else { size };
            let color =
                if p.run.link.is_some() { "0 0.2 0.75 rg".to_string() } else { gray.map_or_else(|| "0 g".to_string(), |g| format!("{g:.2} g")) };
            let x = x0 + p.x;
            self.out(&format!("BT {color} /{font} {s:.2} Tf 1 0 0 1 {x:.2} {baseline:.2} Tm "));
            let lit = literal(&win_ansi(&p.text));
            self.out_bytes(&lit);
            self.out(" Tj ET\n");
            if let Some(url) = p.run.link.clone() {
                let w = face.text_width(&p.text, s);
                self.out(&format!("0 0.2 0.75 RG 0.5 w {x:.2} {:.2} m {:.2} {:.2} l S\n", baseline - 1.5, x + w, baseline - 1.5));
                let i = self.page_i();
                if let Some(pg) = self.pages.get_mut(i) {
                    pg.links.push(([x, baseline - 2.5, x + w, baseline + s * 0.8], url));
                }
            }
        }
    }

    /// A paragraph of runs at indent `x`, `width` wide: Latin drawn now, other scripts queued.
    fn para(&mut self, runs: &[Run], x: f64, width: f64, size: f64, gray: Option<f64>) {
        let text = plain(runs);
        if foreign(&text) {
            let bold = runs.iter().any(|r| r.bold);
            let h = foreign_height(&text, size, width, bold);
            self.need(h);
            let top = self.l.margin + self.y;
            self.foreign.push(Foreign {
                page: self.page_i(),
                rect: [self.l.margin + x, top, self.l.margin + x + width, top + h],
                text,
                size,
                bold,
                rtl: is_rtl(&plain(runs)),
            });
            self.y += h;
            return;
        }
        let lh = size * LINE;
        for line in Self::lines(runs, size, width) {
            self.need(lh);
            let baseline = self.py(self.y + size);
            self.draw_line(&line, self.l.margin + x, baseline, size, gray);
            self.y += lh;
        }
    }

    pub fn block(&mut self, b: &Block, base: &ImageSource<'_>) {
        let s = self.l.size;
        let w = self.content_w();
        if self.pending_break && !matches!(b, Block::PageBreak) {
            self.pending_break = false;
            if self.y > 0.0 {
                self.new_page();
            }
        }
        match b {
            Block::Heading(level, runs) => {
                let k = match level {
                    1 => 2.0,
                    2 => 1.6,
                    3 => 1.35,
                    4 => 1.15,
                    _ => 1.05,
                };
                let size = s * k;
                let runs: Vec<Run> = runs.iter().map(|r| Run { bold: true, ..r.clone() }).collect();
                let before = if self.y > 0.0 { size * 0.8 } else { 0.0 };
                // A heading keeps its first following line with it.
                self.need(before + size * LINE + s * LINE * 2.0);
                self.y += before;
                self.para(&runs, 0.0, w, size, None);
                if *level <= 2 {
                    let y = self.py(self.y + 1.0);
                    self.out(&format!("0.75 G 0.6 w {:.2} {y:.2} m {:.2} {y:.2} l S\n", self.l.margin, self.l.margin + w));
                    self.y += 3.0;
                }
                self.y += size * 0.35;
            }
            Block::Para(runs) => {
                self.para(runs, 0.0, w, s, None);
                self.y += s * 0.6;
            }
            Block::Item(marker, depth, runs) => {
                let indent = 16.0 + 16.0 * (*depth as f64).min(8.0);
                let lh = s * LINE;
                self.need(lh);
                let baseline = self.py(self.y + s);
                let (_, face) = font_of(false, false, false);
                let mw = face.text_width(marker, s);
                let mx = self.l.margin + indent - 5.0 - mw;
                self.out(&format!("BT 0 g /F1 {s:.2} Tf 1 0 0 1 {mx:.2} {baseline:.2} Tm "));
                let lit = literal(&win_ansi(marker));
                self.out_bytes(&lit);
                self.out(" Tj ET\n");
                self.para(runs, indent, w - indent, s, None);
                self.y += s * 0.25;
            }
            Block::Quote(runs) => {
                let start_page = self.page_i();
                let start = self.y;
                self.para(runs, 14.0, w - 14.0, s, Some(0.3));
                if self.page_i() == start_page {
                    let (y0, y1) = (self.py(start), self.py(self.y));
                    self.out(&format!("0.7 G 2 w {:.2} {y0:.2} m {:.2} {y1:.2} l S\n", self.l.margin + 4.0, self.l.margin + 4.0));
                }
                self.y += s * 0.6;
            }
            Block::Code(text) => {
                let size = s * 0.88;
                let lh = size * 1.25;
                let run = Run { text: String::new(), code: true, ..Default::default() };
                let pad = 6.0;
                let lines: Vec<String> = text.split('\n').map(str::to_string).collect();
                let mut k = 0;
                while k < lines.len() {
                    // As many lines as fit on this page, on one shaded box.
                    self.need(lh + 2.0 * pad);
                    let room = ((self.content_h() - self.y - 2.0 * pad) / lh).floor().max(1.0) as usize;
                    let chunk: Vec<String> = lines.iter().skip(k).take(room).cloned().collect();
                    let h = chunk.len() as f64 * lh + 2.0 * pad;
                    let top = self.py(self.y);
                    self.out(&format!("0.95 g {:.2} {:.2} {:.2} {h:.2} re f\n", self.l.margin, top - h, w));
                    for (j, line) in chunk.iter().enumerate() {
                        if foreign(line) {
                            let t = self.l.margin + self.y + pad + j as f64 * lh;
                            self.foreign.push(Foreign {
                                page: self.page_i(),
                                rect: [self.l.margin + pad, t, self.l.margin + w - pad, t + lh],
                                text: line.clone(),
                                size,
                                bold: false,
                                rtl: is_rtl(line),
                            });
                            continue;
                        }
                        let baseline = top - pad - size - j as f64 * lh;
                        let piece = Piece { x: 0.0, text: line.replace('\t', "    "), run: run.clone() };
                        self.draw_line(&[piece], self.l.margin + pad, baseline, size / 0.92, None);
                    }
                    self.y += h;
                    k += chunk.len().max(1);
                }
                self.y += s * 0.6;
            }
            Block::Rule => {
                self.need(s);
                let y = self.py(self.y + s * 0.5);
                self.out(&format!("0.6 G 0.8 w {:.2} {y:.2} m {:.2} {y:.2} l S\n", self.l.margin, self.l.margin + w));
                self.y += s;
            }
            Block::PageBreak => self.pending_break = true,
            Block::Image { src, alt } => match base(src) {
                Some((path, bytes)) => {
                    let (iw, ih) = image_size(&bytes).unwrap_or((w, w * 0.6));
                    let scale = (w / iw).min((self.content_h() * 0.9) / ih).min(1.0);
                    let (dw, dh) = (iw * scale, ih * scale);
                    self.need(dh + s * 0.6);
                    let top = self.l.margin + self.y;
                    let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| src.clone());
                    self.images.push(PlacedImage { page: self.page_i(), rect: [self.l.margin, top, self.l.margin + dw, top + dh], path, name });
                    self.y += dh + s * 0.6;
                }
                None => {
                    self.notes.push(format!("image {src} was not found; its alt text is shown instead"));
                    let runs = vec![Run { text: format!("[image: {}]", if alt.is_empty() { src } else { alt }), italic: true, ..Default::default() }];
                    self.para(&runs, 0.0, w, s, Some(0.4));
                    self.y += s * 0.6;
                }
            },
            Block::Table { rows, header } => self.table(rows, *header),
        }
    }

    fn table(&mut self, rows: &[Vec<Vec<Run>>], header: bool) {
        let s = self.l.size * 0.92;
        let w = self.content_w();
        let ncol = rows.iter().map(Vec::len).max().unwrap_or(0);
        if ncol == 0 {
            return;
        }
        let pad = 3.5;
        // Columns sized by their content (the widest line), at least a share of the width.
        let mut want = vec![0.0f64; ncol];
        for r in rows {
            for (c, cell) in r.iter().enumerate() {
                let words = plain(cell);
                let longest = words.split_whitespace().map(|x| Std14::HelveticaBold.text_width(x, s)).fold(0.0, f64::max);
                let total = Std14::Helvetica.text_width(&words, s);
                if let Some(v) = want.get_mut(c) {
                    *v = v.max(total.min(w * 0.6)).max(longest);
                }
            }
        }
        let sum: f64 = want.iter().map(|v| v + 2.0 * pad).sum();
        let min = w / ncol as f64 * 0.35;
        let mut cols: Vec<f64> = want.iter().map(|v| (v + 2.0 * pad).max(min)).collect();
        let total: f64 = cols.iter().sum();
        if total > 0.0 {
            let k = if sum < w { (w / total).min(1.0).max(total.min(w) / total) } else { w / total };
            for c in &mut cols {
                *c *= k;
            }
        }
        let table_w: f64 = cols.iter().sum();
        let lh = s * LINE;
        let row_layout = |cells: &[Vec<Run>]| -> (f64, Vec<Option<Vec<Vec<Piece>>>>) {
            let mut h: f64 = lh;
            let mut laid = Vec::new();
            for (c, cell) in cells.iter().enumerate() {
                let cw = cols.get(c).copied().unwrap_or(min) - 2.0 * pad;
                let text = plain(cell);
                if foreign(&text) {
                    h = h.max(foreign_height(&text, s, cw, cell.iter().any(|r| r.bold)));
                    laid.push(None);
                } else {
                    let lines = Self::lines(cell, s, cw);
                    h = h.max(lines.len() as f64 * lh);
                    laid.push(Some(lines));
                }
            }
            (h + 2.0 * pad, laid)
        };
        let head = if header { rows.first() } else { None };
        let draw_row = |t: &mut Typeset, cells: &[Vec<Run>], is_head: bool| {
            let (h, laid) = row_layout(cells);
            t.need(h);
            let top = t.py(t.y);
            let x0 = t.l.margin;
            if is_head {
                t.out(&format!("0.88 g {x0:.2} {:.2} {table_w:.2} {h:.2} re f\n", top - h));
            }
            let mut x = x0;
            for (c, lines) in laid.into_iter().enumerate() {
                let cw = cols.get(c).copied().unwrap_or(min);
                match lines {
                    Some(lines) => {
                        for (j, line) in lines.iter().enumerate() {
                            let baseline = top - pad - s - j as f64 * lh;
                            t.draw_line(line, x + pad, baseline, s, None);
                        }
                    }
                    None => {
                        let text = cells.get(c).map(|r| plain(r)).unwrap_or_default();
                        let ttop = t.l.margin + t.y + pad;
                        t.foreign.push(Foreign {
                            page: t.page_i(),
                            rect: [x + pad, ttop, x + cw - pad, ttop + h - 2.0 * pad],
                            rtl: is_rtl(&text),
                            text,
                            size: s,
                            bold: is_head,
                        });
                    }
                }
                x += cw;
            }
            // The grid: the row's box and its column lines.
            t.out(&format!("0.35 G 0.6 w {x0:.2} {:.2} {table_w:.2} {h:.2} re S\n", top - h));
            let mut cx = x0;
            for cw in cols.iter().take(ncol.saturating_sub(1)) {
                cx += cw;
                t.out(&format!("{cx:.2} {top:.2} m {cx:.2} {:.2} l S\n", top - h));
            }
            t.y += h;
        };
        let mut first_on_page = true;
        let mut page = self.page_i();
        for (i, r) in rows.iter().enumerate() {
            let is_head = header && i == 0;
            let (h, _) = row_layout(r);
            if self.y > 0.0 && self.y + h > self.content_h() {
                self.new_page();
            }
            if self.page_i() != page {
                page = self.page_i();
                first_on_page = true;
            }
            if first_on_page
                && !is_head
                && i > 0
                && let Some(hr) = head
            {
                draw_row(self, hr, true);
            }
            first_on_page = false;
            draw_row(self, r, is_head);
        }
        self.y += self.l.size * 0.8;
    }

    /// The pages as a PDF (Latin text, rules, tables and links drawn; images and other scripts
    /// still to place).
    pub fn finish(&self, title: &str) -> Result<Document, String> {
        let mut doc = Document::new_empty();
        let mut fonts = Dict::new();
        for (key, base) in [
            ("F1", "Helvetica"),
            ("F2", "Helvetica-Bold"),
            ("F3", "Helvetica-Oblique"),
            ("F4", "Helvetica-BoldOblique"),
            ("F5", "Courier"),
            ("F6", "Courier-Bold"),
        ] {
            let mut f = Dict::new();
            f.set(b"Type".to_vec(), Object::name("Font"));
            f.set(b"Subtype".to_vec(), Object::name("Type1"));
            f.set(b"BaseFont".to_vec(), Object::name(base));
            f.set(b"Encoding".to_vec(), Object::name("WinAnsiEncoding"));
            let r = doc.add(f);
            fonts.set(key.as_bytes().to_vec(), Object::Ref(r));
        }
        for p in &self.pages {
            let mut res = Dict::new();
            res.set(b"Font".to_vec(), Object::Dict(fonts.clone()));
            let page =
                pdfcraft_create::append_page(&mut doc, self.l.width, self.l.height, res, Some(p.content.clone())).map_err(|e| e.to_string())?;
            if !p.links.is_empty() {
                let annots: Vec<Object> = p
                    .links
                    .iter()
                    .map(|(r, url)| {
                        let mut action = Dict::new();
                        action.set(b"S".to_vec(), Object::name("URI"));
                        action.set(b"URI".to_vec(), PdfString::text(url));
                        let mut a = Dict::new();
                        a.set(b"Type".to_vec(), Object::name("Annot"));
                        a.set(b"Subtype".to_vec(), Object::name("Link"));
                        a.set(b"Rect".to_vec(), Object::Array(r.iter().map(|v| Object::Real(*v)).collect()));
                        a.set(b"Border".to_vec(), Object::Array(vec![Object::Int(0), Object::Int(0), Object::Int(0)]));
                        a.set(b"A".to_vec(), Object::Dict(action));
                        Object::Ref(doc.add(a))
                    })
                    .collect();
                doc.update_dict(page, |d| d.set(b"Annots".to_vec(), Object::Array(annots))).map_err(|e| e.to_string())?;
            }
        }
        pdfcraft_create::set_document_title(&mut doc, title);
        Ok(doc)
    }

    #[cfg(test)]
    pub fn page_count(&self) -> usize {
        self.pages.len()
    }
}

/// The height text of another script takes in a box `width` wide, as the editor lays it out.
fn foreign_height(text: &str, size: f64, width: f64, bold: bool) -> f64 {
    let t = pdfcraft_edit::AddedText { rect: [0.0, 0.0, width, 1000.0], text: text.to_string(), size, bold, ..Default::default() };
    let n = pdfcraft_edit::added::lines(&t).len().max(1);
    n as f64 * size * 1.2 + size * 0.3
}

/// An image's size in pixels (taken as points at 72 dpi) from its header: PNG, JPEG, GIF, BMP.
fn image_size(b: &[u8]) -> Option<(f64, f64)> {
    let be32 = |i: usize| b.get(i..i + 4).map(|s| u32::from_be_bytes([s[0], s[1], s[2], s[3]]));
    if b.starts_with(b"\x89PNG") {
        return Some((f64::from(be32(16)?), f64::from(be32(20)?)));
    }
    if b.starts_with(b"GIF8") {
        let w = u16::from_le_bytes([*b.get(6)?, *b.get(7)?]);
        let h = u16::from_le_bytes([*b.get(8)?, *b.get(9)?]);
        return Some((f64::from(w), f64::from(h)));
    }
    if b.starts_with(b"BM") {
        let w = i32::from_le_bytes([*b.get(18)?, *b.get(19)?, *b.get(20)?, *b.get(21)?]);
        let h = i32::from_le_bytes([*b.get(22)?, *b.get(23)?, *b.get(24)?, *b.get(25)?]);
        return Some((f64::from(w.unsigned_abs()), f64::from(h.unsigned_abs())));
    }
    if b.starts_with(&[0xFF, 0xD8]) {
        let mut i = 2usize;
        while i + 9 < b.len() {
            if *b.get(i)? != 0xFF {
                i += 1;
                continue;
            }
            let m = *b.get(i + 1)?;
            let len = usize::from(u16::from_be_bytes([*b.get(i + 2)?, *b.get(i + 3)?]));
            if matches!(m, 0xC0..=0xC3 | 0xC5..=0xC7 | 0xC9..=0xCB | 0xCD..=0xCF) {
                let h = u16::from_be_bytes([*b.get(i + 5)?, *b.get(i + 6)?]);
                let w = u16::from_be_bytes([*b.get(i + 7)?, *b.get(i + 8)?]);
                return Some((f64::from(w), f64::from(h)));
            }
            i = i.checked_add(2 + len)?;
        }
    }
    None
}

// ---- the tool -----------------------------------------------------------------------------

enum Source {
    Markdown(String, Option<PathBuf>),
    Html(String, Option<PathBuf>),
    File(PathBuf),
}

/// Typeset blocks into PDF bytes in the session (other scripts and images placed with the editor).
fn typeset(e: &mut Engine, blocks: &[Block], l: Layout, title: &str, dir: Option<&Path>, ctx: &Ctx) -> Res<(Arc<Vec<u8>>, Vec<String>)> {
    let read_image = |src: &str| -> Option<(PathBuf, Vec<u8>)> {
        let mut cands = Vec::new();
        let p = Path::new(src.strip_prefix("file://").unwrap_or(src));
        if p.is_absolute() {
            cands.push(p.to_path_buf());
        } else {
            if let Some(d) = dir {
                cands.push(d.join(p));
            }
            cands.push(e.read_path(src));
        }
        cands.into_iter().find_map(|c| {
            let checked = e.auto.resolve(&c.to_string_lossy(), false).ok()?;
            std::fs::read(&checked).ok().map(|b| (checked, b))
        })
    };
    let mut t = Typeset::new(l);
    ctx.stage("typesetting", blocks.len() as u64);
    for (i, b) in blocks.iter().enumerate() {
        ctx.check()?;
        t.block(b, &read_image);
        ctx.step((i + 1) as u64);
    }
    let doc = t.finish(title).map_err(|m| WhiqError::failed(format!("the PDF could not be built: {m}"), "simplify the input and try again"))?;
    let bytes = e.auto.session.create_from_document(&doc).map_err(|err| classify(&err.to_string(), "convert"))?;
    if t.foreign.is_empty() && t.images.is_empty() {
        return Ok((bytes, t.notes));
    }
    // Images and other scripts are placed on the built pages by the editor, then written out.
    let id = e.auto.session.open_new(format!("{title}.pdf"), bytes).map_err(|err| classify(&err.to_string(), "convert"))?;
    let mut notes = t.notes.clone();
    for im in &t.images {
        ctx.check()?;
        let r = e.auto.call("page_add_image", &json!({"doc": id.0, "page": im.page + 1, "path": im.path.to_string_lossy(), "rect": im.rect}));
        if let Err(err) = r {
            notes.push(format!("image {} was left out: {err}", im.name));
        }
    }
    for fo in &t.foreign {
        ctx.check()?;
        let mut args = json!({"doc": id.0, "page": fo.page + 1, "text": fo.text, "rect": fo.rect, "size": fo.size, "bold": fo.bold});
        if fo.rtl {
            args["align"] = json!("right");
        }
        if let Err(err) = e.auto.call("page_add_text", &args) {
            notes.push(format!("text \"{}\" could not be drawn: {err}", fo.text.chars().take(40).collect::<String>()));
        }
    }
    let out = e.auto.session.save_full_bytes(id).map_err(|err| classify(&err.to_string(), "convert"));
    let _ = e.auto.call("doc_close", &json!({"doc": id.0, "discard_changes": true}));
    Ok((out?, notes))
}

pub fn convert(e: &mut Engine, a: &Map<String, Value>, ctx: &Ctx) -> Res<Reply> {
    let (w, h) = page_size(a.get("page_size"), a.get("landscape").and_then(Value::as_bool).unwrap_or(false))?;
    let margin = a.get("margin").and_then(Value::as_f64).unwrap_or(56.0).clamp(0.0, (w.min(h) / 2.0 - 36.0).max(0.0));
    let size = a.get("font_size").and_then(Value::as_f64).unwrap_or(10.5).clamp(4.0, 72.0);
    let l = Layout { width: w, height: h, margin, size };
    let mut sources: Vec<(String, Source)> = Vec::new();
    if let Some(md) = a.get("markdown").and_then(Value::as_str) {
        sources.push(("markdown".into(), Source::Markdown(md.to_string(), None)));
    }
    if let Some(html) = a.get("html").and_then(Value::as_str) {
        sources.push(("html".into(), Source::Html(html.to_string(), None)));
    }
    if let Some(inputs) = a.get("inputs").and_then(Value::as_array) {
        for v in inputs {
            let Some(p) = v.as_str() else { continue };
            let path = e.resolve_input(p)?;
            let ext = path.extension().map(|x| x.to_string_lossy().to_ascii_lowercase()).unwrap_or_default();
            let dir = path.parent().map(Path::to_path_buf);
            let src = match ext.as_str() {
                "md" | "markdown" | "mdown" => Source::Markdown(read_resolved_text(&path)?, dir),
                "html" | "htm" | "xhtml" => Source::Html(read_resolved_text(&path)?, dir),
                _ => Source::File(path),
            };
            sources.push((p.to_string(), src));
        }
    }
    if sources.is_empty() {
        return Err(WhiqError::invalid("nothing to convert", "give inputs (files), markdown or html"));
    }
    let out = a
        .get("path")
        .and_then(Value::as_str)
        .filter(|s| !s.trim().is_empty())
        .ok_or_else(|| WhiqError::invalid("path is required", "name the PDF to write"))?;
    let (target, renamed) = e.resolve_output(out, a.get("overwrite").and_then(Value::as_bool).unwrap_or(false))?;
    let default_title = target.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_else(|| "Document".into());
    let mut title = a.get("title").and_then(Value::as_str).map(str::to_string);
    let mut parts: Vec<(String, Arc<Vec<u8>>)> = Vec::new();
    let mut notes = Vec::new();
    for (label, src) in sources {
        ctx.check()?;
        let bytes = match src {
            Source::Markdown(md, dir) => {
                let blocks = markdown_blocks(&md);
                let t = title.clone().unwrap_or_else(|| first_heading(&blocks).unwrap_or_else(|| default_title.clone()));
                title.get_or_insert(t.clone());
                let (b, n) = typeset(e, &blocks, l, &t, dir.as_deref(), ctx)?;
                notes.extend(n);
                b
            }
            Source::Html(html, dir) => {
                let (blocks, html_title) = html_blocks(&html);
                let t = title.clone().or(html_title).unwrap_or_else(|| first_heading(&blocks).unwrap_or_else(|| default_title.clone()));
                title.get_or_insert(t.clone());
                let (b, n) = typeset(e, &blocks, l, &t, dir.as_deref(), ctx)?;
                notes.extend(n);
                b
            }
            Source::File(path) => {
                let raw = std::fs::read(&path).map_err(|err| classify(&format!("{}: {err}", path.display()), "convert"))?;
                let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
                let (_, b) = e.auto.session.convert_to_pdf(&name, &Arc::new(raw)).map_err(|err| classify(&format!("{label}: {err}"), "convert"))?;
                b
            }
        };
        parts.push((label, bytes));
    }
    let bytes = if parts.len() == 1 {
        parts.pop().map(|p| p.1).unwrap_or_default()
    } else {
        let labels: Vec<String> = parts.iter().map(|p| p.0.clone()).collect();
        let named: Vec<(String, Arc<Vec<u8>>)> = parts
            .into_iter()
            .map(|(l, b)| (format!("{}.pdf", Path::new(&l).file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or(l)), b))
            .collect();
        let _ = labels;
        e.auto.session.combine(&named).map_err(|err| classify(&err.to_string(), "convert"))?
    };
    crate::paths::write_atomic(&target, &bytes).map_err(|err| classify(&err.to_string(), "convert"))?;
    e.wrote(&target);
    let id = e.open_path(&target.to_string_lossy(), None)?;
    if let Some(t) = &title
        && e.doc(id).ok().and_then(|d| d.info.title.clone()).is_none()
    {
        let _ = e.tool("doc_set_info", json!({"doc": id, "key": "Title", "value": t}));
    }
    let pages = e.doc(id).map(|d| d.info.pages.len()).unwrap_or(0);
    let mut v =
        json!({"path": target.to_string_lossy(), "bytes": bytes.len(), "pages": pages, "document": handle(id), "title": title, "notes": notes});
    if !renamed.is_empty() {
        v["previous_files_renamed"] = json!(renamed);
    }
    Ok(v.into())
}

fn first_heading(blocks: &[Block]) -> Option<String> {
    blocks.iter().find_map(|b| match b {
        Block::Heading(_, r) => Some(plain(r)).filter(|s| !s.trim().is_empty()),
        _ => None,
    })
}

/// `p` was resolved inside a root by the caller ([`Engine::resolve_input`]).
fn read_resolved_text(p: &Path) -> Res<String> {
    let raw = std::fs::read(p).map_err(|err| classify(&format!("{}: {err}", p.display()), "convert"))?;
    Ok(pdfcraft_create::decode_text(&raw))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn markdown_becomes_blocks() {
        let b = markdown_blocks(
            "# Title\n\nSome **bold** text.\n\n- one\n- two\n\n1. first\n\n| A | B |\n|---|---|\n| 1 | 2 |\n\n\\pagebreak\n\n> quoted\n\n```\ncode\n```\n",
        );
        assert!(matches!(b.first(), Some(Block::Heading(1, _))));
        assert!(b.iter().any(|x| matches!(x, Block::Para(r) if r.iter().any(|r| r.bold && r.text == "bold"))));
        assert_eq!(b.iter().filter(|x| matches!(x, Block::Item(m, 0, _) if m == "\u{2022}")).count(), 2);
        assert!(b.iter().any(|x| matches!(x, Block::Item(m, _, _) if m == "1.")));
        assert!(b.iter().any(|x| matches!(x, Block::Table { rows, header: true } if rows.len() == 2)));
        assert!(b.contains(&Block::PageBreak));
        assert!(b.iter().any(|x| matches!(x, Block::Quote(_))));
        assert!(b.contains(&Block::Code("code".into())));
    }

    #[test]
    fn html_becomes_blocks() {
        let (b, title) = html_blocks(
            "<html><head><title>T &amp; Co</title><style>p{}</style></head><body><h2>Head</h2><p>A <b>bold</b> &lt;word&gt;</p><ul><li>x</li><li>y</li></ul><table><tr><th>H</th></tr><tr><td>c</td></tr></table><div style=\"page-break-before: always\"></div><pre>a\n b</pre></body></html>",
        );
        assert_eq!(title.as_deref(), Some("T & Co"));
        assert!(matches!(b.first(), Some(Block::Heading(2, _))));
        assert!(b.iter().any(|x| matches!(x, Block::Para(r) if plain(r) == "A bold <word>")));
        assert_eq!(b.iter().filter(|x| matches!(x, Block::Item(..))).count(), 2);
        assert!(b.iter().any(|x| matches!(x, Block::Table { header: true, .. })));
        assert!(b.contains(&Block::PageBreak));
        assert!(b.contains(&Block::Code("a\n b".into())));
    }

    #[test]
    fn long_text_breaks_into_pages_and_a_table_repeats_its_header() {
        let l = Layout { width: 300.0, height: 300.0, margin: 20.0, size: 10.0 };
        let mut t = Typeset::new(l);
        let none = |_: &str| None;
        for _ in 0..30 {
            t.block(&Block::Para(vec![Run { text: "word ".repeat(40), ..Default::default() }]), &none);
        }
        assert!(t.page_count() > 5);
        let rows: Vec<Vec<Vec<Run>>> = (0..80)
            .map(|i| vec![vec![Run { text: format!("r{i}"), ..Default::default() }], vec![Run { text: "x".into(), ..Default::default() }]])
            .collect();
        let before = t.page_count();
        t.block(&Block::Table { rows, header: true }, &none);
        assert!(t.page_count() > before);
        assert!(t.finish("t").is_ok());
    }

    #[test]
    fn image_headers_give_sizes() {
        let mut png = b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR".to_vec();
        png.extend_from_slice(&40u32.to_be_bytes());
        png.extend_from_slice(&20u32.to_be_bytes());
        assert_eq!(image_size(&png), Some((40.0, 20.0)));
        assert_eq!(image_size(b"nope"), None);
    }
}
