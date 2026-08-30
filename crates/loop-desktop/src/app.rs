//! Root GPUI application shell.

use std::collections::HashSet;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

use gpui::prelude::FluentBuilder;
use gpui::*;
use gpui_component::button::*;
use gpui_component::h_flex;
use gpui_component::input::{InputEvent, Textarea, TextareaState};
use gpui_component::menu::DropdownMenu;
use gpui_component::menu::PopupMenuItem;
use gpui_component::progress::ProgressCircle;
use gpui_component::scroll::{Scrollbar, ScrollableElement};
use gpui_component::separator::Separator;
use gpui_component::spinner::Spinner;
use gpui_component::tooltip::Tooltip;
use gpui_component::v_flex;
use gpui_component::{
    Icon, IconName, Sizable, Theme, ThemeMode, VirtualListScrollHandle, v_virtual_list, *,
};

use crate::controller::{DesktopCommand, DesktopController, DesktopSnapshot};
use crate::state::{ChatRow, ComposerStats, ToolCardStatus};

const SESSION_ROW_HEIGHT: f32 = 48.0;
const SIDEBAR_WIDTH: f32 = 248.0;
const DIFF_PANEL_WIDTH: f32 = 400.0;
const TOOLBAR_HEIGHT: f32 = 32.0;
const FOLLOW_BOTTOM_PX: f32 = 80.0;

struct DesktopApp {
    controller: Arc<DesktopController>,
    composer: Entity<TextareaState>,
    chat_scroll: ScrollHandle,
    session_scroll: VirtualListScrollHandle,
    session_item_sizes: Rc<Vec<gpui::Size<Pixels>>>,
    /// Tracks chat length + last message size to auto-scroll on new content.
    chat_scroll_sig: (usize, usize, bool),
    /// Stick to new tokens until the user scrolls away from the bottom.
    follow_chat: bool,
    diff_panel_open: bool,
    sidebar_open: bool,
    /// Expanded thinking / tool output cards (ids are unique across row kinds).
    expanded_rows: HashSet<String>,
    _subscriptions: Vec<Subscription>,
}

impl DesktopApp {
    fn send_prompt(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let text = self.composer.read(cx).value().trim().to_string();
        if text.is_empty() || self.controller.snapshot().streaming {
            return;
        }
        self.follow_chat = true;
        self.composer.update(cx, |state, cx| {
            state.set_value("", window, cx);
        });
        let controller = Arc::clone(&self.controller);
        tokio::spawn(async move {
            if let Err(error) = controller.handle_command(DesktopCommand::Prompt(text)).await {
                tracing::warn!("prompt failed: {error:#}");
            }
        });
    }

    fn sync_session_sizes(&mut self, count: usize) {
        self.session_item_sizes =
            Rc::new(vec![size(px(SIDEBAR_WIDTH - 16.), px(SESSION_ROW_HEIGHT)); count.max(1)]);
    }

    fn run_command(&self, cmd: DesktopCommand) {
        let controller = Arc::clone(&self.controller);
        tokio::spawn(async move {
            if let Err(error) = controller.handle_command(cmd).await {
                tracing::warn!("desktop command failed: {error:#}");
            }
        });
    }

    fn chat_is_near_bottom(&self) -> bool {
        let max_y = self.chat_scroll.max_offset().y;
        if max_y <= px(1.) {
            return true;
        }
        max_y + self.chat_scroll.offset().y <= px(FOLLOW_BOTTOM_PX)
    }

    fn maybe_scroll_chat(&mut self, snap: &DesktopSnapshot) {
        if !self.follow_chat && self.chat_is_near_bottom() {
            self.follow_chat = true;
        }
        let sig = chat_scroll_signature(snap);
        let content_changed = sig != self.chat_scroll_sig;
        self.chat_scroll_sig = sig;
        if self.follow_chat && (content_changed || snap.streaming) {
            self.chat_scroll.scroll_to_bottom();
        }
    }

    fn on_chat_scroll_wheel(
        &mut self,
        event: &ScrollWheelEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let delta = event.delta.pixel_delta(px(16.));
        // Offset is 0 at the top and more negative as you scroll down, so a
        // positive wheel delta moves toward older content.
        if delta.y > px(0.) {
            self.follow_chat = false;
        }
        cx.on_next_frame(window, |this, _, cx| {
            if this.chat_is_near_bottom() {
                this.follow_chat = true;
            }
            cx.notify();
        });
    }
}

fn chat_scroll_signature(snap: &DesktopSnapshot) -> (usize, usize, bool) {
    let last_len = snap
        .chat_rows
        .last()
        .map(|row| match row {
            ChatRow::User { text, .. } | ChatRow::Assistant { text, .. } => text.len(),
            ChatRow::Thinking { text, .. } => text.len(),
            ChatRow::Tool { detail, summary, .. } => detail.len().saturating_add(summary.len()),
            ChatRow::Shell { output, command, .. } => output.len().saturating_add(command.len()),
            _ => 0,
        })
        .unwrap_or(0);
    (snap.chat_rows.len(), last_len, snap.streaming)
}

impl Render for DesktopApp {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let snap = self.controller.snapshot();
        self.sync_session_sizes(snap.sessions.len());
        self.maybe_scroll_chat(&snap);
        let app = cx.entity().clone();
        let project = crate::chat_ui::project_label(&snap.cwd);
        window.set_window_title(&format!("Loop — {project}"));

