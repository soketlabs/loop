//! Per-stage reports: a box of text, an `NN_stage.json` file and a section in
//! `report.md`.

use std::collections::BTreeSet;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::Instant;

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

/// Width of a stage box, borders included.
const BOX_WIDTH: usize = 92;

/// The outcome of one stage.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct StageResult {
    /// For example `0 resolve` or `6 grade (attempt 1)`.
    pub stage: String,
    /// `PASS`, `FAIL`, `SKIP` or `STOP`.
    pub status: String,
    /// One line.
    pub summary: String,
    /// `DISCOVERED`, `BUILDABLE`, `PATCH_VERIFIED`, ...
    pub lifecycle: Option<String>,
    /// Shown as `key: value` lines, in insertion order.
    pub facts: Map<String, Value>,
    /// Saved to the JSON file only.
    pub data: Map<String, Value>,
    /// Seconds the stage took.
    pub secs: f64,
}

impl StageResult {
    /// A result with no lifecycle, facts or data.
    pub fn new(stage: impl Into<String>, status: &str, summary: impl Into<String>) -> Self {
        Self {
            stage: stage.into(),
            status: status.to_owned(),
            summary: summary.into(),
            lifecycle: None,
            facts: Map::new(),
            data: Map::new(),
            secs: 0.0,
        }
    }

    /// Set the lifecycle.
    pub fn lifecycle(mut self, lifecycle: impl Into<String>) -> Self {
        self.lifecycle = Some(lifecycle.into());
        self
    }

    /// Add a fact.
    pub fn fact(mut self, key: &str, value: impl Into<Value>) -> Self {
        self.facts.insert(key.to_owned(), value.into());
        self
    }

    /// Add facts from `(key, value)` pairs.
    pub fn facts(mut self, facts: impl IntoIterator<Item = (String, Value)>) -> Self {
        self.facts.extend(facts);
        self
    }

    /// Add a data entry (JSON file only).
    pub fn data(mut self, key: &str, value: impl Into<Value>) -> Self {
        self.data.insert(key.to_owned(), value.into());
        self
    }

    /// Set the duration.
    pub fn secs(mut self, secs: f64) -> Self {
        self.secs = secs;
        self
    }

    /// The stage as a text box.
    pub fn render(&self) -> String {
        let icon = match self.status.as_str() {
            "PASS" => "✔",
            "FAIL" => "✘",
            "SKIP" => "–",
            "STOP" => "■",
            _ => "?",
        };
        let width = BOX_WIDTH - 2;
        let title = format!(
            " {icon} STAGE {} — {} ({:.1}s) ",
            self.stage.to_uppercase(),
            self.status,
            self.secs
        );
        let mut out = format!("┌{}┐\n", pad(&title, width, '─'));
        let mut lines = wrap(&self.summary, width - 2, "");
        if lines.is_empty() {
            lines.push(String::new());
        }
        if let Some(lifecycle) = &self.lifecycle {
            lines.push(format!("lifecycle: {lifecycle}"));
        }
        for (k, v) in &self.facts {
            let wrapped = wrap(&format!("{k}: {}", value_text(v)), width - 2, "    ");
            if wrapped.is_empty() {
                lines.push(format!("{k}:"));
            } else {
                lines.extend(wrapped);
            }
        }
        for line in lines {
            out.push_str(&format!("│ {} │\n", pad(&line, width - 2, ' ')));
        }
        out.push_str(&format!("└{}┘\n", "─".repeat(width)));
        out
    }
}

/// How a fact value is printed: strings bare, everything else as JSON-ish text.
pub fn value_text(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Bool(true) => "True".into(),
        Value::Bool(false) => "False".into(),
        Value::Null => "None".into(),
        other => other.to_string(),
    }
}

fn pad(s: &str, width: usize, fill: char) -> String {
    let n = s.chars().count();
    if n >= width {
        s.to_owned()
    } else {
        let mut out = s.to_owned();
        out.extend(std::iter::repeat_n(fill, width - n));
        out
    }
}

