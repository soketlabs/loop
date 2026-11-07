//! Markdown both ways: the block model the document writers lay out, and
//! the helpers the readers use to emit markdown.

use pulldown_cmark::{Event, HeadingLevel, Options, Parser, Tag, TagEnd};

/// A run of inline text with one style.
#[derive(Debug, Clone, Default, PartialEq)]
pub(super) struct Span {
    pub text: String,
    pub bold: bool,
    pub italic: bool,
    pub code: bool,
}

/// How a list item is marked.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(super) enum Marker {
    Bullet,
    Number(u64),
    /// A later paragraph of the same item: indented, no marker.
    Continued,
}

#[derive(Debug, Clone, PartialEq)]
pub(super) enum Block {
    Heading {
        level: u8,
        spans: Vec<Span>,
    },
    Paragraph(Vec<Span>),
    /// A list item, flattened out of its list. `list` identifies the
    /// innermost list it belongs to, so numbering can restart per list.
    Item {
        depth: usize,
        marker: Marker,
        list: usize,
        spans: Vec<Span>,
    },
    Quote(Vec<Span>),
    Code(String),
    /// Rows of cells; the first row is the header.
    Table(Vec<Vec<Vec<Span>>>),
    Rule,
}

/// The blocks of `markdown` (CommonMark with tables, strikethrough and task lists).
pub(super) fn parse(markdown: &str) -> Vec<Block> {
    let options =
        Options::ENABLE_TABLES | Options::ENABLE_STRIKETHROUGH | Options::ENABLE_TASKLISTS;
    let mut builder = Builder::default();
    for event in Parser::new_ext(markdown, options) {
        builder.event(event);
    }
    builder.flush();
    builder.blocks
}

/// The text of `spans` without styling.
pub(super) fn plain(spans: &[Span]) -> String {
    spans.iter().map(|s| s.text.as_str()).collect()
}

#[derive(Default)]
struct Builder {
    blocks: Vec<Block>,
    spans: Vec<Span>,
    bold: usize,
    italic: usize,
    heading: Option<u8>,
    quote: usize,
    /// The open lists, innermost last: id and next number (`None` for bullets).
    lists: Vec<(usize, Option<u64>)>,
    list_ids: usize,
    marker: Option<Marker>,
    code: Option<String>,
    table: Option<Vec<Vec<Vec<Span>>>>,
    row: Vec<Vec<Span>>,
    /// Open links: destination and the byte where their text starts in the
    /// plain text of `spans`.
    links: Vec<(String, usize)>,
}

impl Builder {
    fn event(&mut self, event: Event) {
        match event {
            Event::Start(tag) => self.start(tag),
            Event::End(tag) => self.end(tag),
            Event::Text(text) => match &mut self.code {
                Some(code) => code.push_str(&text),
                None => self.push(&text, false),
            },
            Event::Code(text) => self.push(&text, true),
            Event::SoftBreak => self.push(" ", false),
            Event::HardBreak => self.push("\n", false),
            Event::InlineHtml(html) | Event::Html(html) => {
                let tag = html.trim().to_ascii_lowercase();
                if matches!(tag.as_str(), "<br>" | "<br/>" | "<br />") {
                    self.push("\n", false);
                }
            }
            Event::Rule => {
                self.flush();
                self.blocks.push(Block::Rule);
            }
            Event::TaskListMarker(done) => self.push(if done { "[x] " } else { "[ ] " }, false),
            Event::FootnoteReference(name) => self.push(&format!("[^{name}]"), false),
            _ => {}
        }
    }

    fn start(&mut self, tag: Tag) {
        match tag {
            Tag::Heading { level, .. } => {
                self.flush();
                self.heading = Some(heading_level(level));
            }
            Tag::Paragraph => self.flush(),
            Tag::BlockQuote(_) => {
                self.flush();
                self.quote += 1;
            }
            Tag::CodeBlock(_) => {
                self.flush();
                self.code = Some(String::new());
            }
            Tag::List(start) => {
                self.flush();
                self.list_ids += 1;
                self.lists.push((self.list_ids, start));
            }
            Tag::Item => {
                self.flush();
                self.marker = Some(match self.lists.last_mut() {
                    Some((_, Some(n))) => {
                        let marker = Marker::Number(*n);
                        *n += 1;
                        marker
                    }
                    _ => Marker::Bullet,
                });
            }
            Tag::Table(_) => {
                self.flush();
                self.table = Some(Vec::new());
            }
            Tag::TableHead | Tag::TableRow => self.row.clear(),
            Tag::TableCell => self.spans.clear(),
            Tag::Emphasis => self.italic += 1,
            Tag::Strong => self.bold += 1,
            Tag::Link { dest_url, .. } => {
                let start = plain(&self.spans).len();
                self.links.push((dest_url.to_string(), start));
            }
            Tag::Image { .. } => self.push("[image: ", false),
            _ => {}
        }
    }

