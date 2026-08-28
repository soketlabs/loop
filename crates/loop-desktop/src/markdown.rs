//! Markdown rendering for assistant messages.

use gpui::{
    App, HighlightStyle, IntoElement, ParentElement as _, SharedString, Styled, div, px, rems,
};
use gpui_component::clipboard::Clipboard;
use gpui_component::h_flex;
use gpui_component::text::{TextView, TextViewStyle};
use gpui_component::ActiveTheme as _;

/// Dark-aware markdown style with syntax highlighting for fenced code.
pub fn markdown_style(cx: &App) -> TextViewStyle {
    let mut style = TextViewStyle::default();
    style.highlight_theme = cx.theme().highlight_theme.clone();
    style.is_dark = cx.theme().is_dark();
    style.paragraph_gap = rems(0.3);
    style.heading_base_font_size = px(15.);
    style.inline_code = HighlightStyle {
        background_color: Some(cx.theme().muted.opacity(0.7)),
        ..Default::default()
    };
    style
}

/// Selectable markdown view keyed by a stable row id.
pub fn render_markdown(
    id: impl Into<SharedString>,
    source: impl Into<SharedString>,
    cx: &App,
) -> impl IntoElement {
    let id = id.into();
    let copy_id = format!("{id}-copy");
    TextView::markdown(id, source)
        .style(markdown_style(cx))
        .selectable(true)
        .w_full()
        .code_block_actions(move |code_block, _, _| {
            let code = code_block.code();
            let block_id = format!("{copy_id}-{}", code.len());
            h_flex().w_full().justify_end().child(
                Clipboard::new(block_id)
                    .value(code)
                    .tooltip("Copy code"),
            )
        })
}

/// Blinking caret shown while an assistant message is streaming.
pub fn streaming_caret(cx: &App) -> impl IntoElement {
    use gpui::{ease_in_out, Animation, AnimationExt as _};
    use std::time::Duration;

    div()
        .w(px(7.))
        .h(px(16.))
        .rounded_sm()
        .bg(cx.theme().accent)
        .with_animation(
            "stream-caret",
            Animation::new(Duration::from_millis(900))
                .repeat()
                .with_easing(ease_in_out),
            |this, delta| this.opacity(if delta < 0.5 { 1.0 } else { 0.12 }),
        )
}
