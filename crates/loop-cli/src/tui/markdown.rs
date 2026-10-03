//! Markdown → ratatui lines via termimad (tables, headers, lists), with
//! syntect highlighting spliced back in for fenced code blocks.

use ansi_to_tui::IntoText;
use pulldown_cmark::{CodeBlockKind, Event, Options, Parser, Tag, TagEnd};
use ratatui::style::Color as RatColor;
use ratatui::style::Modifier;
use ratatui::text::{Line, Span};
use termimad::crossterm::style::Attribute as CtAttribute;
use termimad::crossterm::style::Color as CtColor;
use termimad::minimad::Alignment;
use termimad::{CompoundStyle, MadSkin};
use unicode_width::UnicodeWidthStr;

use crate::theme::Theme;

use super::highlight::{self, HighlightState};

/// Render markdown text to styled lines suitable for a terminal.
pub fn render_lines(text: &str, theme: &Theme) -> Vec<Line<'static>> {
    let mut out: Vec<Line<'static>> = Vec::new();

    for segment in split_segments(text) {
        match segment {
            Segment::Markdown(md) => {
                out.extend(render_termimad(&md, theme));
            }
            Segment::Code { lang, body } => {
                out.extend(render_code_block(&lang, &body, theme));
            }
        }
    }

    // Drop trailing blank lines for tighter layout.
    while out.last().is_some_and(line_is_blank) {
        out.pop();
    }
    if out.is_empty() && !text.trim().is_empty() {
        for l in text.lines() {
            out.push(Line::from(Span::styled(
                sanitize_terminal_text(l),
                theme.style("text"),
            )));
        }
    }
    out
}

/// A top-level slice of the message: plain markdown or a fenced code block.
enum Segment {
    Markdown(String),
    Code { lang: String, body: String },
}

/// Width termimad uses to lay out tables (and stretch horizontal rules,
/// which we cap back down to `MAX_RULE_WIDTH`).
const TABLE_LAYOUT_WIDTH: usize = 10_000;
const MAX_RULE_WIDTH: usize = 60;

/// Split source into alternating markdown / fenced-code segments. During
/// streaming a trailing unterminated fence still yields a Code segment, so
/// partial code streams with highlighting and no table reflow.
fn split_segments(text: &str) -> Vec<Segment> {
    let mut options = Options::empty();
    options.insert(Options::ENABLE_STRIKETHROUGH);
    options.insert(Options::ENABLE_TABLES);
    options.insert(Options::ENABLE_TASKLISTS);
    let parser = Parser::new_ext(text, options).into_offset_iter();

    let mut segments = Vec::new();
    let mut cursor = 0usize;
    let mut code_start: Option<(usize, String)> = None;

    for (event, range) in parser {
        match event {
            Event::Start(Tag::CodeBlock(CodeBlockKind::Fenced(lang))) => {
                if code_start.is_none() {
                    code_start = Some((range.start, lang.to_string()));
                }
            }
            Event::End(TagEnd::CodeBlock) => {
                if let Some((start, lang)) = code_start.take() {
                    if start > cursor {
                        segments.push(Segment::Markdown(text[cursor..start].to_string()));
                    }
                    segments.push(Segment::Code {
                        lang,
                        body: text[start..range.end].to_string(),
                    });
                    cursor = range.end;
                }
            }
            _ => {}
        }
    }

    if let Some((start, lang)) = code_start {
        // Unterminated fence (still streaming): rest of the text is code.
        if start > cursor {
            segments.push(Segment::Markdown(text[cursor..start].to_string()));
        }
        segments.push(Segment::Code {
            lang,
            body: text[start..].to_string(),
        });
    } else if cursor < text.len() {
        segments.push(Segment::Markdown(text[cursor..].to_string()));
    }
    segments
}

/// Strip the fence markers from a fenced code segment's raw source.
fn code_body(raw: &str) -> String {
    let mut body = String::new();
    for (i, line) in raw.lines().enumerate() {
        let trimmed = line.trim_start();
        if i == 0 && trimmed.starts_with("```") || trimmed.starts_with("~~~") {
            continue; // opening fence (possibly unterminated closer too)
        }
        if trimmed.starts_with("```") || trimmed.starts_with("~~~") {
            continue; // closing fence
        }
        body.push_str(line);
        body.push('\n');
    }
    body
}

