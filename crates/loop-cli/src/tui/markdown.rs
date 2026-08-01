//! Lightweight markdown → ratatui lines (restrained styling).

use pulldown_cmark::{CodeBlockKind, Event, Options, Parser, Tag, TagEnd};
use ratatui::text::{Line, Span};

use crate::theme::Theme;

/// Render markdown text to styled lines.
pub fn render_lines(text: &str, theme: &Theme) -> Vec<Line<'static>> {
    let mut options = Options::empty();
    options.insert(Options::ENABLE_STRIKETHROUGH);
    let parser = Parser::new_ext(text, options);

    let mut lines: Vec<Line<'static>> = Vec::new();
    let mut current: Vec<Span<'static>> = Vec::new();
    let mut style_stack = vec![theme.style("text")];
    let mut in_code_block = false;

    let flush = |current: &mut Vec<Span<'static>>, lines: &mut Vec<Line<'static>>| {
        if current.is_empty() {
            lines.push(Line::from(""));
        } else {
            lines.push(Line::from(std::mem::take(current)));
        }
    };

    for event in parser {
        match event {
            Event::Start(Tag::Heading { .. }) => {
                style_stack.push(theme.style("mdHeading"));
            }
            Event::End(TagEnd::Heading(_)) => {
                flush(&mut current, &mut lines);
                style_stack.pop();
            }
            Event::Start(Tag::CodeBlock(kind)) => {
                in_code_block = true;
                style_stack.push(theme.style("mdCodeBlock"));
                let lang = match kind {
                    CodeBlockKind::Fenced(l) => l.to_string(),
                    CodeBlockKind::Indented => String::new(),
                };
                if !lang.is_empty() {
                    current.push(Span::styled(format!("```{lang}"), theme.style("mdCodeBlockBorder")));
                    flush(&mut current, &mut lines);
                }
            }
            Event::End(TagEnd::CodeBlock) => {
                in_code_block = false;
                flush(&mut current, &mut lines);
                style_stack.pop();
            }
            Event::Start(Tag::Emphasis) => style_stack.push(theme.style("text")),
            Event::End(TagEnd::Emphasis) => {
                style_stack.pop();
            }
            Event::Start(Tag::Strong) => {
                use ratatui::style::Modifier;
                style_stack.push(theme.style("text").add_modifier(Modifier::BOLD));
            }
            Event::End(TagEnd::Strong) => {
                style_stack.pop();
            }
            Event::Start(Tag::Link { dest_url, .. }) => {
                style_stack.push(theme.style("mdLink"));
                let _ = dest_url;
            }
            Event::End(TagEnd::Link) => {
                style_stack.pop();
            }
            Event::Start(Tag::Item) => {
                current.push(Span::styled("• ", theme.style("mdListBullet")));
            }
            Event::End(TagEnd::Item) => {
                flush(&mut current, &mut lines);
            }
            Event::End(TagEnd::Paragraph) => {
                flush(&mut current, &mut lines);
            }
            Event::Text(t) => {
                let style = *style_stack.last().unwrap_or(&theme.style("text"));
                if in_code_block {
                    for line in t.lines() {
                        current.push(Span::styled(line.to_string(), style));
                        flush(&mut current, &mut lines);
                    }
                    if t.ends_with('\n') {
                        // already flushed
                    }
                } else {
                    current.push(Span::styled(t.to_string(), style));
                }
            }
            Event::Code(t) => {
                current.push(Span::styled(t.to_string(), theme.style("mdCode")));
            }
            Event::SoftBreak | Event::HardBreak => {
                flush(&mut current, &mut lines);
            }
            Event::Rule => {
                flush(&mut current, &mut lines);
                current.push(Span::styled("───", theme.style("mdHr")));
                flush(&mut current, &mut lines);
            }
            _ => {}
        }
    }
    if !current.is_empty() {
        flush(&mut current, &mut lines);
    }
    if lines.is_empty() {
        // Fallback: plain lines
        for l in text.lines() {
            lines.push(Line::from(Span::styled(l.to_string(), theme.style("text"))));
        }
    }
    lines
}
