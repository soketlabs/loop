//! Rich Text Format (.rtf): paragraphs, emphasis, headings, lists and tables.

use std::fmt::Write as _;

use encoding_rs::{Encoding, WINDOWS_1252};

use super::markdown::{self, Block, Markdown, Marker, Run, Span};

/// Destinations whose text is not part of the document body.
const SKIPPED: &[&str] = &[
    "annotation",
    "atnauthor",
    "atnid",
    "bkmkend",
    "bkmkstart",
    "colortbl",
    "colorschememapping",
    "comment",
    "datastore",
    "docvar",
    "fldinst",
    "filetbl",
    "fonttbl",
    "footer",
    "footerf",
    "footerl",
    "footerr",
    "footnote",
    "generator",
    "header",
    "headerf",
    "headerl",
    "headerr",
    "info",
    "latentstyles",
    "listoverridetable",
    "listtable",
    "mmathPr",
    "nesttableprops",
    "nonshppict",
    "object",
    "objdata",
    "pgdsctbl",
    "pict",
    "protusertbl",
    "revtbl",
    "rsidtbl",
    "shp",
    "shppict",
    "stylesheet",
    "template",
    "themedata",
    "userprops",
    "xmlnstbl",
];

pub(super) fn read(bytes: &[u8]) -> Result<String, String> {
    if !bytes.trim_ascii_start().starts_with(b"{\\rtf") {
        return Err("not an RTF file (it does not start with {\\rtf)".into());
    }
    let mut reader = Reader::default();
    reader.run(bytes);
    Ok(reader.finish())
}

#[derive(Clone)]
struct Group {
    skip: bool,
    /// Inside `\pntext` / `\listtext`: the list marker, not body text.
    marker: bool,
    bold: bool,
    italic: bool,
    /// Fallback characters after each `\u`.
    uc: usize,
}

impl Default for Group {
    fn default() -> Self {
        Self {
            skip: false,
            marker: false,
            bold: false,
            italic: false,
            uc: 1,
        }
    }
}

#[derive(Default)]
struct Paragraph {
    outline: Option<u8>,
    indent: i32,
    in_table: bool,
    level: Option<usize>,
    list: bool,
    marker: String,
    runs: Vec<Run>,
}

struct Reader {
    md: Markdown,
    encoding: &'static Encoding,
    group: Group,
    stack: Vec<Group>,
    paragraph: Paragraph,
    /// `\'hh` bytes not yet decoded, so multi-byte code pages decode whole.
    pending: Vec<u8>,
    /// Fallback characters still to drop after a `\u`.
    skip_chars: usize,
    high_surrogate: Option<u32>,
    cell: Vec<String>,
    row: Vec<String>,
    rows: Vec<Vec<String>>,
}

impl Default for Reader {
    fn default() -> Self {
        Self {
            md: Markdown::default(),
            encoding: WINDOWS_1252,
            group: Group::default(),
            stack: Vec::new(),
            paragraph: Paragraph::default(),
            pending: Vec::new(),
            skip_chars: 0,
            high_surrogate: None,
            cell: Vec::new(),
            row: Vec::new(),
            rows: Vec::new(),
        }
    }
}

impl Reader {
    fn run(&mut self, src: &[u8]) {
        let mut i = 0;
        while i < src.len() {
            match src[i] {
                b'{' => {
                    self.flush_bytes();
                    self.stack.push(self.group.clone());
                    i += 1;
                }
                b'}' => {
                    self.flush_bytes();
                    if let Some(group) = self.stack.pop() {
                        self.group = group;
                    }
                    self.skip_chars = 0;
                    i += 1;
                }
                b'\\' => i = self.control(src, i + 1),
                b'\r' | b'\n' => i += 1,
                _ => {
                    let start = i;
                    while i < src.len() && !matches!(src[i], b'{' | b'}' | b'\\' | b'\r' | b'\n') {
                        i += 1;
                    }
                    let mut chunk = &src[start..i];
                    let dropped = self.skip_chars.min(chunk.len());
                    self.skip_chars -= dropped;
                    chunk = &chunk[dropped..];
                    self.pending.extend_from_slice(chunk);
                }
            }
        }
        self.flush_bytes();
    }

