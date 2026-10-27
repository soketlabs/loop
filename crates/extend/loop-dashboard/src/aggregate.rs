//! Usage summary built from trace files.
//!
//! Each line of a trace file is one finished span (see `loop-telemetry`'s file exporter).
//! Only measurements and names are used — model, provider, token usage, durations, tool,
//! skill and MCP server names, agent kind and session ids — which is all the anonymous
//! log contains.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use chrono::{DateTime, Duration, Local, Timelike, Utc};
use loop_telemetry::obs::keys;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

/// Sessions listed under "recent sessions".
const RECENT_SESSIONS: usize = 25;
/// Session ids of workflow sub-agents start with this.
const SUB_AGENT_SESSION_PREFIX: &str = "wf-task-";
/// Tool that starts a workflow sub-agent.
const SPAWN_TOOL: &str = "spawn_task";
/// Source of tools that are not from an MCP server.
pub const BUILT_IN: &str = "built-in";

/// One span as written by the file exporter; unknown fields are ignored.
#[derive(Debug, Clone, Deserialize)]
pub struct SpanRecord {
    pub trace_id: String,
    pub name: String,
    pub start_time: DateTime<Utc>,
    #[serde(default)]
    pub duration_ms: f64,
    #[serde(default)]
    pub attributes: Map<String, Value>,
}

impl SpanRecord {
    fn attr_str(&self, key: &str) -> Option<&str> {
        self.attributes.get(key).and_then(Value::as_str)
    }

    fn metadata(&self, key: &str) -> Option<&Value> {
        self.attributes
            .get(&format!("{}{key}", keys::METADATA_PREFIX))
    }

    fn observation_type(&self) -> &str {
        self.attr_str(keys::OBSERVATION_TYPE).unwrap_or("span")
    }

    /// Numbers inside a JSON-object attribute (`usage_details`).
    fn detail(&self, key: &str, field: &str) -> f64 {
        json_value(self.attributes.get(key))
            .as_ref()
            .and_then(|v| v.get(field))
            .and_then(Value::as_f64)
            .unwrap_or(0.0)
    }

    /// Time to first token, when the generation recorded it.
    fn ttft_ms(&self) -> Option<f64> {
        let first = self.attr_str(keys::COMPLETION_START_TIME)?;
        let first = DateTime::parse_from_rfc3339(first).ok()?;
        let ms = (first.with_timezone(&Utc) - self.start_time).num_microseconds()? as f64 / 1e3;
        (ms >= 0.0).then_some(ms)
    }

    /// Skills named on this span: activated for a run, or loaded by a `read` call.
    fn skills(&self) -> Vec<String> {
        let mut out = Vec::new();
        if let Some(Value::Array(names)) = json_value(self.metadata("skills")).as_ref() {
            out.extend(names.iter().filter_map(Value::as_str).map(str::to_string));
        }
        if let Some(name) = self.metadata("skill").and_then(Value::as_str) {
            out.push(name.to_string());
        }
        out
    }
}

/// A JSON attribute that may have been stored as JSON text.
fn json_value(value: Option<&Value>) -> Option<Value> {
    match value? {
        Value::String(text) => serde_json::from_str(text).ok(),
        other => Some(other.clone()),
    }
}

/// `mcp__<server>__<tool>` → `<server>`.
pub fn mcp_server(tool: &str) -> Option<&str> {
    let (server, _) = tool.strip_prefix("mcp__")?.split_once("__")?;
    (!server.is_empty()).then_some(server)
}

/// Totals over the selected period.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct Totals {
    /// Chat sessions (sub-agent sessions are counted in `sub_agent_runs`).
    pub sessions: u64,
    /// Runs of the chat's own agent.
    pub runs: u64,
    /// Runs of workflow sub-agents.
    pub sub_agent_runs: u64,
    /// `spawn_task` calls that started a sub-agent.
    pub sub_agents_spawned: u64,
    pub llm_calls: u64,
    pub tool_calls: u64,
    /// Tool calls served by MCP servers.
    pub mcp_calls: u64,
    /// Skills activated for a run or loaded by the model.
    pub skill_uses: u64,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_read_tokens: u64,
    pub reasoning_tokens: u64,
}

/// Activity on one local calendar day.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct DayRow {
    /// `YYYY-MM-DD`, local time.
    pub date: String,
    pub llm_calls: u64,
    pub tool_calls: u64,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub runs: u64,
    pub sub_agent_runs: u64,
}

