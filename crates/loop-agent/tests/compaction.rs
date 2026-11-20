//! Compaction helper tests.

use loop_agent::harness::compaction::{
    compaction_limit, compaction_limit_with_reply_room, default_compaction_settings,
    effective_keep_recent_tokens, find_cut_point, prepare_compaction, should_compact,
};
use loop_agent::{convert_to_llm, AgentMessage};
use loop_ai::Message;

#[test]
fn should_compact_threshold() {
    let settings = default_compaction_settings();
    // reserve_tokens default 16384 → 110_000 + 16384 >= 120_000
    assert!(should_compact(110_000, 120_000, &settings));
    assert!(!should_compact(1000, 120_000, &settings));
}

#[test]
fn compacts_at_ninety_percent_of_the_window() {
    let settings = default_compaction_settings();
    assert_eq!(settings.trigger_percent, 90);
    // 1M window: 90% = 943_718; the old 16k reserve rule alone would wait until ~1_032_192.
    assert!(!should_compact(900_000, 1_048_576, &settings));
    assert!(should_compact(950_000, 1_048_576, &settings));
}

#[test]
fn configured_trigger_can_only_lower_the_ninety_percent_limit() {
    let mut settings = default_compaction_settings();
    settings.trigger_tokens = Some(100_000);
    assert_eq!(compaction_limit(1_048_576, &settings), Some(100_000));
    settings.trigger_tokens = Some(5_000_000);
    assert_eq!(compaction_limit(1_048_576, &settings), Some(943_718));
    // Unknown window: only the configured trigger applies.
    assert_eq!(compaction_limit(0, &settings), Some(5_000_000));
    settings.enabled = false;
    assert!(!should_compact(2_000_000, 1_048_576, &settings));
}

#[test]
fn cut_point_and_prepare() {
    let messages: Vec<AgentMessage> = (0..40)
        .map(|i| AgentMessage::user_text(format!("msg {i} {}", "x".repeat(800))))
        .collect();
    let llm = convert_to_llm(&messages);
    let cut = find_cut_point(&llm, 200);
    assert!(cut < llm.len());
    let mut settings = default_compaction_settings();
    settings.keep_recent_tokens = 200;
    let prep = prepare_compaction(&messages, &llm, &settings).unwrap();
    assert!(!prep.to_summarize.is_empty());
    assert!(!prep.retained.is_empty());
    let _ = Message::user_text("x");
}

#[test]
fn limit_edge_cases() {
    let mut settings = default_compaction_settings();
    // A percentage above 100 is clamped to the window itself.
    settings.trigger_percent = 250;
    assert_eq!(compaction_limit(10_000, &settings), Some(10_000));
    // Huge windows must not overflow.
    settings.trigger_percent = 90;
    assert_eq!(
        compaction_limit(u64::MAX, &settings),
        Some((u128::from(u64::MAX) * 90 / 100) as u64)
    );
    // Percent 0 and no configured trigger: nothing to compare against, never compacts.
    settings.trigger_percent = 0;
    assert_eq!(compaction_limit(1_000_000, &settings), None);
    assert!(!should_compact(u64::MAX, 1_000_000, &settings));
    // Percent 0 with a configured trigger: the configured trigger applies.
    settings.trigger_tokens = Some(500);
    assert_eq!(compaction_limit(1_000_000, &settings), Some(500));
    // A zero trigger_tokens is ignored, like unset.
    settings.trigger_tokens = Some(0);
    settings.trigger_percent = 90;
    assert_eq!(compaction_limit(1_000, &settings), Some(900));
    // Tiny windows still get a limit (rounded down).
    assert_eq!(compaction_limit(1, &settings), Some(0));
    assert!(should_compact(0, 1, &settings));
}