        v_flex()
            .id("desktop-root")
            .size_full()
            .min_h_0()
            .min_w_0()
            .overflow_hidden()
            .bg(cx.theme().background)
            .child(render_toolbar(
                &snap,
                self.diff_panel_open,
                self.sidebar_open,
                cx,
            ))
            .child(
                h_flex()
                    .id("desktop-main")
                    .flex_1()
                    .min_h_0()
                    .min_w_0()
                    .when(self.sidebar_open, |el| {
                        el.child(render_sidebar(
                            &snap,
                            &self.session_scroll,
                            self.session_item_sizes.clone(),
                            cx,
                        ))
                    })
                    .child(
                        v_flex()
                            .id("desktop-chat-column")
                            .flex_1()
                            .h_full()
                            .min_w_0()
                            .min_h_0()
                            .child(render_chat_panel(
                                &snap,
                                &self.expanded_rows,
                                self.chat_scroll.clone(),
                                cx,
                            ))
                            .child(render_composer(&snap, &self.composer, app, cx)),
                    )
                    .when(self.diff_panel_open, |el| {
                        el.child(render_diff_panel(&snap, cx))
                    }),
            )
            .when(snap.approval_prompt.is_some(), |el| {
                el.child(render_approval_overlay(
                    snap.approval_prompt.as_ref().unwrap(),
                    cx,
                ))
            })
    }
}

actions!(desktop, [ToggleDiffPanel, ToggleSidebar, ToggleTheme]);

fn theme_mode_from_name(name: &str) -> ThemeMode {
    if name.eq_ignore_ascii_case("light") {
        ThemeMode::Light
    } else {
        ThemeMode::Dark
    }
}

fn toggle_theme(this: &mut DesktopApp, window: &mut Window, cx: &mut Context<DesktopApp>) {
    let next = if cx.theme().is_dark() {
        ThemeMode::Light
    } else {
        ThemeMode::Dark
    };
    Theme::change(next, Some(window), cx);
    cx.refresh_windows();
    let name = next.name().to_string();
    let controller = Arc::clone(&this.controller);
    tokio::spawn(async move {
        controller.persist_theme(&name);
    });
    cx.notify();
}

fn render_toolbar(
    snap: &DesktopSnapshot,
    diff_open: bool,
    sidebar_open: bool,
    cx: &mut Context<DesktopApp>,
) -> impl IntoElement {
    let session = snap
        .sessions
        .iter()
        .find(|s| s.active)
        .map(|s| crate::session_title::display_title(s.name.as_deref()))
        .unwrap_or_else(|| "New chat".into());
    let activity = crate::chat_ui::activity_label(snap.streaming, snap.phase, &snap.chat_rows);

    h_flex()
        .id("desktop-toolbar")
        .w_full()
        .h(px(TOOLBAR_HEIGHT))
        .flex_shrink_0()
        .items_center()
        .justify_between()
        .px_2()
        .gap_2()
        .border_b_1()
        .border_color(cx.theme().border)
        .bg(cx.theme().sidebar)
        .child(
            h_flex()
                .items_center()
                .gap_1()
                .min_w_0()
                .child(
                    Button::new("toggle-sidebar")
                        .ghost()
                        .xsmall()
                        .icon(if sidebar_open {
                            IconName::PanelLeftClose
                        } else {
                            IconName::PanelLeftOpen
                        })
                        .tooltip(if sidebar_open {
                            "Hide sessions"
                        } else {
                            "Show sessions"
                        })
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.sidebar_open = !this.sidebar_open;
                            cx.notify();
                        })),
                )
                .child(
                    div()
                        .min_w_0()
                        .text_sm()
                        .font_medium()
                        .text_ellipsis()
                        .text_color(cx.theme().foreground)
                        .child(session),
                ),
        )
        .child(
            h_flex()
                .items_center()
                .gap_2()
                .flex_shrink_0()
                .when_some(activity, |el, label| {
                    el.child(
                        h_flex()
                            .items_center()
                            .gap_1()
                            .child(Spinner::new().xsmall().color(cx.theme().accent))
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(cx.theme().accent)
                                    .child(label)
                                    .with_animation(
                                        "activity-pulse",
                                        Animation::new(Duration::from_millis(1400)).repeat(),
                                        |this, delta| {
                                            let t = (delta * std::f32::consts::PI * 2.).sin().abs();
                                            this.opacity(0.45 + 0.55 * t)
                                        },
                                    ),
                            ),
                    )
                })
                .child(
                    div()
                        .text_xs()
                        .text_color(cx.theme().muted_foreground)
                        .child(short_model(&snap.model_label)),
                )
                .child(
                    Button::new("toggle-theme")
                        .ghost()
                        .xsmall()
                        .icon(if cx.theme().is_dark() {
                            IconName::Sun
                        } else {
                            IconName::Moon
                        })
                        .tooltip(if cx.theme().is_dark() {
                            "Switch to light theme"
                        } else {
                            "Switch to dark theme"
                        })
                        .on_click(cx.listener(|this, _, window, cx| {
                            toggle_theme(this, window, cx);
                        })),
                )
                .child(
                    Button::new("toggle-diff-panel")
                        .ghost()
                        .xsmall()
                        .icon(if diff_open {
                            IconName::PanelRightClose
                        } else {
                            IconName::PanelRightOpen
                        })
                        .tooltip(if diff_open {
                            "Hide review panel"
                        } else {
                            "Show review panel"
                        })
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.diff_panel_open = !this.diff_panel_open;
                            cx.notify();
                        })),
                ),
        )
}

fn render_sidebar(
    snap: &DesktopSnapshot,
    scroll: &VirtualListScrollHandle,
    item_sizes: Rc<Vec<gpui::Size<Pixels>>>,
    cx: &mut Context<DesktopApp>,
) -> impl IntoElement {
    let sessions = snap.sessions.clone();
    v_flex()
        .w(px(SIDEBAR_WIDTH))
        .min_w(px(200.))
        .max_w(px(320.))
        .flex_shrink_0()
        .h_full()
        .min_h_0()
        .bg(cx.theme().sidebar)
        .border_r_1()
        .border_color(cx.theme().border)
        .child(
            h_flex()
                .items_center()
                .justify_between()
                .px_2()
                .h(px(TOOLBAR_HEIGHT))
                .flex_shrink_0()
                .child(
                    div()
                        .text_xs()
                        .font_semibold()
                        .text_color(cx.theme().muted_foreground)
                        .child("Sessions"),
                )
                .child(
                    Button::new("new-session")
                        .ghost()
                        .xsmall()
                        .icon(IconName::Plus)
                        .tooltip("New chat")
                        .on_click(cx.listener(|this, _, _, _| {
                            this.follow_chat = true;
                            this.run_command(DesktopCommand::NewSession);
                        })),
                ),
        )
        .child(
            div().px_2().pb_2().child(
                Button::new("new-session-full")
                    .outline()
                    .compact()
                    .w_full()
                    .label("New chat")
                    .icon(IconName::Plus)
                    .on_click(cx.listener(|this, _, _, _| {
                        this.follow_chat = true;
                        this.run_command(DesktopCommand::NewSession);
                    })),
            ),
        )
        .child(
            v_flex().flex_1().min_h_0().child(
                v_virtual_list(
                    cx.entity().clone(),
                    "sessions",
                    item_sizes,
                    move |_, visible_range, _, cx| {
                        visible_range
                            .filter_map(|ix| sessions.get(ix).cloned())
                            .map(|s| render_session_row(s, cx).into_any_element())
                            .collect::<Vec<_>>()
                    },
                )
                .track_scroll(scroll)
                .flex_1(),
            ),
        )
}

