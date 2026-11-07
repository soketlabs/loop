//! PDF: text with its structure (headings, paragraphs, lists, pages) from
//! the text layer, OCR for scanned files when configured, and markdown laid
//! out into a new PDF.

use std::collections::HashMap;
use std::fmt::Write as _;
use std::sync::Arc;

use pdf_extract::{MediaBox, OutputDev, OutputError, Transform};

use super::markdown::{Block, Markdown, Marker, Span};
use super::ocr;

/// Average non-space characters per page below which the text layer is
/// treated as missing (a scan, or text drawn as shapes).
const MIN_CHARS_PER_PAGE: usize = 20;

/// The PDF's text and what produced it (`reader` or `ocr`).
pub(super) async fn read(bytes: Vec<u8>) -> Result<(String, &'static str), String> {
    let bytes = Arc::new(bytes);
    let shared = Arc::clone(&bytes);
    let extracted = super::blocking(move || extract(&shared)).await;
    if let Ok(text) = &extracted {
        if text.readable {
            return Ok((text.markdown.clone(), "reader"));
        }
    }
    let partial = extracted
        .as_ref()
        .ok()
        .map(|t| t.markdown.clone())
        .filter(|t| !t.trim().is_empty());
    if ocr::enabled() {
        return match (ocr::run(&bytes).await, partial) {
            (Ok(text), _) => Ok((text, "ocr")),
            (Err(e), Some(text)) => Ok((format!("{text}\n\n[OCR failed ({e}); this is the little text the PDF itself contains.]"), "reader")),
            (Err(e), None) => Err(e),
        };
    }
    match (extracted, partial) {
        (_, Some(text)) => Ok((
            format!("{text}\n\n[Little text could be extracted; the PDF may be scanned. OCR is off: set {} to enable it.]", ocr::KEY_ENV),
            "reader",
        )),
        (Ok(_), None) => Err(format!(
            "this PDF has no text layer (it may be scanned); set {} to read it with OCR",
            ocr::KEY_ENV
        )),
        (Err(e), None) => Err(format!("could not read the PDF ({e}); set {} to read it with OCR", ocr::KEY_ENV)),
    }
}

pub(super) struct Extracted {
    pub markdown: String,
    readable: bool,
}

/// The text layer of `bytes` as markdown.
pub(super) fn extract(bytes: &[u8]) -> Result<Extracted, String> {
    let mut doc = pdf_extract::Document::load_mem(bytes).map_err(|e| e.to_string())?;
    if doc.is_encrypted() {
        doc.decrypt("")
            .map_err(|_| "the PDF is password-protected".to_owned())?;
    }
    let mut collector = Collector::default();
    pdf_extract::output_doc(&doc, &mut collector).map_err(|e| e.to_string())?;
    let pages: Vec<Vec<Line>> = collector.pages.into_iter().map(lines).collect();
    let markdown = layout(&pages);
    let chars = markdown.chars().filter(|c| !c.is_whitespace()).count();
    let garbled = markdown
        .chars()
        .filter(|c| {
            *c == '\u{FFFD}'
                || ('\u{E000}'..='\u{F8FF}').contains(c)
                || c.is_control() && !c.is_whitespace()
        })
        .count();
    let readable = chars >= MIN_CHARS_PER_PAGE * pages.len().max(1) && garbled * 5 < chars;
    Ok(Extracted { markdown, readable })
}

struct Glyph {
    x: f64,
    y: f64,
    end: f64,
    size: f64,
    text: String,
}

#[derive(Default)]
struct Collector {
    pages: Vec<Vec<Glyph>>,
    page: Vec<Glyph>,
}

impl OutputDev for Collector {
    fn begin_page(
        &mut self,
        _: u32,
        _: &MediaBox,
        _: Option<(f64, f64, f64, f64)>,
    ) -> Result<(), OutputError> {
        self.page.clear();
        Ok(())
    }

    fn end_page(&mut self) -> Result<(), OutputError> {
        self.pages.push(std::mem::take(&mut self.page));
        Ok(())
    }

    fn output_character(
        &mut self,
        trm: &Transform,
        width: f64,
        spacing: f64,
        font_size: f64,
        ch: &str,
    ) -> Result<(), OutputError> {
        let scale = (trm.m11 * trm.m22 - trm.m12 * trm.m21).abs().sqrt();
        let size = font_size * scale;
        let x = trm.m31;
        self.page.push(Glyph {
            x,
            y: trm.m32,
            end: x + (width * font_size + spacing) * scale,
            size,
            text: ch.to_owned(),
        });
        Ok(())
    }

