//! Headless `--print` runs, each recorded as one Langfuse trace when built with the
//! `telemetry` feature.

#[cfg(feature = "telemetry")]
use std::collections::BTreeMap;
use std::future::Future;

use loop_agent::AgentMessage;
use loop_ai::{AssistantContent, Message};
use loop_protocol::{
    AgentBackend, ContentBlock, Role, SessionRef, SessionUpdate, StopReason, TurnOutcome,
    UpdateEnvelope, Usage,
};
#[cfg(feature = "telemetry")]
use loop_telemetry::{obs, ObservationExt, TelemetryHandle, TraceAttrs};
use tokio::sync::broadcast::error::RecvError;
#[cfg(feature = "telemetry")]
use tracing::Instrument;

use crate::{CliRuntime, PendingAgent};

/// Name of the root observation of a print-mode trace.
#[cfg(feature = "telemetry")]
pub const ROOT_SPAN_NAME: &str = "loop.print";
/// Prefix of the stderr line carrying the trace id, for benchmark runners.
pub const TRACE_ID_PREFIX: &str = "loop-trace-id: ";

/// Per-run trace labels for `--print`; without the `telemetry` feature there are none.
#[cfg(not(feature = "telemetry"))]
#[derive(Debug, Clone, Default, PartialEq, Eq, clap::Args)]
pub struct TraceArgs {}

/// Per-run trace labels for `--print` (e.g. benchmark run and task ids).
#[cfg(feature = "telemetry")]
#[derive(Debug, Clone, Default, PartialEq, Eq, clap::Args)]
pub struct TraceArgs {
    /// Langfuse trace name (default: `loop.print`).
    #[arg(long = "trace-name", global = true)]
    pub name: Option<String>,
    /// Langfuse trace tag; repeatable.
    #[arg(long = "trace-tag", global = true)]
    pub tags: Vec<String>,
    /// Langfuse trace metadata as `key=value`; repeatable.
    #[arg(long = "trace-metadata", global = true, value_parser = parse_key_value)]
    pub metadata: Vec<(String, String)>,
    /// Langfuse session id (default: the Loop session id).
    #[arg(long = "trace-session", global = true)]
    pub session: Option<String>,
    /// Langfuse environment, e.g. `benchmark`.
    #[arg(long = "trace-env", global = true)]
    pub environment: Option<String>,
    /// W3C traceparent of an enclosing trace, so a runner can own the parent span.
    #[arg(long = "trace-parent", global = true, env = "TRACEPARENT")]
    pub parent: Option<String>,
}

#[cfg(feature = "telemetry")]
impl TraceArgs {
    /// Trace attributes for the root span.
    pub fn trace_attrs(&self, default_session: &str) -> TraceAttrs {
        TraceAttrs {
            name: self.name.clone(),
            session_id: Some(
                self.session
                    .clone()
                    .unwrap_or_else(|| default_session.to_string()),
            ),
            user_id: None,
            tags: self.tags.clone(),
            metadata: self.metadata.iter().cloned().collect::<BTreeMap<_, _>>(),
            environment: self.environment.clone(),
            release: Some(env!("CARGO_PKG_VERSION").to_string()),
        }
    }
}

#[cfg(feature = "telemetry")]
fn parse_key_value(raw: &str) -> Result<(String, String), String> {
    match raw.split_once('=') {
        Some((key, value)) if !key.trim().is_empty() => {
            Ok((key.trim().to_string(), value.to_string()))
        }
        _ => Err(format!("expected key=value, got `{raw}`")),
    }
}

/// Outcome of a traced print run.
#[derive(Debug)]
pub struct PrintOutcome {
    /// Final assistant text, or why the run failed.
    pub result: anyhow::Result<String>,
    /// Trace id when the run was exported.
    pub trace_id: Option<String>,
}

/// Run `prompt` through `run`; untraced without the `telemetry` feature.
#[cfg(not(feature = "telemetry"))]
pub async fn traced_print<F, Fut>(
    _prompt: &str,
    _trace: &TraceArgs,
    _default_session: &str,
    run: F,
) -> PrintOutcome
where
    F: FnOnce() -> Fut,
    Fut: Future<Output = anyhow::Result<String>>,
{
    PrintOutcome {
        result: run().await,
        trace_id: None,
    }
}