/// Greedy word wrap like Python's `textwrap.wrap`.
pub fn wrap(text: &str, width: usize, subsequent_indent: &str) -> Vec<String> {
    let mut lines: Vec<String> = Vec::new();
    // The line being filled; "" or only the indent means nothing is on it yet.
    let mut cur = String::new();
    for word in text.split_whitespace() {
        let len = |s: &str| s.chars().count();
        if !cur.trim().is_empty() {
            if len(&cur) + 1 + len(word) <= width {
                cur.push(' ');
                cur.push_str(word);
                continue;
            }
            lines.push(std::mem::take(&mut cur));
        }
        if cur.is_empty() && !lines.is_empty() {
            cur = subsequent_indent.to_owned();
        }
        // Place the word on a fresh line, breaking it when it is longer than one.
        let mut rest: Vec<char> = word.chars().collect();
        loop {
            let room = width.saturating_sub(len(&cur)).max(1);
            if rest.len() <= room {
                cur.extend(rest);
                break;
            }
            cur.extend(rest.drain(..room));
            lines.push(std::mem::replace(&mut cur, subsequent_indent.to_owned()));
        }
    }
    if !cur.trim().is_empty() {
        lines.push(cur);
    }
    lines
}

/// Where rendered stage boxes go.
pub type Sink = Box<dyn FnMut(&str) + Send>;

/// Writes each stage to the sink, `NN_stage.json` and `report.md`.
///
/// Fact keys in `hidden_keys` (oracle test names) are kept out of the visible
/// outputs and written only to `hidden_dir`.
pub struct Reporter {
    /// The run directory.
    pub run_dir: PathBuf,
    /// `report.md` in the run directory.
    pub md: PathBuf,
    /// Every stage emitted so far.
    pub results: Vec<StageResult>,
    hidden_keys: BTreeSet<String>,
    hidden_dir: Option<PathBuf>,
    buffer: String,
}

impl Reporter {
    /// Start `report.md` for `title`.
    pub fn new(run_dir: &Path, title: &str) -> std::io::Result<Self> {
        let md = run_dir.join("report.md");
        fs::write(
            &md,
            format!("# Defensive run: {title}\n\nRun directory: `{}`\n\n", run_dir.display()),
        )?;
        Ok(Self {
            run_dir: run_dir.to_path_buf(),
            md,
            results: Vec::new(),
            hidden_keys: BTreeSet::new(),
            hidden_dir: None,
            buffer: String::new(),
        })
    }

    /// Keep `keys` out of visible facts; write the full result to `dir`.
    pub fn hide(mut self, keys: &[&str], dir: &Path) -> Self {
        self.hidden_keys = keys.iter().map(|k| (*k).to_owned()).collect();
        self.hidden_dir = Some(dir.to_path_buf());
        self
    }

    /// Append free text (a heading line) to the output buffer.
    pub fn say(&mut self, text: &str) {
        self.buffer.push_str(text);
        if !text.ends_with('\n') {
            self.buffer.push('\n');
        }
    }

    /// Everything printed since the last call.
    pub fn take_output(&mut self) -> String {
        std::mem::take(&mut self.buffer)
    }

    /// Record a stage.
    pub fn emit(&mut self, mut r: StageResult) -> StageResult {
        let slug = r.stage.replace(' ', "_");
        if let Some(dir) = &self.hidden_dir {
            if r.facts.keys().any(|k| self.hidden_keys.contains(k)) {
                let _ = write_json(&dir.join(format!("{slug}.json")), &serde_json::to_value(&r).unwrap_or_default());
                r.facts.retain(|k, _| !self.hidden_keys.contains(k));
            }
        }
        let _ = write_json(
            &self.run_dir.join(format!("{slug}.json")),
            &serde_json::to_value(&r).unwrap_or_default(),
        );
        self.buffer.push_str(&r.render());
        if let Ok(mut f) = fs::OpenOptions::new().append(true).open(&self.md) {
            let mut s = format!("## Stage {}: {}\n\n{}\n\n", r.stage, r.status, r.summary);
            if let Some(lifecycle) = &r.lifecycle {
                s.push_str(&format!("- **lifecycle**: {lifecycle}\n"));
            }
            for (k, v) in &r.facts {
                s.push_str(&format!("- **{k}**: {}\n", value_text(v)));
            }
            s.push_str(&format!("- **time**: {:.1}s\n\n", r.secs));
            let _ = f.write_all(s.as_bytes());
        }
        self.results.push(r.clone());
        r
    }