    fn begin_word(&mut self) -> Result<(), OutputError> {
        Ok(())
    }

    fn end_word(&mut self) -> Result<(), OutputError> {
        Ok(())
    }

    fn end_line(&mut self) -> Result<(), OutputError> {
        Ok(())
    }
}

struct Line {
    x: f64,
    end: f64,
    /// Baseline, PDF space (up is larger).
    y: f64,
    size: f64,
    text: String,
}

/// Glyphs grouped into lines in content order, each read left to right.
fn lines(glyphs: Vec<Glyph>) -> Vec<Line> {
    let mut groups: Vec<Vec<Glyph>> = Vec::new();
    for glyph in glyphs {
        let same_line = groups.last().and_then(|g| g.last()).is_some_and(|last| {
            (last.y - glyph.y).abs() <= last.size.max(glyph.size).max(1.0) * 0.45
        });
        if same_line {
            groups.last_mut().unwrap().push(glyph);
        } else {
            groups.push(vec![glyph]);
        }
    }
    let mut out = Vec::new();
    for mut group in groups {
        group.sort_by(|a, b| a.x.total_cmp(&b.x));
        let mut text = String::new();
        let mut last_end: Option<f64> = None;
        let mut size: f64 = 0.0;
        for g in &group {
            if let Some(end) = last_end {
                if g.x - end > g.size * 0.2 && !text.ends_with(' ') && !g.text.starts_with(' ') {
                    text.push(' ');
                }
            }
            text.push_str(&g.text);
            last_end = Some(last_end.map_or(g.end, |e| e.max(g.end)));
            if !g.text.trim().is_empty() {
                size = size.max(g.size);
            }
        }
        let text = text.split_whitespace().collect::<Vec<_>>().join(" ");
        if text.is_empty() {
            continue;
        }
        let x = group
            .iter()
            .find(|g| !g.text.trim().is_empty())
            .map_or(group[0].x, |g| g.x);
        out.push(Line {
            x,
            end: last_end.unwrap_or(x),
            y: group[0].y,
            size,
            text,
        });
    }
    out
}

/// A block being assembled from lines: heading level, list depth and kind,
/// text so far, and its last line.
type Pending<'a> = (Option<u8>, Option<(usize, bool)>, String, &'a Line);

/// Markdown for the pages: font size picks headings, vertical gaps split
/// paragraphs, and bullet glyphs make list items.
fn layout(pages: &[Vec<Line>]) -> String {
    let mut weight: HashMap<i64, usize> = HashMap::new();
    for line in pages.iter().flatten() {
        *weight.entry(half_points(line.size)).or_default() += line.text.len();
    }
    let body = weight
        .iter()
        .max_by_key(|(_, n)| **n)
        .map_or(0, |(s, _)| *s);
    let mut heading_sizes: Vec<i64> = weight
        .keys()
        .copied()
        .filter(|s| *s as f64 >= body as f64 * 1.15)
        .collect();
    heading_sizes.sort_unstable_by(|a, b| b.cmp(a));
    heading_sizes.truncate(6);
    let level_of = |size: f64| {
        heading_sizes
            .iter()
            .position(|s| *s == half_points(size))
            .map(|i| i as u8 + 1)
    };

    let mut md = Markdown::default();
    for (n, page) in pages.iter().enumerate() {
        if pages.len() > 1 {
            md.block(&format!("<!-- page {} -->", n + 1));
        }
        let left = page.iter().map(|l| l.x).fold(f64::MAX, f64::min);
        let widest = page.iter().map(|l| l.end - l.x).fold(0.0, f64::max);
        let mut current: Option<Pending> = None;
        // Left edges of the open list levels, outermost first.
        let mut indents: Vec<f64> = Vec::new();
        for (i, line) in page.iter().enumerate() {
            let edge = i == 0 || i + 1 == page.len();
            if edge && line.text.len() <= 4 && line.text.chars().all(|c| c.is_ascii_digit()) {
                continue;
            }
            let level = level_of(line.size).filter(|_| line.text.len() <= 200);
            let (bullet, text) = match strip_marker(&line.text).filter(|_| level.is_none()) {
                Some((ordered, rest)) => {
                    while indents.last().is_some_and(|x| *x > line.x + 3.0) {
                        indents.pop();
                    }
                    if indents.last().is_none_or(|x| *x < line.x - 3.0) {
                        indents.push(line.x);
                    }
                    (Some((indents.len() - 1, ordered)), rest.to_owned())
                }
                None => (None, line.text.clone()),
            };
            let starts_new = match &current {
                None => true,
                Some((prev_level, _, _, prev)) => {
                    let gap = prev.y - line.y;
                    let short = prev.end - prev.x < widest * 0.75
                        && prev.text.ends_with(['.', ':', '!', '?']);
                    bullet.is_some()
                        || level != *prev_level
                        || gap > prev.size.max(line.size) * 1.5
                        || gap < 0.0
                        || (short && level.is_none())
                }
            };
            if starts_new {
                if bullet.is_none() && line.x - left < 3.0 {
                    indents.clear();
                }
                if let Some(done) = current.take() {
                    emit(&mut md, done.0, done.1, &done.2);
                }
                current = Some((level, bullet, text, line));
            } else if let Some((_, _, joined, last)) = &mut current {
                join_line(joined, &text);
                *last = line;
            }
        }
        if let Some(done) = current.take() {
            emit(&mut md, done.0, done.1, &done.2);
        }
    }
    md.finish()
}

