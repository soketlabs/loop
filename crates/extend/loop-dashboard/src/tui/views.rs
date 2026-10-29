//! Drawing each view of the usage screen.

use super::widgets::*;
use super::*;

impl App {
    // ---------------------------------------------------------------- frame

    pub(super) fn render(&mut self, frame: &mut Frame) {
        let area = frame.area();
        if area.width < MIN_SIZE.0 || area.height < MIN_SIZE.1 {
            let msg = Paragraph::new(vec![
                Line::from("Terminal too small for the usage view".bold()),
                Line::from(format!(
                    "Resize to at least {}×{} · q to close",
                    MIN_SIZE.0, MIN_SIZE.1
                ))
                .fg(self.p().muted),
            ])
            .alignment(Alignment::Center)
            .wrap(Wrap { trim: true });
            frame.render_widget(msg, centered_rows(area, 2));
            return;
        }
        let notice_h = u16::from(self.options.notice.is_some());
        let [header, notice, body, footer] = Layout::vertical([
            Constraint::Length(2),
            Constraint::Length(notice_h),
            Constraint::Min(0),
            Constraint::Length(1),
        ])
        .areas(area);
        self.render_header(frame, header);
        if let Some(text) = &self.options.notice {
            frame.render_widget(
                Paragraph::new(format!(" {text}")).fg(self.p().series_2),
                notice,
            );
        }
        if self.view.is_history() && self.is_empty() {
            self.render_empty(frame, body);
        } else {
            match self.view {
                View::Now => self.render_now(frame, body),
                View::Usage => self.render_usage(frame, body),
                View::Models => self.render_models(frame, body),
                View::Tools => self.render_tools(frame, body),
                View::Agents => self.render_agents(frame, body),
                View::Sessions => self.render_sessions(frame, body),
            }
        }
        self.render_footer(frame, footer);
    }

    fn p(&self) -> Palette {
        self.options.palette
    }

    fn is_empty(&self) -> bool {
        let t = &self.summary.totals;
        t.runs + t.sub_agent_runs + t.llm_calls + t.tool_calls == 0
    }