    /// Append raw markdown to `report.md`.
    pub fn append_md(&self, text: &str) {
        if let Ok(mut f) = fs::OpenOptions::new().append(true).open(&self.md) {
            let _ = f.write_all(text.as_bytes());
        }
    }
}

/// Pretty JSON with a trailing newline-free body, like `json.dumps(indent=2)`.
pub fn write_json(path: &Path, value: &Value) -> std::io::Result<()> {
    fs::write(path, to_json_pretty(value))
}

/// `json.dumps(value, indent=2)` with non-ASCII escaped, as Python writes it.
pub fn to_json_pretty(value: &Value) -> String {
    let s = serde_json::to_string_pretty(value).unwrap_or_default();
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        if c.is_ascii() {
            out.push(c);
        } else {
            let mut buf = [0u16; 2];
            for unit in c.encode_utf16(&mut buf) {
                out.push_str(&format!("\\u{unit:04x}"));
            }
        }
    }
    out
}

/// Measures a stage.
pub struct Timer(Instant);

impl Timer {
    /// Start now.
    pub fn start() -> Self {
        Self(Instant::now())
    }

    /// Seconds since start.
    pub fn secs(&self) -> f64 {
        self.0.elapsed().as_secs_f64()
    }
}

/// Count by `key`, keeping first-seen order: `tool×n, ...`.
pub fn counts_text<'a>(items: impl IntoIterator<Item = &'a str>) -> String {
    let mut order: Vec<(String, usize)> = Vec::new();
    for it in items {
        match order.iter_mut().find(|(k, _)| k == it) {
            Some((_, n)) => *n += 1,
            None => order.push((it.to_owned(), 1)),
        }
    }
    order
        .iter()
        .map(|(k, n)| format!("{k}×{n}"))
        .collect::<Vec<_>>()
        .join(", ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn box_has_fixed_width_and_wraps_facts() {
        let r = StageResult::new("0 resolve", "PASS", "cloned x")
            .lifecycle("DISCOVERED")
            .fact("files", 45)
            .fact("url", "https://example.com/".repeat(8));
        let text = r.render();
        for line in text.lines() {
            assert_eq!(line.chars().count(), BOX_WIDTH, "{line}");
        }
        assert!(text.starts_with("┌ ✔ STAGE 0 RESOLVE — PASS (0.0s) ─"));
        assert!(text.contains("│ lifecycle: DISCOVERED"));
        assert!(text.contains("│ files: 45"));
    }

    #[test]
    fn wrap_matches_textwrap_shape() {
        assert_eq!(wrap("a b c", 3, ""), vec!["a b", "c"]);
        assert_eq!(wrap("key: aaaa bbbb", 9, "    "), vec!["key: aaaa", "    bbbb"]);
        assert!(wrap("", 10, "").is_empty());
    }

    #[test]
    fn counts_keep_first_seen_order() {
        assert_eq!(counts_text(["b", "a", "b"]), "b×2, a×1");
    }

    #[test]
    fn reporter_writes_json_and_markdown_and_hides_keys() {
        let dir = tempfile::tempdir().unwrap();
        let hidden = tempfile::tempdir().unwrap();
        let mut rep = Reporter::new(dir.path(), "CVE-1").unwrap().hide(&["oracle tests"], hidden.path());
        rep.emit(StageResult::new("2 split", "PASS", "ok").fact("oracle tests", "TestX").fact("pkgs", "./a"));
        let visible = fs::read_to_string(dir.path().join("2_split.json")).unwrap();
        assert!(!visible.contains("TestX"));
        assert!(fs::read_to_string(hidden.path().join("2_split.json")).unwrap().contains("TestX"));
        let md = fs::read_to_string(dir.path().join("report.md")).unwrap();
        assert!(md.contains("## Stage 2 split: PASS") && md.contains("- **pkgs**: ./a"));
        assert!(rep.take_output().contains("STAGE 2 SPLIT"));
    }
}

#[cfg(test)]
mod order_tests {
    use super::*;

    #[test]
    fn facts_keep_insertion_order() {
        let r = StageResult::new("1 scope", "PASS", "x").fact("zeta", 1).fact("alpha", 2);
        let keys: Vec<&String> = r.facts.keys().collect();
        assert_eq!(keys, vec!["zeta", "alpha"]);
    }
}