    /// Handle the control word or symbol after a backslash at `i`; returns
    /// where reading continues.
    fn control(&mut self, src: &[u8], mut i: usize) -> usize {
        let Some(&c) = src.get(i) else { return i };
        if c.is_ascii_alphabetic() {
            let start = i;
            while i < src.len() && src[i].is_ascii_alphabetic() {
                i += 1;
            }
            let word = String::from_utf8_lossy(&src[start..i]).into_owned();
            let negative = src.get(i) == Some(&b'-');
            let digits = if negative { i + 1 } else { i };
            let mut end = digits;
            while end < src.len() && src[end].is_ascii_digit() {
                end += 1;
            }
            let mut param = None;
            if end > digits {
                let value: i64 = std::str::from_utf8(&src[digits..end])
                    .ok()
                    .and_then(|s| s.parse().ok())
                    .unwrap_or(0);
                param = Some(if negative { -value } else { value });
                i = end;
            }
            if src.get(i) == Some(&b' ') {
                i += 1;
            }
            if word == "bin" {
                self.flush_bytes();
                return (i + param.unwrap_or(0).max(0) as usize).min(src.len());
            }
            self.word(&word, param);
            return i;
        }
        match c {
            b'\'' => {
                let byte = src
                    .get(i + 1..i + 3)
                    .and_then(|h| std::str::from_utf8(h).ok())
                    .and_then(|h| u8::from_str_radix(h, 16).ok());
                if let Some(byte) = byte {
                    if self.skip_chars > 0 {
                        self.skip_chars -= 1;
                    } else {
                        self.pending.push(byte);
                    }
                    return i + 3;
                }
            }
            b'*' => self.group.skip = true,
            b'~' => self.text(" "),
            b'_' => self.text("-"),
            b'\\' | b'{' | b'}' => self.text(&(c as char).to_string()),
            b'\n' | b'\r' => self.word("par", None),
            _ => {}
        }
        i + 1
    }

    fn word(&mut self, word: &str, param: Option<i64>) {
        self.flush_bytes();
        match word {
            "par" | "sect" | "page" => self.end_paragraph(),
            "line" => self.text("\n"),
            "tab" => self.text("\t"),
            "emdash" => self.text("\u{2014}"),
            "endash" => self.text("\u{2013}"),
            "bullet" => self.text("\u{2022}"),
            "lquote" => self.text("\u{2018}"),
            "rquote" => self.text("\u{2019}"),
            "ldblquote" => self.text("\u{201C}"),
            "rdblquote" => self.text("\u{201D}"),
            "emspace" | "enspace" | "qmspace" => self.text(" "),
            "u" => {
                let mut value = param.unwrap_or(0);
                if value < 0 {
                    value += 65536;
                }
                self.unicode(value as u32);
                self.skip_chars = self.group.uc;
            }
            "uc" => self.group.uc = param.unwrap_or(1).max(0) as usize,
            "b" => self.group.bold = param != Some(0),
            "i" => self.group.italic = param != Some(0),
            "plain" => (self.group.bold, self.group.italic) = (false, false),
            "pard" => {
                self.paragraph.outline = None;
                self.paragraph.indent = 0;
                self.paragraph.in_table = false;
            }
            "outlinelevel" => {
                self.paragraph.outline = param.filter(|l| (0..9).contains(l)).map(|l| l as u8)
            }
            "li" => self.paragraph.indent = param.unwrap_or(0) as i32,
            "ls" => self.paragraph.list = param.unwrap_or(0) > 0,
            "ilvl" => self.paragraph.level = Some(param.unwrap_or(0).max(0) as usize),
            "pntext" | "listtext" => {
                self.group.marker = true;
                self.paragraph.list = true;
            }
            "intbl" => self.paragraph.in_table = true,
            "cell" => self.end_cell(),
            "row" => self.rows.push(std::mem::take(&mut self.row)),
            "ansicpg" => {
                if let Some(encoding) = param.and_then(code_page) {
                    self.encoding = encoding;
                }
            }
            "mac" => self.encoding = encoding_rs::MACINTOSH,
            w if SKIPPED.contains(&w) => self.group.skip = true,
            _ => {}
        }
    }

    fn unicode(&mut self, value: u32) {
        let ch = match value {
            0xD800..=0xDBFF => {
                self.high_surrogate = Some(value);
                return;
            }
            0xDC00..=0xDFFF => match self.high_surrogate.take() {
                Some(high) => char::from_u32(0x10000 + ((high - 0xD800) << 10) + (value - 0xDC00)),
                None => None,
            },
            v => char::from_u32(v),
        };
        if let Some(ch) = ch {
            self.text(&ch.to_string());
        }
    }

    fn flush_bytes(&mut self) {
        if self.pending.is_empty() {
            return;
        }
        let bytes = std::mem::take(&mut self.pending);
        let (text, _) = self.encoding.decode_without_bom_handling(&bytes);
        self.append(&text);
    }