/// Usage of one model.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct ModelRow {
    pub model: String,
    pub provider: String,
    pub calls: u64,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_read_tokens: u64,
    pub avg_latency_ms: f64,
    pub p95_latency_ms: f64,
    /// Average time to first token, over calls that recorded it.
    pub avg_ttft_ms: Option<f64>,
}

/// Usage of one tool.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct ToolRow {
    pub tool: String,
    /// [`BUILT_IN`] or the MCP server name.
    pub source: String,
    pub calls: u64,
    pub avg_ms: f64,
    pub p95_ms: f64,
}

/// Usage of one MCP server.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct McpRow {
    pub server: String,
    pub calls: u64,
    /// Distinct tools of this server that were called.
    pub tools: u64,
    pub avg_ms: f64,
}

/// Uses of one skill.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct SkillRow {
    pub skill: String,
    pub uses: u64,
    /// Chat sessions it was used in.
    pub sessions: u64,
}

/// One chat session (or a trace without a session id).
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct SessionRow {
    pub session_id: String,
    pub started: DateTime<Utc>,
    pub last_active: DateTime<Utc>,
    pub runs: u64,
    pub llm_calls: u64,
    pub tool_calls: u64,
    pub sub_agents_spawned: u64,
    pub tokens: u64,
    pub models: Vec<String>,
}

/// Everything the dashboard shows.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct Summary {
    /// Start of the period (inclusive); `None` for all retained history.
    pub since: Option<DateTime<Utc>>,
    pub generated_at: DateTime<Utc>,
    /// Trace files read.
    pub files: u64,
    /// Lines that could not be parsed (e.g. a file still being written).
    pub skipped_lines: u64,
    pub totals: Totals,
    pub days: Vec<DayRow>,
    /// LLM calls by local hour of day, 0–23.
    pub hours: [u64; 24],
    pub models: Vec<ModelRow>,
    pub tools: Vec<ToolRow>,
    pub mcp_servers: Vec<McpRow>,
    pub skills: Vec<SkillRow>,
    pub sessions: Vec<SessionRow>,
}

/// Read every trace file in `dir` and summarize spans that started within `days`
/// (all of them when `None`). A missing folder yields an empty summary.
/// One trace file as last seen on disk.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct LogFile {
    /// Where it is.
    pub path: PathBuf,
    /// Size in bytes.
    pub len: u64,
    /// Last write, when the platform reports it.
    pub modified: Option<SystemTime>,
}

/// Trace files in `dir` with their sizes and modification times: changes whenever the
/// log is written to, so a reader can skip re-parsing an unchanged log.
#[must_use]
pub fn log_fingerprint(dir: &Path) -> Vec<LogFile> {
    let entries = std::fs::read_dir(dir).into_iter().flatten().flatten();
    let mut files: Vec<LogFile> = entries
        .filter(|e| loop_telemetry::is_trace_file(&e.path()))
        .map(|e| {
            let meta = e.metadata().ok();
            LogFile {
                path: e.path(),
                len: meta.as_ref().map_or(0, |m| m.len()),
                modified: meta.and_then(|m| m.modified().ok()),
            }
        })
        .collect();
    files.sort();
    files
}

#[must_use]
pub fn summarize_dir(dir: &Path, days: Option<u32>) -> Summary {
    let now = Utc::now();
    let since = days.map(|d| now - Duration::days(i64::from(d)));
    let mut spans = Vec::new();
    let (mut files, mut skipped) = (0, 0);
    let entries = std::fs::read_dir(dir).into_iter().flatten().flatten();
    for path in entries
        .map(|e| e.path())
        .filter(|p| loop_telemetry::is_trace_file(p))
    {
        let Ok(file) = std::fs::File::open(&path) else {
            continue;
        };
        files += 1;
        for line in BufReader::new(file).lines() {
            let Ok(line) = line else {
                skipped += 1;
                break;
            };
            if line.trim().is_empty() {
                continue;
            }
            match serde_json::from_str::<SpanRecord>(&line) {
                Ok(span) if since.is_none_or(|s| span.start_time >= s) => spans.push(span),
                Ok(_) => {}
                Err(_) => skipped += 1,
            }
        }
    }
    let mut summary = summarize(&spans, now);
    summary.since = since;
    summary.files = files;
    summary.skipped_lines = skipped;
    summary
}