fn half_points(size: f64) -> i64 {
    (size * 2.0).round() as i64
}

fn emit(md: &mut Markdown, level: Option<u8>, bullet: Option<(usize, bool)>, text: &str) {
    match (level, bullet) {
        (Some(level), _) => md.heading(level, text),
        (None, Some((depth, ordered))) => md.item(depth, ordered, text),
        (None, None) => md.block(text),
    }
}

/// Whether the line starts with a list marker (numbered or not), and the
/// text after it.
fn strip_marker(text: &str) -> Option<(bool, &str)> {
    let mut chars = text.chars();
    let first = chars.next()?;
    let is_bullet = matches!(
        first,
        '\u{2022}' | '\u{25CF}' | '\u{25AA}' | '\u{25E6}' | '\u{2013}' | '\u{00B7}' | '-' | '*'
    ) || ('\u{F000}'..='\u{F0FF}').contains(&first);
    let rest = chars.as_str();
    if is_bullet && rest.starts_with(' ') {
        return Some((false, rest.trim_start()));
    }
    let digits = text.len() - text.trim_start_matches(|c: char| c.is_ascii_digit()).len();
    let rest = text[digits..].strip_prefix(['.', ')'])?;
    ((1..=3).contains(&digits) && rest.starts_with(' ')).then(|| (true, rest.trim_start()))
}

/// Append a wrapped line, rejoining words hyphenated across the break.
fn join_line(text: &mut String, next: &str) {
    let hyphenated = text.ends_with('-')
        && text.chars().rev().nth(1).is_some_and(char::is_alphabetic)
        && next.chars().next().is_some_and(char::is_lowercase);
    if hyphenated {
        text.pop();
    } else {
        text.push(' ');
    }
    text.push_str(next);
}

// Written PDFs: A4, in points.
const PAGE_W: f64 = 595.0;
const PAGE_H: f64 = 842.0;
const MARGIN: f64 = 56.0;
const BODY: f64 = 11.0;

#[derive(Clone, Copy, PartialEq)]
enum Font {
    Regular,
    Bold,
    Italic,
    BoldItalic,
    Mono,
}

impl Font {
    fn of(bold: bool, italic: bool, code: bool) -> Self {
        match (code, bold, italic) {
            (true, _, _) => Font::Mono,
            (false, true, true) => Font::BoldItalic,
            (false, true, false) => Font::Bold,
            (false, false, true) => Font::Italic,
            (false, false, false) => Font::Regular,
        }
    }

    fn name(self) -> &'static str {
        match self {
            Font::Regular => "F1",
            Font::Bold => "F2",
            Font::Italic => "F3",
            Font::BoldItalic => "F4",
            Font::Mono => "F5",
        }
    }

    /// Advance of `byte` (WinAnsi) in thousandths of the font size.
    fn width(self, byte: u8) -> u16 {
        match self {
            Font::Mono => 600,
            Font::Bold | Font::BoldItalic => match byte {
                32..=126 => HELVETICA_BOLD[(byte - 32) as usize],
                _ => special_width(byte),
            },
            Font::Regular | Font::Italic => match byte {
                32..=126 => HELVETICA[(byte - 32) as usize],
                _ => special_width(byte),
            },
        }
    }
}