#[test]
fn limit_leaves_room_for_the_reply() {
    let mut settings = default_compaction_settings();
    let limit = |window, max_out| compaction_limit_with_reply_room(window, max_out, &settings);
    // 90% is the lower one: nothing changes.
    assert_eq!(limit(200_000, 16_384), Some(180_000));
    assert_eq!(limit(1_048_576, 65_536), Some(943_718));
    // The reply would not fit above 90%: window minus the maximum output.
    assert_eq!(limit(128_000, 16_384), Some(111_616));
    assert_eq!(limit(200_000, 64_000), Some(136_000));
    // A maximum output of more than half the window still leaves half the window.
    assert_eq!(limit(32_000, 16_384), Some(16_000));
    assert_eq!(limit(8_000, 16_384), Some(4_000));
    // Unknown maximum output or unknown window: the plain limit.
    assert_eq!(limit(128_000, 0), Some(115_200));
    assert_eq!(limit(0, 16_384), None);
    // A lower configured trigger still wins.
    settings.trigger_tokens = Some(30_000);
    assert_eq!(
        compaction_limit_with_reply_room(128_000, 16_384, &settings),
        Some(30_000)
    );
    assert_eq!(
        compaction_limit_with_reply_room(0, 16_384, &settings),
        Some(30_000)
    );
}

#[test]
fn exact_boundary_compacts() {
    let settings = default_compaction_settings();
    assert!(!should_compact(899, 1_000, &settings));
    assert!(should_compact(900, 1_000, &settings));
    assert!(should_compact(1_000, 1_000, &settings));
}

#[test]
fn keep_recent_shrinks_on_small_windows() {
    let settings = default_compaction_settings();
    // 1M window: the 20K default is far below a third of the limit and is kept as is.
    let big = compaction_limit(1_048_576, &settings);
    assert_eq!(effective_keep_recent_tokens(big, 20_000), 20_000);
    // 32K window: limit 29_491, a third is 9_830.
    let small = compaction_limit(32_768, &settings);
    assert_eq!(effective_keep_recent_tokens(small, 20_000), 9_830);
    // Benchmark arms keep 0.25 W with a 0.9 W trigger: unchanged.
    assert_eq!(effective_keep_recent_tokens(Some(117_964), 32_768), 32_768);
    assert_eq!(effective_keep_recent_tokens(None, 20_000), 20_000);
    assert_eq!(effective_keep_recent_tokens(Some(0), 20_000), 1);
}

// ---- end to end: auto_compact_context with a fake summarizer ----

mod end_to_end {
    use std::sync::Arc;

    use loop_agent::harness::compaction::{
        auto_compact_context, default_compaction_settings, force_compact_context,
    };
    use loop_agent::{stream_fn_from_models, AgentContext, AgentMessage};
    use loop_ai::providers::{faux_provider, FauxResponse, FauxScript};
    use loop_ai::{AssistantContent, AssistantMessage, Models, SimpleStreamOptions, TextContent};

    fn setup(script: FauxScript) -> (loop_agent::StreamFn, loop_ai::Model) {
        let models = Models::new();
        models.set_provider(faux_provider(script));
        let model = models.get_model("faux", "faux-model").unwrap();
        (stream_fn_from_models(Arc::new(models)), model)
    }

    fn assistant(model: &loop_ai::Model, text: &str) -> AgentMessage {
        let mut a = AssistantMessage::pending(model);
        a.content = vec![AssistantContent::Text(TextContent {
            text: text.into(),
            text_signature: None,
        })];
        AgentMessage::assistant(a)
    }

    fn history(model: &loop_ai::Model, n: usize) -> AgentContext {
        let mut messages = vec![AgentMessage::user_text(
            "task: sum the reports, answer in 3 lines",
        )];
        for i in 0..n {
            messages.push(assistant(model, &format!("step {i} {}", "x".repeat(400))));
        }
        AgentContext {
            system_prompt: "sys".into(),
            messages,
            tools: None,
        }
    }

    fn settings() -> loop_agent::harness::compaction::CompactionSettings {
        let mut s = default_compaction_settings();
        s.trigger_tokens = Some(500);
        s.keep_recent_tokens = 300;
        s
    }