    fn text(&mut self, text: &str) {
        self.flush_bytes();
        self.append(text);
    }

    fn append(&mut self, text: &str) {
        if self.group.skip {
            return;
        }
        if self.group.marker {
            self.paragraph.marker.push_str(text);
            return;
        }
        let (bold, italic) = (self.group.bold, self.group.italic);
        match self.paragraph.runs.last_mut() {
            Some(last) if last.bold == bold && last.italic == italic => last.text.push_str(text),
            _ => self.paragraph.runs.push(Run {
                text: text.to_owned(),
                bold,
                italic,
            }),
        }
    }

    /// Take the paragraph's content, keeping the formatting that carries
    /// over until the next `\pard`.
    fn take_paragraph(&mut self) -> Paragraph {
        let p = &mut self.paragraph;
        let next = Paragraph {
            outline: p.outline,
            indent: p.indent,
            in_table: p.in_table,
            ..Default::default()
        };
        std::mem::replace(p, next)
    }

    fn end_paragraph(&mut self) {
        let p = self.take_paragraph();
        let text = markdown::inline(&p.runs);
        if p.in_table {
            if !text.is_empty() {
                self.cell.push(text);
            }
            return;
        }
        self.flush_table();
        if text.is_empty() {
            return;
        }
        let bullet = text
            .strip_prefix(['\u{2022}', '\u{00B7}', '\u{F0B7}'])
            .map(str::trim_start);
        if let Some(level) = p.outline {
            let plain: String = p.runs.iter().map(|r| r.text.as_str()).collect();
            self.md.heading(level + 1, &plain);
        } else if p.list || bullet.is_some() {
            let depth = p
                .level
                .unwrap_or_else(|| ((p.indent - 360).max(0) / 720) as usize);
            let marker = p.marker.trim();
            let ordered = marker.ends_with(['.', ')']) && marker.chars().any(char::is_alphanumeric);
            self.md.item(depth, ordered, bullet.unwrap_or(&text));
        } else {
            self.md.block(&text);
        }
    }

    fn end_cell(&mut self) {
        let p = self.take_paragraph();
        let text = markdown::inline(&p.runs);
        if !text.is_empty() {
            self.cell.push(text);
        }
        let cell = std::mem::take(&mut self.cell).join("\n");
        self.row.push(cell);
    }

    fn flush_table(&mut self) {
        if !self.row.is_empty() {
            let row = std::mem::take(&mut self.row);
            self.rows.push(row);
        }
        if !self.rows.is_empty() {
            let rows = std::mem::take(&mut self.rows);
            self.md.block(&markdown::table(&rows));
        }
    }

    fn finish(mut self) -> String {
        self.flush_bytes();
        self.end_paragraph();
        self.flush_table();
        self.md.finish()
    }
}

fn code_page(page: i64) -> Option<&'static Encoding> {
    match page {
        932 => Some(encoding_rs::SHIFT_JIS),
        936 => Some(encoding_rs::GBK),
        949 => Some(encoding_rs::EUC_KR),
        950 => Some(encoding_rs::BIG5),
        10000 => Some(encoding_rs::MACINTOSH),
        874 | 1250..=1258 => Encoding::for_label(format!("windows-{page}").as_bytes()),
        _ => None,
    }
}