fn special_width(byte: u8) -> u16 {
    match byte {
        0x95 => 350,
        0x97 | 0x85 => 1000,
        0x91 | 0x92 => 222,
        0x93 | 0x94 => 333,
        0xA0 => 278,
        _ => 556,
    }
}

#[rustfmt::skip]
const HELVETICA: [u16; 95] = [
    278, 278, 355, 556, 556, 889, 667, 191, 333, 333, 389, 584, 278, 333, 278, 278,
    556, 556, 556, 556, 556, 556, 556, 556, 556, 556, 278, 278, 584, 584, 584, 556,
    1015, 667, 667, 722, 722, 667, 611, 778, 722, 278, 500, 667, 556, 833, 722, 778,
    667, 778, 722, 667, 611, 722, 667, 944, 667, 667, 611, 278, 278, 278, 469, 556,
    333, 556, 556, 500, 556, 556, 278, 556, 556, 222, 222, 500, 222, 833, 556, 556,
    556, 556, 333, 500, 278, 556, 500, 722, 500, 500, 500, 334, 260, 334, 584,
];

#[rustfmt::skip]
const HELVETICA_BOLD: [u16; 95] = [
    278, 333, 474, 556, 556, 889, 722, 238, 333, 333, 389, 584, 278, 333, 278, 278,
    556, 556, 556, 556, 556, 556, 556, 556, 556, 556, 333, 333, 584, 584, 584, 611,
    975, 722, 722, 722, 722, 667, 611, 778, 722, 278, 556, 722, 611, 833, 722, 778,
    667, 778, 722, 667, 611, 722, 667, 944, 667, 667, 611, 333, 278, 333, 584, 556,
    333, 556, 611, 556, 611, 556, 333, 611, 611, 278, 278, 556, 278, 889, 611, 611,
    611, 611, 389, 556, 333, 611, 556, 778, 556, 556, 500, 389, 280, 389, 584,
];

/// `ch` in WinAnsiEncoding, the encoding of the standard fonts; characters
/// it lacks become `?`.
fn win_ansi(ch: char) -> u8 {
    match ch {
        ' '..='~' => ch as u8,
        '\u{A0}'..='\u{FF}' => ch as u32 as u8,
        '\u{20AC}' => 0x80,
        '\u{201A}' => 0x82,
        '\u{0192}' => 0x83,
        '\u{201E}' => 0x84,
        '\u{2026}' => 0x85,
        '\u{2020}' => 0x86,
        '\u{2021}' => 0x87,
        '\u{02C6}' => 0x88,
        '\u{2030}' => 0x89,
        '\u{0160}' => 0x8A,
        '\u{2039}' => 0x8B,
        '\u{0152}' => 0x8C,
        '\u{017D}' => 0x8E,
        '\u{2018}' => 0x91,
        '\u{2019}' => 0x92,
        '\u{201C}' => 0x93,
        '\u{201D}' => 0x94,
        '\u{2022}' => 0x95,
        '\u{2013}' => 0x96,
        '\u{2014}' => 0x97,
        '\u{02DC}' => 0x98,
        '\u{2122}' => 0x99,
        '\u{0161}' => 0x9A,
        '\u{203A}' => 0x9B,
        '\u{0153}' => 0x9C,
        '\u{017E}' => 0x9E,
        '\u{0178}' => 0x9F,
        _ => b'?',
    }
}

/// Text in one font, already encoded.
struct Piece {
    font: Font,
    bytes: Vec<u8>,
}

fn measure(font: Font, bytes: &[u8], size: f64) -> f64 {
    bytes.iter().map(|b| font.width(*b) as f64).sum::<f64>() * size / 1000.0
}

