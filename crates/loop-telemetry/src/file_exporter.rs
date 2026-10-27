//! Local destination: spans appended as JSON lines to a file, no collector needed.
//!
//! Each process writes its own `loop-<start time>-<pid>.jsonl`, created on the first
//! export so runs that trace nothing leave no file behind.

use std::fs::{File, OpenOptions};
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use chrono::{DateTime, SecondsFormat, Utc};
use opentelemetry::trace::{SpanId, Status};
use opentelemetry::{Array, KeyValue, Value};
use opentelemetry_sdk::error::{OTelSdkError, OTelSdkResult};
use opentelemetry_sdk::trace::{SpanData, SpanExporter};
use parking_lot::Mutex;
use serde_json::{json, Map};

/// Folder of the always-on anonymous usage log: `<OS temp dir>/loop-traces`.
pub fn default_trace_dir() -> PathBuf {
    std::env::temp_dir().join("loop-traces")
}

/// How long usage log files are kept.
pub const TRACE_RETENTION: Duration = Duration::from_secs(30 * 24 * 60 * 60);

/// Whether `path` is a trace file written by [`trace_file_path`].
pub fn is_trace_file(path: &Path) -> bool {
    path.extension().is_some_and(|e| e == "jsonl")
        && path
            .file_name()
            .and_then(|n| n.to_str())
            .is_some_and(|n| n.starts_with("loop-"))
}

/// Delete trace files in `dir` last written more than `max_age` ago; returns how many.
pub(crate) fn prune_old_traces(dir: &Path, max_age: Duration) -> usize {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return 0;
    };
    let now = SystemTime::now();
    entries
        .flatten()
        .filter(|entry| is_trace_file(&entry.path()))
        .filter(|entry| {
            entry
                .metadata()
                .and_then(|m| m.modified())
                .ok()
                .and_then(|modified| now.duration_since(modified).ok())
                .is_some_and(|age| age > max_age)
        })
        .filter(|entry| std::fs::remove_file(entry.path()).is_ok())
        .count()
}

/// Trace file for this process inside `dir`.
pub(crate) fn trace_file_path(dir: &Path) -> PathBuf {
    let started = Utc::now().format("%Y%m%d-%H%M%S");
    dir.join(format!("loop-{started}-{}.jsonl", std::process::id()))
}

/// Appends each exported span to a JSON-lines file.
#[derive(Debug)]
pub(crate) struct FileSpanExporter {
    path: PathBuf,
    writer: Mutex<Option<BufWriter<File>>>,
}

impl FileSpanExporter {
    pub(crate) fn new(path: PathBuf) -> Self {
        Self {
            path,
            writer: Mutex::new(None),
        }
    }

    fn write_batch(&self, batch: &[SpanData]) -> std::io::Result<()> {
        let mut writer = self.writer.lock();
        if writer.is_none() {
            if let Some(dir) = self.path.parent() {
                std::fs::create_dir_all(dir)?;
            }
            let file = OpenOptions::new()
                .create(true)
                .append(true)
                .open(&self.path)?;
            *writer = Some(BufWriter::new(file));
        }
        let out = writer.as_mut().expect("opened above");
        for span in batch {
            serde_json::to_writer(&mut *out, &span_json(span))?;
            out.write_all(b"\n")?;
        }
        out.flush()
    }
}

impl SpanExporter for FileSpanExporter {
    async fn export(&self, batch: Vec<SpanData>) -> OTelSdkResult {
        // The batch processor exports from its own thread, so blocking I/O is fine here.
        self.write_batch(&batch)
            .map_err(|err| OTelSdkError::InternalFailure(format!("{}: {err}", self.path.display())))
    }
}

