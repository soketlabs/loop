//! Reading the usage log: the JSON-lines spans written by `loop-telemetry`'s file
//! exporter, and the Langfuse / OTel attribute keys on them.
//!
//! This is the one module that knows that format. A reader over the Runtime's Event Log
//! can replace it without touching [`crate::aggregate`] or the views.

use std::collections::BTreeMap;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use chrono::{DateTime, Duration, Utc};
use loop_telemetry::obs::keys;
use serde::Deserialize;
use serde_json::{Map, Value};

use crate::aggregate::{summarize_since, Summary};

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
    pub(crate) fn attr_str(&self, key: &str) -> Option<&str> {
        self.attributes.get(key).and_then(Value::as_str)
    }

    pub(crate) fn metadata(&self, key: &str) -> Option<&Value> {
        self.attributes
            .get(&format!("{}{key}", keys::METADATA_PREFIX))
    }

    pub(crate) fn observation_type(&self) -> &str {
        self.attr_str(keys::OBSERVATION_TYPE).unwrap_or("span")
    }

    /// Numbers inside a JSON-object attribute (`usage_details`).
    pub(crate) fn detail(&self, key: &str, field: &str) -> f64 {
        json_value(self.attributes.get(key))
            .as_ref()
            .and_then(|v| v.get(field))
            .and_then(Value::as_f64)
            .unwrap_or(0.0)
    }

    /// Time to first token, when the generation recorded it.
    pub(crate) fn ttft_ms(&self) -> Option<f64> {
        let first = self.attr_str(keys::COMPLETION_START_TIME)?;
        let first = DateTime::parse_from_rfc3339(first).ok()?;
        let ms = (first.with_timezone(&Utc) - self.start_time).num_microseconds()? as f64 / 1e3;
        (ms >= 0.0).then_some(ms)
    }

    /// Skills named on this span: activated for a run, or loaded by a `read` call.
    pub(crate) fn skills(&self) -> Vec<String> {
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

/// Parsed spans of every trace file in a folder. [`refresh`](Self::refresh) re-reads only
/// the files whose size or modification time changed.
#[derive(Debug, Default)]
pub struct LogCache {
    files: BTreeMap<PathBuf, CachedFile>,
}

#[derive(Debug)]
struct CachedFile {
    seen: LogFile,
    spans: Vec<SpanRecord>,
    skipped: u64,
}

impl LogCache {
    /// Bring the cache up to date with `dir`; returns how many files were read.
    pub fn refresh(&mut self, dir: &Path) -> usize {
        let current = log_fingerprint(dir);
        self.files
            .retain(|path, _| current.iter().any(|file| &file.path == path));
        let mut read = 0;
        for file in current {
            if self
                .files
                .get(&file.path)
                .is_some_and(|cached| cached.seen == file)
            {
                continue;
            }
            let Some((spans, skipped)) = read_trace_file(&file.path) else {
                self.files.remove(&file.path);
                continue;
            };
            read += 1;
            self.files.insert(
                file.path.clone(),
                CachedFile {
                    seen: file,
                    spans,
                    skipped,
                },
            );
        }
        read
    }

    /// Summarize the cached spans that started within `days` of `now` (all of them when
    /// `None`).
    #[must_use]
    pub fn summarize(&self, days: Option<u32>, now: DateTime<Utc>) -> Summary {
        let since = days.map(|d| now - Duration::days(i64::from(d)));
        let spans: Vec<&SpanRecord> = self.files.values().flat_map(|f| &f.spans).collect();
        let mut summary = summarize_since(&spans, since, now);
        summary.since = since;
        summary.files = self.files.len() as u64;
        summary.skipped_lines = self.files.values().map(|f| f.skipped).sum();
        summary
    }
}

#[cfg(test)]
impl LogCache {
    /// A cache holding `spans` as if read from one file.
    pub(crate) fn from_spans(spans: Vec<SpanRecord>) -> Self {
        let path = PathBuf::from("loop-test-1.jsonl");
        let seen = LogFile {
            path: path.clone(),
            len: 0,
            modified: None,
        };
        let file = CachedFile {
            seen,
            spans,
            skipped: 0,
        };
        Self {
            files: BTreeMap::from([(path, file)]),
        }
    }
}

/// Spans in one trace file and the number of lines that could not be parsed; `None` when
/// the file can't be opened.
fn read_trace_file(path: &Path) -> Option<(Vec<SpanRecord>, u64)> {
    let file = std::fs::File::open(path).ok()?;
    let (mut spans, mut skipped) = (Vec::new(), 0);
    for line in BufReader::new(file).lines() {
        let Ok(line) = line else {
            skipped += 1;
            break;
        };
        if line.trim().is_empty() {
            continue;
        }
        match serde_json::from_str::<SpanRecord>(&line) {
            Ok(span) => spans.push(span),
            Err(_) => skipped += 1,
        }
    }
    Some((spans, skipped))
}
