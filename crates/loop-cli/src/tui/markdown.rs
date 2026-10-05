//! Markdown → ratatui lines via termimad (tables, headers, lists), with
//! syntect highlighting spliced back in for fenced code blocks.

use ansi_to_tui::IntoText;
use pulldown_cmark::{CodeBlockKind, Event, Options, Parser, Tag, TagEnd};
use ratatui::style::Color as RatColor;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use termimad::crossterm::style::Attribute as CtAttribute;
use termimad::crossterm::style::Color as CtColor;
use termimad::minimad::Alignment;
use termimad::{CompoundStyle, MadSkin};
use unicode_width::UnicodeWidthStr;

use crate::theme::Theme;

use super::highlight::{self, HighlightState};

/// Render markdown text to styled lines suitable for a terminal.
///
/// `width` is the available terminal columns. Tables are laid out with a
/// balanced column fitter (termimad's starves long trailing columns); other
/// markdown is rendered via termimad at this width.
pub fn render_lines(text: &str, theme: &Theme, width: usize) -> Vec<Line<'static>> {
    let mut out: Vec<Line<'static>> = Vec::new();
    let width = width.max(8);

    for segment in split_segments(text) {
        match segment {
            Segment::Markdown(md) => {
                for chunk in split_table_chunks(&md) {
                    match chunk {
                        TableChunk::Prose(prose) => {
                            if !prose.trim().is_empty() {
                                out.extend(render_termimad(&prose, theme, width));
                            }
                        }
                        TableChunk::Table(block) => {
                            out.extend(render_markdown_table(&block, theme, width));
                        }
                    }
                }
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

/// Cap termimad horizontal rules (it stretches them to the layout width).
const MAX_RULE_WIDTH: usize = 60;
/// Floor for a wrapped table column so cells stay readable.
const MIN_TABLE_COL_WIDTH: usize = 8;

/// Slice of a markdown segment: prose (termimad) or a GFM table block.
enum TableChunk {
    Prose(String),
    Table(String),
}

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

/// Split a markdown segment into prose and GFM table blocks so tables can use
/// our balanced column fitter instead of termimad's (which starves long
/// trailing columns once earlier columns claim their natural max width).
fn split_table_chunks(md: &str) -> Vec<TableChunk> {
    let lines: Vec<&str> = md.split_inclusive('\n').collect();
    let mut out = Vec::new();
    let mut i = 0usize;
    let mut prose = String::new();

    let push_prose = |prose: &mut String, out: &mut Vec<TableChunk>| {
        if !prose.is_empty() {
            out.push(TableChunk::Prose(std::mem::take(prose)));
        }
    };

    while i < lines.len() {
        let line = lines[i];
        let trimmed = line.trim_end_matches(['\n', '\r']);
        // GFM table: a row line followed by a separator line.
        if is_table_row_line(trimmed)
            && i + 1 < lines.len()
            && is_table_separator_line(lines[i + 1].trim_end_matches(['\n', '\r']))
        {
            push_prose(&mut prose, &mut out);
            let start = i;
            i += 2; // header + separator
            while i < lines.len() && is_table_row_line(lines[i].trim_end_matches(['\n', '\r'])) {
                i += 1;
            }
            let mut block = String::new();
            for l in &lines[start..i] {
                block.push_str(l);
            }
            out.push(TableChunk::Table(block));
            continue;
        }
        prose.push_str(line);
        i += 1;
    }
    push_prose(&mut prose, &mut out);
    out
}

fn is_table_row_line(line: &str) -> bool {
    let t = line.trim();
    if !t.starts_with('|') {
        return false;
    }
    // At least two pipes (one cell) and not a separator.
    t.matches('|').count() >= 2 && !is_table_separator_line(t)
}

fn is_table_separator_line(line: &str) -> bool {
    let t = line.trim();
    if !t.starts_with('|') || !t.contains('-') {
        return false;
    }
    let inner = t.trim_matches('|');
    !inner.is_empty()
        && inner.split('|').all(|cell| {
            let c = cell.trim();
            !c.is_empty() && c.chars().all(|ch| matches!(ch, '-' | ':' | ' '))
        })
}

fn split_table_cells(line: &str) -> Vec<String> {
    let t = line.trim();
    let t = t.strip_prefix('|').unwrap_or(t);
    let t = t.strip_suffix('|').unwrap_or(t);
    t.split('|')
        .map(|c| sanitize_terminal_text(c.trim()))
        .collect()
}

/// Shrink the widest columns first so long trailing columns are not crushed
/// to the minimum while earlier columns keep unused natural width.
fn balance_col_widths(natural: &[usize], available: usize, min_col: usize) -> Vec<usize> {
    let n = natural.len();
    if n == 0 {
        return Vec::new();
    }
    let min_col = min_col.max(1);
    // Prefer natural widths when the table already fits; only enforce the
    // readability floor once wrapping is required.
    let natural: Vec<usize> = natural.iter().map(|&w| w.max(1)).collect();
    let sum_natural: usize = natural.iter().sum();
    if sum_natural <= available {
        return natural;
    }
    let available = available.max(min_col * n);
    let mut widths: Vec<usize> = natural.iter().map(|&w| w.max(min_col)).collect();
    let mut sum: usize = widths.iter().sum();
    while sum > available {
        let Some(idx) = widths
            .iter()
            .enumerate()
            .filter(|(_, &w)| w > min_col)
            .max_by_key(|(_, &w)| w)
            .map(|(i, _)| i)
        else {
            break;
        };
        widths[idx] -= 1;
        sum -= 1;
    }
    widths
}

/// Parse inline markdown in a table cell into styled spans (markers stripped).
fn parse_cell_spans(text: &str, theme: &Theme, base: Style) -> Vec<Span<'static>> {
    let mut options = Options::empty();
    options.insert(Options::ENABLE_STRIKETHROUGH);
    let parser = Parser::new_ext(text, options);

    let mut spans: Vec<Span<'static>> = Vec::new();
    let mut style_stack = vec![base];
    let push_text = |spans: &mut Vec<Span<'static>>, s: &str, style: Style| {
        if s.is_empty() {
            return;
        }
        let content = sanitize_terminal_text(s);
        if content.is_empty() {
            return;
        }
        match spans.last_mut() {
            Some(last) if last.style == style => last.content.to_mut().push_str(&content),
            _ => spans.push(Span::styled(content, style)),
        }
    };

    for event in parser {
        let style = *style_stack.last().unwrap_or(&base);
        match event {
            Event::Start(Tag::Strong) => {
                style_stack.push(style.add_modifier(Modifier::BOLD));
            }
            Event::Start(Tag::Emphasis) => {
                style_stack.push(style.add_modifier(Modifier::ITALIC));
            }
            Event::Start(Tag::Strikethrough) => {
                style_stack.push(style.add_modifier(Modifier::CROSSED_OUT));
            }
            Event::Start(Tag::Link { .. }) => {
                style_stack.push(style.add_modifier(Modifier::UNDERLINED));
            }
            Event::End(TagEnd::Strong)
            | Event::End(TagEnd::Emphasis)
            | Event::End(TagEnd::Strikethrough)
            | Event::End(TagEnd::Link) => {
                if style_stack.len() > 1 {
                    style_stack.pop();
                }
            }
            Event::Code(code) => {
                push_text(&mut spans, code.as_ref(), theme.style("mdCode"));
            }
            Event::Text(t) => push_text(&mut spans, t.as_ref(), style),
            Event::SoftBreak | Event::HardBreak => push_text(&mut spans, " ", style),
            // Block wrappers / HTML / images: ignore structure, keep visible text via Text events.
            _ => {}
        }
    }

    if spans.is_empty() {
        // Preserve empty cells for column alignment.
        spans.push(Span::styled(String::new(), base));
    }
    spans
}

fn spans_width(spans: &[Span<'_>]) -> usize {
    spans
        .iter()
        .map(|s| UnicodeWidthStr::width(s.content.as_ref()))
        .sum()
}

fn push_styled_char(row: &mut Vec<Span<'static>>, ch: char, style: Style) {
    match row.last_mut() {
        Some(last) if last.style == style => last.content.to_mut().push(ch),
        _ => row.push(Span::styled(ch.to_string(), style)),
    }
}

/// Word-wrap a styled cell to `width`, preserving per-span styles.
fn wrap_styled_cell(spans: &[Span<'static>], width: usize) -> Vec<Vec<Span<'static>>> {
    let width = width.max(1);
    if spans_width(spans) == 0 {
        return vec![vec![Span::styled(
            String::new(),
            spans.first().map(|s| s.style).unwrap_or_default(),
        )]];
    }

    // Flatten to (char, style), then wrap like wrap_table_cell (whitespace-separated).
    let mut chars: Vec<(char, Style)> = Vec::new();
    for span in spans {
        for ch in span.content.chars() {
            chars.push((ch, span.style));
        }
    }

    let mut rows: Vec<Vec<Span<'static>>> = Vec::new();
    let mut row: Vec<Span<'static>> = Vec::new();
    let mut row_w = 0usize;
    let mut word: Vec<(char, Style)> = Vec::new();

    let word_width = |word: &[(char, Style)]| -> usize {
        word.iter()
            .map(|(ch, _)| UnicodeWidthStr::width(ch.to_string().as_str()).max(1))
            .sum()
    };
    let flush_row =
        |row: &mut Vec<Span<'static>>, row_w: &mut usize, rows: &mut Vec<Vec<Span<'static>>>| {
            rows.push(std::mem::take(row));
            *row_w = 0;
        };
    let append_word = |row: &mut Vec<Span<'static>>,
                       row_w: &mut usize,
                       word: &[(char, Style)],
                       width: usize,
                       rows: &mut Vec<Vec<Span<'static>>>| {
        let ww = word_width(word);
        if *row_w == 0 {
            if ww <= width {
                for &(ch, style) in word {
                    push_styled_char(row, ch, style);
                }
                *row_w = ww;
            } else {
                for &(ch, style) in word {
                    let cw = UnicodeWidthStr::width(ch.to_string().as_str()).max(1);
                    if *row_w + cw > width && *row_w > 0 {
                        flush_row(row, row_w, rows);
                    }
                    push_styled_char(row, ch, style);
                    *row_w += cw;
                }
            }
            return;
        }
        if *row_w + 1 + ww <= width {
            let space_style = row.last().map(|s| s.style).unwrap_or(word[0].1);
            push_styled_char(row, ' ', space_style);
            for &(ch, style) in word {
                push_styled_char(row, ch, style);
            }
            *row_w += 1 + ww;
        } else if ww <= width {
            flush_row(row, row_w, rows);
            for &(ch, style) in word {
                push_styled_char(row, ch, style);
            }
            *row_w = ww;
        } else {
            flush_row(row, row_w, rows);
            for &(ch, style) in word {
                let cw = UnicodeWidthStr::width(ch.to_string().as_str()).max(1);
                if *row_w + cw > width && *row_w > 0 {
                    flush_row(row, row_w, rows);
                }
                push_styled_char(row, ch, style);
                *row_w += cw;
            }
        }
    };

    for (ch, style) in chars {
        if ch.is_whitespace() {
            if !word.is_empty() {
                append_word(&mut row, &mut row_w, &word, width, &mut rows);
                word.clear();
            }
        } else {
            word.push((ch, style));
        }
    }
    if !word.is_empty() {
        append_word(&mut row, &mut row_w, &word, width, &mut rows);
    }
    if !row.is_empty() || rows.is_empty() {
        rows.push(row);
    }
    rows
}

fn pad_styled_cell(
    mut spans: Vec<Span<'static>>,
    width: usize,
    pad_style: Style,
) -> Vec<Span<'static>> {
    let w = spans_width(&spans);
    if w < width {
        spans.push(Span::styled(" ".repeat(width - w), pad_style));
    }
    spans
}

/// Render a GFM table with balanced wrapping columns.
fn render_markdown_table(block: &str, theme: &Theme, width: usize) -> Vec<Line<'static>> {
    let row_lines: Vec<&str> = block
        .lines()
        .map(str::trim_end)
        .filter(|l| !l.trim().is_empty())
        .collect();
    if row_lines.len() < 2 {
        return render_termimad(block, theme, width);
    }

    let mut raw_rows: Vec<Vec<String>> = Vec::new();
    let mut header_len = 0usize;
    let mut saw_sep = false;
    for line in &row_lines {
        if is_table_separator_line(line) {
            if !saw_sep {
                header_len = raw_rows.len();
                saw_sep = true;
            }
            continue;
        }
        if is_table_row_line(line) {
            raw_rows.push(split_table_cells(line));
        }
    }
    if raw_rows.is_empty() {
        return render_termimad(block, theme, width);
    }
    if saw_sep && header_len == 0 {
        header_len = 1.min(raw_rows.len());
    }
    let ncols = raw_rows.iter().map(|r| r.len()).max().unwrap_or(0).max(1);
    for row in &mut raw_rows {
        row.resize(ncols, String::new());
    }

    let style = theme.style("text");
    let header_style = style.add_modifier(Modifier::BOLD);

    // Parse inline markdown once so width math uses rendered text (no ** markers).
    let rows: Vec<Vec<Vec<Span<'static>>>> = raw_rows
        .iter()
        .enumerate()
        .map(|(ri, row)| {
            let base = if ri < header_len { header_style } else { style };
            row.iter()
                .map(|cell| parse_cell_spans(cell, theme, base))
                .collect()
        })
        .collect();

    // Borders: │cell│cell│ → 1 + ncols verticals, ncols cell widths.
    let border_overhead = ncols + 1;
    let available = width
        .saturating_sub(border_overhead)
        .max(ncols * MIN_TABLE_COL_WIDTH);
    let natural: Vec<usize> = (0..ncols)
        .map(|c| {
            rows.iter()
                .map(|r| spans_width(&r[c]))
                .max()
                .unwrap_or(0)
                .max(1)
        })
        .collect();
    let col_widths = balance_col_widths(&natural, available, MIN_TABLE_COL_WIDTH);

    let border = |s: String| Span::styled(s, style);

    let emit_row = |cells: &[Vec<Span<'static>>], out: &mut Vec<Line<'static>>| {
        let wrapped: Vec<Vec<Vec<Span<'static>>>> = cells
            .iter()
            .enumerate()
            .map(|(i, c)| wrap_styled_cell(c, col_widths[i]))
            .collect();
        let height = wrapped.iter().map(|w| w.len()).max().unwrap_or(1).max(1);
        let pad_style = cells
            .first()
            .and_then(|c| c.first())
            .map(|s| s.style)
            .unwrap_or(style);
        for r in 0..height {
            let mut spans = vec![border("│".into())];
            for (c, lines) in wrapped.iter().enumerate() {
                let cell_spans = lines
                    .get(r)
                    .cloned()
                    .unwrap_or_else(|| vec![Span::styled(String::new(), pad_style)]);
                spans.extend(pad_styled_cell(cell_spans, col_widths[c], pad_style));
                spans.push(border("│".into()));
            }
            out.push(Line::from(spans));
        }
    };

    let mut out = Vec::new();
    let header_len = header_len.min(rows.len());
    let (header_rows, body_rows) = rows.split_at(header_len);

    for row in header_rows {
        emit_row(row, &mut out);
    }
    if saw_sep {
        let mut rule = String::from("├");
        for (i, &w) in col_widths.iter().enumerate() {
            if i > 0 {
                rule.push('┼');
            }
            rule.push_str(&"─".repeat(w));
        }
        rule.push('┤');
        out.push(Line::from(border(rule)));
    }
    for row in body_rows {
        emit_row(row, &mut out);
    }
    out
}

/// Render a markdown segment with termimad → ANSI → ratatui lines.
fn render_termimad(md: &str, theme: &Theme, width: usize) -> Vec<Line<'static>> {
    let skin = theme_skin(theme);
    let fmt = skin.text(md, Some(width));
    let ansi = fmt.to_string();
    match ansi.into_text() {
        Ok(text) => text
            .lines
            .into_iter()
            .map(|mut line| {
                // Cap runaway horizontal rules (pure ─ runs) that termimad
                // stretches to the full layout width.
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
            .collect(),
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
///
/// Tables are expected to already fit (termimad wraps cells at render time).
/// Oversized table lines are left as-is so we never silently truncate cell
/// content; non-table lines wrap normally.
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
                    || f.starts_with('╞')
                    || f.starts_with('┝')
            }
            None => false,
        }
    };
    let mut out = Vec::new();
    for line in lines {
        if line.width() <= width || is_table_line(&line) {
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
        let lines = render_lines(md, &test_theme(), 80);
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
        let lines = render_lines(md, &test_theme(), 80);
        let text = plain(&lines);
        assert!(text.contains("┌─ rust"), "{text}");
        assert!(text.contains("fn main() {}"), "{text}");
        assert!(text.contains("more"), "{text}");
    }

    #[test]
    fn unterminated_code_block_streams() {
        let md = "intro\n\n```python\nprint('hi')";
        let lines = render_lines(md, &test_theme(), 80);
        let text = plain(&lines);
        assert!(text.contains("print('hi')"), "{text}");
        assert!(text.contains("intro"), "{text}");
    }

    #[test]
    fn inline_formatting_survives() {
        let md = "some **bold** and *italic* and `code` text";
        let lines = render_lines(md, &test_theme(), 80);
        let text = plain(&lines);
        assert!(text.contains("bold"), "{text}");
        assert!(text.contains("italic"), "{text}");
        assert!(text.contains("code"), "{text}");
    }

    #[test]
    fn table_cells_render_inline_markdown() {
        let md = concat!(
            "| | **Soket AI** | **Sarvam AI** |\n",
            "|---|---|---|\n",
            "| **HQ** | India | Bengaluru |\n",
            "| Differentiator | *MCR agents* | **Sarvam-30B** + `code` |\n",
        );
        let lines = render_lines(md, &test_theme(), 80);
        let text = plain(&lines);
        assert!(text.contains("Soket AI"), "{text}");
        assert!(text.contains("Sarvam AI"), "{text}");
        assert!(text.contains("MCR agents"), "{text}");
        assert!(text.contains("Sarvam-30B"), "{text}");
        assert!(text.contains("code"), "{text}");
        // Markdown markers must be stripped from the rendered table.
        assert!(!text.contains("**"), "bold markers leaked:\n{text}");
        assert!(
            !text.contains("*MCR") && !text.contains("agents*"),
            "italic markers leaked:\n{text}"
        );
        assert!(!text.contains('`'), "code markers leaked:\n{text}");

        let has_bold = lines.iter().any(|l| {
            l.spans.iter().any(|s| {
                s.content.contains("Soket") && s.style.add_modifier.contains(Modifier::BOLD)
            })
        });
        assert!(has_bold, "expected bold style on Soket AI:\n{text}");

        let has_italic = lines.iter().any(|l| {
            l.spans.iter().any(|s| {
                s.content.contains("MCR") && s.style.add_modifier.contains(Modifier::ITALIC)
            })
        });
        assert!(has_italic, "expected italic style on MCR agents:\n{text}");
    }

    #[test]
    fn blank_tail_trimmed() {
        let lines = render_lines("hello\n\n\n", &test_theme(), 80);
        assert_eq!(lines.len(), 1);
    }

    #[test]
    fn tables_wrap_long_columns_to_width() {
        let theme = test_theme();
        let width = 40;
        // Long content in the *first* column — should wrap that column, not
        // only the last one, and keep the table within `width`.
        let md = concat!(
            "| a very long first column that must wrap | short |\n",
            "|---|---|\n",
            "| more long content for the first cell here | y |\n",
        );
        let lines = render_lines(md, &theme, width);
        let wrapped = wrap_rendered_lines(lines, width);
        let text = plain(&wrapped);
        for l in &wrapped {
            assert!(
                l.width() <= width,
                "line too wide ({}): {:?}",
                l.width(),
                plain(&[l.clone()])
            );
        }
        // Cell text must survive (wrapped across rows), not be truncated away.
        assert!(text.contains("first"), "{text}");
        assert!(text.contains("column") || text.contains("wrap"), "{text}");
        let table_rows = text.lines().filter(|l| l.contains('│')).count();
        assert!(table_rows > 2, "expected multi-line wrapped cells:\n{text}");

        // Long paragraph text still wraps.
        let md2 = "word ".repeat(30);
        let lines2 = render_lines(&md2, &theme, width);
        let wrapped2 = wrap_rendered_lines(lines2, width);
        assert!(wrapped2.len() > 1);
        for l in &wrapped2 {
            assert!(l.width() <= width);
        }
    }

    #[test]
    fn table_columns_balance_instead_of_starving_last() {
        let theme = test_theme();
        let width = 60;
        let md = concat!(
            "| | Soket AI | Sarvam AI |\n",
            "|---|---|---|\n",
            "| HQ | India (research lab) | Bengaluru (Axonwise Pvt Ltd) |\n",
            "| Founded | More recent, smaller & quieter lab | August 2023, by Vivek Raghavan & Pratyush Kumar |\n",
        );
        let lines = render_lines(md, &theme, width);
        let text = plain(&lines);
        // Find a body row and measure column widths between │ separators.
        let row = text
            .lines()
            .find(|l| l.contains("Bengaluru") || l.contains("Ben"))
            .unwrap_or_else(|| panic!("missing body row:\n{text}"));
        let cols: Vec<&str> = row.split('│').filter(|c| !c.is_empty()).collect();
        assert_eq!(cols.len(), 3, "row={row}\n{text}");
        let w2 = UnicodeWidthStr::width(cols[1]);
        let w3 = UnicodeWidthStr::width(cols[2]);
        // Last column must not be crushed to the old termimad minimum (~3).
        assert!(w3 >= 12, "last column too narrow ({w3}): {row}\n{text}");
        // And the middle column should not keep nearly all leftover space.
        assert!(
            w2 <= w3 + 8,
            "columns unbalanced mid={w2} last={w3}: {row}\n{text}"
        );
        assert!(
            text.contains("Bengaluru") || text.contains("Axonwise"),
            "{text}"
        );
    }

    #[test]
    fn balance_col_widths_shrinks_widest_first() {
        let widths = balance_col_widths(&[9, 44, 100], 56, 8);
        assert_eq!(widths.iter().sum::<usize>(), 56);
        assert!(widths[0] >= 8 && widths[0] <= 9, "{widths:?}");
        assert!(widths[2] >= 12, "last starved: {widths:?}");
        assert!(
            (widths[1] as isize - widths[2] as isize).abs() <= 2,
            "{widths:?}"
        );
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
        let lines_mid = render_lines(mid, &theme, 80);
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
        let lines_full = render_lines(full, &theme, 80);
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