fn render_session_row(s: crate::state::SessionRow, cx: &mut Context<DesktopApp>) -> impl IntoElement {
    let label = crate::session_title::display_title(s.name.as_deref());
    let session_id = s.id.clone();
    let active = s.active;
    let running = s.running;
    let time = crate::chat_ui::relative_time(s.updated_at);

    h_flex()
        .id(SharedString::from(session_id.clone()))
        .w_full()
        .h(px(SESSION_ROW_HEIGHT))
        .items_center()
        .gap_2()
        .px_2()
        .rounded_md()
        .cursor_pointer()
        .when(active, |el| el.bg(cx.theme().accent.opacity(0.14)))
        .hover(|el| {
            if active {
                el
            } else {
                el.bg(cx.theme().muted.opacity(0.35))
            }
        })
        .on_click(cx.listener({
            move |this, _, _, _| {
                this.follow_chat = true;
                this.run_command(DesktopCommand::SelectSession(session_id.clone()));
            }
        }))
        .child(
            v_flex()
                .flex_1()
                .min_w_0()
                .gap_0()
                .child(
                    h_flex()
                        .items_center()
                        .gap_1()
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .text_sm()
                                .font_medium()
                                .text_ellipsis()
                                .text_color(if active {
                                    cx.theme().foreground
                                } else {
                                    cx.theme().sidebar_foreground
                                })
                                .child(label),
                        )
                        .when(running, |el| {
                            el.child(
                                Spinner::new()
                                    .xsmall()
                                    .color(cx.theme().accent),
                            )
                        }),
                )
                .child(
                    div()
                        .text_xs()
                        .text_color(cx.theme().muted_foreground)
                        .child(time),
                ),
        )
}

fn render_chat_panel(
    snap: &DesktopSnapshot,
    expanded_rows: &HashSet<String>,
    chat_scroll: ScrollHandle,
    cx: &mut Context<DesktopApp>,
) -> impl IntoElement {
    let rows = snap.chat_rows.clone();
    let pending = snap.pending_changes.clone();
    let expanded = expanded_rows.clone();
    let empty = rows.is_empty();
    let streaming = snap.streaming;
    let selected = snap.selected_change_id.clone();
    let show_working = streaming
        && !rows.iter().any(|r| match r {
            ChatRow::Assistant { streaming: true, .. } => true,
            ChatRow::Thinking { done: false, .. } => true,
            ChatRow::Tool {
                status: ToolCardStatus::Running | ToolCardStatus::Pending,
                ..
            } => true,
            _ => false,
        });

    let content = v_flex()
        .id("chat-scroll-content")
        .w_full()
        .flex_none()
        .h_auto()
        .min_h_full()
        .px_4()
        .py_3()
        .gap_3()
        .when(empty && !streaming, |el| el.child(render_empty_state(snap, cx)))
        .children(rows.iter().map(|row| {
            render_chat_row(
                row,
                expanded.contains(row_id(row)),
                selected.as_deref(),
                &pending,
                cx,
            )
        }))
        .when(show_working, |el| el.child(render_working_row(snap, cx)));

    div()
        .id("chat-panel")
        .flex_1()
        .min_h_0()
        .size_full()
        .relative()
        .child(
            div()
                .id("chat-scroll")
                .size_full()
                .flex()
                .flex_col()
                .overflow_y_scroll()
                .track_scroll(&chat_scroll)
                .restrict_scroll_to_axis()
                .on_scroll_wheel(cx.listener(|this, event, window, cx| {
                    this.on_chat_scroll_wheel(event, window, cx);
                }))
                .child(content),
        )
        .child(Scrollbar::vertical(&chat_scroll))
}

fn row_id(row: &ChatRow) -> &str {
    match row {
        ChatRow::User { id, .. }
        | ChatRow::Assistant { id, .. }
        | ChatRow::Thinking { id, .. }
        | ChatRow::Tool { id, .. }
        | ChatRow::FileChange { id, .. }
        | ChatRow::Shell { id, .. } => id,
        ChatRow::System(_) => "system",
    }
}

fn render_empty_state(snap: &DesktopSnapshot, cx: &mut Context<DesktopApp>) -> impl IntoElement {
    let project = crate::chat_ui::project_label(&snap.cwd);
    v_flex()
        .w_full()
        .py_8()
        .items_center()
        .justify_center()
        .gap_3()
        .child(
            div()
                .size_12()
                .rounded_full()
                .bg(cx.theme().accent.opacity(0.15))
                .flex()
                .items_center()
                .justify_center()
                .child(
                    Icon::new(IconName::Bot)
                        .size_8()
                        .text_color(cx.theme().accent),
                ),
        )
        .child(
            div()
                .text_lg()
                .font_semibold()
                .text_color(cx.theme().foreground)
                .child("What should we work on?"),
        )
        .child(
            div()
                .text_sm()
                .text_color(cx.theme().muted_foreground)
                .child(format!("Ask Loop to edit, search, or run commands in {project}")),
        )
}

