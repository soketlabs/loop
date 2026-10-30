use ratatui::backend::TestBackend;

use super::widgets::*;
use super::*;
use crate::aggregate::{McpRow, SessionRow, SkillRow, Totals};

fn summary() -> Summary {
    let today = Local::now().date_naive();
    Summary {
        files: 2,
        totals: Totals {
            sessions: 3,
            runs: 4,
            sub_agent_runs: 2,
            sub_agents_spawned: 2,
            llm_calls: 20,
            tool_calls: 30,
            mcp_calls: 5,
            skill_uses: 3,
            input_tokens: 1_234_567,
            output_tokens: 45_678,
            cache_read_tokens: 400_000,
            reasoning_tokens: 0,
        },
        days: vec![DayRow {
            date: today.format("%Y-%m-%d").to_string(),
            llm_calls: 20,
            tool_calls: 30,
            input_tokens: 1_234_567,
            output_tokens: 45_678,
            runs: 4,
            sub_agent_runs: 2,
        }],
        hours: std::array::from_fn(|h| (h as u64 * 7) % 11),
        models: vec![ModelRow {
            model: "sarvam-m".into(),
            provider: "soket".into(),
            calls: 20,
            input_tokens: 1_234_567,
            output_tokens: 45_678,
            cache_read_tokens: 400_000,
            avg_latency_ms: 2400.0,
            p95_latency_ms: 6100.0,
            avg_ttft_ms: Some(640.0),
        }],
        tools: vec![
            ToolRow {
                tool: "read".into(),
                source: BUILT_IN.into(),
                calls: 25,
                avg_ms: 12.0,
                p95_ms: 40.0,
            },
            ToolRow {
                tool: "mcp__github__list_issues".into(),
                source: "github".into(),
                calls: 5,
                avg_ms: 1500.0,
                p95_ms: 4000.0,
            },
        ],
        mcp_servers: vec![McpRow {
            server: "github".into(),
            calls: 5,
            tools: 1,
            avg_ms: 1500.0,
        }],
        skills: vec![SkillRow {
            skill: "pdf-tools".into(),
            uses: 3,
            sessions: 2,
        }],
        sessions: vec![SessionRow {
            session_id: "0192aaaa-bbbb".into(),
            started: Utc::now(),
            last_active: Utc::now(),
            runs: 4,
            llm_calls: 20,
            tool_calls: 30,
            sub_agents_spawned: 2,
            tokens: 1_280_245,
            models: vec!["sarvam-m".into()],
        }],
        ..Default::default()
    }
}

fn live() -> LiveStatus {
    LiveStatus {
        taken_at: Local::now(),
        model: "soket/sarvam-m".into(),
        effort: Some("high".into()),
        permission_mode: "ask".into(),
        sandbox: "off".into(),
        web: "on".into(),
        cwd: "F:/soket labs/loop".into(),
        session_id: "0192aaaa-bbbb".into(),
        context_tokens: Some(54_000),
        context_window: 128_000,
        session_tokens: 1_200_000,
        main_agent_working: true,
        main_agent_step: "tool · bash".into(),
        external_agent: None,
        sub_agents: vec![
            SubAgent {
                id: "t1".into(),
                description: "Survey the repo".into(),
                state: SubAgentState::Running,
            },
            SubAgent {
                id: "t2".into(),
                description: "Write the report".into(),
                state: SubAgentState::Finished,
            },
        ],
        skills_loaded: vec!["pdf-tools".into(), "review".into()],
        skills_active: vec!["review".into()],
        skills_disabled: 1,
        mcp_servers: vec![
            McpServerStatus {
                name: "github".into(),
                tools: 28,
                connected: true,
            },
            McpServerStatus {
                name: "slack".into(),
                tools: 0,
                connected: false,
            },
        ],
        extensions: 1,
        hooks: 2,
        usage_log: Some("/home/u/.loop/agent/usage/loop-1.jsonl".into()),
    }
}

fn app(summary: Summary, live: Option<LiveStatus>) -> App {
    let mut app = App::new(Options {
        dir: PathBuf::from("/nonexistent/usage"),
        palette: Palette::default(),
        notice: None,
        live,
    });
    app.summary = summary;
    app
}

fn draw(app: &mut App, w: u16, h: u16) -> String {
    let mut terminal = Terminal::new(TestBackend::new(w, h)).unwrap();
    terminal.draw(|f| app.render(f)).unwrap();
    let buf = terminal.backend().buffer();
    (0..h)
        .map(|y| (0..w).map(|x| buf[(x, y)].symbol()).collect::<String>())
        .collect::<Vec<_>>()
        .join("\n")
}

fn assert_shows(screen: &str, texts: &[&str]) {
    for text in texts {
        assert!(screen.contains(text), "missing {text:?}\n{screen}");
    }
}