/// `spans` broken into lines no wider than `width`.
fn wrap(spans: &[Span], size: f64, width: f64, force: Option<Font>) -> Vec<Vec<Piece>> {
    // Words keep their trailing space so styles can change mid-line.
    let mut words: Vec<(Font, Vec<u8>, bool)> = Vec::new();
    for span in spans {
        let font = force.unwrap_or_else(|| Font::of(span.bold, span.italic, span.code));
        for (i, line) in span.text.replace('\t', "    ").split('\n').enumerate() {
            if i > 0 {
                words.push((font, Vec::new(), true));
            }
            let mut word = Vec::new();
            for ch in line.chars() {
                word.push(win_ansi(ch));
                if ch == ' ' {
                    words.push((font, std::mem::take(&mut word), false));
                }
            }
            if !word.is_empty() {
                words.push((font, word, false));
            }
        }
    }
    let mut lines: Vec<Vec<Piece>> = vec![Vec::new()];
    let mut used = 0.0;
    for (font, mut word, newline) in words {
        if newline {
            lines.push(Vec::new());
            used = 0.0;
            continue;
        }
        let mut w = measure(font, &word, size);
        let trimmed = word
            .strip_suffix(b" ")
            .map_or(w, |t| measure(font, t, size));
        if used > 0.0 && used + trimmed > width {
            lines.push(Vec::new());
            used = 0.0;
        }
        if used == 0.0 {
            while word.first() == Some(&b' ') {
                word.remove(0);
            }
            w = measure(font, &word, size);
            // A word wider than the line is split where it overflows.
            while measure(font, &word, size) > width && word.len() > 1 {
                let mut cut = word.len() - 1;
                while cut > 1 && measure(font, &word[..cut], size) > width {
                    cut -= 1;
                }
                let rest = word.split_off(cut);
                lines.last_mut().unwrap().push(Piece { font, bytes: word });
                lines.push(Vec::new());
                word = rest;
                w = measure(font, &word, size);
            }
        }
        let line = lines.last_mut().unwrap();
        match line.last_mut() {
            Some(last) if last.font == font => last.bytes.extend_from_slice(&word),
            _ => line.push(Piece { font, bytes: word }),
        }
        used += w;
    }
    lines
}

struct Writer {
    pages: Vec<String>,
    page: String,
    /// Top of the space left on the page.
    y: f64,
}

impl Writer {
    fn new() -> Self {
        Self {
            pages: Vec::new(),
            page: String::new(),
            y: PAGE_H - MARGIN,
        }
    }

    fn space(&mut self, points: f64) {
        if self.y < PAGE_H - MARGIN {
            self.y -= points;
        }
    }

    fn ensure(&mut self, height: f64) {
        if self.y - height < MARGIN && self.y < PAGE_H - MARGIN {
            self.pages.push(std::mem::take(&mut self.page));
            self.y = PAGE_H - MARGIN;
        }
    }

    fn text_line(&mut self, x: f64, baseline: f64, pieces: &[Piece], size: f64) {
        let _ = write!(self.page, "BT {x:.2} {baseline:.2} Td ");
        for piece in pieces {
            let _ = write!(self.page, "/{} {size} Tf (", piece.font.name());
            for b in &piece.bytes {
                match b {
                    b'(' | b')' | b'\\' => {
                        self.page.push('\\');
                        self.page.push(*b as char);
                    }
                    32..=126 => self.page.push(*b as char),
                    _ => {
                        let _ = write!(self.page, "\\{b:03o}");
                    }
                }
            }
            self.page.push_str(") Tj ");
        }
        self.page.push_str("ET\n");
    }

    /// Wrapped text at `x`, `width` wide; returns nothing, moves `y` down.
    fn paragraph(
        &mut self,
        spans: &[Span],
        x: f64,
        width: f64,
        size: f64,
        force: Option<Font>,
        marker: Option<&str>,
    ) {
        let leading = size * 1.35;
        let lines = wrap(spans, size, width, force);
        for (i, line) in lines.iter().enumerate() {
            self.ensure(leading);
            let baseline = self.y - size;
            if i == 0 {
                if let Some(marker) = marker {
                    let bytes: Vec<u8> = marker.chars().map(win_ansi).collect();
                    let w = measure(Font::Regular, &bytes, size);
                    self.text_line(
                        x - w - 6.0,
                        baseline,
                        &[Piece {
                            font: Font::Regular,
                            bytes,
                        }],
                        size,
                    );
                }
            }
            self.text_line(x, baseline, line, size);
            self.y -= leading;
        }
    }