fn render_working_row(snap: &DesktopSnapshot, cx: &mut Context<DesktopApp>) -> impl IntoElement {
    let label = crate::chat_ui::activity_label(true, snap.phase, &snap.chat_rows)
        .unwrap_or_else(|| "Working".into());
    h_flex()
        .items_center()
        .gap_2()
        .py_1()
        .child(Spinner::new().small().color(cx.theme().accent))
        .child(
            div()
                .text_sm()
                .text_color(cx.theme().muted_foreground)
                .child(label)
                .with_animation(
                    "working-fade",
                    Animation::new(Duration::from_millis(1200)).repeat(),
                    |this, delta| {
                        let t = (delta * std::f32::consts::PI * 2.).sin().abs();
                        this.opacity(0.5 + 0.5 * t)
                    },
                ),
        )
}

fn render_chat_row(
    row: &ChatRow,
    row_expanded: bool,
    selected_change: Option<&str>,
    pending: &[crate::state::PendingFileChange],
    cx: &mut Context<DesktopApp>,
) -> impl IntoElement {
    match row {
        ChatRow::User { text, .. } => h_flex().w_full().justify_end().child(
            div()
                .max_w(px(560.))
                .px_4()
                .py_3()
                .rounded_xl()
                .border_1()
                .border_color(cx.theme().border)
                .bg(cx.theme().accent.opacity(0.16))
                .text_color(cx.theme().foreground)
                .child(text.clone()),
        ).into_any_element(),
        ChatRow::Assistant { id, text, streaming } => v_flex()
            .w_full()
            .gap_1()
            .child(crate::markdown::render_markdown(
                format!("assistant-{id}"),
                text.clone(),
                cx,
            ))
            .when(*streaming, |el| el.child(crate::markdown::streaming_caret(cx)))
            .into_any_element(),
        ChatRow::Thinking { id, text, done } => {
            let open = row_expanded;
            let think_id = id.clone();
            let still_thinking = !*done;
            v_flex()
                .w_full()
                .gap_1()
                .child(
                    h_flex()
                        .id(SharedString::from(format!("think-toggle-{id}")))
                        .items_center()
                        .gap_2()
                        .cursor_pointer()
                        .on_click(cx.listener(move |this, _, _, cx| {
                            if this.expanded_rows.contains(&think_id) {
                                this.expanded_rows.remove(&think_id);
                            } else {
                                this.expanded_rows.insert(think_id.clone());
                                // Follow streaming thought content after expand.
                                this.follow_chat = true;
                            }
                            cx.notify();
                        }))
                        .child(
                            Icon::new(if open {
                                IconName::ChevronDown
                            } else {
                                IconName::ChevronRight
                            })
                            .size_4()
                            .text_color(cx.theme().muted_foreground),
                        )
                        .when(still_thinking, |el| {
                            el.child(Spinner::new().xsmall().color(cx.theme().muted_foreground))
                        })
                        .child(
                            div()
                                .text_sm()
                                .italic()
                                .text_color(cx.theme().muted_foreground)
                                .child(if *done {
                                    "Thought".to_string()
                                } else {
                                    "Thinking".to_string()
                                })
                                .with_animation(
                                    SharedString::from(format!("thinking-pulse-{id}")),
                                    Animation::new(Duration::from_millis(1100)).repeat(),
                                    move |this, delta| {
                                        if still_thinking && !open {
                                            let t = (delta * std::f32::consts::PI * 2.)
                                                .sin()
                                                .abs();
                                            this.opacity(0.45 + 0.55 * t)
                                        } else {
                                            this.opacity(1.)
                                        }
                                    },
                                ),
                        )
                        .when(!open && *done && !text.is_empty(), |el| {
                            el.child(
                                div()
                                    .text_sm()
                                    .text_color(cx.theme().muted_foreground.opacity(0.7))
                                    .child("· click to expand"),
                            )
                        }),
                )
                .when(open && !text.is_empty(), |el| {
                    el.child(
                        div()
                            .pl_6()
                            .text_sm()
                            .italic()
                            .text_color(cx.theme().muted_foreground)
                            .child(text.clone()),
                    )
                })
                .into_any_element()
        }
        ChatRow::Tool {
            id,
            name,
            summary,
            detail,
            status,
        } => render_tool_row(id, name, summary, detail, *status, row_expanded, cx)
            .into_any_element(),
        ChatRow::Shell {
            id,
            command,
            output,
            exit_code,
        } => {
            let status = match exit_code {
                Some(0) => ToolCardStatus::Success,
                Some(_) => ToolCardStatus::Error,
                None => ToolCardStatus::Error,
            };
            render_tool_row(id, "bash", command, output, status, row_expanded, cx)
                .into_any_element()
        }
        ChatRow::FileChange {
            id,
            path,
            added,
            removed,
            ..
        } => {
            let selected = selected_change == Some(id.as_str());
            let change_id = id.clone();
            let change = pending.iter().find(|c| c.id == *id);
            let preview = change
                .map(|c| {
                    crate::state::preview_diff_lines(
                        c.before.as_deref().unwrap_or(""),
                        &c.after,
                        18,
                    )
                })
                .unwrap_or_default();

            v_flex()
                .id(SharedString::from(format!("change-{id}")))
                .w_full()
                .rounded_lg()
                .border_1()
                .border_color(if selected {
                    cx.theme().accent.opacity(0.55)
                } else {
                    cx.theme().border
                })
                .bg(cx.theme().muted.opacity(0.22))
                .overflow_hidden()
                .cursor_pointer()
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.diff_panel_open = true;
                    this.run_command(DesktopCommand::SelectFileChange(change_id.clone()));
                    cx.notify();
                }))
                .child(
                    h_flex()
                        .w_full()
                        .items_center()
                        .gap_2()
                        .px_3()
                        .py_2()
                        .border_b_1()
                        .border_color(cx.theme().border.opacity(0.8))
                        .child(
                            Icon::new(IconName::File)
                                .size_4()
                                .text_color(cx.theme().muted_foreground),
                        )
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .text_sm()
                                .font_family(cx.theme().mono_font_family.clone())
                                .text_ellipsis()
                                .text_color(cx.theme().foreground)
                                .child(short_path(path)),
                        )
                        .child(
                            div()
                                .text_xs()
                                .text_color(cx.theme().success)
                                .child(format!("+{added}")),
                        )
                        .child(
                            div()
                                .text_xs()
                                .text_color(cx.theme().danger)
                                .child(format!("−{removed}")),
                        ),
                )
                .when(!preview.is_empty(), |el| {
                    el.child(render_diff_preview_body(&preview, cx))
                })
                .child(
                    div()
                        .px_3()
                        .py_2()
                        .border_t_1()
                        .border_color(cx.theme().border.opacity(0.8))
                        .text_xs()
                        .text_color(cx.theme().muted_foreground)
                        .child("Click to view diff"),
                )
                .into_any_element()
        }
        ChatRow::System(text) => div()
            .w_full()
            .text_sm()
            .text_color(cx.theme().muted_foreground)
            .child(text.clone())
            .into_any_element(),
    }
}