#[test]
fn now_shows_session_context_agents_and_capabilities() {
    let mut app = app(summary(), Some(live()));
    assert_eq!(app.view, View::Now, "live status opens on Now");
    let screen = draw(&mut app, 140, 40);
    assert_shows(
        &screen,
        &[
            "soket/sarvam-m · effort high",
            "Permissions",
            "42% used · 54K of 128K",
            "Agents · 2 running",
            "● working · tool · bash",
            "1 running · 1 finished",
            "Survey the repo",
            "2 loaded · active: review · 1 off",
            "1 of 2 connected · 28 tools",
            "not connected",
            "Tokens today",
            "● live · ",
        ],
    );
}

#[test]
fn now_follows_live_updates() {
    let mut app = app(summary(), Some(live()));
    assert!(draw(&mut app, 140, 40).contains("Agents · 2 running"));
    let mut later = live();
    later.main_agent_working = false;
    later.sub_agents[0].state = SubAgentState::Finished;
    app.options.live = Some(later);
    let screen = draw(&mut app, 140, 40);
    assert_shows(
        &screen,
        &["Agents · 0 running", "○ idle", "0 running · 2 finished"],
    );
}

#[test]
fn now_without_live_status_explains_where_to_get_it() {
    let mut app = app(summary(), None);
    assert_eq!(app.view, View::Usage, "standalone opens on Usage");
    app.set_view(View::Now);
    assert!(draw(&mut app, 100, 30).contains("Live status needs a running loop"));
}

#[test]
fn usage_shows_tokens_activity_and_no_cost_or_errors() {
    let mut app = app(summary(), None);
    let screen = draw(&mut app, 140, 40);
    assert_shows(
        &screen,
        &[
            "Tokens",
            "1.3M",
            "Cached",
            "Agent runs",
            "2 subagents",
            "Tokens per day",
            "Calls per day",
            "Model calls by hour",
            "Top tools",
            "Models by tokens",
            "q close",
            // Histograms name both axes and their stacked series.
            "Tokens  ■ input   ■ output",
            "Calls  ■ model   ■ tool",
            " Day ",
            " Hour ",
        ],
    );
    for gone in ["Cost", "$", "Error"] {
        assert!(!screen.contains(gone), "{gone:?} should be gone\n{screen}");
    }
}

#[test]
fn agents_view_shows_runs_skills_and_mcp_servers() {
    let mut app = app(summary(), None);
    app.set_view(View::Agents);
    let screen = draw(&mut app, 140, 40);
    assert_shows(
        &screen,
        &[
            "Main agent",
            "Subagents",
            "2 spawned",
            "Agent runs per day",
            "Runs  ■ main   ■ subagents",
            "Skills used",
            "pdf-tools",
            "2 sessions",
            "MCP servers",
            "github",
        ],
    );
}

#[test]
fn every_view_draws_at_common_sizes_without_panicking() {
    for (w, h) in [(60, 18), (80, 24), (100, 30), (200, 60)] {
        for view in View::ALL {
            let mut app = app(summary(), Some(live()));
            app.view = view;
            let screen = draw(&mut app, w, h);
            assert!(
                screen.contains(view.label()),
                "{view:?} at {w}x{h}\n{screen}"
            );
        }
    }
}

#[test]
fn tables_list_models_tools_and_sessions() {
    let mut app = app(summary(), None);
    app.set_view(View::Models);
    let screen = draw(&mut app, 140, 30);
    assert_shows(&screen, &["sarvam-m", "Cached", "400K", "640ms", "6.1s"]);
    app.set_view(View::Tools);
    let screen = draw(&mut app, 140, 30);
    assert_shows(
        &screen,
        &[
            "mcp__github__list_issues",
            "Source",
            "Calls by source",
            "mcp · github",
        ],
    );
    app.set_view(View::Sessions);
    let screen = draw(&mut app, 140, 30);
    assert_shows(&screen, &["0192aaaa-bbbb", "just now", "Subagents"]);
}

#[test]
fn empty_and_tiny_terminals_explain_themselves() {
    let mut app = app(Summary::default(), None);
    assert!(draw(&mut app, 100, 30).contains("No usage recorded"));
    assert!(draw(&mut app, 40, 10).contains("too small"));
    // Now still works with no history.
    let mut app = self::app(Summary::default(), Some(live()));
    assert!(draw(&mut app, 100, 30).contains("Context window"));
}

#[test]
fn keys_switch_views_periods_scroll_and_quit() {
    let key = |code| KeyEvent::new(code, KeyModifiers::NONE);
    let mut app = app(summary(), None);
    app.handle_key(key(KeyCode::Char('5')));
    assert_eq!(app.view, View::Agents);
    app.handle_key(key(KeyCode::Tab));
    assert_eq!(app.view, View::Sessions);
    app.handle_key(key(KeyCode::Tab));
    assert_eq!(app.view, View::Now);
    app.handle_key(key(KeyCode::BackTab));
    assert_eq!(app.view, View::Sessions);
    app.handle_key(key(KeyCode::Left));
    assert_eq!(app.range, Range::Week);
    for _ in 0..3 {
        app.handle_key(key(KeyCode::Right));
    }
    assert_eq!(app.range, Range::All);
    assert_eq!(app.handle_key(key(KeyCode::Char('q'))), Action::Quit);
    assert_eq!(app.handle_key(key(KeyCode::Esc)), Action::Quit);
    assert_eq!(
        app.handle_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL)),
        Action::Quit
    );
}