    fn end(&mut self, tag: TagEnd) {
        match tag {
            TagEnd::Heading(_) => {
                self.flush();
                self.heading = None;
            }
            TagEnd::Paragraph => self.flush(),
            TagEnd::BlockQuote(_) => {
                self.flush();
                self.quote = self.quote.saturating_sub(1);
            }
            TagEnd::CodeBlock => {
                if let Some(code) = self.code.take() {
                    self.blocks
                        .push(Block::Code(code.trim_end_matches('\n').to_owned()));
                }
            }
            TagEnd::List(_) => {
                self.flush();
                self.lists.pop();
            }
            TagEnd::Item => {
                self.flush();
                self.marker = None;
            }
            TagEnd::TableCell => {
                let cell = std::mem::take(&mut self.spans);
                self.row.push(cell);
            }
            TagEnd::TableHead | TagEnd::TableRow => {
                let row = std::mem::take(&mut self.row);
                if let Some(table) = &mut self.table {
                    table.push(row);
                }
            }
            TagEnd::Table => {
                if let Some(table) = self.table.take() {
                    self.blocks.push(Block::Table(table));
                }
            }
            TagEnd::Emphasis => self.italic = self.italic.saturating_sub(1),
            TagEnd::Strong => self.bold = self.bold.saturating_sub(1),
            TagEnd::Link => {
                if let Some((url, start)) = self.links.pop() {
                    let all = plain(&self.spans);
                    let text = all.get(start..).unwrap_or_default();
                    let shown = url.strip_prefix("mailto:").unwrap_or(&url);
                    if !url.is_empty() && !url.starts_with('#') && text.trim() != shown {
                        self.push(&format!(" ({url})"), false);
                    }
                }
            }
            TagEnd::Image => self.push("]", false),
            _ => {}
        }
    }

    fn push(&mut self, text: &str, code: bool) {
        let (bold, italic) = (self.bold > 0, self.italic > 0);
        if let Some(last) = self.spans.last_mut() {
            if last.bold == bold && last.italic == italic && last.code == code {
                last.text.push_str(text);
                return;
            }
        }
        self.spans.push(Span {
            text: text.to_owned(),
            bold,
            italic,
            code,
        });
    }

    /// Turn the pending inline text into a block of the current context.
    fn flush(&mut self) {
        if self.table.is_some() {
            return;
        }
        let spans = std::mem::take(&mut self.spans);
        if spans.iter().all(|s| s.text.trim().is_empty()) {
            return;
        }
        let block = if let Some(level) = self.heading {
            Block::Heading { level, spans }
        } else if let Some((list, _)) = self.lists.last() {
            Block::Item {
                depth: self.lists.len() - 1,
                marker: self.marker.take().unwrap_or(Marker::Continued),
                list: *list,
                spans,
            }
        } else if self.quote > 0 {
            Block::Quote(spans)
        } else {
            Block::Paragraph(spans)
        };
        self.blocks.push(block);
    }
}

fn heading_level(level: HeadingLevel) -> u8 {
    match level {
        HeadingLevel::H1 => 1,
        HeadingLevel::H2 => 2,
        HeadingLevel::H3 => 3,
        HeadingLevel::H4 => 4,
        HeadingLevel::H5 => 5,
        HeadingLevel::H6 => 6,
    }
}

/// A run of document text with its emphasis, as the readers collect it.
#[derive(Debug, Clone, Default, PartialEq)]
pub(super) struct Run {
    pub text: String,
    pub bold: bool,
    pub italic: bool,
}

/// Inline markdown for `runs`: neighbours with the same style are merged
/// and whitespace stays outside the emphasis markers.
pub(super) fn inline(runs: &[Run]) -> String {
    let mut merged: Vec<Run> = Vec::new();
    for run in runs {
        match merged.last_mut() {
            Some(last) if last.bold == run.bold && last.italic == run.italic => {
                last.text.push_str(&run.text)
            }
            _ => merged.push(run.clone()),
        }
    }
    let mut out = String::new();
    for run in merged {
        let marks = match (run.bold, run.italic) {
            (true, true) => "***",
            (true, false) => "**",
            (false, true) => "*",
            (false, false) => "",
        };
        let core = run.text.trim();
        if marks.is_empty() || core.is_empty() {
            out.push_str(&run.text);
            continue;
        }
        let start = run.text.len() - run.text.trim_start().len();
        let end = start + core.len();
        out.push_str(&run.text[..start]);
        out.push_str(marks);
        out.push_str(core);
        out.push_str(marks);
        out.push_str(&run.text[end..]);
    }
    out.trim().to_owned()
}