const TOOL_PREVIEW_LINES: usize = 12;
const TOOL_LINE_HEIGHT_PX: f32 = 18.0;

fn render_tool_row(
    id: &str,
    name: &str,
    summary: &str,
    detail: &str,
    status: ToolCardStatus,
    expanded: bool,
    cx: &mut Context<DesktopApp>,
) -> Div {
    let color = match status {
        ToolCardStatus::Running | ToolCardStatus::Pending => cx.theme().warning,
        ToolCardStatus::Success => cx.theme().muted_foreground,
        ToolCardStatus::Error => cx.theme().danger,
    };
    let title = crate::chat_ui::tool_activity_label(name, summary, status);
    let boxed = crate::chat_ui::is_shell_tool(name)
        || (crate::chat_ui::is_file_mutation_tool(name)
            && matches!(status, ToolCardStatus::Pending | ToolCardStatus::Running));
    let running = matches!(status, ToolCardStatus::Pending | ToolCardStatus::Running);

    if boxed {
        let total_lines = if detail.is_empty() {
            0
        } else {
            detail.lines().count().max(1)
        };
        let (preview, hidden_above) = if detail.is_empty() {
            (
                if running {
                    "…".to_string()
                } else {
                    String::new()
                },
                0,
            )
        } else if expanded {
            (detail.to_string(), 0)
        } else {
            crate::chat_ui::truncate_tool_detail(detail, TOOL_PREVIEW_LINES)
        };
        let can_expand = total_lines > TOOL_PREVIEW_LINES || (running && !detail.is_empty());
        let card_id = id.to_string();
        let body_id = SharedString::from(format!("tool-body-{id}"));

        let mut body = v_flex().w_full().gap_0p5().px_3().py_2();
        if preview.is_empty() {
            body = body.child(
                div()
                    .text_xs()
                    .text_color(cx.theme().muted_foreground)
                    .child("No output"),
            );
        } else {
            for line in preview.lines() {
                body = body.child(
                    div()
                        .text_xs()
                        .font_family(cx.theme().mono_font_family.clone())
                        .text_color(cx.theme().foreground.opacity(0.85))
                        .child(if line.is_empty() {
                            " ".to_string()
                        } else {
                            line.to_string()
                        }),
                );
            }
            if !expanded && hidden_above > 0 {
                body = body.child(
                    div()
                        .pt_1()
                        .text_xs()
                        .text_color(cx.theme().muted_foreground)
                        .child(format!("↑ {hidden_above} earlier lines · click to expand")),
                );
            } else if expanded && can_expand {
                body = body.child(
                    div()
                        .pt_1()
                        .text_xs()
                        .text_color(cx.theme().muted_foreground)
                        .child("click to collapse"),
                );
            }
        }

        let collapsed_max = px(TOOL_LINE_HEIGHT_PX * (TOOL_PREVIEW_LINES as f32 + 2.0));
        let body = body
            .id(body_id)
            .when(can_expand || !detail.is_empty(), |el| {
                el.cursor_pointer().on_click(cx.listener(move |this, _, _, cx| {
                    if this.expanded_rows.contains(&card_id) {
                        this.expanded_rows.remove(&card_id);
                    } else {
                        this.expanded_rows.insert(card_id.clone());
                        this.follow_chat = true;
                    }
                    cx.notify();
                }))
            })
            .when(!expanded, |el| {
                el.max_h(collapsed_max).overflow_hidden()
            });

        v_flex()
            .w_full()
            .rounded_lg()
            .border_1()
            .border_color(color.opacity(0.35))
            .bg(cx.theme().muted.opacity(0.25))
            .overflow_hidden()
            .child(
                h_flex()
                    .w_full()
                    .items_center()
                    .gap_2()
                    .px_3()
                    .py_2()
                    .border_b_1()
                    .border_color(color.opacity(0.25))
                    .bg(color.opacity(0.08))
                    .when(running, |el| el.child(Spinner::new().xsmall().color(color)))
                    .when(status == ToolCardStatus::Success, |el| {
                        el.child(
                            Icon::new(IconName::Check)
                                .size_4()
                                .text_color(cx.theme().success),
                        )
                    })
                    .when(status == ToolCardStatus::Error, |el| {
                        el.child(
                            Icon::new(IconName::CircleX)
                                .size_4()
                                .text_color(cx.theme().danger),
                        )
                    })
                    .when(crate::chat_ui::is_shell_tool(name), |el| {
                        el.child(
                            div()
                                .text_xs()
                                .font_family(cx.theme().mono_font_family.clone())
                                .text_color(cx.theme().muted_foreground)
                                .child(">_"),
                        )
                    })
                    .child(
                        div()
                            .text_sm()
                            .font_family(cx.theme().mono_font_family.clone())
                            .text_color(color)
                            .child(title),
                    ),
            )
            .child(body)
    } else {
        h_flex().w_full().items_center().child(
            h_flex()
                .items_center()
                .gap_2()
                .px_3()
                .py_1()
                .rounded_md()
                .border_l_2()
                .border_color(color.opacity(0.7))
                .bg(color.opacity(0.06))
                .when(running, |el| el.child(Spinner::new().xsmall().color(color)))
                .when(status == ToolCardStatus::Success, |el| {
                    el.child(
                        Icon::new(IconName::Check)
                            .size_4()
                            .text_color(cx.theme().success),
                    )
                })
                .when(status == ToolCardStatus::Error, |el| {
                    el.child(
                        Icon::new(IconName::CircleX)
                            .size_4()
                            .text_color(cx.theme().danger),
                    )
                })
                .child(
                    div()
                        .text_sm()
                        .font_family(cx.theme().mono_font_family.clone())
                        .text_color(color)
                        .child(title),
                ),
        )
    }
}