/// Render a markdown segment with termimad → ANSI → ratatui lines.
fn render_termimad(md: &str, theme: &Theme) -> Vec<Line<'static>> {
    let skin = theme_skin(theme);
    // Generous width: tables get full layout, paragraph wrapping is left to
    // `wrap_rendered_lines` which knows the live terminal width.
    let fmt = skin.text(md, Some(TABLE_LAYOUT_WIDTH));
    let ansi = fmt.to_string();
    match ansi.into_text() {
        Ok(text) => {
            let mut lines: Vec<Line<'static>> = text
                .lines
                .into_iter()
                .map(|mut line| {
                    // Cap runaway horizontal rules (pure ─ runs) that termimad
                    // stretches to the full layout width. Table rules contain
                    // junctions (├ ┼ ┤) and are left alone.
                    if line.width() > MAX_RULE_WIDTH
                        && line
                            .spans
                            .iter()
                            .all(|s| s.content.chars().all(|c| c == '─' || c == ' '))
                        && line.spans.iter().any(|s| s.content.contains('─'))
                    {
                        line.spans = vec![Span::styled(
                            "─".repeat(MAX_RULE_WIDTH),
                            line.spans.first().map(|s| s.style).unwrap_or_default(),
                        )];
                    }
                    if line.spans.is_empty() {
                        line.spans.push(Span::raw(""));
                    }
                    line
                })
                .collect();
            // Bold the table header row (the row right above a ├─ rule).
            for i in 0..lines.len() {
                let is_rule = |l: &Line<'_>| {
                    let s: String = l.spans.iter().map(|sp| sp.content.as_ref()).collect();
                    s.starts_with('├') || s.starts_with('╞') || s.starts_with('┝')
                };
                if i + 1 < lines.len() && is_rule(&lines[i + 1]) && !is_rule(&lines[i]) {
                    let row: String = lines[i].spans.iter().map(|s| s.content.as_ref()).collect();
                    if row.contains('│') {
                        for span in &mut lines[i].spans {
                            span.style = span.style.add_modifier(Modifier::BOLD);
                        }
                    }
                }
            }
            lines
        }
        Err(_) => md
            .lines()
            .map(|l| Line::from(Span::styled(sanitize_terminal_text(l), theme.style("text"))))
            .collect(),
    }
}

/// Fenced code block: syntect-highlighted lines with the existing border look.
fn render_code_block(lang: &str, body_raw: &str, theme: &Theme) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    if !lang.is_empty() {
        lines.push(Line::from(Span::styled(
            format!("┌─ {lang}"),
            theme.style("mdCodeBlockBorder"),
        )));
    }
    let fallback = theme.style("mdCodeBlock");
    let body = code_body(body_raw);
    let mut state = HighlightState::from_language(lang);
    for line in body.lines() {
        let mut row = vec![Span::styled("  ".to_string(), fallback)];
        if let Some(st) = state.as_mut() {
            row.extend(highlight::highlight_line_stateful(
                line, st, theme, fallback,
            ));
        } else {
            row.push(Span::styled(line.to_string(), fallback));
        }
        lines.push(Line::from(row));
    }
    lines
}

/// Map the app theme onto a termimad skin.
fn theme_skin(theme: &Theme) -> MadSkin {
    let ct = |key: &str| to_ct_color(theme.get(key));

    let mut skin = MadSkin::default();
    skin.paragraph.compound_style = CompoundStyle::with_fg(ct("text"));
    for h in &mut skin.headers {
        h.compound_style = CompoundStyle::with_fg(ct("mdHeading"));
        h.compound_style
            .object_style
            .attributes
            .set(CtAttribute::Bold);
        h.align = Alignment::Left;
        h.left_margin = 0;
    }
    skin.bold = CompoundStyle::with_fg(ct("text"));
    skin.bold.object_style.attributes.set(CtAttribute::Bold);
    skin.italic = CompoundStyle::with_fg(ct("text"));
    skin.italic.object_style.attributes.set(CtAttribute::Italic);
    skin.strikeout = CompoundStyle::with_fg(ct("dim"));
    skin.strikeout
        .object_style
        .attributes
        .set(CtAttribute::CrossedOut);
    skin.inline_code = CompoundStyle::with_fg(ct("mdCode"));
    skin.code_block.compound_style = CompoundStyle::with_fg(ct("mdCodeBlock"));
    skin.table = termimad::LineStyle {
        compound_style: CompoundStyle::with_fg(ct("text")),
        align: Alignment::Left,
        left_margin: 1,
        right_margin: 0,
    };
    skin.table_border_chars = termimad::ROUNDED_TABLE_BORDER_CHARS;
    skin.bullet = termimad::StyledChar::from_fg_char(ct("mdListBullet"), '•');
    skin.quote_mark = termimad::StyledChar::from_fg_char(ct("mdQuoteBorder"), '▌');
    skin.horizontal_rule = termimad::StyledChar::from_fg_char(ct("mdHr"), '─');
    skin.ellipsis = CompoundStyle::with_fg(ct("dim"));
    skin
}

