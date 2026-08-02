//! Syntax highlighting for tool output and fenced code blocks.
//!
//! Uses syntect for Sublime/TextMate grammars, mapped onto the theme's
//! `syntax*` (and a few `md*`) color keys so highlighting follows the active theme.

use std::sync::LazyLock;

use ratatui::style::{Modifier, Style};
use ratatui::text::Span;
use syntect::easy::ScopeRegionIterator;
use syntect::highlighting::ScopeSelector;
use syntect::parsing::{MatchPower, ParseState, ScopeStack, SyntaxReference, SyntaxSet};

use crate::theme::Theme;

static SYNTAX_SET: LazyLock<SyntaxSet> = LazyLock::new(two_face::syntax::extra_no_newlines);

/// Scope → theme-key rules, ordered most-specific first for tie-breaking docs.
///
/// At match time we still pick the highest [`MatchPower`].
static SCOPE_STYLES: LazyLock<Vec<(ScopeSelector, ScopeStyle)>> = LazyLock::new(|| {
    let sel = |s: &str| s.parse::<ScopeSelector>().expect("valid scope selector");
    vec![
        (sel("comment"), ScopeStyle::key("syntaxComment")),
        (sel("string"), ScopeStyle::key("syntaxString")),
        (sel("constant.numeric"), ScopeStyle::key("syntaxNumber")),
        (sel("constant.character.escape"), ScopeStyle::key("syntaxString")),
        (sel("constant.language"), ScopeStyle::key("syntaxKeyword")),
        (sel("constant.other.color"), ScopeStyle::key("syntaxNumber")),
        (sel("entity.name.function"), ScopeStyle::key("syntaxFunction")),
        (sel("entity.name.method"), ScopeStyle::key("syntaxFunction")),
        (sel("support.function"), ScopeStyle::key("syntaxFunction")),
        (sel("entity.name.type"), ScopeStyle::key("syntaxType")),
        (sel("entity.name.class"), ScopeStyle::key("syntaxType")),
        (sel("entity.name.struct"), ScopeStyle::key("syntaxType")),
        (sel("entity.name.enum"), ScopeStyle::key("syntaxType")),
        (sel("entity.name.trait"), ScopeStyle::key("syntaxType")),
        (sel("entity.name.interface"), ScopeStyle::key("syntaxType")),
        (sel("entity.name.namespace"), ScopeStyle::key("syntaxType")),
        (sel("entity.name.tag"), ScopeStyle::key("syntaxKeyword")),
        (sel("entity.other.attribute-name"), ScopeStyle::key("syntaxVariable")),
        (sel("storage.type"), ScopeStyle::key("syntaxType")),
        (sel("storage.modifier"), ScopeStyle::key("syntaxKeyword")),
        (sel("storage"), ScopeStyle::key("syntaxKeyword")),
        (sel("keyword.operator"), ScopeStyle::key("syntaxOperator")),
        (sel("keyword"), ScopeStyle::key("syntaxKeyword")),
        (sel("support.type"), ScopeStyle::key("syntaxType")),
        (sel("support.class"), ScopeStyle::key("syntaxType")),
        (sel("support.constant"), ScopeStyle::key("syntaxNumber")),
        (sel("variable.language"), ScopeStyle::key("syntaxKeyword")),
        (sel("variable.function"), ScopeStyle::key("syntaxFunction")),
        (sel("variable.parameter"), ScopeStyle::key("syntaxVariable")),
        (sel("variable"), ScopeStyle::key("syntaxVariable")),
        (sel("punctuation.definition.string"), ScopeStyle::key("syntaxString")),
        (sel("punctuation"), ScopeStyle::key("syntaxPunctuation")),
        (sel("markup.heading"), ScopeStyle::key_mod("mdHeading", Modifier::BOLD)),
        (sel("markup.bold"), ScopeStyle::key_mod("text", Modifier::BOLD)),
        (sel("markup.italic"), ScopeStyle::key_mod("text", Modifier::ITALIC)),
        (sel("markup.underline.link"), ScopeStyle::key("mdLink")),
        (sel("markup.raw"), ScopeStyle::key("mdCode")),
        (sel("markup.quote"), ScopeStyle::key("mdQuote")),
        (sel("markup.list"), ScopeStyle::key("mdListBullet")),
        (sel("meta.separator"), ScopeStyle::key("mdHr")),
        (sel("meta.annotation"), ScopeStyle::key("syntaxType")),
        (sel("entity.name.section"), ScopeStyle::key_mod("mdHeading", Modifier::BOLD)),
        // TOML / INI section headers
        (sel("entity.name.tag.toml"), ScopeStyle::key("syntaxType")),
        (sel("support.type.property-name"), ScopeStyle::key("syntaxVariable")),
        (sel("meta.mapping.key"), ScopeStyle::key("syntaxVariable")),
    ]
});