/// One span as a self-contained JSON object.
fn span_json(span: &SpanData) -> serde_json::Value {
    let context = &span.span_context;
    let (status, status_message) = match &span.status {
        Status::Unset => ("unset", None),
        Status::Ok => ("ok", None),
        Status::Error { description } => (
            "error",
            Some(description.to_string()).filter(|d| !d.is_empty()),
        ),
    };
    let duration_ms = span
        .end_time
        .duration_since(span.start_time)
        .map_or(0.0, |d| d.as_secs_f64() * 1000.0);
    let events: Vec<_> = span
        .events
        .iter()
        .map(|event| {
            json!({
                "name": event.name,
                "time": timestamp(event.timestamp),
                "attributes": attributes_json(&event.attributes),
            })
        })
        .collect();
    json!({
        "trace_id": context.trace_id().to_string(),
        "span_id": context.span_id().to_string(),
        "parent_span_id": (span.parent_span_id != SpanId::INVALID)
            .then(|| span.parent_span_id.to_string()),
        "name": span.name,
        "start_time": timestamp(span.start_time),
        "end_time": timestamp(span.end_time),
        "duration_ms": duration_ms,
        "status": status,
        "status_message": status_message,
        "attributes": attributes_json(&span.attributes),
        "events": events,
    })
}

fn timestamp(at: SystemTime) -> String {
    DateTime::<Utc>::from(at).to_rfc3339_opts(SecondsFormat::Micros, true)
}

fn attributes_json(attributes: &[KeyValue]) -> Map<String, serde_json::Value> {
    attributes
        .iter()
        .map(|kv| (kv.key.to_string(), value_json(&kv.value)))
        .collect()
}

/// Attribute values; JSON-valued strings (inputs, outputs, usage) are embedded as JSON
/// so the file reads naturally with `jq`.
fn value_json(value: &Value) -> serde_json::Value {
    match value {
        Value::Bool(b) => json!(b),
        Value::I64(i) => json!(i),
        Value::F64(f) => json!(f),
        Value::String(s) => {
            let text = s.as_str();
            if text.starts_with(['{', '[']) {
                if let Ok(parsed) = serde_json::from_str(text) {
                    return parsed;
                }
            }
            json!(text)
        }
        Value::Array(Array::Bool(v)) => json!(v),
        Value::Array(Array::I64(v)) => json!(v),
        Value::Array(Array::F64(v)) => json!(v),
        Value::Array(Array::String(v)) => json!(v.iter().map(|s| s.as_str()).collect::<Vec<_>>()),
        other => json!(other.as_str()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn trace_file_is_per_process_jsonl_inside_dir() {
        let path = trace_file_path(Path::new("/tmp/t"));
        assert_eq!(path.parent(), Some(Path::new("/tmp/t")));
        let name = path.file_name().unwrap().to_string_lossy().into_owned();
        assert!(name.starts_with("loop-"), "{name}");
        assert!(
            name.ends_with(&format!("-{}.jsonl", std::process::id())),
            "{name}"
        );
    }

    #[test]
    fn prune_removes_only_old_trace_files() {
        let dir = tempfile::tempdir().unwrap();
        let old = dir.path().join("loop-1-1.jsonl");
        let other = dir.path().join("notes.jsonl");
        for path in [&old, &other] {
            std::fs::write(path, "{}").unwrap();
        }
        assert_eq!(prune_old_traces(dir.path(), Duration::from_secs(3600)), 0);
        std::thread::sleep(Duration::from_millis(20));
        assert_eq!(prune_old_traces(dir.path(), Duration::from_millis(1)), 1);
        assert!(!old.exists() && other.exists());
        assert!(is_trace_file(Path::new("x/loop-a-1.jsonl")));
        assert!(!is_trace_file(Path::new("x/loop-a-1.json")));
    }

    #[test]
    fn json_strings_are_embedded_and_plain_strings_kept() {
        assert_eq!(
            value_json(&Value::from(r#"{"input":3}"#)),
            json!({"input": 3})
        );
        assert_eq!(value_json(&Value::from("[not json")), json!("[not json"));
        assert_eq!(value_json(&Value::from("plain")), json!("plain"));
        assert_eq!(value_json(&Value::I64(7)), json!(7));
    }
}
