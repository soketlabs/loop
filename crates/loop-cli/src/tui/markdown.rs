//! Lightweight markdown → ratatui lines (terminal-safe).

use pulldown_cmark::{CodeBlockKind, Event, Options, Parser, Tag, TagEnd};
use ratatui::style::Modifier;
use ratatui::text::{Line, Span};
use unicode_width::UnicodeWidthStr;

use crate::theme::Theme;

use super::highlight::{self, HighlightState};

/// Render markdown text to styled lines suitable for a terminal.
pub fn render_lines(text: &str, theme: &Theme) -> Vec<Line<'static>> {
    let mut options = Options::empty();
    options.insert(Options::ENABLE_STRIKETHROUGH);
    options.insert(Options::ENABLE_TABLES);
    options.insert(Options::ENABLE_TASKLISTS);
    let parser = Parser::new_ext(text, options);

    let mut lines: Vec<Line<'static>> = Vec::new();
    let mut current: Vec<Span<'static>> = Vec::new();
    let mut style_stack = vec![theme.style("text")];
    let mut in_code_block = false;
    let mut code_highlight: Option<HighlightState> = None;
    let mut list_depth: usize = 0;
    let mut table_row: Vec<String> = Vec::new();

    let flush = |current: &mut Vec<Span<'static>>, lines: &mut Vec<Line<'static>>| {
        if current.is_empty() {
            // Avoid stacking blank lines from consecutive block ends.
            if lines.last().is_some_and(|l| l.spans.is_empty() || line_is_blank(l)) {
                return;
            }
            lines.push(Line::from(""));
        } else {
            lines.push(Line::from(std::mem::take(current)));
        }
    };

    for event in parser {
        match event {
            Event::Start(Tag::Heading { .. }) => {
                flush(&mut current, &mut lines);
                style_stack.push(
                    theme
                        .style("mdHeading")
                        .add_modifier(Modifier::BOLD),
                );
            }
            Event::End(TagEnd::Heading(_)) => {
                flush(&mut current, &mut lines);
                style_stack.pop();
            }
            Event::Start(Tag::CodeBlock(kind)) => {
                in_code_block = true;
                flush(&mut current, &mut lines);
                style_stack.push(theme.style("mdCodeBlock"));
                let lang = match &kind {
                    CodeBlockKind::Fenced(lang) => {
                        let lang = lang.to_string();
                        if !lang.is_empty() {
                            current.push(Span::styled(
                                format!("┌─ {lang}"),
                                theme.style("mdCodeBlockBorder"),
                            ));
                            flush(&mut current, &mut lines);
                        }
                        if lang.is_empty() {
                            None
                        } else {
                            Some(lang)
                        }
                    }
                    CodeBlockKind::Indented => None,
                };
                code_highlight = lang
                    .as_deref()
                    .and_then(HighlightState::from_language);
            }
            Event::End(TagEnd::CodeBlock) => {
                in_code_block = false;
                code_highlight = None;
                flush(&mut current, &mut lines);
                style_stack.pop();
            }
            Event::Start(Tag::Emphasis) => {
                style_stack.push(theme.style("text").add_modifier(Modifier::ITALIC));
            }
            Event::End(TagEnd::Emphasis) => {
                style_stack.pop();
            }
            Event::Start(Tag::Strong) => {
                style_stack.push(theme.style("text").add_modifier(Modifier::BOLD));
            }
            Event::End(TagEnd::Strong) => {
                style_stack.pop();
            }
            Event::Start(Tag::Strikethrough) => {
                style_stack.push(theme.style("dim").add_modifier(Modifier::CROSSED_OUT));
            }
            Event::End(TagEnd::Strikethrough) => {
                style_stack.pop();
            }
            Event::Start(Tag::Link { dest_url, .. }) => {
                style_stack.push(theme.style("mdLink"));
                let _ = dest_url;
            }
            Event::End(TagEnd::Link) => {
                style_stack.pop();
            }
            Event::Start(Tag::List(_)) => {
                list_depth += 1;
            }
            Event::End(TagEnd::List(_)) => {
                list_depth = list_depth.saturating_sub(1);
                flush(&mut current, &mut lines);
            }
            Event::Start(Tag::Item) => {
                let indent = "  ".repeat(list_depth.saturating_sub(1));
                current.push(Span::styled(
                    format!("{indent}• "),
                    theme.style("mdListBullet"),
                ));
            }
            Event::End(TagEnd::Item) => {
                flush(&mut current, &mut lines);
            }
            Event::Start(Tag::Paragraph) => {}
            Event::End(TagEnd::Paragraph) => {
                flush(&mut current, &mut lines);
            }
            Event::Start(Tag::Table(_)) => {
                flush(&mut current, &mut lines);
            }
            Event::End(TagEnd::Table) => {
                flush(&mut current, &mut lines);
            }
            Event::Start(Tag::TableHead) | Event::Start(Tag::TableRow) => {
                table_row.clear();
            }
            Event::End(TagEnd::TableHead) => {
                if !table_row.is_empty() {
                    let row = table_row.join(" │ ");
                    current.push(Span::styled(row, theme.style("mdHeading")));
                    flush(&mut current, &mut lines);
                    // Separator under header.
                    current.push(Span::styled("───", theme.style("mdHr")));
                    flush(&mut current, &mut lines);
                }
                table_row.clear();
            }
            Event::End(TagEnd::TableRow) => {
                if !table_row.is_empty() {
                    let row = table_row.join(" │ ");
                    current.push(Span::styled(sanitize_terminal_text(&row), theme.style("text")));
                    flush(&mut current, &mut lines);
                }
                table_row.clear();
            }
            Event::Start(Tag::TableCell) => {
                current.clear();
            }
            Event::End(TagEnd::TableCell) => {
                let cell: String = current.drain(..).map(|s| s.content.to_string()).collect();
                table_row.push(cell.trim().to_string());
            }
            Event::Start(Tag::BlockQuote(_)) => {
                style_stack.push(theme.style("mdQuote"));
                current.push(Span::styled("│ ", theme.style("mdQuoteBorder")));
            }
            Event::End(TagEnd::BlockQuote(_)) => {
                flush(&mut current, &mut lines);
                style_stack.pop();
            }
            Event::Text(t) => {
                let style = *style_stack.last().unwrap_or(&theme.style("text"));
                let cleaned = sanitize_terminal_text(&t);
                if in_code_block {
                    let fallback = theme.style("mdCodeBlock");
                    for line in cleaned.split('\n') {
                        let mut row = vec![Span::styled("  ".to_string(), fallback)];
                        if let Some(state) = code_highlight.as_mut() {
                            row.extend(highlight::highlight_line_stateful(
                                line, state, theme, fallback,
                            ));
                        } else {
                            row.push(Span::styled(line.to_string(), style));
                        }
                        current = row;
                        flush(&mut current, &mut lines);
                    }
                } else {
                    // Includes table-cell text (harvested on TableCell end from `current`).
                    let mut parts = cleaned.split('\n').peekable();
                    while let Some(part) = parts.next() {
                        if !part.is_empty() {
                            current.push(Span::styled(part.to_string(), style));
                        }
                        if parts.peek().is_some() {
                            flush(&mut current, &mut lines);
                        }
                    }
                }
            }
            Event::Code(t) => {
                current.push(Span::styled(
                    sanitize_terminal_text(&t),
                    theme.style("mdCode"),
                ));
            }
            Event::SoftBreak => {
                current.push(Span::styled(" ", theme.style("text")));
            }
            Event::HardBreak => {
                flush(&mut current, &mut lines);
            }
            Event::Rule => {
                flush(&mut current, &mut lines);
                current.push(Span::styled("────────", theme.style("mdHr")));
                flush(&mut current, &mut lines);
            }
            Event::Html(html) | Event::InlineHtml(html) => {
                // Strip tags; keep text-ish content so HTML in READMEs doesn't break the TUI.
                let stripped = strip_html_tags(&html);
                if !stripped.is_empty() {
                    let style = *style_stack.last().unwrap_or(&theme.style("text"));
                    current.push(Span::styled(stripped, style));
                }
            }
            _ => {}
        }
    }
    if !current.is_empty() {
        flush(&mut current, &mut lines);
    }
    if lines.is_empty() {
        for l in text.lines() {
            lines.push(Line::from(Span::styled(
                sanitize_terminal_text(l),
                theme.style("text"),
            )));
        }
    }
    // Drop a trailing blank line for tighter layout.
    while lines
        .last()
        .is_some_and(|l| l.spans.is_empty() || line_is_blank(l))
    {
        lines.pop();
    }
    lines
}

fn line_is_blank(line: &Line<'_>) -> bool {
    line.spans.iter().all(|s| s.content.trim().is_empty())
}

/// Remove control chars / ANSI that can corrupt the terminal.
fn sanitize_terminal_text(s: &str) -> String {
    s.chars()
        .filter(|c| {
            !c.is_control()
                || *c == '\n'
                || *c == '\t'
        })
        .map(|c| if c == '\t' { ' ' } else { c })
        .collect()
}

fn strip_html_tags(s: &str) -> String {
    let mut out = String::new();
    let mut in_tag = false;
    for c in s.chars() {
        match c {
            '<' => in_tag = true,
            '>' => in_tag = false,
            _ if !in_tag => out.push(c),
            _ => {}
        }
    }
    sanitize_terminal_text(out.trim())
}

/// Soft-wrap already-rendered lines to a terminal width (span-safe).
pub fn wrap_rendered_lines(lines: Vec<Line<'static>>, width: usize) -> Vec<Line<'static>> {
    if width < 8 {
        return lines;
    }
    let mut out = Vec::new();
    for line in lines {
        if line.width() <= width {
            out.push(line);
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
                // Merge into last span when style matches to keep span count down.
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