/// A markdown pipe table of `rows`; the first row is the header.
pub(super) fn table(rows: &[Vec<String>]) -> String {
    let width = rows.iter().map(Vec::len).max().unwrap_or(0);
    if width == 0 {
        return String::new();
    }
    let mut out = String::new();
    for (i, row) in rows.iter().enumerate() {
        out.push('|');
        for c in 0..width {
            let cell = row.get(c).map(String::as_str).unwrap_or("");
            out.push(' ');
            out.push_str(&table_cell(cell));
            out.push_str(" |");
        }
        out.push('\n');
        if i == 0 {
            out.push('|');
            for _ in 0..width {
                out.push_str(" --- |");
            }
            out.push('\n');
        }
    }
    out.trim_end().to_owned()
}

/// `text` safe inside a pipe-table cell.
pub(super) fn table_cell(text: &str) -> String {
    text.trim().replace('|', "\\|").replace('\n', "<br>")
}

/// Markdown assembled block by block; consecutive list items stay one list.
#[derive(Default)]
pub(super) struct Markdown {
    out: String,
    /// Depth and kind (numbered or not) of the previous block when it was
    /// a list item.
    item: Option<(usize, bool)>,
    /// Items written so far at each depth of the current list.
    counts: Vec<u64>,
}

impl Markdown {
    pub(super) fn block(&mut self, text: &str) {
        if text.trim().is_empty() {
            return;
        }
        self.separate(false);
        self.out.push_str(text.trim_end());
        self.item = None;
    }

    pub(super) fn heading(&mut self, level: u8, text: &str) {
        let text = text.replace('\n', " ");
        if text.trim().is_empty() {
            return;
        }
        let level = level.clamp(1, 6) as usize;
        self.block(&format!("{} {}", "#".repeat(level), text.trim()));
    }

    pub(super) fn item(&mut self, depth: usize, ordered: bool, text: &str) {
        if text.trim().is_empty() {
            return;
        }
        // A list can only nest one level deeper than its previous item, and
        // an indented first item would read as a code block.
        let depth = match self.item {
            Some((prev, _)) => depth.min(prev + 1),
            None => 0,
        };
        // Switching between bullets and numbers at one depth starts a new list.
        let same_list = self
            .item
            .is_some_and(|(prev, prev_ordered)| prev != depth || prev_ordered == ordered);
        self.separate(same_list);
        if !same_list {
            self.counts.clear();
        }
        self.counts.resize(depth + 1, 0);
        self.counts[depth] += 1;
        let marker = if ordered {
            format!("{}.", self.counts[depth])
        } else {
            "-".to_owned()
        };
        let text = text.trim().replace('\n', " ");
        self.out
            .push_str(&format!("{}{marker} {text}", "    ".repeat(depth)));
        self.item = Some((depth, ordered));
    }

    fn separate(&mut self, tight: bool) {
        if !self.out.is_empty() {
            self.out.push_str(if tight { "\n" } else { "\n\n" });
        }
    }

    pub(super) fn finish(self) -> String {
        self.out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_flattens_lists_and_keeps_styles() {
        let blocks = parse("# Title\n\nSome **bold** text.\n\n1. one\n2. two\n   - nested\n\n| a | b |\n|---|---|\n| 1 | 2 |\n");
        assert!(matches!(&blocks[0], Block::Heading { level: 1, .. }));
        let Block::Paragraph(spans) = &blocks[1] else {
            panic!("{blocks:?}")
        };
        assert!(spans.iter().any(|s| s.bold && s.text == "bold"));
        assert!(matches!(
            &blocks[2],
            Block::Item {
                depth: 0,
                marker: Marker::Number(1),
                ..
            }
        ));
        assert!(matches!(
            &blocks[3],
            Block::Item {
                depth: 0,
                marker: Marker::Number(2),
                ..
            }
        ));
        assert!(matches!(
            &blocks[4],
            Block::Item {
                depth: 1,
                marker: Marker::Bullet,
                ..
            }
        ));
        let Block::Table(rows) = &blocks[5] else {
            panic!("{blocks:?}")
        };
        assert_eq!(rows.len(), 2);
        assert_eq!(plain(&rows[1][1]), "2");
    }

    #[test]
    fn links_keep_their_destination() {
        let blocks = parse("See [the docs](https://example.com).");
        let Block::Paragraph(spans) = &blocks[0] else {
            panic!()
        };
        assert_eq!(plain(spans), "See the docs (https://example.com).");
    }

    #[test]
    fn inline_keeps_whitespace_outside_markers() {
        let runs = [
            Run {
                text: "a ".into(),
                ..Default::default()
            },
            Run {
                text: "bold ".into(),
                bold: true,
                italic: false,
            },
            Run {
                text: "b".into(),
                ..Default::default()
            },
        ];
        assert_eq!(inline(&runs), "a **bold** b");
    }

    #[test]
    fn markdown_keeps_lists_tight_and_nests_safely() {
        let mut md = Markdown::default();
        md.block("Intro");
        md.item(2, false, "first");
        md.item(1, true, "second");
        md.block("After");
        assert_eq!(md.finish(), "Intro\n\n- first\n    1. second\n\nAfter");
    }
}