/// Run `prompt` through `run` inside the `loop.print` root observation.
#[cfg(feature = "telemetry")]
pub async fn traced_print<F, Fut>(
    prompt: &str,
    trace: &TraceArgs,
    default_session: &str,
    run: F,
) -> PrintOutcome
where
    F: FnOnce() -> Fut,
    Fut: Future<Output = anyhow::Result<String>>,
{
    let root = obs::span(ROOT_SPAN_NAME);
    if let Some(parent) = &trace.parent {
        if !root.continue_trace(parent) {
            tracing::warn!(target: "loop_cli", traceparent = %parent, "ignoring invalid traceparent");
        }
    }
    root.record_trace(&trace.trace_attrs(default_session));
    root.record_input(prompt);

    let result = run().instrument(root.clone()).await;
    match &result {
        Ok(text) => root.record_output(text),
        Err(err) => root.record_error(&format!("{err:#}")),
    }
    PrintOutcome {
        result,
        trace_id: root.trace_id(),
    }
}

/// `loop --print`: run one prompt, write the result in `format`, report the
/// trace id on stderr. Returns the process exit code ([`TurnEnd::exit_code`]).
pub async fn run_print(
    runtime: &mut CliRuntime,
    prompt: String,
    format: OutputFormat,
    trace: &TraceArgs,
) -> anyhow::Result<i32> {
    for notice in std::mem::take(&mut runtime.startup_notices) {
        eprintln!("loop: {notice}");
    }
    // No one can approve anything here: external agents get the default
    // host, which rejects every request, so they decline the action.
    match runtime.pending_agent.take() {
        Some(PendingAgent::New { id, model }) => runtime.use_agent(&id, model.as_deref()).await?,
        Some(PendingAgent::Resume(record)) => runtime.resume_agent(&record).await?,
        None => {}
    }
    let backend = runtime.backend.clone();
    let session = runtime.session_ref();
    let input = prompt.clone();
    let turn_session = session.clone();
    let slot = std::sync::Arc::new(std::sync::Mutex::new(None));
    let report_slot = std::sync::Arc::clone(&slot);
    let outcome = traced_print(&input, trace, &runtime.session_id, || async move {
        let report = run_turn(backend.as_ref(), &turn_session, prompt, |envelope| {
            if format == OutputFormat::StreamJson {
                print_json_line(&serde_json::to_value(envelope).unwrap_or_default());
            }
        })
        .await;
        // The trace records the answer, or why there is none.
        let traced = match report.end.error() {
            None => Ok(report.text.clone()),
            Some(error) => Err(anyhow::anyhow!(error)),
        };
        *report_slot.lock().unwrap_or_else(|e| e.into_inner()) = Some(report);
        traced
    })
    .await;
    if let Some(trace_id) = &outcome.trace_id {
        eprintln!("{TRACE_ID_PREFIX}{trace_id}");
    }
    let report = slot.lock().unwrap_or_else(|e| e.into_inner()).take();
    let Some(report) = report else {
        // The run never reached the turn.
        return outcome.result.map(|_| 1);
    };
    match format {
        OutputFormat::Text => match report.end.error() {
            None => println!("{}", report.text),
            Some(error) => eprintln!("loop: {error}"),
        },
        OutputFormat::Json | OutputFormat::StreamJson => {
            print_json_line(&report.result_json(&session))
        }
    }
    Ok(report.exit_code())
}

/// Write one JSON value as a line on stdout and flush it, so a reader sees
/// each update as it happens.
fn print_json_line(value: &serde_json::Value) {
    use std::io::Write;
    let mut out = std::io::stdout().lock();
    let _ = writeln!(out, "{value}");
    let _ = out.flush();
}

/// Flush and stop exporting before the process exits; reports a failed last export.
#[cfg(feature = "telemetry")]
pub async fn shutdown_telemetry(telemetry: &TelemetryHandle) {
    let handle = telemetry.clone();
    // Shutdown blocks on the exporter; keep it off the async workers.
    let _ = tokio::task::spawn_blocking(move || handle.shutdown()).await;
    if let Some(err) = telemetry.status().last_error {
        eprintln!("loop: trace export failed: {err}");
    }
}

/// How `--print` writes its result.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, clap::ValueEnum)]
pub enum OutputFormat {
    /// The text of the agent's last message.
    #[default]
    Text,
    /// One `result` object when the turn ends.
    Json,
    /// Every update of the turn as one JSON line, then the `result` object.
    StreamJson,
}

/// How a `--print` turn ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TurnEnd {
    /// The turn finished.
    Completed,
    /// The turn failed, or could not start.
    Failed(String),
    /// The turn was cancelled.
    Cancelled,
    /// Contact with the agent was lost mid-turn; edits or commands may have
    /// run.
    Uncertain,
}