fn to_ct_color(c: RatColor) -> CtColor {
    match c {
        RatColor::Reset => CtColor::Reset,
        RatColor::Black => CtColor::Black,
        RatColor::Red => CtColor::DarkRed,
        RatColor::Green => CtColor::DarkGreen,
        RatColor::Yellow => CtColor::DarkYellow,
        RatColor::Blue => CtColor::DarkBlue,
        RatColor::Magenta => CtColor::DarkMagenta,
        RatColor::Cyan => CtColor::DarkCyan,
        RatColor::Gray => CtColor::Grey,
        RatColor::DarkGray => CtColor::DarkGrey,
        RatColor::LightRed => CtColor::Red,
        RatColor::LightGreen => CtColor::Green,
        RatColor::LightYellow => CtColor::Yellow,
        RatColor::LightBlue => CtColor::Blue,
        RatColor::LightMagenta => CtColor::Magenta,
        RatColor::LightCyan => CtColor::Cyan,
        RatColor::White => CtColor::White,
        RatColor::Rgb(r, g, b) => CtColor::Rgb { r, g, b },
        RatColor::Indexed(i) => CtColor::AnsiValue(i),
    }
}

fn line_is_blank(line: &Line<'_>) -> bool {
    line.spans.iter().all(|s| s.content.trim().is_empty())
}

/// Remove control chars / ANSI that can corrupt the terminal.
fn sanitize_terminal_text(s: &str) -> String {
    s.chars()
        .filter(|c| !c.is_control() || *c == '\n' || *c == '\t')
        .map(|c| if c == '\t' { ' ' } else { c })
        .collect()
}