fn render_diff_preview_body(
    lines: &[crate::state::DiffPreviewLine],
    cx: &mut Context<DesktopApp>,
) -> Div {
    let mut body = v_flex()
        .w_full()
        .gap_0()
        .font_family(cx.theme().mono_font_family.clone())
        .text_xs();
    for line in lines {
        let (sign, fg, bg) = match line.tag {
            similar::ChangeTag::Insert => (
                "+",
                cx.theme().success,
                cx.theme().success.opacity(0.12),
            ),
            similar::ChangeTag::Delete => (
                "−",
                cx.theme().danger,
                cx.theme().danger.opacity(0.12),
            ),
            similar::ChangeTag::Equal => (
                " ",
                cx.theme().muted_foreground,
                gpui::transparent_black(),
            ),
        };
        let gutter = line
            .new_no
            .or(line.old_no)
            .map(|n| format!("{n:>4}"))
            .unwrap_or_else(|| "    ".into());
        body = body.child(
            h_flex()
                .w_full()
                .items_start()
                .gap_2()
                .px_2()
                .py_0p5()
                .bg(bg)
                .child(
                    div()
                        .text_color(cx.theme().muted_foreground.opacity(0.7))
                        .child(gutter),
                )
                .child(
                    div()
                        .text_color(fg)
                        .child(format!("{sign}{}", if line.text.is_empty() { " " } else { &line.text })),
                ),
        );
    }
    body
}

fn short_path(path: &str) -> String {
    let p = std::path::Path::new(path);
    match (p.parent().and_then(|d| d.file_name()), p.file_name()) {
        (Some(parent), Some(name)) => format!("{}/{}", parent.to_string_lossy(), name.to_string_lossy()),
        (_, Some(name)) => name.to_string_lossy().into_owned(),
        _ => path.to_string(),
    }
}

fn render_composer(
    snap: &DesktopSnapshot,
    composer: &Entity<TextareaState>,
    app: Entity<DesktopApp>,
    cx: &mut Context<DesktopApp>,
) -> impl IntoElement {
    let models = snap.available_models.clone();
    let current_model = snap.model_label.clone();
    let border = cx.theme().border;

    v_flex()
        .w_full()
        .flex_shrink_0()
        .border_t_1()
        .border_color(border)
        .bg(cx.theme().background)
        // Row 1: multiline input + submit
        .child(
            h_flex()
                .w_full()
                .items_end()
                .gap_2()
                .px_3()
                .pt_2()
                .pb_2()
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .child(Textarea::new(composer).appearance(false).bordered(false)),
                )
                .child(
                    Button::new("send")
                        .primary()
                        .icon(IconName::ArrowUp)
                        .tooltip(if snap.streaming {
                            "Working…"
                        } else {
                            "Send"
                        })
                        .loading(snap.streaming)
                        .disabled(snap.streaming)
                        .on_click(cx.listener(|this, _, window, cx| {
                            this.send_prompt(window, cx);
                        })),
                ),
        )
        // Row 2: status bar — line under input, columns separated by vertical lines
        .child(
            h_flex()
                .w_full()
                .items_center()
                .border_t_1()
                .border_color(border)
                .px_1()
                .min_h(px(36.))
                .child(composer_status_cell(
                    Button::new("model")
                        .ghost()
                        .small()
                        .label(if models.is_empty() {
                            snap.model_label.clone()
                        } else {
                            short_model(&snap.model_label)
                        })
                        .dropdown_caret(!models.is_empty())
                        .dropdown_menu({
                            let models = models.clone();
                            let current_model = current_model.clone();
                            let app = app.clone();
                            move |mut menu, _, _| {
                                if models.is_empty() {
                                    return menu.label("No models loaded yet");
                                }
                                menu = menu.scrollable(true).max_h(px(320.));
                                for (provider, model_id, name) in &models {
                                    let label = if name.is_empty() {
                                        format!("{provider}/{model_id}")
                                    } else {
                                        format!("{name} ({provider}/{model_id})")
                                    };
                                    let checked =
                                        format!("{provider}/{model_id}") == current_model;
                                    let p = provider.clone();
                                    let m = model_id.clone();
                                    let app = app.clone();
                                    menu = menu.item(
                                        PopupMenuItem::new(label)
                                            .checked(checked)
                                            .on_click(move |_, _, cx| {
                                                app.update(cx, |this, _| {
                                                    this.run_command(DesktopCommand::SetModel {
                                                        provider: p.clone(),
                                                        model_id: m.clone(),
                                                    });
                                                });
                                            }),
                                    );
                                }
                                menu
                            }
                        }),
                    true,
                    cx,
                ))
                .child(composer_status_cell(
                    Button::new("thinking")
                        .ghost()
                        .small()
                        .label(format!("think: {}", snap.thinking_label))
                        .tooltip("Cycle thinking level")
                        .on_click(cx.listener(|this, _, _, _| {
                            this.run_command(DesktopCommand::CycleThinking);
                        })),
                    true,
                    cx,
                ))
                .child(composer_status_cell(
                    render_context_ring(&snap.stats, cx),
                    true,
                    cx,
                ))
                .child(composer_status_cell(
                    div()
                        .text_xs()
                        .text_color(cx.theme().muted_foreground)
                        .whitespace_nowrap()
                        .child(snap.stats.tokens_label()),
                    false,
                    cx,
                )),
        )
}