pub(super) fn write(blocks: &[Block]) -> String {
    let mut out = String::from(
        "{\\rtf1\\ansi\\ansicpg1252\\deff0\\uc1{\\fonttbl{\\f0\\fswiss\\fcharset0 Calibri;}{\\f1\\fmodern\\fcharset0 Courier New;}}\n\\viewkind4\\f0\\fs22\n",
    );
    for block in blocks {
        match block {
            Block::Heading { level, spans } => {
                let level = (*level).clamp(1, 6);
                let size = [36, 32, 28, 24, 22, 22][level as usize - 1];
                let _ = write!(
                    out,
                    "\\pard\\outlinelevel{}\\keepn\\sb240\\sa120\\b\\fs{size} ",
                    level - 1
                );
                spans_into(&mut out, spans);
                out.push_str("\\b0\\fs22\\par\n");
            }
            Block::Paragraph(spans) => {
                out.push_str("\\pard\\sa160 ");
                spans_into(&mut out, spans);
                out.push_str("\\par\n");
            }
            Block::Quote(spans) => {
                out.push_str("\\pard\\li720\\sa160\\i ");
                spans_into(&mut out, spans);
                out.push_str("\\i0\\par\n");
            }
            Block::Item {
                depth,
                marker,
                spans,
                ..
            } => {
                let indent = 720 * (depth + 1);
                match marker {
                    Marker::Bullet => {
                        let _ = write!(out, "\\pard{{\\pntext\\bullet\\tab}}{{\\*\\pn\\pnlvlblt\\pnindent360{{\\pntxtb\\bullet}}}}\\fi-360\\li{indent}\\sa60 ");
                    }
                    Marker::Number(n) => {
                        let _ = write!(out, "\\pard{{\\pntext {n}.\\tab}}{{\\*\\pn\\pnlvlbody\\pndec\\pnstart{n}\\pnindent360{{\\pntxta .}}}}\\fi-360\\li{indent}\\sa60 ");
                    }
                    Marker::Continued => {
                        let _ = write!(out, "\\pard\\li{indent}\\sa60 ");
                    }
                }
                spans_into(&mut out, spans);
                out.push_str("\\par\n");
            }
            Block::Code(code) => {
                out.push_str("\\pard\\sa160\\f1\\fs20 ");
                escape_into(&mut out, code);
                out.push_str("\\f0\\fs22\\par\n");
            }
            Block::Rule => out.push_str("\\pard\\brdrb\\brdrs\\brdrw10\\brsp20 \\par\n"),
            Block::Table(rows) => {
                let cols = rows.iter().map(Vec::len).max().unwrap_or(0);
                if cols == 0 {
                    continue;
                }
                let width = 9360 / cols;
                for (i, row) in rows.iter().enumerate() {
                    out.push_str("\\trowd\\trgaph108");
                    for c in 0..cols {
                        let _ = write!(out, "\\clbrdrt\\brdrs\\clbrdrl\\brdrs\\clbrdrb\\brdrs\\clbrdrr\\brdrs\\cellx{}", width * (c + 1));
                    }
                    out.push('\n');
                    for c in 0..cols {
                        out.push_str(if i == 0 {
                            "\\pard\\intbl\\b "
                        } else {
                            "\\pard\\intbl "
                        });
                        if let Some(cell) = row.get(c) {
                            spans_into(&mut out, cell);
                        }
                        out.push_str(if i == 0 { "\\b0\\cell\n" } else { "\\cell\n" });
                    }
                    out.push_str("\\row\n");
                }
                out.push_str("\\pard\n");
            }
        }
    }
    out.push('}');
    out
}

fn spans_into(out: &mut String, spans: &[Span]) {
    for span in spans {
        let mut words = String::new();
        if span.bold {
            words.push_str("\\b");
        }
        if span.italic {
            words.push_str("\\i");
        }
        if span.code {
            words.push_str("\\f1");
        }
        out.push('{');
        if !words.is_empty() {
            out.push_str(&words);
            out.push(' ');
        }
        escape_into(out, &span.text);
        out.push('}');
    }
}

fn escape_into(out: &mut String, text: &str) {
    for ch in text.chars() {
        match ch {
            '\\' => out.push_str("\\\\"),
            '{' => out.push_str("\\{"),
            '}' => out.push_str("\\}"),
            '\n' => out.push_str("\\line "),
            '\t' => out.push_str("\\tab "),
            '\r' => {}
            c if c.is_ascii() => out.push(c),
            c => {
                let mut units = [0u16; 2];
                for unit in c.encode_utf16(&mut units) {
                    let _ = write!(out, "\\u{}?", *unit as i16);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn markdown_round_trips_through_rtf() {
        let source = "# Notes\n\nCaf\u{e9} with **bold** text.\n\n- one\n- two\n\n1. first\n\n| a | b |\n| --- | --- |\n| 1 | 2 |";
        let rtf = write(&markdown::parse(source));
        let text = read(rtf.as_bytes()).unwrap();
        assert_eq!(text, "# Notes\n\nCaf\u{e9} with **bold** text.\n\n- one\n- two\n\n1. first\n\n| **a** | **b** |\n| --- | --- |\n| 1 | 2 |");
    }

    #[test]
    fn reads_word_style_rtf() {
        let rtf = br"{\rtf1\ansi\ansicpg1252{\fonttbl{\f0 Times;}}{\*\generator Word;}{\info{\title T}}\pard\plain\s1\outlinelevel0 Title\par\pard\plain{\listtext\'b7\tab}\ls1\ilvl0 Item \'e9\par\pard Body \u8364? end\par}";
        assert_eq!(
            read(rtf).unwrap(),
            "# Title\n\n- Item \u{e9}\n\nBody \u{20ac} end"
        );
    }
}