#[derive(Default)]
struct ModelAcc {
    row: ModelRow,
    latencies: Vec<f64>,
    ttfts: Vec<f64>,
}

#[derive(Default)]
struct DurationAcc {
    calls: u64,
    durations: Vec<f64>,
}

#[derive(Default)]
struct McpAcc {
    durations: Vec<f64>,
    tools: BTreeSet<String>,
}

#[derive(Default)]
struct SkillAcc {
    uses: u64,
    sessions: BTreeSet<String>,
}

/// Summarize already-loaded spans.
pub fn summarize(spans: &[SpanRecord], now: DateTime<Utc>) -> Summary {
    // Session ids are recorded on the root span; every span of a trace shares them.
    let session_of: HashMap<&str, &str> = spans
        .iter()
        .filter_map(|s| Some((s.trace_id.as_str(), s.attr_str(keys::SESSION_ID)?)))
        .collect();

    let mut totals = Totals::default();
    let mut days: BTreeMap<String, DayRow> = BTreeMap::new();
    let mut hours = [0u64; 24];
    let mut models: BTreeMap<(String, String), ModelAcc> = BTreeMap::new();
    let mut tools: BTreeMap<String, DurationAcc> = BTreeMap::new();
    let mut mcp: BTreeMap<String, McpAcc> = BTreeMap::new();
    let mut skills: BTreeMap<String, SkillAcc> = BTreeMap::new();
    let mut sessions: HashMap<String, SessionRow> = HashMap::new();

    for span in spans {
        let local = span.start_time.with_timezone(&Local);
        let date = local.format("%Y-%m-%d").to_string();
        let day = days.entry(date.clone()).or_insert_with(|| DayRow {
            date,
            ..Default::default()
        });
        let session_id = session_of.get(span.trace_id.as_str()).map_or_else(
            || format!("trace {}", short(&span.trace_id)),
            |s| s.to_string(),
        );
        let is_sub_agent_session = session_id.starts_with(SUB_AGENT_SESSION_PREFIX);
        // Sub-agent sessions are not chat sessions; their work still counts in totals.
        let mut scratch = SessionRow::default();
        let session = if is_sub_agent_session {
            &mut scratch
        } else {
            let session = sessions
                .entry(session_id.clone())
                .or_insert_with(|| SessionRow {
                    session_id: session_id.clone(),
                    started: span.start_time,
                    last_active: span.start_time,
                    ..Default::default()
                });
            session.started = session.started.min(span.start_time);
            session.last_active = session.last_active.max(span.start_time);
            session
        };

        for skill in span.skills() {
            totals.skill_uses += 1;
            let acc = skills.entry(skill).or_default();
            acc.uses += 1;
            if !is_sub_agent_session {
                acc.sessions.insert(session_id.clone());
            }
        }

        match span.observation_type() {
            "agent" => {
                let sub_agent = match span.metadata("agent").and_then(Value::as_str) {
                    Some(agent) => agent == "sub-agent",
                    // Traces from before agent labels: tell by the session id.
                    None => is_sub_agent_session,
                };
                if sub_agent {
                    totals.sub_agent_runs += 1;
                    day.sub_agent_runs += 1;
                } else {
                    totals.runs += 1;
                    day.runs += 1;
                    session.runs += 1;
                }
            }
            "generation" => {
                let input = span.detail(keys::USAGE_DETAILS, "input") as u64;
                let output = span.detail(keys::USAGE_DETAILS, "output") as u64;
                let cache_read = span.detail(keys::USAGE_DETAILS, "cache_read") as u64;
                totals.llm_calls += 1;
                totals.input_tokens += input;
                totals.output_tokens += output;
                totals.cache_read_tokens += cache_read;
                totals.reasoning_tokens += span.detail(keys::USAGE_DETAILS, "reasoning") as u64;
                day.llm_calls += 1;
                day.input_tokens += input;
                day.output_tokens += output;
                hours[local.hour() as usize] += 1;
                session.llm_calls += 1;
                session.tokens += input + output;

                let model = span.attr_str(keys::MODEL_NAME).unwrap_or("unknown");
                let provider = span.attr_str(keys::GEN_AI_SYSTEM).unwrap_or("");
                if !session.models.iter().any(|m| m == model) {
                    session.models.push(model.to_string());
                }
                let acc = models
                    .entry((model.to_string(), provider.to_string()))
                    .or_default();
                acc.row.calls += 1;
                acc.row.input_tokens += input;
                acc.row.output_tokens += output;
                acc.row.cache_read_tokens += cache_read;
                acc.latencies.push(span.duration_ms);
                acc.ttfts.extend(span.ttft_ms());
            }
            "tool" => {
                let name = span
                    .attr_str(keys::GEN_AI_TOOL_NAME)
                    .or_else(|| span.name.strip_prefix("tool "))
                    .unwrap_or(&span.name);
                totals.tool_calls += 1;
                day.tool_calls += 1;
                session.tool_calls += 1;
                if name == SPAWN_TOOL {
                    totals.sub_agents_spawned += 1;
                    session.sub_agents_spawned += 1;
                }
                let acc = tools.entry(name.to_string()).or_default();
                acc.calls += 1;
                acc.durations.push(span.duration_ms);
                if let Some(server) = mcp_server(name) {
                    totals.mcp_calls += 1;
                    let acc = mcp.entry(server.to_string()).or_default();
                    acc.durations.push(span.duration_ms);
                    acc.tools.insert(name.to_string());
                }
            }
            _ => {}
        }
    }

    let mut models: Vec<ModelRow> = models
        .into_iter()
        .map(|((model, provider), mut acc)| {
            acc.row.model = model;
            acc.row.provider = provider;
            acc.row.avg_latency_ms = mean(&acc.latencies);
            acc.row.p95_latency_ms = p95(&mut acc.latencies);
            acc.row.avg_ttft_ms = (!acc.ttfts.is_empty()).then(|| mean(&acc.ttfts));
            acc.row
        })
        .collect();
    models.sort_by(|a, b| b.calls.cmp(&a.calls).then(a.model.cmp(&b.model)));

    let mut tools: Vec<ToolRow> = tools
        .into_iter()
        .map(|(tool, mut acc)| ToolRow {
            source: mcp_server(&tool).unwrap_or(BUILT_IN).to_string(),
            tool,
            calls: acc.calls,
            avg_ms: mean(&acc.durations),
            p95_ms: p95(&mut acc.durations),
        })
        .collect();
    tools.sort_by(|a, b| b.calls.cmp(&a.calls).then(a.tool.cmp(&b.tool)));

    let mut mcp_servers: Vec<McpRow> = mcp
        .into_iter()
        .map(|(server, acc)| McpRow {
            server,
            calls: acc.durations.len() as u64,
            tools: acc.tools.len() as u64,
            avg_ms: mean(&acc.durations),
        })
        .collect();
    mcp_servers.sort_by(|a, b| b.calls.cmp(&a.calls).then(a.server.cmp(&b.server)));

    let mut skills: Vec<SkillRow> = skills
        .into_iter()
        .map(|(skill, acc)| SkillRow {
            skill,
            uses: acc.uses,
            sessions: acc.sessions.len() as u64,
        })
        .collect();
    skills.sort_by(|a, b| b.uses.cmp(&a.uses).then(a.skill.cmp(&b.skill)));

    // Spans without any model call or tool (e.g. a bare root span) are not sessions.
    let mut sessions: Vec<SessionRow> = sessions
        .into_values()
        .filter(|s| s.runs + s.llm_calls + s.tool_calls > 0)
        .collect();
    totals.sessions = sessions.len() as u64;
    sessions.sort_by_key(|s| std::cmp::Reverse(s.last_active));
    sessions.truncate(RECENT_SESSIONS);

    Summary {
        generated_at: now,
        totals,
        days: days.into_values().collect(),
        hours,
        models,
        tools,
        mcp_servers,
        skills,
        sessions,
        ..Default::default()
    }
}