#[test]
fn scrolling_is_clamped_to_rows() {
    let mut app = app(summary(), None);
    app.set_view(View::Tools);
    app.scroll(10);
    assert_eq!(app.table.selected(), Some(1));
    app.scroll(-10);
    assert_eq!(app.table.selected(), Some(0));
}

#[test]
fn fill_days_covers_the_period_with_idle_days() {
    let today = NaiveDate::from_ymd_opt(2026, 10, 26).unwrap();
    let rows = vec![DayRow {
        date: "2026-10-24".into(),
        input_tokens: 5,
        runs: 2,
        ..Default::default()
    }];
    let week = fill_days(&rows, Range::Week, today);
    assert_eq!(week.len(), 7);
    assert_eq!(week[6].label, "Oct 26");
    assert_eq!((week[4].input, week[4].runs), (5, 2));
    assert_eq!(fill_days(&rows, Range::All, today).len(), 3);
    assert_eq!(fill_days(&[], Range::All, today).len(), 1);
}

#[test]
fn histogram_stacks_series_and_labels_axes() {
    let app = app(Summary::default(), None);
    let mut terminal = Terminal::new(TestBackend::new(40, 12)).unwrap();
    terminal
        .draw(|f| {
            app.histogram(
                f,
                f.area(),
                &Histogram {
                    title: "Test",
                    y_name: "Tokens",
                    x_name: "Day",
                    series: &[
                        Legend {
                            name: "a",
                            color: Color::Blue,
                        },
                        Legend {
                            name: "b",
                            color: Color::Yellow,
                        },
                    ],
                    bars: &[vec![4, 4], vec![0, 0], vec![8, 0]],
                    labels: &["d1".into(), "d2".into(), "d3".into()],
                    ticks: &[0, 2],
                },
            )
        })
        .unwrap();
    let buf = terminal.backend().buffer();
    let row = |y: u16| (0..40).map(|x| buf[(x, y)].symbol()).collect::<String>();
    let screen = (0..12).map(row).collect::<Vec<_>>().join(
        "
",
    );
    for text in ["Tokens", "■ a", "■ b", "10┤", "0└", " Day ", "d1", "d3"] {
        assert!(
            screen.contains(text),
            "missing {text:?}
{screen}"
        );
    }
    // The first bar is half blue (bottom) and half yellow (top); the second is empty.
    let colors_in_column = |x: u16| {
        (0..12)
            .map(|y| &buf[(x, y)])
            .filter(|c| c.symbol() == "█")
            .map(|c| c.fg)
            .collect::<Vec<_>>()
    };
    let first_bar_x = (0..40).find(|&x| !colors_in_column(x).is_empty()).unwrap();
    let colors = colors_in_column(first_bar_x);
    assert!(
        colors.contains(&Color::Blue) && colors.contains(&Color::Yellow),
        "{colors:?}"
    );
    assert_eq!(colors.first(), Some(&Color::Yellow), "yellow sits on top");
}

#[test]
fn narrow_tables_drop_low_priority_columns_first() {
    let columns = [
        Column::text("Name", 20),
        Column::new("Low", 10, 1),
        Column::new("High", 10, 9),
        Column::new("Mid", 10, 5),
    ];
    assert_eq!(fit_columns(&columns, 200), [0, 1, 2, 3]);
    assert_eq!(fit_columns(&columns, 60), [0, 1, 2, 3], "exactly fits");
    assert_eq!(fit_columns(&columns, 55), [0, 2, 3]);
    assert_eq!(fit_columns(&columns, 40), [0, 2]);
    assert_eq!(
        fit_columns(&columns, 10),
        [0],
        "the first column always stays"
    );
}

#[test]
fn formatting() {
    assert_eq!(compact(950), "950");
    assert_eq!(compact(1_000), "1K");
    assert_eq!(compact(12_345), "12.3K");
    assert_eq!(compact(4_500_000), "4.5M");
    assert_eq!(thousands(1_234_567), "1,234,567");
    assert_eq!(duration(850.0), "850ms");
    assert_eq!(duration(4200.0), "4.2s");
    assert_eq!(duration(185_000.0), "3m 05s");
    assert_eq!(bar(1.0, 4), "████");
    assert_eq!(bar(0.5, 3), "█▌");
    assert_eq!(bar(0.001, 4), "▏");
    assert_eq!(gauge(0.5, 4), "██░░");
    assert_eq!(nice_max(7.3), 10.0);
    assert_eq!(nice_max(0.0), 1.0);
}