    fn panel(&self, title: &str) -> Block<'static> {
        Block::bordered()
            .border_type(BorderType::Rounded)
            .border_style(Style::new().fg(self.p().border))
            .title(Line::from(format!(" {title} ")).fg(self.p().text).bold())
    }

    fn render_header(&self, frame: &mut Frame, area: Rect) {
        let p = self.p();
        let range_w = 26;
        // Narrow terminals: no "usage" title, and numbers only except the selected view.
        let full_width =
            16 + View::ALL.iter().map(|v| v.label().len() + 4).sum::<usize>() + range_w;
        let compact = (area.width as usize) < full_width;
        let mut left = vec![Span::from(" ◆ loop ").fg(p.accent).bold()];
        if !compact {
            left.push(Span::from("usage").fg(p.text).bold());
            left.push(Span::from("   "));
        }
        for (i, view) in View::ALL.iter().enumerate() {
            let label = if compact && *view != self.view {
                format!(" {} ", i + 1)
            } else {
                format!(" {} {} ", i + 1, view.label())
            };
            left.push(if *view == self.view {
                Span::from(label).fg(p.accent).bg(p.selected_bg).bold()
            } else {
                Span::from(label).fg(p.muted)
            });
        }
        let right: Vec<Span> = if self.view.is_history() {
            let mut spans = vec![Span::from("◀ ").fg(p.dim)];
            for range in Range::ALL {
                let label = format!(" {} ", range.label());
                spans.push(if range == self.range {
                    Span::from(label).fg(p.accent).bg(p.selected_bg).bold()
                } else {
                    Span::from(label).fg(p.muted)
                });
            }
            spans.push(Span::from(" ▶ ").fg(p.dim));
            spans
        } else {
            let at = self
                .options
                .live
                .as_ref()
                .map(|l| l.taken_at.format("%H:%M:%S").to_string());
            match at {
                Some(at) => vec![
                    Span::from("● ").fg(p.good),
                    Span::from(format!("live · {at} ")).fg(p.dim),
                ],
                None => vec![],
            }
        };
        let [l, r] = Layout::horizontal([Constraint::Min(0), Constraint::Length(range_w as u16)])
            .areas(Rect { height: 1, ..area });
        frame.render_widget(Line::from(left), l);
        frame.render_widget(Line::from(right).alignment(Alignment::Right), r);
        let rule = Rect {
            y: area.y + 1,
            height: 1,
            ..area
        };
        frame.render_widget(
            Paragraph::new("─".repeat(area.width as usize)).fg(p.border),
            rule,
        );
    }

    fn render_footer(&self, frame: &mut Frame, area: Rect) {
        let p = self.p();
        let hints = [
            ("1-6", "view"),
            ("←→", "period"),
            ("↑↓", "scroll"),
            ("r", "refresh"),
            ("q", "close"),
        ];
        let mut spans = vec![Span::from(" ")];
        for (key, what) in hints {
            spans.push(Span::from(key).fg(p.accent).bold());
            spans.push(Span::from(format!(" {what}   ")).fg(p.dim));
        }
        let s = &self.summary;
        let updated = s.generated_at.with_timezone(&Local).format("%H:%M:%S");
        let hints_w: usize = spans.iter().map(|s| s.content.chars().count()).sum();
        let room = (area.width as usize).saturating_sub(hints_w);
        let status = [
            format!(
                "anonymous · local only · {} file{} · updated {updated} ",
                s.files,
                if s.files == 1 { "" } else { "s" },
            ),
            format!("updated {updated} "),
        ]
        .into_iter()
        .find(|text| text.chars().count() <= room)
        .unwrap_or_default();
        let [l, r] = Layout::horizontal([
            Constraint::Min(0),
            Constraint::Length(status.chars().count() as u16),
        ])
        .areas(area);
        frame.render_widget(Line::from(spans), l);
        frame.render_widget(Line::from(status).fg(p.dim), r);
    }

    fn render_empty(&self, frame: &mut Frame, area: Rect) {
        let p = self.p();
        let lines = vec![
            Line::from("No usage recorded in this period".bold().fg(p.text)),
            Line::from(""),
            Line::from("Loop keeps an anonymous usage log on this machine while you chat.")
                .fg(p.muted),
            Line::from(format!("Reading {}", self.options.dir.display())).fg(p.dim),
            Line::from(""),
            Line::from(vec![
                Span::from("←→").fg(p.accent).bold(),
                Span::from(" try a longer period").fg(p.dim),
            ]),
        ];
        frame.render_widget(
            Paragraph::new(lines).alignment(Alignment::Center),
            centered_rows(area, 6),
        );
    }

    // ---------------------------------------------------------------- now

    fn render_now(&self, frame: &mut Frame, area: Rect) {
        let p = self.p();
        let Some(live) = &self.options.live else {
            let lines = vec![
                Line::from("Live status needs a running loop".bold().fg(p.text)),
                Line::from(""),
                Line::from(vec![
                    Span::from("Open it with ").fg(p.muted),
                    Span::from("/usage").fg(p.accent),
                    Span::from(" inside loop to see the session, context window,").fg(p.muted),
                ]),
                Line::from("agents and subagents, skills and MCP servers.").fg(p.muted),
                Line::from(""),
                Line::from(vec![
                    Span::from("2").fg(p.accent).bold(),
                    Span::from(" Usage shows the history of every session.").fg(p.dim),
                ]),
            ];
            frame.render_widget(
                Paragraph::new(lines).alignment(Alignment::Center),
                centered_rows(area, 6),
            );
            return;
        };
        let [top, bottom] =
            Layout::vertical([Constraint::Min(10), Constraint::Length(4)]).areas(area);
        let [left, right] =
            Layout::horizontal([Constraint::Percentage(50), Constraint::Percentage(50)]).areas(top);
        let [session, context] =
            Layout::vertical([Constraint::Min(8), Constraint::Length(5)]).areas(left);
        let agents_h = (live.sub_agents.len().min(6) as u16 + 5).min(right.height / 2 + 2);
        let [agents, capabilities] =
            Layout::vertical([Constraint::Length(agents_h), Constraint::Min(6)]).areas(right);
        self.render_session(frame, session, live);
        self.render_context(frame, context, live);
        self.render_live_agents(frame, agents, live);
        self.render_capabilities(frame, capabilities, live);
        self.render_today(frame, bottom);
    }

    fn render_session(&self, frame: &mut Frame, area: Rect, live: &LiveStatus) {
        let model = match &live.effort {
            Some(effort) => format!("{} · effort {effort}", live.model),
            None => live.model.clone(),
        };
        let rows = [
            ("Model", model),
            ("Directory", live.cwd.clone()),
            ("Permissions", live.permission_mode.clone()),
            ("Sandbox", live.sandbox.clone()),
            ("Web tools", live.web.clone()),
            ("Session", live.session_id.clone()),
            (
                "Usage log",
                live.usage_log
                    .as_ref()
                    .map_or("not available in this build".into(), |path| {
                        format!("anonymous, local only · {path}")
                    }),
            ),
        ];
        self.key_values(frame, area, "Session", &rows);
    }

    fn key_values(&self, frame: &mut Frame, area: Rect, title: &str, rows: &[(&str, String)]) {
        let p = self.p();
        let block = self.panel(title);
        let inner = block.inner(area).inner(Margin::new(1, 0));
        frame.render_widget(block, area);
        let key_w = rows.iter().map(|(k, _)| k.len()).max().unwrap_or(0) + 2;
        let value_w = (inner.width as usize).saturating_sub(key_w);
        let lines: Vec<Line> = rows
            .iter()
            .map(|(key, value)| {
                Line::from(vec![
                    Span::from(format!("{key:<key_w$}")).fg(p.muted),
                    Span::from(truncate(value, value_w)).fg(p.text),
                ])
            })
            .collect();
        frame.render_widget(Paragraph::new(lines), inner);
    }

    fn render_context(&self, frame: &mut Frame, area: Rect, live: &LiveStatus) {
        let p = self.p();
        let block = self.panel("Context window");
        let inner = block.inner(area).inner(Margin::new(1, 0));
        frame.render_widget(block, area);
        let mut lines = Vec::new();
        match (live.context_tokens, live.context_window) {
            (Some(used), window) if window > 0 => {
                let fraction = used as f64 / window as f64;
                let label = format!(
                    " {:.0}% used · {} of {}",
                    100.0 * fraction,
                    compact(used),
                    compact(window)
                );
                let bar_w = (inner.width as usize).saturating_sub(label.chars().count());
                let color = if fraction >= 0.8 {
                    p.series_2
                } else {
                    p.series_1
                };
                lines.push(Line::from(vec![
                    Span::from(gauge(fraction, bar_w)).fg(color),
                    Span::from(label).fg(p.text),
                ]));
                lines.push(
                    Line::from(format!(
                        "{} left before the conversation is compacted",
                        compact(window.saturating_sub(used))
                    ))
                    .fg(p.dim),
                );
            }
            (_, window) if window > 0 => lines.push(
                Line::from(format!(
                    "{} window · fills as the conversation grows",
                    compact(window)
                ))
                .fg(p.muted),
            ),
            _ => lines.push(Line::from("Context window unknown for this model").fg(p.muted)),
        }
        lines.push(Line::from(vec![
            Span::from("Session tokens ").fg(p.muted),
            Span::from(thousands(live.session_tokens)).fg(p.text).bold(),
        ]));
        frame.render_widget(Paragraph::new(lines), inner);
    }

    fn render_live_agents(&self, frame: &mut Frame, area: Rect, live: &LiveStatus) {
        let p = self.p();
        let running = live
            .sub_agents
            .iter()
            .filter(|a| a.state == SubAgentState::Running)
            .count();
        let finished = live.sub_agents.len() - running;
        let active = usize::from(live.main_agent_working) + running;
        let block = self.panel(&format!("Agents · {active} running"));
        let inner = block.inner(area).inner(Margin::new(1, 0));
        frame.render_widget(block, area);
        let status = |on: bool, label: &str| {
            if on {
                Span::from(format!("● {label}")).fg(p.good).bold()
            } else {
                Span::from(format!("○ {label}")).fg(p.dim)
            }
        };
        let mut lines = vec![Line::from(vec![
            Span::from(format!("{:<13}", "Main agent")).fg(p.muted),
            if live.main_agent_working {
                let step = live.main_agent_step.trim();
                let label = if step.is_empty() {
                    "working".to_string()
                } else {
                    let room = (inner.width as usize).saturating_sub(13 + 12);
                    format!("working · {}", truncate(step, room))
                };
                status(true, &label)
            } else {
                status(false, "idle")
            },
        ])];
        if let Some(external) = &live.external_agent {
            lines.push(Line::from(vec![
                Span::from(format!("{:<13}", "Subscription")).fg(p.muted),
                Span::from(external.clone()).fg(p.text),
            ]));
        }
        lines.push(Line::from(vec![
            Span::from(format!("{:<13}", "Subagents")).fg(p.muted),
            Span::from(format!("{running} running")).fg(if running > 0 { p.good } else { p.text }),
            Span::from(format!(" · {finished} finished")).fg(p.muted),
        ]));
        let width = inner.width as usize;
        let mut agents: Vec<&SubAgent> = live.sub_agents.iter().collect();
        agents.sort_by_key(|a| a.state != SubAgentState::Running);
        for agent in agents {
            let (mark, color) = match agent.state {
                SubAgentState::Running => ("▶", p.good),
                SubAgentState::Finished => ("✓", p.dim),
            };
            let id = truncate(&agent.id, 10);
            let desc_w = width.saturating_sub(id.chars().count() + 5);
            lines.push(Line::from(vec![
                Span::from(format!("  {mark} ")).fg(color),
                Span::from(format!("{id} ")).fg(p.muted),
                Span::from(truncate(&agent.description, desc_w)).fg(p.text),
            ]));
        }
        frame.render_widget(Paragraph::new(lines), inner);
    }

    fn render_capabilities(&self, frame: &mut Frame, area: Rect, live: &LiveStatus) {
        let p = self.p();
        let block = self.panel("Skills · MCP · extensions");
        let inner = block.inner(area).inner(Margin::new(1, 0));
        frame.render_widget(block, area);
        let width = inner.width as usize;
        let label = |text: &str| Span::from(format!("{text:<13}")).fg(p.muted);
        let mut skills = format!("{} loaded", live.skills_loaded.len());
        if !live.skills_active.is_empty() {
            skills.push_str(&format!(" · active: {}", live.skills_active.join(", ")));
        }
        if live.skills_disabled > 0 {
            skills.push_str(&format!(" · {} off", live.skills_disabled));
        }
        let connected = live.mcp_servers.iter().filter(|s| s.connected).count();
        let tools: usize = live.mcp_servers.iter().map(|s| s.tools).sum();
        let mut lines = vec![
            Line::from(vec![
                label("Skills"),
                Span::from(truncate(&skills, width.saturating_sub(13))).fg(p.text),
            ]),
            Line::from(vec![
                label("MCP servers"),
                Span::from(format!(
                    "{connected} of {} connected · {tools} tools",
                    live.mcp_servers.len()
                ))
                .fg(p.text),
            ]),
        ];
        let name_w = live
            .mcp_servers
            .iter()
            .map(|s| s.name.chars().count())
            .max()
            .unwrap_or(0)
            .min(20);
        for server in &live.mcp_servers {
            let (mark, color, detail) = if server.connected {
                ("●", p.good, format!("{} tools", server.tools))
            } else {
                ("○", p.dim, "not connected".to_string())
            };
            lines.push(Line::from(vec![
                Span::from(format!("  {mark} ")).fg(color),
                Span::from(format!("{:<name_w$} ", truncate(&server.name, name_w))).fg(p.text),
                Span::from(detail).fg(p.muted),
            ]));
        }
        lines.push(Line::from(vec![
            label("Extensions"),
            Span::from(live.extensions.to_string()).fg(p.text),
            Span::from("   hooks ").fg(p.muted),
            Span::from(live.hooks.to_string()).fg(p.text),
        ]));
        frame.render_widget(Paragraph::new(lines), inner);
    }

    /// Today's totals from the usage log, under the live panels.
    fn render_today(&self, frame: &mut Frame, area: Rect) {
        let today = Local::now().format("%Y-%m-%d").to_string();
        let day = self
            .summary
            .days
            .iter()
            .find(|d| d.date == today)
            .cloned()
            .unwrap_or_default();
        let tiles = [
            Tile::new(
                "Tokens today",
                compact(day.input_tokens + day.output_tokens),
                String::new(),
            ),
            Tile::new("Model calls", thousands(day.llm_calls), String::new()),
            Tile::new("Tool calls", thousands(day.tool_calls), String::new()),
            Tile::new(
                "Agent runs",
                format!(
                    "{} · {} sub",
                    thousands(day.runs),
                    thousands(day.sub_agent_runs)
                ),
                String::new(),
            ),
        ];
        self.tile_row(frame, area, &tiles);
    }

    // ---------------------------------------------------------------- usage

    fn render_usage(&self, frame: &mut Frame, area: Rect) {
        let tiles_h = if area.width >= 108 { 4 } else { 7 };
        let bottom_h = if area.height >= 30 { 10 } else { 8 };
        let [tiles, charts, bottom] = Layout::vertical([
            Constraint::Length(tiles_h),
            Constraint::Min(8),
            Constraint::Length(bottom_h),
        ])
        .areas(area);
        self.render_usage_tiles(frame, tiles);

        let [tokens, activity] =
            Layout::horizontal([Constraint::Percentage(55), Constraint::Percentage(45)])
                .areas(charts);
        let days = fill_days(&self.summary.days, self.range, Local::now().date_naive());
        let peak = days
            .iter()
            .max_by_key(|d| d.input + d.output)
            .filter(|d| d.input + d.output > 0);
        let title = match peak {
            Some(d) => format!(
                "Tokens per day · peak {} on {}",
                compact(d.input + d.output),
                d.label
            ),
            None => "Tokens per day".into(),
        };
        self.daily_histogram(
            frame,
            tokens,
            &title,
            "Tokens",
            &days,
            &[
                Series {
                    name: "input",
                    value: |d| d.input as f64,
                },
                Series {
                    name: "output",
                    value: |d| d.output as f64,
                },
            ],
        );
        self.daily_histogram(
            frame,
            activity,
            "Calls per day",
            "Calls",
            &days,
            &[
                Series {
                    name: "model",
                    value: |d| d.llm_calls as f64,
                },
                Series {
                    name: "tool",
                    value: |d| d.tool_calls as f64,
                },
            ],
        );

        let [hours, tools, models] = Layout::horizontal([
            Constraint::Percentage(34),
            Constraint::Percentage(33),
            Constraint::Percentage(33),
        ])
        .areas(bottom);
        self.render_hours(frame, hours);
        self.render_top_tools(frame, tools);
        self.render_top_models(frame, models);
    }

    fn render_usage_tiles(&self, frame: &mut Frame, area: Rect) {
        let t = &self.summary.totals;
        let cached_share = if t.input_tokens > 0 {
            format!(
                "{:.0}% of input",
                100.0 * t.cache_read_tokens as f64 / t.input_tokens as f64
            )
        } else {
            String::new()
        };
        let tiles = [
            Tile::new(
                "Tokens",
                compact(t.input_tokens + t.output_tokens),
                format!(
                    "{} in · {} out",
                    compact(t.input_tokens),
                    compact(t.output_tokens)
                ),
            ),
            Tile::new("Cached", compact(t.cache_read_tokens), cached_share),
            Tile::new(
                "Model calls",
                thousands(t.llm_calls),
                format!("avg {}", duration(weighted_latency(&self.summary))),
            ),
            Tile::new(
                "Tool calls",
                thousands(t.tool_calls),
                format!("{} via MCP", thousands(t.mcp_calls)),
            ),
            Tile::new(
                "Agent runs",
                thousands(t.runs + t.sub_agent_runs),
                format!("{} subagents", thousands(t.sub_agent_runs)),
            ),
            Tile::new(
                "Sessions",
                thousands(t.sessions),
                format!("{} skill uses", thousands(t.skill_uses)),
            ),
        ];
        self.tile_row(frame, area, &tiles);
    }

    /// Bordered stat tiles: one row, or two rows when the area is tall enough.
    fn tile_row(&self, frame: &mut Frame, area: Rect, tiles: &[Tile]) {
        let p = self.p();
        let rows: Vec<Rect> = if area.height >= 7 && tiles.len() > 3 {
            Layout::vertical([Constraint::Length(4), Constraint::Length(3)])
                .split(area)
                .to_vec()
        } else {
            vec![area]
        };
        let per_row = tiles.len().div_ceil(rows.len());
        for (r, row) in rows.iter().enumerate() {
            let cells =
                Layout::horizontal(vec![Constraint::Ratio(1, per_row as u32); per_row]).split(*row);
            for (i, cell) in cells.iter().enumerate() {
                let Some(Tile { label, value, note }) = tiles.get(r * per_row + i) else {
                    continue;
                };
                let block = Block::bordered()
                    .border_type(BorderType::Rounded)
                    .border_style(Style::new().fg(p.border))
                    .title(Line::from(format!(" {label} ")).fg(p.muted));
                let mut lines = vec![Line::from(Span::from(value.clone()).fg(p.text).bold())];
                if !note.is_empty() {
                    if cell.height >= 4 {
                        lines.push(Line::from(note.clone()).fg(p.dim));
                    } else {
                        lines[0].push_span(Span::from(format!("  {note}")).fg(p.dim));
                    }
                }
                frame.render_widget(Paragraph::new(lines).block(block), *cell);
            }
        }
    }

    /// Stacked histogram of daily series: one bar per day, named axes.
    fn daily_histogram(
        &self,
        frame: &mut Frame,
        area: Rect,
        title: &str,
        y_name: &str,
        days: &[DayPoint],
        series: &[Series],
    ) {
        let p = self.p();
        let colors = [p.series_1, p.series_2];
        let last = days.len().saturating_sub(1);
        self.histogram(
            frame,
            area,
            &Histogram {
                title,
                y_name,
                x_name: "Day",
                series: &series
                    .iter()
                    .zip(colors)
                    .map(|(series, color)| Legend {
                        name: series.name,
                        color,
                    })
                    .collect::<Vec<_>>(),
                bars: &days
                    .iter()
                    .map(|d| {
                        series
                            .iter()
                            .map(|series| (series.value)(d) as u64)
                            .collect()
                    })
                    .collect::<Vec<_>>(),
                labels: &days.iter().map(|d| d.label.clone()).collect::<Vec<_>>(),
                ticks: &[0, last / 2, last],
            },
        );
    }

    /// Bar chart drawn cell by cell: series stacked bottom to top in eighth-block steps,
    /// y axis with its name and 0 / half / max ticks, x axis with its name and tick labels.
    pub(super) fn histogram(&self, frame: &mut Frame, area: Rect, h: &Histogram) {
        const LOWER: [&str; 9] = [" ", "▁", "▂", "▃", "▄", "▅", "▆", "▇", "█"];
        let p = self.p();
        let block = self.panel(h.title);
        let inner = block.inner(area).inner(Margin::new(1, 0));
        frame.render_widget(block, area);
        if inner.height < 4 || inner.width < 16 || h.bars.is_empty() {
            return;
        }
        let max_total = h
            .bars
            .iter()
            .map(|b| b.iter().sum::<u64>())
            .max()
            .unwrap_or(0);
        let max = nice_max(max_total as f64);
        let ticks = [compact_f(max), compact_f(max / 2.0), compact_f(0.0)];
        let gutter = ticks
            .iter()
            .map(|t| t.chars().count())
            .chain([h.y_name.chars().count()])
            .max()
            .unwrap_or(0) as u16;
        // Rows: names/legend, plot, x axis line, x tick labels.
        let legend_y = inner.y;
        let axis_y = inner.bottom() - 2;
        let label_y = inner.bottom() - 1;
        let plot_top = inner.y + 1;
        let plot_h = axis_y - plot_top;
        let plot_x = inner.x + gutter + 1;
        let plot_w = inner.right().saturating_sub(plot_x);
        if plot_h == 0 || plot_w == 0 {
            return;
        }
        let buf = frame.buffer_mut();
        let muted = Style::new().fg(p.muted);
        let axis = Style::new().fg(p.border);

        // Y axis name, then the legend after it.
        buf.set_string(
            inner.x,
            legend_y,
            h.y_name,
            muted.add_modifier(Modifier::BOLD),
        );
        if h.series.len() > 1 {
            let mut x = plot_x + 1;
            for Legend { name, color } in h.series {
                if x + 3 + name.chars().count() as u16 > inner.right() {
                    break;
                }
                buf.set_string(x, legend_y, "■", Style::new().fg(*color));
                buf.set_string(x + 2, legend_y, name, muted);
                x += 2 + name.chars().count() as u16 + 3;
            }
        }

        // X axis line (its name goes at the right end, below).
        buf.set_string(
            plot_x - 1,
            axis_y,
            format!("└{}", "─".repeat(plot_w as usize)),
            axis,
        );

        // Y axis: line with max, half and zero ticks.
        for y in plot_top..axis_y {
            buf.set_string(plot_x - 1, y, "│", axis);
        }
        // Max at the top edge, half at the middle row, zero on the x axis itself.
        let tick_rows = [plot_top, plot_top + plot_h / 2, axis_y];
        for (i, (tick, y)) in ticks.iter().zip(tick_rows).enumerate() {
            // Too short for a separate half-way row.
            if i == 1 && plot_h < 3 {
                continue;
            }
            let x = inner.x + gutter - tick.chars().count() as u16;
            buf.set_string(x, y, tick, muted);
            if y != axis_y {
                buf.set_string(plot_x - 1, y, "┤", axis);
            }
        }

        // X axis name at the right end of the axis line.
        let x_name = format!(" {} ", h.x_name);
        let x_name_w = x_name.chars().count() as u16;
        if x_name_w + 4 < plot_w {
            buf.set_string(
                inner.right() - x_name_w,
                axis_y,
                &x_name,
                muted.add_modifier(Modifier::BOLD),
            );
        }

        // Bars.
        let n = h.bars.len() as u16;
        let slot = |i: u16| plot_x + (u32::from(i) * u32::from(plot_w) / u32::from(n)) as u16;
        let mut last_label_end = 0u16;
        for (i, values) in h.bars.iter().enumerate() {
            let i = i as u16;
            let (x0, x1) = (slot(i), slot(i + 1).max(slot(i) + 1));
            let gap = u16::from(x1 - x0 >= 3);
            let width = (x1 - x0 - gap).max(1);
            // Cumulative heights in eighths of a row.
            let scale = f64::from(plot_h) * 8.0 / max;
            let mut bounds = Vec::with_capacity(values.len());
            let mut sum = 0u64;
            for v in values {
                sum += v;
                let mut eighths = (sum as f64 * scale).round() as u32;
                if sum > 0 && eighths == 0 {
                    eighths = 1;
                }
                bounds.push(eighths);
            }
            let top = bounds.last().copied().unwrap_or(0);
            for row in 0..u32::from(plot_h) {
                let (lo, hi) = (row * 8, row * 8 + 8);
                if top <= lo {
                    break;
                }
                // Series covering the bottom of this cell, and where it ends.
                let s = bounds.iter().position(|b| *b > lo).unwrap_or(0);
                let color = h.series.get(s).map_or(p.series_1, |l| l.color);
                let (symbol, style) = if bounds[s] >= hi {
                    ("█", Style::new().fg(color))
                } else if top <= hi || s + 1 >= bounds.len() {
                    (LOWER[(top.min(hi) - lo) as usize], Style::new().fg(color))
                } else {
                    // Two series meet inside this cell: lower one in front, upper behind.
                    let upper = h.series.get(s + 1).map_or(color, |l| l.color);
                    (
                        LOWER[(bounds[s] - lo) as usize],
                        Style::new().fg(color).bg(upper),
                    )
                };
                let y = axis_y - 1 - row as u16;
                for x in x0..(x0 + width).min(inner.right()) {
                    buf[(x, y)].set_symbol(symbol).set_style(style);
                }
            }
            // X tick label centred under its bar, when it does not collide.
            if h.ticks.contains(&usize::from(i)) {
                let label = &h.labels[usize::from(i)];
                let len = label.chars().count() as u16;
                let start = (x0 + width / 2)
                    .saturating_sub(len / 2)
                    .clamp(plot_x, inner.right().saturating_sub(len));
                if start >= last_label_end {
                    buf.set_string(start, label_y, label, muted);
                    last_label_end = start + len + 1;
                }
            }
        }
    }

    fn render_hours(&self, frame: &mut Frame, area: Rect) {
        let hours = &self.summary.hours;
        let busiest = (0..24).max_by_key(|h| hours[*h]).unwrap_or(0);
        let title = if area.width >= 42 {
            format!("Model calls by hour · busiest {busiest:02}:00")
        } else {
            format!("By hour · peak {busiest:02}:00")
        };
        self.histogram(
            frame,
            area,
            &Histogram {
                title: &title,
                y_name: "Calls",
                x_name: "Hour",
                series: &[Legend {
                    name: "model calls",
                    color: self.p().series_1,
                }],
                bars: &hours.iter().map(|n| vec![*n]).collect::<Vec<_>>(),
                labels: &(0..24).map(|h| format!("{h:02}")).collect::<Vec<_>>(),
                ticks: &[0, 6, 12, 18],
            },
        );
    }

    /// Ranked horizontal bars in aligned columns: label, bar, value, note.
    fn bar_list(&self, frame: &mut Frame, area: Rect, title: &str, items: &[BarItem]) {
        let p = self.p();
        let block = self.panel(title);
        let inner = block.inner(area).inner(Margin::new(1, 0));
        frame.render_widget(block, area);
        if items.is_empty() {
            frame.render_widget(Paragraph::new("Nothing in this period").fg(p.dim), inner);
            return;
        }
        let width = inner.width as usize;
        let widest = |f: fn(&BarItem) -> &str| {
            items
                .iter()
                .map(|i| f(i).chars().count())
                .max()
                .unwrap_or(0)
        };
        let label_w = widest(|i| &i.label).clamp(4, 24).min(width / 2);
        let value_w = widest(|i| &i.value_text);
        let note_w = match widest(|i| &i.note) {
            0 => 0,
            w => w + 1,
        };
        let mut bar_w = width.saturating_sub(label_w + 2 + value_w + note_w);
        let note_w = if bar_w < 4 { 0 } else { note_w };
        if note_w == 0 {
            bar_w = width.saturating_sub(label_w + 2 + value_w);
        }
        let max = items
            .iter()
            .map(|i| i.value)
            .fold(0.0, f64::max)
            .max(f64::EPSILON);
        let lines: Vec<Line> = items
            .iter()
            .take(inner.height as usize)
            .map(|i| {
                Line::from(vec![
                    Span::from(format!("{:<label_w$} ", truncate(&i.label, label_w))).fg(p.muted),
                    Span::from(format!("{:<bar_w$}", bar(i.value / max, bar_w))).fg(p.series_1),
                    Span::from(format!(" {:>value_w$}", i.value_text)).fg(p.text),
                    Span::from(if note_w > 0 {
                        format!("{:>note_w$}", i.note)
                    } else {
                        String::new()
                    })
                    .fg(p.dim),
                ])
            })
            .collect();
        frame.render_widget(Paragraph::new(lines), inner);
    }

    fn render_top_tools(&self, frame: &mut Frame, area: Rect) {
        let items: Vec<BarItem> = self
            .summary
            .tools
            .iter()
            .map(|t| BarItem::new(&t.tool, t.calls as f64, thousands(t.calls)))
            .collect();
        self.bar_list(frame, area, "Top tools", &items);
    }

    fn render_top_models(&self, frame: &mut Frame, area: Rect) {
        let total =
            (self.summary.totals.input_tokens + self.summary.totals.output_tokens).max(1) as f64;
        let mut models: Vec<&ModelRow> = self.summary.models.iter().collect();
        models.sort_by_key(|m| std::cmp::Reverse(m.input_tokens + m.output_tokens));
        let items: Vec<BarItem> = models
            .iter()
            .map(|m| {
                let tokens = m.input_tokens + m.output_tokens;
                BarItem::new(&m.model, tokens as f64, compact(tokens))
                    .note(format!("{:.0}%", 100.0 * tokens as f64 / total))
            })
            .collect();
        self.bar_list(frame, area, "Models by tokens", &items);
    }

    // ---------------------------------------------------------------- tables

    /// A table that keeps the columns that fit `width`, dropping the lowest `keep` first.
    fn table(
        &self,
        title: String,
        columns: &[Column],
        rows: Vec<Vec<Cell<'static>>>,
        width: u16,
    ) -> Table<'static> {
        let p = self.p();
        let kept = fit_columns(columns, width);
        let widths: Vec<Constraint> = kept
            .iter()
            .map(|&i| match columns[i].fill {
                true => Constraint::Min(columns[i].width),
                false => Constraint::Length(columns[i].width),
            })
            .collect();
        let header = Row::new(kept.iter().map(|&i| {
            let h = columns[i].header;
            match h.strip_prefix('>') {
                Some(text) => Cell::from(Line::from(text).alignment(Alignment::Right)),
                None => Cell::from(h),
            }
        }))
        .style(Style::new().fg(p.muted).add_modifier(Modifier::BOLD))
        .bottom_margin(1);
        let rows: Vec<Row> = rows
            .into_iter()
            .map(|cells| {
                Row::new(
                    cells
                        .into_iter()
                        .enumerate()
                        .filter(|(i, _)| kept.contains(i))
                        .map(|(_, cell)| cell),
                )
            })
            .collect();
        Table::new(rows, widths)
            .header(header)
            .block(self.panel(&title))
            .column_spacing(2)
            .flex(Flex::Start)
            .row_highlight_style(Style::new().bg(p.selected_bg))
            .highlight_symbol("▌ ")
    }

    /// Table on top, sized to its rows; comparison panels (one row per item) below when
    /// there is room.
    fn split_table_area(area: Rect, rows: usize, panels: usize) -> (Rect, Vec<Rect>) {
        let table_h = (rows as u16 + 4).max(6);
        if area.height < table_h + 5 {
            return (area, Vec::new());
        }
        let table_h = table_h.min(area.height / 2).max(6);
        let panel_h = (rows as u16 + 2).max(4).min(area.height - table_h);
        let [table, rest, _] = Layout::vertical([
            Constraint::Length(table_h),
            Constraint::Length(panel_h),
            Constraint::Min(0),
        ])
        .areas(area);
        let cells = Layout::horizontal(vec![Constraint::Ratio(1, panels as u32); panels])
            .split(rest)
            .to_vec();
        (table, cells)
    }

    fn render_models(&mut self, frame: &mut Frame, area: Rect) {
        let p = self.p();
        let models = &self.summary.models;
        let total_tokens =
            (self.summary.totals.input_tokens + self.summary.totals.output_tokens).max(1) as f64;
        let columns = [
            Column::text(
                "Model",
                column_width(models.iter().map(|m| m.model.as_str()), "Model", 32),
            ),
            Column::new("Provider", 10, 2),
            Column::new(">Calls", 6, 9),
            Column::new(">Input", 7, 8),
            Column::new(">Output", 7, 7),
            Column::new(">Cached", 7, 4),
            Column::new(">Avg", 7, 6),
            Column::new(">p95", 7, 3),
            Column::new(">1st token", 9, 5),
        ];
        let rows: Vec<Vec<Cell>> = models
            .iter()
            .map(|m| {
                vec![
                    Cell::from(m.model.clone()).fg(p.text),
                    Cell::from(if m.provider.is_empty() {
                        "–".into()
                    } else {
                        m.provider.clone()
                    })
                    .fg(p.muted),
                    right(thousands(m.calls)),
                    right(compact(m.input_tokens)),
                    right(compact(m.output_tokens)),
                    right(compact(m.cache_read_tokens)),
                    right(duration(m.avg_latency_ms)),
                    right(duration(m.p95_latency_ms)),
                    right(m.avg_ttft_ms.map_or("–".into(), duration)),
                ]
            })
            .collect();
        let count = rows.len();
        let (table_area, panels) = Self::split_table_area(area, count, 3);
        let table = self.table(format!("Models · {count}"), &columns, rows, area.width);
        frame.render_stateful_widget(table, table_area, &mut self.table);
        if let [latency, ttft, tokens] = panels.as_slice() {
            let models = &self.summary.models;
            let by = |value: fn(&ModelRow) -> f64| {
                let mut sorted: Vec<&ModelRow> = models.iter().collect();
                sorted.sort_by(|a, b| value(b).total_cmp(&value(a)));
                sorted
            };
            let items: Vec<BarItem> = by(|m| m.p95_latency_ms)
                .iter()
                .map(|m| {
                    BarItem::new(&m.model, m.p95_latency_ms, duration(m.p95_latency_ms))
                        .note(format!("avg {}", duration(m.avg_latency_ms)))
                })
                .collect();
            self.bar_list(frame, *latency, "Latency · p95", &items);
            let items: Vec<BarItem> = by(|m| m.avg_ttft_ms.unwrap_or(0.0))
                .iter()
                .filter_map(|m| {
                    let ttft = m.avg_ttft_ms?;
                    Some(BarItem::new(&m.model, ttft, duration(ttft)))
                })
                .collect();
            self.bar_list(frame, *ttft, "First token · avg", &items);
            let items: Vec<BarItem> = by(|m| (m.input_tokens + m.output_tokens) as f64)
                .iter()
                .map(|m| {
                    let tokens = m.input_tokens + m.output_tokens;
                    BarItem::new(&m.model, tokens as f64, compact(tokens))
                        .note(format!("{:.0}%", 100.0 * tokens as f64 / total_tokens))
                })
                .collect();
            self.bar_list(frame, *tokens, "Tokens", &items);
        }
    }

    fn render_tools(&mut self, frame: &mut Frame, area: Rect) {
        let p = self.p();
        let tools = &self.summary.tools;
        let max = tools.first().map_or(1, |t| t.calls).max(1);
        let columns = [
            Column::text(
                "Tool",
                column_width(tools.iter().map(|t| t.tool.as_str()), "Tool", 32),
            ),
            Column::new(
                "Source",
                column_width(tools.iter().map(|t| t.source.as_str()), "Source", 16),
                6,
            ),
            Column::new(">Calls", 7, 9),
            Column::new("", 16, 1),
            Column::new(">Avg", 8, 5),
            Column::new(">p95", 8, 4),
        ];
        let rows: Vec<Vec<Cell>> = tools
            .iter()
            .map(|t| {
                vec![
                    Cell::from(t.tool.clone()).fg(p.text),
                    Cell::from(t.source.clone()).fg(if t.source == BUILT_IN {
                        p.dim
                    } else {
                        p.muted
                    }),
                    right(thousands(t.calls)),
                    Cell::from(bar(t.calls as f64 / max as f64, 16)).fg(p.series_1),
                    right(duration(t.avg_ms)),
                    right(duration(t.p95_ms)),
                ]
            })
            .collect();
        let count = rows.len();
        let (table_area, panels) = Self::split_table_area(area, count, 2);
        let table = self.table(format!("Tools · {count}"), &columns, rows, area.width);
        frame.render_stateful_widget(table, table_area, &mut self.table);
        if let [slowest, sources] = panels.as_slice() {
            let tools = &self.summary.tools;
            let mut by_p95: Vec<&ToolRow> = tools.iter().collect();
            by_p95.sort_by(|a, b| b.p95_ms.total_cmp(&a.p95_ms));
            let items: Vec<BarItem> = by_p95
                .iter()
                .map(|t| {
                    BarItem::new(&t.tool, t.p95_ms, duration(t.p95_ms))
                        .note(format!("avg {}", duration(t.avg_ms)))
                })
                .collect();
            self.bar_list(frame, *slowest, "Slowest · p95", &items);
            let mut by_source: Vec<(String, u64)> = Vec::new();
            for t in tools {
                match by_source.iter_mut().find(|(s, _)| *s == t.source) {
                    Some((_, calls)) => *calls += t.calls,
                    None => by_source.push((t.source.clone(), t.calls)),
                }
            }
            by_source.sort_by_key(|(_, calls)| std::cmp::Reverse(*calls));
            let total = self.summary.totals.tool_calls.max(1) as f64;
            let items: Vec<BarItem> = by_source
                .iter()
                .map(|(source, calls)| {
                    let label = if source == BUILT_IN {
                        BUILT_IN.to_string()
                    } else {
                        format!("mcp · {source}")
                    };
                    BarItem::new(&label, *calls as f64, thousands(*calls))
                        .note(format!("{:.0}%", 100.0 * *calls as f64 / total))
                })
                .collect();
            self.bar_list(frame, *sources, "Calls by source", &items);
        }
    }

    // ---------------------------------------------------------------- agents

    fn render_agents(&self, frame: &mut Frame, area: Rect) {
        let t = &self.summary.totals;
        let tiles = [
            Tile::new(
                "Main agent",
                thousands(t.runs),
                format!("runs in {} sessions", thousands(t.sessions)),
            ),
            Tile::new(
                "Subagents",
                thousands(t.sub_agent_runs),
                format!("{} spawned", thousands(t.sub_agents_spawned)),
            ),
            Tile::new(
                "Skills",
                thousands(t.skill_uses),
                format!("uses of {} skills", self.summary.skills.len()),
            ),
            Tile::new(
                "MCP",
                thousands(t.mcp_calls),
                format!("calls to {} servers", self.summary.mcp_servers.len()),
            ),
        ];
        let lists_h = (self
            .summary
            .skills
            .len()
            .max(self.summary.mcp_servers.len())
            .clamp(1, 10)
            + 2) as u16;
        let [tiles_area, chart, lists] = Layout::vertical([
            Constraint::Length(4),
            Constraint::Min(8),
            Constraint::Length(lists_h.max(4)),
        ])
        .areas(area);
        self.tile_row(frame, tiles_area, &tiles);
        let days = fill_days(&self.summary.days, self.range, Local::now().date_naive());
        self.daily_histogram(
            frame,
            chart,
            "Agent runs per day",
            "Runs",
            &days,
            &[
                Series {
                    name: "main",
                    value: |d| d.runs as f64,
                },
                Series {
                    name: "subagents",
                    value: |d| d.sub_agent_runs as f64,
                },
            ],
        );
        let [skills, servers] =
            Layout::horizontal([Constraint::Percentage(50), Constraint::Percentage(50)])
                .areas(lists);
        let items: Vec<BarItem> = self
            .summary
            .skills
            .iter()
            .map(|s| {
                BarItem::new(&s.skill, s.uses as f64, thousands(s.uses)).note(format!(
                    "{} session{}",
                    s.sessions,
                    if s.sessions == 1 { "" } else { "s" }
                ))
            })
            .collect();
        self.bar_list(frame, skills, "Skills used", &items);
        let items: Vec<BarItem> = self
            .summary
            .mcp_servers
            .iter()
            .map(|s| {
                BarItem::new(&s.server, s.calls as f64, thousands(s.calls)).note(format!(
                    "{} tool{} · avg {}",
                    s.tools,
                    if s.tools == 1 { "" } else { "s" },
                    duration(s.avg_ms)
                ))
            })
            .collect();
        self.bar_list(frame, servers, "MCP servers", &items);
    }

    // ---------------------------------------------------------------- sessions

    fn render_sessions(&mut self, frame: &mut Frame, area: Rect) {
        let p = self.p();
        let now = Utc::now();
        let sessions = &self.summary.sessions;
        let columns = [
            Column::text(
                "Session",
                column_width(
                    sessions.iter().map(|s| s.session_id.as_str()),
                    "Session",
                    36,
                ),
            ),
            Column::new("Last active", 11, 9),
            Column::new(">Runs", 4, 5),
            Column::new(">Subagents", 10, 4),
            Column::new(">Calls", 5, 7),
            Column::new(">Tools", 5, 6),
            Column::new(">Tokens", 7, 8),
            Column {
                fill: true,
                ..Column::new("Models", 10, 3)
            },
        ];
        let rows: Vec<Vec<Cell>> = sessions
            .iter()
            .map(|s| {
                vec![
                    Cell::from(s.session_id.clone()).fg(p.text),
                    Cell::from(ago(s.last_active, now)).fg(p.muted),
                    right(thousands(s.runs)),
                    right(thousands(s.sub_agents_spawned)),
                    right(thousands(s.llm_calls)),
                    right(thousands(s.tool_calls)),
                    right(compact(s.tokens)),
                    Cell::from(s.models.join(", ")).fg(p.muted),
                ]
            })
            .collect();
        let shown = rows.len();
        let total = self.summary.totals.sessions;
        let title = if total as usize > shown {
            format!("Recent sessions · {shown} of {total}")
        } else {
            format!("Sessions · {total}")
        };
        let table = self.table(title, &columns, rows, area.width);
        frame.render_stateful_widget(table, area, &mut self.table);
    }
}