fn composer_status_cell(
    content: impl IntoElement,
    with_divider: bool,
    cx: &App,
) -> impl IntoElement {
    h_flex()
        .flex_none()
        .items_center()
        .justify_center()
        .px_3()
        .py_2()
        .h_full()
        .when(with_divider, |el| {
            el.border_r_1().border_color(cx.theme().border)
        })
        .child(content)
}

fn short_model(label: &str) -> String {
    label
        .rsplit_once('/')
        .map(|(_, id)| id.to_string())
        .unwrap_or_else(|| label.to_string())
}

fn render_context_ring(stats: &ComposerStats, cx: &App) -> impl IntoElement {
    let pct = stats.context_pct();
    let tip = stats.context_tooltip();
    let color = if pct >= 90.0 {
        cx.theme().danger
    } else if pct >= 70.0 {
        cx.theme().warning
    } else {
        cx.theme().muted_foreground
    };

    div()
        .id("composer-ctx-ring")
        .flex_shrink_0()
        .tooltip(move |window, cx| Tooltip::new(tip.clone()).build(window, cx))
        .child(
            ProgressCircle::new("composer-ctx")
                .small()
                .value(pct)
                .color(color)
                .accessibility_label(stats.context_tooltip()),
        )
}

fn render_diff_panel(snap: &DesktopSnapshot, cx: &mut Context<DesktopApp>) -> impl IntoElement {
    let selected = snap
        .pending_changes
        .iter()
        .find(|c| Some(c.id.as_str()) == snap.selected_change_id.as_deref())
        .or_else(|| snap.pending_changes.last());

    let editor_label = snap
        .detected_editors
        .first()
        .cloned()
        .unwrap_or_else(|| "Open".into());

    v_flex()
        .w(px(DIFF_PANEL_WIDTH))
        .min_w(px(280.))
        .max_w(px(520.))
        .flex_shrink_0()
        .h_full()
        .min_h_0()
        .bg(cx.theme().sidebar)
        .border_l_1()
        .border_color(cx.theme().border)
        .child(
            h_flex()
                .items_center()
                .justify_between()
                .px_3()
                .py_2()
                .border_b_1()
                .border_color(cx.theme().border)
                .child(
                    div()
                        .text_sm()
                        .font_medium()
                        .text_ellipsis()
                        .child(
                            selected
                                .map(|c| short_path(&c.path.display().to_string()))
                                .unwrap_or_else(|| "Review".into()),
                        ),
                )
                .child(
                    h_flex()
                        .items_center()
                        .gap_1()
                        .when_some(selected, |el, c| {
                            el.child(
                                div()
                                    .text_xs()
                                    .text_color(cx.theme().success)
                                    .child(format!("+{}", c.added)),
                            )
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(cx.theme().danger)
                                    .child(format!("−{}", c.removed)),
                            )
                        })
                        .child(
                            Button::new("open-editor")
                                .ghost()
                                .xsmall()
                                .label(editor_label)
                                .disabled(selected.is_none())
                                .on_click(cx.listener(|this, _, _, _| {
                                    this.run_command(DesktopCommand::OpenInEditor);
                                })),
                        )
                        .child(
                            Button::new("close-diff-panel")
                                .ghost()
                                .xsmall()
                                .icon(IconName::PanelRightClose)
                                .tooltip("Hide review panel")
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.diff_panel_open = false;
                                    cx.notify();
                                })),
                        ),
                ),
        )
        .when(snap.pending_changes.len() > 1, |el| {
            el.child(
                v_flex()
                    .px_2()
                    .py_2()
                    .gap_1()
                    .border_b_1()
                    .border_color(cx.theme().border)
                    .children(snap.pending_changes.iter().map(|c| {
                        let id = c.id.clone();
                        let selected = Some(c.id.as_str()) == snap.selected_change_id.as_deref();
                        Button::new(format!("diff-file-{id}"))
                            .ghost()
                            .small()
                            .label(format!(
                                "{}  +{} −{}",
                                short_path(&c.path.display().to_string()),
                                c.added,
                                c.removed
                            ))
                            .when(selected, |b| b.primary())
                            .on_click(cx.listener(move |this, _, _, _| {
                                this.run_command(DesktopCommand::SelectFileChange(id.clone()));
                            }))
                    })),
            )
        })
        .child(
            v_flex()
                .gap_0()
                .p_3()
                .overflow_y_scrollbar()
                .flex_1()
                .font_family(cx.theme().mono_font_family.clone())
                .text_sm()
                .children(
                    selected
                        .map(|c| {
                            let diff = similar::TextDiff::from_lines(
                                c.before.as_deref().unwrap_or(""),
                                &c.after,
                            );
                            diff.iter_all_changes()
                                .map(|change| {
                                    let (sign, color, bg) = match change.tag() {
                                        similar::ChangeTag::Insert => {
                                            ("+", cx.theme().success, cx.theme().success.opacity(0.08))
                                        }
                                        similar::ChangeTag::Delete => {
                                            ("−", cx.theme().danger, cx.theme().danger.opacity(0.08))
                                        }
                                        similar::ChangeTag::Equal => {
                                            (" ", cx.theme().muted_foreground, cx.theme().background)
                                        }
                                    };
                                    div()
                                        .px_1()
                                        .bg(bg)
                                        .text_color(color)
                                        .child(format!("{sign}{}", change.value().trim_end()))
                                })
                                .collect::<Vec<_>>()
                        })
                        .into_iter()
                        .flatten(),
                )
                .when(selected.is_none(), |el| {
                    el.child(
                        div()
                            .text_sm()
                            .text_color(cx.theme().muted_foreground)
                            .child("No pending file changes"),
                    )
                }),
        )
        .child(
            h_flex()
                .gap_2()
                .p_3()
                .border_t_1()
                .border_color(cx.theme().border)
                .child(
                    Button::new("accept")
                        .primary()
                        .label("Keep")
                        .disabled(selected.is_none())
                        .on_click(cx.listener(|this, _, _, _| {
                            if let Some(id) = this
                                .controller
                                .snapshot()
                                .selected_change_id
                                .clone()
                                .or_else(|| {
                                    this.controller
                                        .snapshot()
                                        .pending_changes
                                        .last()
                                        .map(|c| c.id.clone())
                                })
                            {
                                this.run_command(DesktopCommand::AcceptFileChange(id));
                            }
                        })),
                )
                .child(
                    Button::new("reject")
                        .danger()
                        .label("Revert")
                        .disabled(selected.is_none())
                        .on_click(cx.listener(|this, _, _, _| {
                            if let Some(id) = this
                                .controller
                                .snapshot()
                                .selected_change_id
                                .clone()
                                .or_else(|| {
                                    this.controller
                                        .snapshot()
                                        .pending_changes
                                        .last()
                                        .map(|c| c.id.clone())
                                })
                            {
                                this.run_command(DesktopCommand::RejectFileChange(id));
                            }
                        })),
                ),
        )
}