#[derive(Clone, Copy)]
struct ScopeStyle {
    key: &'static str,
    modifier: Modifier,
}

impl ScopeStyle {
    const fn key(key: &'static str) -> Self {
        Self {
            key,
            modifier: Modifier::empty(),
        }
    }

    const fn key_mod(key: &'static str, modifier: Modifier) -> Self {
        Self { key, modifier }
    }

    fn to_style(self, theme: &Theme, _fallback: Style) -> Style {
        let style = theme.style(self.key);
        if self.modifier.is_empty() {
            style
        } else {
            style.add_modifier(self.modifier)
        }
    }
}

/// Highlight `text` using an optional language token and/or file path hint.
///
/// Returns one span-list per input line. Unknown languages fall back to `fallback`.
pub fn highlight_lines(
    text: &str,
    language: Option<&str>,
    path: Option<&str>,
    theme: &Theme,
    fallback: Style,
) -> Vec<Vec<Span<'static>>> {
    let ss = &*SYNTAX_SET;
    let Some(syntax) = resolve_syntax(ss, language, path) else {
        return plain_lines(text, fallback);
    };
    highlight_with_syntax(text, syntax, ss, theme, fallback)
}

/// Highlight a single line, advancing parse state (for streaming / multi-line).
pub fn highlight_line_stateful(
    line: &str,
    state: &mut HighlightState,
    theme: &Theme,
    fallback: Style,
) -> Vec<Span<'static>> {
    highlight_one_line(line, state, &*SYNTAX_SET, theme, fallback)
}

/// Mutable highlighter state for multi-line / streaming blocks.
pub struct HighlightState {
    parse: ParseState,
    stack: ScopeStack,
}

impl HighlightState {
    /// Start highlighting with an optional language token or path.
    pub fn new(language: Option<&str>, path: Option<&str>) -> Option<Self> {
        let ss = &*SYNTAX_SET;
        let syntax = resolve_syntax(ss, language, path)?;
        Some(Self {
            parse: ParseState::new(syntax),
            stack: ScopeStack::new(),
        })
    }

    /// Start highlighting from an explicit language token (e.g. fenced `rust`).
    pub fn from_language(language: &str) -> Option<Self> {
        Self::new(Some(language), None)
    }
}

fn resolve_syntax<'a>(
    ss: &'a SyntaxSet,
    language: Option<&str>,
    path: Option<&str>,
) -> Option<&'a SyntaxReference> {
    if let Some(lang) = language {
        let token = normalize_language(lang);
        if let Some(s) = ss.find_syntax_by_token(&token) {
            if s.name != "Plain Text" {
                return Some(s);
            }
        }
        if let Some(s) = ss.find_syntax_by_extension(&token) {
            if s.name != "Plain Text" {
                return Some(s);
            }
        }
        if let Some(s) = ss.find_syntax_by_name(&token) {
            if s.name != "Plain Text" {
                return Some(s);
            }
        }
    }
    if let Some(p) = path {
        if let Ok(Some(s)) = ss.find_syntax_for_file(p) {
            if s.name != "Plain Text" {
                return Some(s);
            }
        }
        // Bare filenames / truncated summaries still carry an extension.
        if let Some(ext) = std::path::Path::new(p)
            .extension()
            .and_then(|e| e.to_str())
        {
            if let Some(s) = ss.find_syntax_by_extension(ext) {
                if s.name != "Plain Text" {
                    return Some(s);
                }
            }
        }
    }
    None
}

fn normalize_language(lang: &str) -> String {
    match lang.trim().to_ascii_lowercase().as_str() {
        "rs" => "rust".into(),
        "js" | "mjs" | "cjs" => "javascript".into(),
        "ts" => "typescript".into(),
        "tsx" | "jsx" => "javascript".into(),
        "py" => "python".into(),
        "rb" => "ruby".into(),
        "kt" => "kotlin".into(),
        "cs" => "c#".into(),
        "sh" | "zsh" | "shell" => "bash".into(),
        "ps1" => "powershell".into(),
        "yml" => "yaml".into(),
        "md" | "mdx" => "markdown".into(),
        "htm" => "html".into(),
        "svg" => "xml".into(),
        "dockerfile" => "docker".into(),
        "make" => "makefile".into(),
        "cfg" | "conf" => "ini".into(),
        "patch" => "diff".into(),
        "txt" | "text" | "plain" => "plain text".into(),
        other => other.to_string(),
    }
}