    fn code(&mut self, code: &str) {
        let size = 9.5;
        let leading = size * 1.3;
        let x = MARGIN + 8.0;
        let width = PAGE_W - 2.0 * MARGIN - 16.0;
        let span = Span {
            text: code.to_owned(),
            code: true,
            ..Default::default()
        };
        for line in wrap(&[span], size, width, Some(Font::Mono)) {
            self.ensure(leading);
            let _ = writeln!(
                self.page,
                "0.95 g {:.2} {:.2} {:.2} {:.2} re f 0 g",
                MARGIN,
                self.y - leading,
                PAGE_W - 2.0 * MARGIN,
                leading
            );
            self.text_line(x, self.y - size, &line, size);
            self.y -= leading;
        }
    }

    fn table(&mut self, rows: &[Vec<Vec<Span>>]) {
        let cols = rows.iter().map(Vec::len).max().unwrap_or(0);
        if cols == 0 {
            return;
        }
        let size = 10.0;
        let leading = size * 1.3;
        let pad = 4.0;
        let col_w = (PAGE_W - 2.0 * MARGIN) / cols as f64;
        for (r, row) in rows.iter().enumerate() {
            let force = (r == 0).then_some(Font::Bold);
            let cells: Vec<Vec<Vec<Piece>>> = (0..cols)
                .map(|c| {
                    row.get(c)
                        .map_or_else(Vec::new, |cell| wrap(cell, size, col_w - 2.0 * pad, force))
                })
                .collect();
            let height =
                cells.iter().map(Vec::len).max().unwrap_or(1).max(1) as f64 * leading + 2.0 * pad;
            self.ensure(height);
            let top = self.y;
            for (c, lines) in cells.iter().enumerate() {
                let x = MARGIN + c as f64 * col_w;
                if r == 0 {
                    let _ = writeln!(
                        self.page,
                        "0.9 g {x:.2} {:.2} {col_w:.2} {height:.2} re f 0 g",
                        top - height
                    );
                }
                let _ = writeln!(
                    self.page,
                    "0.5 w {x:.2} {:.2} {col_w:.2} {height:.2} re S",
                    top - height
                );
                for (i, line) in lines.iter().enumerate() {
                    let baseline = top - pad - size - i as f64 * leading;
                    self.text_line(x + pad, baseline, line, size);
                }
            }
            self.y -= height;
        }
    }

    fn finish(mut self) -> Vec<String> {
        if !self.page.is_empty() || self.pages.is_empty() {
            self.pages.push(self.page);
        }
        self.pages
    }
}

/// A PDF laying out `blocks` with the standard fonts (Latin text only).
pub(super) fn write(blocks: &[Block]) -> Vec<u8> {
    let mut w = Writer::new();
    let width = PAGE_W - 2.0 * MARGIN;
    for block in blocks {
        match block {
            Block::Heading { level, spans } => {
                let size = [20.0, 16.0, 13.5, 12.0, 11.0, 11.0][(*level).clamp(1, 6) as usize - 1];
                w.space(if *level <= 2 { 10.0 } else { 6.0 });
                w.ensure(size * 3.0);
                w.paragraph(spans, MARGIN, width, size, Some(Font::Bold), None);
                w.space(4.0);
            }
            Block::Paragraph(spans) => {
                w.paragraph(spans, MARGIN, width, BODY, None, None);
                w.space(6.0);
            }
            Block::Quote(spans) => {
                let top = w.y;
                w.paragraph(
                    spans,
                    MARGIN + 14.0,
                    width - 14.0,
                    BODY,
                    Some(Font::Italic),
                    None,
                );
                if w.y < top {
                    let _ = writeln!(
                        w.page,
                        "0.7 g {MARGIN:.2} {:.2} 2 {:.2} re f 0 g",
                        w.y,
                        top - w.y
                    );
                }
                w.space(6.0);
            }
            Block::Item {
                depth,
                marker,
                spans,
                ..
            } => {
                let indent = 18.0 * (*depth as f64 + 1.0);
                let label = match marker {
                    Marker::Bullet => Some("\u{2022}".to_owned()),
                    Marker::Number(n) => Some(format!("{n}.")),
                    Marker::Continued => None,
                };
                w.paragraph(
                    spans,
                    MARGIN + indent,
                    width - indent,
                    BODY,
                    None,
                    label.as_deref(),
                );
                w.space(2.0);
            }
            Block::Code(code) => {
                w.code(code);
                w.space(6.0);
            }
            Block::Rule => {
                w.ensure(12.0);
                let y = w.y - 6.0;
                let _ = writeln!(
                    w.page,
                    "0.6 G 0.5 w {MARGIN:.2} {y:.2} m {:.2} {y:.2} l S 0 G",
                    PAGE_W - MARGIN
                );
                w.y -= 12.0;
            }
            Block::Table(rows) => {
                w.table(rows);
                w.space(8.0);
            }
        }
    }
    assemble(&w.finish())
}