    #[tokio::test]
    async fn summarizes_at_the_trigger_and_keeps_head_and_tail() {
        let script = FauxScript::new();
        script.push(FauxResponse::Text(
            "STATE: running total 337198 after report_45".into(),
        ));
        let (stream_fn, model) = setup(script);
        let ctx = history(&model, 30);
        let done = auto_compact_context(
            &ctx,
            &model,
            &stream_fn,
            SimpleStreamOptions::default(),
            &settings(),
        )
        .await
        .unwrap()
        .expect("over the trigger: must compact");
        let msgs = &done.context.messages;
        assert!(msgs.len() < ctx.messages.len());
        assert_eq!(msgs[0].role(), "user");
        assert_eq!(
            format!("{:?}", msgs[0]),
            format!("{:?}", ctx.messages[0]),
            "task prompt kept verbatim"
        );
        let summary = format!("{:?}", msgs[1]);
        assert!(
            summary.contains("running total 337198 after report_45"),
            "{summary}"
        );
        assert!(summary.contains("Earlier work was summarized"));
        assert_eq!(
            format!("{:?}", msgs.last().unwrap()),
            format!("{:?}", ctx.messages.last().unwrap()),
            "newest message kept verbatim"
        );
        assert!(
            done.notice.contains('→') && done.notice.starts_with("Context compacted"),
            "{}",
            done.notice
        );
        assert_eq!(done.context.system_prompt, "sys");
    }

    fn tool_call(model: &loop_ai::Model, name: &str, args: serde_json::Value) -> AgentMessage {
        let mut a = AssistantMessage::pending(model);
        a.content = vec![AssistantContent::ToolCall(loop_ai::ToolCall {
            id: "c".into(),
            name: name.into(),
            arguments: args,
            thought_signature: None,
        })];
        AgentMessage::assistant(a)
    }

    fn tool_result(text: String) -> AgentMessage {
        AgentMessage::tool_result(loop_ai::ToolResultMessage {
            tool_call_id: "c".into(),
            tool_name: "read".into(),
            content: vec![loop_ai::ToolResultContent::Text(TextContent {
                text,
                text_signature: None,
            })],
            details: None,
            usage: None,
            added_tool_names: None,
            is_error: false,
            timestamp: 0,
        })
    }

    #[tokio::test]
    async fn the_summary_lists_the_files_the_tool_calls_touched() {
        let script = FauxScript::new();
        script.push(FauxResponse::Text("DONE: fixed the parser".into()));
        let (stream_fn, model) = setup(script);
        let mut ctx = history(&model, 30);
        ctx.messages.insert(
            1,
            tool_call(&model, "read", serde_json::json!({"path": "src/lexer.rs"})),
        );
        ctx.messages.insert(
            2,
            tool_call(
                &model,
                "edit",
                serde_json::json!({"path": "src/parser.rs", "oldText": "a", "newText": "b"}),
            ),
        );
        let done = auto_compact_context(
            &ctx,
            &model,
            &stream_fn,
            SimpleStreamOptions::default(),
            &settings(),
        )
        .await
        .unwrap()
        .expect("compacts");
        assert!(done.summary.starts_with("DONE: fixed the parser"));
        assert!(
            done.summary
                .ends_with("changed:\n- src/parser.rs\nread:\n- src/lexer.rs"),
            "{}",
            done.summary
        );
        // The same text is what the model sees next.
        let in_context = format!("{:?}", done.context.messages[1]);
        assert!(in_context.contains("- src/parser.rs"), "{in_context}");
    }

    #[tokio::test]
    async fn a_huge_result_in_the_kept_part_does_not_leave_the_context_over_the_limit() {
        let script = FauxScript::new();
        script.push(FauxResponse::Text("STATE: ok".into()));
        let (stream_fn, mut model) = setup(script);
        model.context_window = 20_000;
        model.max_tokens = 2_000;
        let mut ctx = history(&model, 30);
        // The newest turn read a file four times the size of the window.
        ctx.messages.push(tool_call(
            &model,
            "read",
            serde_json::json!({"path": "big.log"}),
        ));
        ctx.messages
            .push(tool_result(format!("HEAD{}TAIL", "z".repeat(320_000))));
        let mut s = default_compaction_settings();
        s.keep_recent_tokens = 300;
        let done =
            auto_compact_context(&ctx, &model, &stream_fn, SimpleStreamOptions::default(), &s)
                .await
                .unwrap()
                .expect("over the limit: must compact");
        let after = loop_agent::harness::compaction::estimate_tokens(&loop_agent::convert_to_llm(
            &done.context.messages,
        ));
        assert!(after <= 18_000, "{after} tokens left, limit 18,000");
        let kept = format!("{:?}", done.context.messages.last().unwrap());
        assert!(kept.contains("HEAD") && kept.contains("TAIL"));
        assert!(kept.contains("middle removed to save context"));
        assert!(done.notice.contains("→"));
    }