fn highlight_with_syntax(
    text: &str,
    syntax: &SyntaxReference,
    ss: &SyntaxSet,
    theme: &Theme,
    fallback: Style,
) -> Vec<Vec<Span<'static>>> {
    let mut state = HighlightState {
        parse: ParseState::new(syntax),
        stack: ScopeStack::new(),
    };
    // Preserve a trailing blank line when the source ends with `\n\n`, but match
    // `str::lines()` for the common case (no phantom empty line after final `\n`).
    text.lines()
        .map(|line| highlight_one_line(line, &mut state, ss, theme, fallback))
        .collect()
}

fn highlight_one_line(
    line: &str,
    state: &mut HighlightState,
    ss: &SyntaxSet,
    theme: &Theme,
    fallback: Style,
) -> Vec<Span<'static>> {
    let ops = match state.parse.parse_line(line, ss) {
        Ok(ops) => ops,
        Err(_) => {
            return vec![Span::styled(sanitize(line), fallback)];
        }
    };

    let mut spans = Vec::new();
    for (text, op) in ScopeRegionIterator::new(&ops, line) {
        let _ = state.stack.apply(op);
        if text.is_empty() {
            continue;
        }
        let style = style_for_stack(&state.stack, theme, fallback);
        spans.push(Span::styled(sanitize(text), style));
    }
    if spans.is_empty() {
        spans.push(Span::styled(sanitize(line), fallback));
    }
    spans
}

fn style_for_stack(stack: &ScopeStack, theme: &Theme, fallback: Style) -> Style {
    let mut best: Option<(MatchPower, ScopeStyle)> = None;
    let scopes = stack.as_slice();
    for (selector, mapped) in SCOPE_STYLES.iter() {
        if let Some(score) = selector.does_match(scopes) {
            match best {
                Some((prev, _)) if score <= prev => {}
                _ => best = Some((score, *mapped)),
            }
        }
    }
    match best {
        Some((_, mapped)) => mapped.to_style(theme, fallback),
        None => fallback,
    }
}

fn plain_lines(text: &str, fallback: Style) -> Vec<Vec<Span<'static>>> {
    text.lines()
        .map(|l| vec![Span::styled(sanitize(l), fallback)])
        .collect()
}

fn sanitize(s: &str) -> String {
    s.chars()
        .filter(|c| !c.is_control() || *c == '\t')
        .map(|c| if c == '\t' { ' ' } else { c })
        .collect()
}

/// Truncate a list of spans to a display width, preserving styles.
pub fn truncate_spans(spans: Vec<Span<'static>>, max_width: usize) -> Vec<Span<'static>> {
    use unicode_width::UnicodeWidthStr;
    if max_width == 0 {
        return Vec::new();
    }
    let mut out = Vec::new();
    let mut used = 0usize;
    for span in spans {
        if used >= max_width {
            break;
        }
        let content = span.content.as_ref();
        let w = UnicodeWidthStr::width(content);
        if used + w <= max_width {
            used += w;
            out.push(span);
            continue;
        }
        let mut truncated = String::new();
        let mut tw = 0usize;
        for ch in content.chars() {
            let cw = UnicodeWidthStr::width(ch.to_string().as_str()).max(1);
            if used + tw + cw > max_width {
                break;
            }
            truncated.push(ch);
            tw += cw;
        }
        if !truncated.is_empty() {
            out.push(Span::styled(truncated, span.style));
        }
        break;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::theme::Theme;

    #[test]
    fn highlights_toml_keys_and_strings() {
        let theme = Theme::dark();
        let lines = highlight_lines(
            "name = \"loop\"\n",
            Some("toml"),
            Some("Cargo.toml"),
            &theme,
            theme.style("toolOutput"),
        );
        assert!(!lines.is_empty());
        let flat: String = lines[0].iter().map(|s| s.content.as_ref()).collect();
        assert!(flat.contains("name"));
        assert!(flat.contains("\"loop\""));
        let styles: Vec<_> = lines[0].iter().map(|s| s.style).collect();
        assert!(
            styles.iter().any(|s| *s != theme.style("toolOutput")),
            "expected non-fallback styles on TOML assignment"
        );
    }

    #[test]
    fn highlights_markdown_heading() {
        let theme = Theme::dark();
        let lines = highlight_lines(
            "# loop\n",
            Some("markdown"),
            Some("README.md"),
            &theme,
            theme.style("toolOutput"),
        );
        assert!(!lines.is_empty());
        let styles: Vec<_> = lines[0].iter().map(|s| s.style).collect();
        assert!(styles.iter().any(|s| *s != theme.style("toolOutput")));
    }

    #[test]
    fn unknown_language_is_plain() {
        let theme = Theme::dark();
        let lines = highlight_lines(
            "hello\n",
            Some("not-a-real-lang-xyz"),
            None,
            &theme,
            theme.style("toolOutput"),
        );
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0].len(), 1);
        assert_eq!(lines[0][0].style, theme.style("toolOutput"));
    }
}