/// Soft-wrap already-rendered lines to a terminal width (span-safe).
/// Table lines (start with │ or a table-rule junction) are truncated rather
/// than wrapped so columns stay aligned; everything else wraps normally.
pub fn wrap_rendered_lines(lines: Vec<Line<'static>>, width: usize) -> Vec<Line<'static>> {
    if width < 8 {
        return lines;
    }
    let is_table_line = |line: &Line<'static>| {
        let first = line
            .spans
            .iter()
            .find(|s| !s.content.trim().is_empty())
            .map(|s| s.content.trim_start());
        match first {
            Some(f) => {
                f.starts_with('│')
                    || f.starts_with('├')
                    || f.starts_with('┌')
                    || f.starts_with('└')
                    || f.starts_with('╭')
                    || f.starts_with('╰')
            }
            None => false,
        }
    };
    let mut out = Vec::new();
    for line in lines {
        if line.width() <= width {
            out.push(line);
            continue;
        }
        if is_table_line(&line) {
            // Likely a table row / rule: truncate to keep columns intact.
            let mut row: Vec<Span<'static>> = Vec::new();
            let mut row_w = 0usize;
            for span in line.spans {
                let style = span.style;
                for ch in span.content.chars() {
                    let cw = UnicodeWidthStr::width(ch.to_string().as_str()).max(1);
                    if row_w + cw > width {
                        break;
                    }
                    match row.last_mut() {
                        Some(last) if last.style == style => {
                            last.content.to_mut().push(ch);
                        }
                        _ => row.push(Span::styled(ch.to_string(), style)),
                    }
                    row_w += cw;
                }
                if row_w >= width {
                    break;
                }
            }
            out.push(Line::from(row));
            continue;
        }
        let mut row: Vec<Span<'static>> = Vec::new();
        let mut row_w = 0usize;
        for span in line.spans {
            let style = span.style;
            for ch in span.content.chars() {
                let cw = UnicodeWidthStr::width(ch.to_string().as_str()).max(1);
                if row_w + cw > width && !row.is_empty() {
                    out.push(Line::from(std::mem::take(&mut row)));
                    row_w = 0;
                }
                match row.last_mut() {
                    Some(last) if last.style == style => {
                        last.content.to_mut().push(ch);
                    }
                    _ => row.push(Span::styled(ch.to_string(), style)),
                }
                row_w += cw;
            }
        }
        if !row.is_empty() {
            out.push(Line::from(row));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_theme() -> Theme {
        Theme::dark()
    }

    fn plain(lines: &[Line<'static>]) -> String {
        lines
            .iter()
            .map(|l| {
                l.spans
                    .iter()
                    .map(|s| s.content.as_ref())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn renders_aligned_table() {
        let md = "before\n\n| Name | Value |\n|------|-------|\n| foo  | 1     |\n| longer-cell | 22  |\n\nafter\n";
        let lines = render_lines(md, &test_theme());
        let text = plain(&lines);
        assert!(text.contains("before"), "{text}");
        assert!(text.contains("after"), "{text}");
        // Table rows should have aligned │ separators at equal positions.
        let table_rows: Vec<&str> = text
            .lines()
            .filter(|l| l.contains('│') && (l.contains("foo") || l.contains("longer-cell")))
            .collect();
        assert_eq!(table_rows.len(), 2, "{text}");
        let pos: Vec<usize> = table_rows.iter().map(|r| r.find('│').unwrap()).collect();
        assert_eq!(pos[0], pos[1], "columns should align:\n{text}");
    }

    #[test]
    fn code_block_keeps_highlight_path() {
        let md = "text\n\n```rust\nfn main() {}\n```\n\nmore\n";
        let lines = render_lines(md, &test_theme());
        let text = plain(&lines);
        assert!(text.contains("┌─ rust"), "{text}");
        assert!(text.contains("fn main() {}"), "{text}");
        assert!(text.contains("more"), "{text}");
    }

    #[test]
    fn unterminated_code_block_streams() {
        let md = "intro\n\n```python\nprint('hi')";
        let lines = render_lines(md, &test_theme());
        let text = plain(&lines);
        assert!(text.contains("print('hi')"), "{text}");
        assert!(text.contains("intro"), "{text}");
    }

    #[test]
    fn inline_formatting_survives() {
        let md = "some **bold** and *italic* and `code` text";
        let lines = render_lines(md, &test_theme());
        let text = plain(&lines);
        assert!(text.contains("bold"), "{text}");
        assert!(text.contains("italic"), "{text}");
        assert!(text.contains("code"), "{text}");
    }

    #[test]
    fn blank_tail_trimmed() {
        let lines = render_lines("hello\n\n\n", &test_theme());
        assert_eq!(lines.len(), 1);
    }

    #[test]
    fn wrap_truncates_tables_but_wraps_text() {
        let theme = test_theme();
        let md = "| a very long cell | another long cell |\n|---|---|\n| x | y |\n";
        let lines = render_lines(md, &theme);
        let wrapped = wrap_rendered_lines(lines, 20);
        for l in &wrapped {
            assert!(l.width() <= 20, "line too wide: {:?}", plain(&[l.clone()]));
        }
        // Long text lines wrap to multiple rows.
        let md2 = "word ".repeat(30);
        let lines2 = render_lines(&md2, &theme);
        let wrapped2 = wrap_rendered_lines(lines2, 20);
        assert!(wrapped2.len() > 1);
    }
}

#[cfg(test)]
mod stream_sim {
    use super::*;

    /// Simulate the TUI streaming path: the same text with growing prefixes,
    /// re-rendered each "frame". Table should be rough mid-stream, aligned at end.
    #[test]
    fn streaming_table_progression() {
        let full =
            "intro\n\n| Name | Age |\n|------|-----|\n| Alice | 30 |\n| Bob | 25 |\n\ndone\n";
        let theme = Theme::dark();

        // Mid-stream: only the header + delimiter have arrived.
        let mid = &full[..full.find("| Alice").unwrap()];
        let lines_mid = render_lines(mid, &theme);
        let text_mid = lines_mid
            .iter()
            .map(|l| {
                l.spans
                    .iter()
                    .map(|s| s.content.as_ref())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n");
        eprintln!("=== MID-STREAM ===\n{text_mid}\n");

        // Full render.
        let lines_full = render_lines(full, &theme);
        let text_full = lines_full
            .iter()
            .map(|l| {
                l.spans
                    .iter()
                    .map(|s| s.content.as_ref())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n");
        eprintln!("=== FULL ===\n{text_full}\n");

        // Full render: all table rows have aligned │ separators.
        let rows: Vec<&str> = text_full
            .lines()
            .filter(|l| l.contains('│') && (l.contains("Alice") || l.contains("Bob")))
            .collect();
        assert_eq!(rows.len(), 2, "{text_full}");
        assert_eq!(
            rows[0].find('│'),
            rows[1].find('│'),
            "columns aligned in final render:\n{text_full}"
        );
        assert!(text_full.contains('├'), "has a table rule:\n{text_full}");
    }
}