fn short(id: &str) -> &str {
    &id[..id.len().min(8)]
}

fn mean(values: &[f64]) -> f64 {
    if values.is_empty() {
        0.0
    } else {
        values.iter().sum::<f64>() / values.len() as f64
    }
}

/// Nearest-rank 95th percentile.
fn p95(values: &mut [f64]) -> f64 {
    if values.is_empty() {
        return 0.0;
    }
    values.sort_by(f64::total_cmp);
    let rank = (values.len() as f64 * 0.95).ceil() as usize;
    values[rank.clamp(1, values.len()) - 1]
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn meta(key: &str) -> String {
        format!("{}{key}", keys::METADATA_PREFIX)
    }

    fn span(trace: &str, kind: &str, start: &str, ms: f64, attrs: Value) -> SpanRecord {
        let mut attributes = attrs.as_object().cloned().unwrap_or_default();
        attributes.insert(keys::OBSERVATION_TYPE.into(), json!(kind));
        SpanRecord {
            trace_id: trace.into(),
            name: kind.into(),
            start_time: start.parse().unwrap(),
            duration_ms: ms,
            attributes,
        }
    }

    fn run(trace: &str, session: &str, extra: Value) -> SpanRecord {
        let mut attrs = json!({ keys::SESSION_ID: session });
        attrs
            .as_object_mut()
            .unwrap()
            .extend(extra.as_object().cloned().unwrap_or_default());
        span(trace, "agent", "2026-10-20T09:59:59Z", 3000.0, attrs)
    }

    fn generation(trace: &str, model: &str, input: u64, ms: f64) -> SpanRecord {
        span(
            trace,
            "generation",
            "2026-10-20T10:00:00Z",
            ms,
            json!({
                keys::MODEL_NAME: model,
                keys::GEN_AI_SYSTEM: "soket",
                keys::USAGE_DETAILS: {"input": input, "output": 5, "cache_read": 1},
                keys::COMPLETION_START_TIME: "2026-10-20T10:00:00.250Z",
            }),
        )
    }

    fn tool(trace: &str, name: &str, ms: f64) -> SpanRecord {
        tool_with(trace, name, ms, json!({}))
    }

    fn tool_with(trace: &str, name: &str, ms: f64, extra: Value) -> SpanRecord {
        let mut attrs = json!({ keys::GEN_AI_TOOL_NAME: name });
        attrs
            .as_object_mut()
            .unwrap()
            .extend(extra.as_object().cloned().unwrap_or_default());
        span(trace, "tool", "2026-10-20T10:00:01Z", ms, attrs)
    }

    fn fixture() -> Vec<SpanRecord> {
        vec![
            run(
                "t1",
                "s1",
                json!({ meta("agent"): "main", meta("skills"): ["review"] }),
            ),
            generation("t1", "m-large", 100, 1000.0),
            generation("t1", "m-large", 50, 3000.0),
            tool("t1", "read", 10.0),
            tool_with("t1", "read", 12.0, json!({ meta("skill"): "pdf" })),
            tool("t1", "spawn_task", 50.0),
            tool("t1", "mcp__github__list_issues", 300.0),
            tool("t1", "mcp__github__create_issue", 500.0),
            // A sub-agent started by spawn_task: its own wf-task session.
            run("w1", "wf-task-t1", json!({ meta("agent"): "sub-agent" })),
            generation("w1", "m-small", 10, 400.0),
            tool("w1", "read", 30.0),
            // An older trace without agent labels.
            run("t2", "s2", json!({})),
            run("w2", "wf-task-t2", json!({})),
        ]
    }

    #[test]
    fn totals_separate_main_and_sub_agent_runs() {
        let summary = summarize(&fixture(), Utc::now());
        let t = &summary.totals;
        assert_eq!((t.sessions, t.runs, t.sub_agent_runs), (2, 2, 2));
        assert_eq!(t.sub_agents_spawned, 1);
        assert_eq!((t.llm_calls, t.tool_calls, t.mcp_calls), (3, 6, 2));
        assert_eq!(
            (t.input_tokens, t.output_tokens, t.cache_read_tokens),
            (160, 15, 3)
        );
        assert_eq!(summary.hours.iter().sum::<u64>(), 3);
        let day = &summary.days[0];
        assert_eq!((day.runs, day.sub_agent_runs), (2, 2));
    }

    #[test]
    fn skills_count_activations_and_loads() {
        let summary = summarize(&fixture(), Utc::now());
        assert_eq!(summary.totals.skill_uses, 2);
        let names: Vec<_> = summary
            .skills
            .iter()
            .map(|s| (s.skill.as_str(), s.uses, s.sessions))
            .collect();
        assert_eq!(names, [("pdf", 1, 1), ("review", 1, 1)]);
    }

    #[test]
    fn mcp_servers_and_tool_sources() {
        let summary = summarize(&fixture(), Utc::now());
        assert_eq!(
            summary.mcp_servers,
            [McpRow {
                server: "github".into(),
                calls: 2,
                tools: 2,
                avg_ms: 400.0,
            }]
        );
        let source = |name: &str| {
            summary
                .tools
                .iter()
                .find(|t| t.tool == name)
                .unwrap()
                .source
                .clone()
        };
        assert_eq!(source("read"), BUILT_IN);
        assert_eq!(source("mcp__github__list_issues"), "github");
        assert_eq!(mcp_server("mcp__fs__read_file"), Some("fs"));
        assert_eq!(mcp_server("mcp__"), None);
        assert_eq!(mcp_server("read"), None);
    }

    #[test]
    fn models_have_latency_percentiles_ttft_and_cache() {
        let summary = summarize(&fixture(), Utc::now());
        let large = &summary.models[0];
        assert_eq!(
            (large.model.as_str(), large.provider.as_str()),
            ("m-large", "soket")
        );
        assert_eq!(
            (large.calls, large.input_tokens, large.cache_read_tokens),
            (2, 150, 2)
        );
        assert_eq!(large.avg_latency_ms, 2000.0);
        assert_eq!(large.p95_latency_ms, 3000.0);
        assert_eq!(large.avg_ttft_ms, Some(250.0));
        assert_eq!(summary.models[1].model, "m-small");
    }

    #[test]
    fn sessions_exclude_sub_agent_sessions() {
        let summary = summarize(&fixture(), Utc::now());
        let ids: Vec<_> = summary
            .sessions
            .iter()
            .map(|s| s.session_id.as_str())
            .collect();
        assert!(ids.contains(&"s1") && ids.contains(&"s2"), "{ids:?}");
        assert!(!ids.iter().any(|id| id.starts_with("wf-task-")));
        let s1 = summary
            .sessions
            .iter()
            .find(|s| s.session_id == "s1")
            .unwrap();
        assert_eq!(
            (s1.runs, s1.llm_calls, s1.tool_calls, s1.sub_agents_spawned),
            (1, 2, 5, 1)
        );
        assert_eq!(s1.tokens, 160);
        assert_eq!(s1.models, ["m-large"]);
    }

    #[test]
    fn p95_uses_nearest_rank() {
        assert_eq!(p95(&mut []), 0.0);
        assert_eq!(p95(&mut [5.0]), 5.0);
        let mut values: Vec<f64> = (1..=100).map(f64::from).collect();
        assert_eq!(p95(&mut values), 95.0);
    }

    #[test]
    fn summarize_dir_reads_trace_files_skips_bad_lines_and_filters_by_days() {
        let dir = tempfile::tempdir().unwrap();
        let recent = Utc::now().to_rfc3339();
        let line = |start: &str| {
            json!({
                "trace_id": "t", "span_id": "s", "name": "llm.generate",
                "start_time": start, "duration_ms": 5.0, "status": "unset",
                "attributes": {
                    keys::OBSERVATION_TYPE: "generation",
                    keys::USAGE_DETAILS: {"input": 7, "output": 1},
                },
            })
            .to_string()
        };
        std::fs::write(
            dir.path().join("loop-a-1.jsonl"),
            format!(
                "{}\n{}\nnot json\n",
                line(&recent),
                line("2020-01-01T00:00:00Z")
            ),
        )
        .unwrap();
        std::fs::write(dir.path().join("other.jsonl"), line(&recent)).unwrap();

        let week = summarize_dir(dir.path(), Some(7));
        assert_eq!((week.files, week.skipped_lines), (1, 1));
        assert_eq!(week.totals.llm_calls, 1);
        assert_eq!(week.totals.input_tokens, 7);
        assert!(week.since.is_some());
        assert_eq!(summarize_dir(dir.path(), None).totals.llm_calls, 2);
        let missing = summarize_dir(&dir.path().join("missing"), None);
        assert_eq!(missing.files, 0);
        assert_eq!(missing.totals, Totals::default());
    }

    #[test]
    fn log_fingerprint_changes_only_when_the_log_is_written() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("loop-a-1.jsonl");
        std::fs::write(
            &file, "{}
",
        )
        .unwrap();
        std::fs::write(dir.path().join("notes.txt"), "x").unwrap();
        let before = log_fingerprint(dir.path());
        assert_eq!(before.len(), 1, "only trace files count");
        assert_eq!(log_fingerprint(dir.path()), before);
        std::fs::write(
            &file, "{}
{}
",
        )
        .unwrap();
        assert_ne!(log_fingerprint(dir.path()), before);
    }
}