fn assemble(pages: &[String]) -> Vec<u8> {
    const FONTS: [&str; 5] = [
        "Helvetica",
        "Helvetica-Bold",
        "Helvetica-Oblique",
        "Helvetica-BoldOblique",
        "Courier",
    ];
    let mut objects: Vec<Vec<u8>> = Vec::new();
    // 1 catalog, 2 page tree, 3-7 fonts, then a page and its content per page.
    let first_page = 3 + FONTS.len();
    let kids: Vec<String> = (0..pages.len())
        .map(|i| format!("{} 0 R", first_page + 2 * i))
        .collect();
    objects.push(b"<< /Type /Catalog /Pages 2 0 R >>".to_vec());
    objects.push(
        format!(
            "<< /Type /Pages /Kids [{}] /Count {} >>",
            kids.join(" "),
            pages.len()
        )
        .into_bytes(),
    );
    for font in FONTS {
        objects.push(
            format!(
                "<< /Type /Font /Subtype /Type1 /BaseFont /{font} /Encoding /WinAnsiEncoding >>"
            )
            .into_bytes(),
        );
    }
    let fonts: String = (0..FONTS.len())
        .map(|i| format!("/F{} {} 0 R ", i + 1, 3 + i))
        .collect();
    for (i, content) in pages.iter().enumerate() {
        let content_id = first_page + 2 * i + 1;
        objects.push(
            format!(
                "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 {PAGE_W} {PAGE_H}] /Resources << /Font << {fonts}>> >> /Contents {content_id} 0 R >>"
            )
            .into_bytes(),
        );
        objects.push(
            format!(
                "<< /Length {} >>\nstream\n{content}endstream",
                content.len()
            )
            .into_bytes(),
        );
    }
    let mut out: Vec<u8> = b"%PDF-1.4\n%\xE2\xE3\xCF\xD3\n".to_vec();
    let mut offsets = Vec::new();
    for (i, body) in objects.iter().enumerate() {
        offsets.push(out.len());
        out.extend_from_slice(format!("{} 0 obj\n", i + 1).as_bytes());
        out.extend_from_slice(body);
        out.extend_from_slice(b"\nendobj\n");
    }
    let xref = out.len();
    let mut table = format!("xref\n0 {}\n0000000000 65535 f \n", objects.len() + 1);
    for offset in offsets {
        let _ = writeln!(table, "{offset:010} 00000 n ");
    }
    let _ = write!(
        table,
        "trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n",
        objects.len() + 1
    );
    out.extend_from_slice(table.as_bytes());
    out
}

#[cfg(test)]
mod tests {
    use super::super::markdown;
    use super::*;

    #[test]
    fn written_pdfs_read_back_with_structure() {
        let source = "# Quarterly Report\n\nRevenue grew in every region this quarter, led by strong demand.\n\n## Highlights\n\n- New customers signed\n- Costs held flat\n\nClosing paragraph with caf\u{e9} text.";
        let bytes = write(&markdown::parse(source));
        let text = extract(&bytes).unwrap();
        assert!(text.readable, "{}", text.markdown);
        assert_eq!(
            text.markdown,
            "# Quarterly Report\n\nRevenue grew in every region this quarter, led by strong demand.\n\n## Highlights\n\n- New customers signed\n- Costs held flat\n\nClosing paragraph with caf\u{e9} text."
        );
    }

    #[test]
    fn long_documents_break_pages() {
        let paragraph = "Lorem ipsum dolor sit amet, consectetur adipiscing elit. ".repeat(20);
        let source = (0..30)
            .map(|_| paragraph.clone())
            .collect::<Vec<_>>()
            .join("\n\n");
        let bytes = write(&markdown::parse(&source));
        let text = extract(&bytes).unwrap();
        assert!(text.markdown.contains("<!-- page 2 -->"));
    }
}