impl TurnEnd {
    /// The process exit code: 0 completed, 1 failed, 3 lost contact, 130
    /// cancelled. Clap uses 2 for usage errors.
    pub fn exit_code(&self) -> i32 {
        match self {
            Self::Completed => 0,
            Self::Failed(_) => 1,
            Self::Uncertain => 3,
            Self::Cancelled => 130,
        }
    }

    /// Why the turn did not complete.
    pub fn error(&self) -> Option<String> {
        match self {
            Self::Completed => None,
            Self::Failed(message) => Some(message.clone()),
            Self::Cancelled => Some("the turn was cancelled".into()),
            Self::Uncertain => {
                Some("lost contact with the agent; the turn may be incomplete".into())
            }
        }
    }
}

/// What one `--print` turn produced.
#[derive(Debug, Clone, PartialEq)]
pub struct TurnReport {
    /// The text of the agent's last message.
    pub text: String,
    /// Why the turn ended, when the backend said.
    pub stop: Option<StopReason>,
    /// The latest token usage.
    pub usage: Option<Usage>,
    /// How it ended.
    pub end: TurnEnd,
}

impl TurnReport {
    /// The process exit code ([`TurnEnd::exit_code`]).
    pub fn exit_code(&self) -> i32 {
        self.end.exit_code()
    }

    /// The `result` object for `--output-format json` and `stream-json`.
    pub fn result_json(&self, session: &SessionRef) -> serde_json::Value {
        let stop = self.stop.unwrap_or(match self.end {
            TurnEnd::Completed => StopReason::EndTurn,
            TurnEnd::Cancelled => StopReason::Cancelled,
            TurnEnd::Failed(_) | TurnEnd::Uncertain => StopReason::Error,
        });
        serde_json::json!({
            "type": "result",
            "session": session.key,
            "stop": stop,
            "text": self.text,
            "usage": self.usage,
            "error": self.end.error(),
            "exit_code": self.exit_code(),
        })
    }
}

/// Run one turn and return the text of the agent's last message, or why the
/// turn failed.
pub async fn print_turn(
    backend: &dyn AgentBackend,
    session: &SessionRef,
    prompt: String,
) -> anyhow::Result<String> {
    let report = run_turn(backend, session, prompt, |_| {}).await;
    match report.end.error() {
        None => Ok(report.text),
        Some(error) => Err(anyhow::anyhow!(error)),
    }
}

/// Run one turn, handing each of its updates to `on_update` as it arrives.
pub async fn run_turn(
    backend: &dyn AgentBackend,
    session: &SessionRef,
    prompt: String,
    mut on_update: impl FnMut(&UpdateEnvelope),
) -> TurnReport {
    let mut report = TurnReport {
        text: String::new(),
        stop: None,
        usage: None,
        end: TurnEnd::Completed,
    };
    // Subscribe before prompting so no update of this turn is missed.
    let mut updates = backend.subscribe();
    let turn = match backend
        .prompt(session, vec![ContentBlock::text(prompt)])
        .await
    {
        Ok(turn) => turn,
        Err(err) => {
            report.end = TurnEnd::Failed(err.to_string());
            return report;
        }
    };
    loop {
        let envelope = match updates.recv().await {
            Ok(envelope) => envelope,
            Err(RecvError::Lagged(missed)) => {
                report.end = TurnEnd::Failed(format!("missed {missed} updates from the agent"));
                return report;
            }
            Err(RecvError::Closed) => break,
        };
        if envelope.session != session.key {
            continue;
        }
        on_update(&envelope);
        match envelope.update {
            SessionUpdate::MessageStarted {
                role: Role::Agent, ..
            } => report.text.clear(),
            SessionUpdate::MessageChunk {
                role: Role::Agent,
                content,
                ..
            } => {
                if let Some(text) = content.as_text() {
                    report.text.push_str(text);
                }
            }
            SessionUpdate::Usage(usage) => report.usage = Some(usage),
            SessionUpdate::TurnEnded { stop, error, .. } => {
                report.stop = Some(stop);
                report.end = match (error, stop) {
                    (Some(error), _) => TurnEnd::Failed(error.message),
                    (None, StopReason::Cancelled) => TurnEnd::Cancelled,
                    (None, _) => TurnEnd::Completed,
                };
                return report;
            }
            _ => {}
        }
    }
    // The stream closed without a turn end; the turn's outcome decides.
    report.end = match turn.outcome().await {
        TurnOutcome::Completed(stop) => {
            report.stop = Some(stop);
            TurnEnd::Completed
        }
        TurnOutcome::Cancelled => TurnEnd::Cancelled,
        TurnOutcome::Failed(error) => TurnEnd::Failed(error.message),
        TurnOutcome::Uncertain => TurnEnd::Uncertain,
    };
    report
}