fn render_approval_overlay(
    prompt: &crate::approval::ApprovalUiPrompt,
    cx: &mut Context<DesktopApp>,
) -> impl IntoElement {
    div()
        .absolute()
        .inset_0()
        .bg(rgba(0x00000099))
        .flex()
        .items_center()
        .justify_center()
        .child(
            v_flex()
                .w(px(480.0))
                .rounded_xl()
                .border_1()
                .border_color(cx.theme().border)
                .bg(cx.theme().background)
                .shadow_lg()
                .child(
                    h_flex()
                        .items_center()
                        .gap_2()
                        .p_4()
                        .border_b_1()
                        .border_color(cx.theme().border)
                        .child(
                            Icon::new(IconName::TriangleAlert)
                                .size_4()
                                .text_color(cx.theme().warning),
                        )
                        .child(
                            div()
                                .font_semibold()
                                .child(format!("Allow {}?", prompt.tool_name)),
                        ),
                )
                .child(
                    v_flex()
                        .gap_2()
                        .p_4()
                        .child(prompt.summary.clone())
                        .when(!prompt.detail.is_empty(), |el| {
                            el.child(
                                div()
                                    .text_sm()
                                    .font_family(cx.theme().mono_font_family.clone())
                                    .text_color(cx.theme().muted_foreground)
                                    .child(prompt.detail.clone()),
                            )
                        }),
                )
                .child(Separator::horizontal())
                .child(
                    h_flex()
                        .gap_2()
                        .p_4()
                        .justify_end()
                        .child(
                            Button::new("reject")
                                .label("Reject")
                                .on_click(cx.listener(|this, _, _, _| {
                                    this.run_command(DesktopCommand::ResolveApproval {
                                        accept: false,
                                        session: false,
                                        reason: None,
                                    });
                                })),
                        )
                        .child(
                            Button::new("accept")
                                .primary()
                                .label("Allow")
                                .on_click(cx.listener(|this, _, _, _| {
                                    this.run_command(DesktopCommand::ResolveApproval {
                                        accept: true,
                                        session: false,
                                        reason: None,
                                    });
                                })),
                        )
                        .child(
                            Button::new("accept-session")
                                .outline()
                                .label("Always allow")
                                .on_click(cx.listener(|this, _, _, _| {
                                    this.run_command(DesktopCommand::ResolveApproval {
                                        accept: true,
                                        session: true,
                                        reason: None,
                                    });
                                })),
                        ),
                ),
        )
}

/// Launch the desktop application.
pub async fn run_desktop(cwd: PathBuf) -> anyhow::Result<()> {
    let controller = Arc::new(DesktopController::new(cwd).await?);
    let initial_theme = theme_mode_from_name(controller.theme_name());

    let controller_ui = Arc::clone(&controller);
    let ui_rx = controller.ui_receiver();

    gpui_platform::application()
        .with_assets(gpui_component_assets::Assets)
        .run(move |cx| {
            gpui_component::init(cx);
            Theme::change(initial_theme, None, cx);

            cx.spawn(async move |cx| {
                let mut options = WindowOptions::default();
                options.window_min_size = Some(size(px(840.), px(560.)));
                options.window_bounds = Some(cx.update(|cx| {
                    WindowBounds::centered(size(px(1280.), px(840.)), cx)
                }));

                cx.open_window(options, move |window, cx| {
                    Theme::change(initial_theme, Some(window), cx);
                    let composer = cx.new(|cx| {
                        TextareaState::new(window, cx)
                            .auto_grow(1, 8)
                            .submit_on_enter(true)
                            .placeholder("Ask Loop to make a change…")
                    });

                    let view = cx.new(|cx| {
                        let subscriptions = vec![cx.subscribe_in(
                            &composer,
                            window,
                            |this: &mut DesktopApp, input, event, window, cx| {
                                if let InputEvent::PressEnter { shift, .. } = event {
                                    if !shift {
                                        this.send_prompt(window, cx);
                                        input.update(cx, |state, cx| {
                                            state.set_value("", window, cx);
                                        });
                                    }
                                }
                            },
                        )];

                        DesktopApp {
                            controller: Arc::clone(&controller_ui),
                            composer,
                            chat_scroll: ScrollHandle::new(),
                            session_scroll: VirtualListScrollHandle::new(),
                            session_item_sizes: Rc::new(vec![size(
                                px(SIDEBAR_WIDTH - 16.),
                                px(SESSION_ROW_HEIGHT),
                            )]),
                            chat_scroll_sig: (0, 0, false),
                            follow_chat: true,
                            diff_panel_open: false,
                            sidebar_open: true,
                            expanded_rows: HashSet::new(),
                            _subscriptions: subscriptions,
                        }
                    });

                    let poll_rx = ui_rx.clone();
                    let view_entity = view.clone();
                    cx.spawn(async move |cx| {
                        loop {
                            if poll_rx.recv().await.is_err() {
                                break;
                            }
                            while poll_rx.try_recv().is_ok() {}
                            let _ = view_entity.update(cx, |_, cx| cx.notify());
                            cx.background_executor()
                                .timer(Duration::from_millis(16))
                                .await;
                        }
                    })
                    .detach();

                    cx.new(|cx| Root::new(view, window, cx).bg(cx.theme().background))
                })
                .expect("open window");
            })
            .detach();
        });

    Ok(())
}