    #[tokio::test]
    async fn reports_the_summary_and_how_many_newest_messages_stay() {
        let script = FauxScript::new();
        script.push(FauxResponse::Text("STATE: total 41".into()));
        let (stream_fn, model) = setup(script);
        let ctx = history(&model, 30);
        let done = auto_compact_context(
            &ctx,
            &model,
            &stream_fn,
            SimpleStreamOptions::default(),
            &settings(),
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(done.summary, "STATE: total 41");
        // head + summary, then the kept tail.
        assert_eq!(done.tail_len, done.context.messages.len() - 2);
        assert!(done.tail_len >= 1);
    }

    #[tokio::test]
    async fn a_summary_left_by_an_earlier_compaction_stays_at_the_front() {
        let script = FauxScript::new();
        script.push(FauxResponse::Text("STATE: total 99".into()));
        let (stream_fn, model) = setup(script);
        let mut ctx = history(&model, 30);
        ctx.messages[0] =
            loop_agent::create_compaction_summary_message("EARLIER: total 41 after report_9");
        let done = auto_compact_context(
            &ctx,
            &model,
            &stream_fn,
            SimpleStreamOptions::default(),
            &settings(),
        )
        .await
        .unwrap()
        .expect("over the trigger: must compact");
        let msgs = &done.context.messages;
        assert_eq!(
            msgs[0].role(),
            "compactionSummary",
            "kept verbatim, not summarized away"
        );
        assert!(format!("{:?}", msgs[0]).contains("EARLIER: total 41"));
        assert!(format!("{:?}", msgs[1]).contains("STATE: total 99"));
    }

    #[tokio::test]
    async fn below_the_trigger_nothing_happens() {
        let (stream_fn, model) = setup(FauxScript::new());
        let ctx = history(&model, 3);
        let mut s = settings();
        s.trigger_tokens = Some(1_000_000);
        s.trigger_percent = 0;
        let out =
            auto_compact_context(&ctx, &model, &stream_fn, SimpleStreamOptions::default(), &s)
                .await;
        assert!(out.unwrap().is_none());
    }

    #[tokio::test]
    async fn a_failed_summary_leaves_the_context_untouched() {
        let script = FauxScript::new();
        script.push(FauxResponse::Error("provider exploded".into()));
        let (stream_fn, model) = setup(script);
        let ctx = history(&model, 30);
        let out = auto_compact_context(
            &ctx,
            &model,
            &stream_fn,
            SimpleStreamOptions::default(),
            &settings(),
        )
        .await;
        let error = out
            .err()
            .expect("no summary, no change: history must not be lost");
        assert!(error.contains("provider exploded"), "{error}");
    }

    #[tokio::test]
    async fn an_empty_summary_is_rejected() {
        let script = FauxScript::new();
        script.push(FauxResponse::Text("   ".into()));
        let (stream_fn, model) = setup(script);
        let ctx = history(&model, 30);
        let out = auto_compact_context(
            &ctx,
            &model,
            &stream_fn,
            SimpleStreamOptions::default(),
            &settings(),
        )
        .await;
        let error = out.err().expect("an empty summary is a failure");
        assert!(error.contains("empty summary"), "{error}");
    }

    #[tokio::test]
    async fn disabled_compaction_never_runs() {
        let (stream_fn, model) = setup(FauxScript::new());
        let ctx = history(&model, 30);
        let mut s = settings();
        s.enabled = false;
        assert!(
            auto_compact_context(&ctx, &model, &stream_fn, SimpleStreamOptions::default(), &s)
                .await
                .unwrap()
                .is_none()
        );
        assert!(force_compact_context(
            &ctx,
            &model,
            &stream_fn,
            SimpleStreamOptions::default(),
            &s
        )
        .await
        .unwrap()
        .is_none());
    }

    #[tokio::test]
    async fn forced_compaction_ignores_the_trigger() {
        let script = FauxScript::new();
        script.push(FauxResponse::Text("STATE: x".into()));
        let (stream_fn, model) = setup(script);
        let ctx = history(&model, 30);
        let mut s = settings();
        s.trigger_tokens = Some(10_000_000);
        s.trigger_percent = 0;
        let done =
            force_compact_context(&ctx, &model, &stream_fn, SimpleStreamOptions::default(), &s)
                .await
                .unwrap()
                .expect("forced: must compact below the trigger");
        assert!(done.context.messages.len() < ctx.messages.len());
    }

    #[tokio::test]
    async fn a_short_history_cannot_be_compacted() {
        let (stream_fn, model) = setup(FauxScript::new());
        let ctx = history(&model, 1);
        let out = force_compact_context(
            &ctx,
            &model,
            &stream_fn,
            SimpleStreamOptions::default(),
            &settings(),
        )
        .await;
        assert!(out.unwrap().is_none());
    }

    /// A reply that hit the output limit with nothing but reasoning in it.
    fn out_of_tokens(model: &loop_ai::Model) -> FauxResponse {
        let mut message = AssistantMessage::pending(model);
        message.stop_reason = loop_ai::StopReason::Length;
        FauxResponse::Events(vec![loop_ai::AssistantMessageEvent::Done {
            reason: loop_ai::StopReason::Length,
            message,
        }])
    }

    #[tokio::test]
    async fn a_summary_that_ran_out_of_output_tokens_is_tried_once_more() {
        let script = FauxScript::new();
        let (stream_fn, model) = setup(script.clone());
        script.push(out_of_tokens(&model));
        script.push(FauxResponse::Text("STATE: total 7".into()));
        let done = auto_compact_context(
            &history(&model, 30),
            &model,
            &stream_fn,
            SimpleStreamOptions::default(),
            &settings(),
        )
        .await
        .unwrap()
        .expect("the second try gives the summary");
        assert_eq!(done.summary, "STATE: total 7");

        // Out of tokens twice: give up and say why.
        script.push(out_of_tokens(&model));
        script.push(out_of_tokens(&model));
        let error = auto_compact_context(
            &history(&model, 30),
            &model,
            &stream_fn,
            SimpleStreamOptions::default(),
            &settings(),
        )
        .await
        .err()
        .expect("no summary");
        assert!(error.contains("ran out of output tokens"), "{error}");
    }

    #[tokio::test]
    async fn a_long_history_gets_shorter_previews_before_anything_is_dropped() {
        use loop_agent::harness::compaction::render_for_summary;
        let script = FauxScript::new();
        let (stream_fn, mut model) = setup(script.clone());
        // 8,000 characters of transcript at most (the floor for small windows).
        model.context_window = 2_000;
        let mut messages = vec![AgentMessage::user_text("task")];
        for i in 0..40 {
            messages.push(assistant(&model, &format!("step-{i:02}")));
            messages.push(AgentMessage::tool_result(loop_ai::ToolResultMessage {
                tool_call_id: format!("c{i}"),
                tool_name: "read".into(),
                content: vec![loop_ai::ToolResultContent::Text(TextContent {
                    text: "r".repeat(1000),
                    text_signature: None,
                })],
                details: None,
                usage: None,
                added_tool_names: None,
                is_error: false,
                timestamp: 0,
            }));
        }
        assert!(render_for_summary(&messages).len() > 8_000);
        let seen = Arc::new(parking_lot::Mutex::new(String::new()));
        let ctx = AgentContext {
            system_prompt: "sys".into(),
            messages,
            tools: None,
        };
        let mut s = settings();
        s.keep_recent_tokens = 1;
        let capture = {
            let seen = Arc::clone(&seen);
            let inner = stream_fn.clone();
            let f: loop_agent::StreamFn = Arc::new(move |m, c: loop_ai::Context, o| {
                *seen.lock() = format!("{:?}", c.messages);
                inner(m, c, o)
            });
            f
        };
        script.push(FauxResponse::Text("STATE: ok".into()));
        force_compact_context(&ctx, &model, &capture, SimpleStreamOptions::default(), &s)
            .await
            .unwrap()
            .expect("compacts");
        let sent = seen.lock().clone();
        assert!(!sent.contains("[earlier part omitted]"), "nothing dropped");
        assert!(sent.contains("step-00") && sent.contains("step-38"));
    }
}