/// The assistant's text, or an error when the run stopped on error / abort.
pub fn final_text(message: &AgentMessage) -> anyhow::Result<String> {
    let Some(Message::Assistant(assistant)) = message.as_llm() else {
        return Ok(format!("{message:?}"));
    };
    if let Some(failure) = assistant.failure() {
        anyhow::bail!("{failure}");
    }
    Ok(assistant
        .content
        .iter()
        .filter_map(|block| match block {
            AssistantContent::Text(t) => Some(t.text.as_str()),
            _ => None,
        })
        .collect())
}

#[cfg(test)]
mod tests {
    #[cfg(feature = "telemetry")]
    use clap::Parser;
    use loop_ai::StopReason;

    use super::*;

    #[cfg(feature = "telemetry")]
    #[derive(Parser)]
    struct Cli {
        #[command(flatten)]
        trace: TraceArgs,
    }

    #[cfg(feature = "telemetry")]
    fn parse(args: &[&str]) -> Result<TraceArgs, clap::Error> {
        Cli::try_parse_from(std::iter::once("loop").chain(args.iter().copied())).map(|c| c.trace)
    }

    #[cfg(feature = "telemetry")]
    #[test]
    fn trace_flags_map_to_trace_attrs() {
        let args = parse(&[
            "--trace-name",
            "swe-bench",
            "--trace-tag",
            "bench",
            "--trace-tag",
            "nightly",
            "--trace-metadata",
            "task_id=django-123",
            "--trace-metadata",
            "run=r1=a",
            "--trace-env",
            "benchmark",
        ])
        .unwrap();
        let attrs = args.trace_attrs("sess-default");
        assert_eq!(attrs.name.as_deref(), Some("swe-bench"));
        assert_eq!(attrs.tags, vec!["bench", "nightly"]);
        assert_eq!(attrs.metadata["task_id"], "django-123");
        assert_eq!(attrs.metadata["run"], "r1=a");
        assert_eq!(attrs.environment.as_deref(), Some("benchmark"));
        assert_eq!(attrs.session_id.as_deref(), Some("sess-default"));
        assert_eq!(attrs.release.as_deref(), Some(env!("CARGO_PKG_VERSION")));
    }

    #[cfg(feature = "telemetry")]
    #[test]
    fn trace_session_overrides_default() {
        let attrs = parse(&["--trace-session", "run-7"])
            .unwrap()
            .trace_attrs("sess");
        assert_eq!(attrs.session_id.as_deref(), Some("run-7"));
    }

    #[cfg(feature = "telemetry")]
    #[test]
    fn malformed_metadata_is_rejected() {
        assert!(parse(&["--trace-metadata", "novalue"]).is_err());
        assert!(parse(&["--trace-metadata", "=x"]).is_err());
    }

    #[test]
    fn exit_codes_follow_how_the_turn_ended() {
        assert_eq!(TurnEnd::Completed.exit_code(), 0);
        assert_eq!(TurnEnd::Failed("x".into()).exit_code(), 1);
        assert_eq!(TurnEnd::Uncertain.exit_code(), 3);
        assert_eq!(TurnEnd::Cancelled.exit_code(), 130);
        assert_eq!(TurnEnd::Completed.error(), None);
        assert!(TurnEnd::Uncertain.error().unwrap().contains("lost contact"));
    }

    #[test]
    fn final_text_joins_text_blocks_and_fails_on_error() {
        let mut assistant = loop_ai::AssistantMessage {
            content: vec![
                AssistantContent::Text(loop_ai::TextContent {
                    text: "hel".into(),
                    text_signature: None,
                }),
                AssistantContent::Text(loop_ai::TextContent {
                    text: "lo".into(),
                    text_signature: None,
                }),
            ],
            api: "faux".into(),
            provider: "faux".into(),
            model: "m".into(),
            response_model: None,
            response_id: None,
            usage: Default::default(),
            stop_reason: StopReason::Stop,
            error_message: None,
            raw_stop_reason: None,
            timestamp: 0,
        };
        assert_eq!(
            final_text(&AgentMessage::assistant(assistant.clone())).unwrap(),
            "hello"
        );
        assistant.stop_reason = StopReason::Error;
        assistant.error_message = Some("rate limited".into());
        let err = final_text(&AgentMessage::assistant(assistant)).unwrap_err();
        assert_eq!(err.to_string(), "rate limited");
    }
}
