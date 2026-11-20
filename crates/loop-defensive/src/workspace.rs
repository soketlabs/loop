//! The workflow's own sandbox tools (Loop's `read`, `write`, `edit` and
//! `bash` cover the rest, see [`crate::env`]): listing, search, analyzers,
//! the diff and submissions, inside the task container.
//!
//! Every file operation runs in the container (`podman exec`), so a symlink
//! the agent's own commands planted can only ever point into the container's
//! filesystem, never at the host. Paths are checked twice: as relative paths
//! here, and with `realpath` inside the container. What the agent submits (its
//! report, findings) is kept in `state_dir`, outside the agent-writable tree.

use std::fs;
use std::io::{Cursor, Read, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use regex::Regex;
use serde_json::{json, Value};

use crate::analyzers;
use crate::patchlib::{self, trunc, MAX_FILE_BYTES, SKIP_DIRS};
use crate::report::to_json_pretty;
use crate::sandbox::{shell_quote, Container};

/// Largest tool output returned to the model.
pub const MAX_OUT: usize = 20_000;
const FILE_OP: Duration = Duration::from_secs(900);
const COMMAND: Duration = Duration::from_secs(600);
const GO_SUBCOMMANDS: &[&str] = &["build", "test", "vet", "list"];
const GO_BANNED_FLAGS: &[&str] = &["-exec", "-toolexec", "-modfile", "-overlay"];

/// `patch` (writable `/work`) or `audit` (read-only source).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// `/patch`.
    Patch,
    /// `/audit`.
    Audit,
}

/// One running task container and everything its tools need.
pub struct Sandbox {
    /// The container.
    pub container: Container,
    /// Host copy of the tree the container sees at `inner`.
    pub root: PathBuf,
    /// The tree inside the container (`/work`, `/opt/audit/source`).
    pub inner: String,
    /// Patch or audit.
    pub mode: Mode,
    /// Language profile (`go`, `node`, ...).
    pub lang: String,
    /// Host copy of the original source, for `show_diff`.
    pub baseline: PathBuf,
    /// Public task facts (`get_task`).
    pub task_json: PathBuf,
    /// Where submissions and the tool log go (outside the workspace).
    pub state_dir: PathBuf,
    /// Analyzer paths inside the container.
    pub analyzers: analyzers::Paths,
}

/// A tool's outcome: `Ok` for a result the model reads (which may itself say
/// `ok: false`), `Err` for a failed call.
pub type ToolOutcome = Result<Value, String>;

fn arg_str<'a>(args: &'a Value, key: &str) -> Option<&'a str> {
    args.get(key).and_then(Value::as_str)
}

fn arg_int(args: &Value, key: &str) -> Option<i64> {
    match args.get(key)? {
        Value::Number(n) => n.as_i64(),
        Value::String(s) => s.trim().parse().ok(),
        _ => None,
    }
}

fn required<'a>(args: &'a Value, key: &str) -> Result<&'a str, String> {
    arg_str(args, key).ok_or_else(|| format!("TypeError: missing required argument '{key}'"))
}

impl Sandbox {
    /// Normalize a tool path to `.` or a safe relative path.
    fn rel(&self, path: &str, for_write: bool) -> Result<String, String> {
        let mut s = path.trim();
        while let Some(rest) = s.strip_prefix("./") {
            s = rest;
        }
        let rel = if s.is_empty() || s == "." {
            ".".to_owned()
        } else {
            patchlib::safe_relpath(s.trim_end_matches('/'))
                .ok_or_else(|| format!("ValueError: path not allowed: '{path}'"))?
        };
        if for_write {
            if self.mode == Mode::Audit {
                return Err("ValueError: the audit source is read-only".into());
            }
            let first = rel.split('/').next().unwrap_or("");
            if rel == "." || first == ".defensive" || first == ".git" {
                return Err("ValueError: that location is reserved".into());
            }
        }
        Ok(rel)
    }

    fn inner_path(&self, rel: &str) -> String {
        if rel == "." {
            self.inner.clone()
        } else {
            format!("{}/{rel}", self.inner)
        }
    }

    /// A shell prelude that resolves `$1` (an existing path) into `$p` and
    /// exits 4 if it escapes the tree.
    fn resolve_script(&self) -> String {
        let root = shell_quote(&self.inner);
        format!(
            "set -u; root={root}; p=$(realpath -e -- \"$1\" 2>/dev/null) || {{ echo 'no such file or directory' >&2; exit 3; }}; \
             case \"$p\" in \"$root\"|\"$root\"/*) ;; *) echo 'path escapes the workspace' >&2; exit 4;; esac; "
        )
    }

    fn exec_script(&self, script: &str, arg: &str, stdin: Option<&str>) -> crate::sandbox::Output {
        self.container.exec(&["bash", "-c", script, "defensive", arg], None, &[("LC_ALL", "C")], stdin, FILE_OP)
    }

    fn script_error(r: &crate::sandbox::Output, path: &str) -> String {
        match r.exit {
            Some(3) => format!("FileNotFoundError: no such file or directory: '{path}'"),
            Some(4) => format!("ValueError: path escapes the workspace: '{path}'"),
            Some(5) => "ValueError: refusing to write through a symlink".into(),
            Some(6) => format!("IsADirectoryError: '{path}' is a directory"),
            _ => format!("OSError: {}", patchlib::tail(r.stderr.trim(), 300)),
        }
    }

    /// `get_task`.
    pub fn get_task(&self) -> ToolOutcome {
        let text = fs::read_to_string(&self.task_json).map_err(|e| format!("OSError: {e}"))?;
        let mut t: Value = serde_json::from_str(&text).map_err(|e| format!("JSONDecodeError: {e}"))?;
        t["workspace"] = "Paths are relative to the module root of the vulnerable source.".into();
        Ok(t)
    }

    /// `list_directory`.
    pub fn list_directory(&self, args: &Value) -> ToolOutcome {
        let path = arg_str(args, "path").unwrap_or(".");
        let rel = self.rel(path, false)?;
        let script = format!("{}[ -d \"$p\" ] || {{ echo 'not a directory' >&2; exit 8; }}; cd -- \"$p\" && ls -1Ap", self.resolve_script());
        let r = self.exec_script(&script, &self.inner_path(&rel), None);
        if !r.ok() {
            return Err(if r.exit == Some(8) {
                format!("NotADirectoryError: '{path}'")
            } else {
                Self::script_error(&r, path)
            });
        }
        let entries: Vec<&str> = r
            .stdout
            .lines()
            .filter(|e| !e.is_empty() && *e != ".defensive/" && *e != ".defensive")
            .take(500)
            .collect();
        Ok(json!({"path": path, "entries": entries}))
    }

    /// `search_code`: the tree is streamed out of the container as a tar and
    /// searched in memory; nothing is unpacked on the host.
    pub fn search_code(&self, args: &Value) -> ToolOutcome {
        let pattern = required(args, "pattern")?;
        let rx = Regex::new(pattern).map_err(|e| format!("error: bad regex: {e}"))?;
        let path = arg_str(args, "path").unwrap_or(".");
        let max = arg_int(args, "max_results").unwrap_or(100).max(1) as usize;
        let rel = self.rel(path, false)?;
        let excludes: String = SKIP_DIRS.iter().map(|d| format!(" --exclude={}", shell_quote(d))).collect();
        let script = format!(
            "{}if [ -d \"$p\" ]; then tar -C \"$p\"{excludes} -cf - .; else tar -C \"$(dirname -- \"$p\")\" -cf - \"$(basename -- \"$p\")\"; fi",
            self.resolve_script()
        );
        let out = self.container_bytes(&script, &self.inner_path(&rel))?;
        let mut archive = tar::Archive::new(Cursor::new(out.0));
        let mut hits = Vec::new();
        let is_dir = out.1;
        for entry in archive.entries().map_err(|e| format!("OSError: {e}"))? {
            let Ok(mut entry) = entry else { continue };
            if !entry.header().entry_type().is_file() || entry.header().size().unwrap_or(0) > MAX_FILE_BYTES {
                continue;
            }
            let name = entry.path().map(|p| p.to_string_lossy().into_owned()).unwrap_or_default();
            let name = name.trim_start_matches("./").to_owned();
            let mut data = Vec::new();
            if entry.read_to_end(&mut data).is_err() || data.iter().take(8192).any(|b| *b == 0) {
                continue;
            }
            let shown = if !is_dir {
                path.to_owned()
            } else if rel == "." {
                name.clone()
            } else {
                format!("{}/{name}", path.trim_end_matches('/'))
            };
            for (i, line) in String::from_utf8_lossy(&data).lines().enumerate() {
                if rx.is_match(line) {
                    let snippet: String = line.trim().chars().take(200).collect();
                    hits.push(format!("{shown}:{}: {snippet}", i + 1));
                    if hits.len() >= max {
                        return Ok(json!({"matches": hits, "truncated": true}));
                    }
                }
            }
        }
        Ok(json!({"matches": hits, "truncated": false}))
    }

    /// Run a script that writes binary data; returns `(stdout bytes, is_dir)`.
    fn container_bytes(&self, script: &str, arg: &str) -> Result<(Vec<u8>, bool), String> {
        let probe = self.exec_script(&format!("{}[ -d \"$p\" ]", self.resolve_script()), arg, None);
        if matches!(probe.exit, Some(3) | Some(4)) {
            return Err(Self::script_error(&probe, arg));
        }
        let out = std::process::Command::new("podman")
            .args(["exec", "-e", "LC_ALL=C", &self.container.name, "timeout", "-k", "5", "900", "bash", "-c", script, "defensive", arg])
            .output()
            .map_err(|e| format!("OSError: {e}"))?;
        if !out.status.success() {
            return Err(format!("OSError: {}", patchlib::tail(String::from_utf8_lossy(&out.stderr).trim(), 300)));
        }
        Ok((out.stdout, probe.ok()))
    }

    /// `run_go`.
    pub fn run_go(&self, args: &Value) -> ToolOutcome {
        let raw = required(args, "args")?;
        let mut argv = split_shell(raw).map_err(|e| format!("ValueError: {e}"))?;
        if argv.first().map(String::as_str) == Some("go") {
            argv.remove(0);
        }
        if argv.first().is_none_or(|a| !GO_SUBCOMMANDS.contains(&a.as_str())) {
            return Ok(json!({"error": format!("allowed: go {GO_SUBCOMMANDS:?} ...")}));
        }
        if argv.iter().any(|a| GO_BANNED_FLAGS.contains(&a.split('=').next().unwrap_or(""))) {
            return Ok(json!({"error": format!("flags not allowed: {GO_BANNED_FLAGS:?}")}));
        }
        let mut full: Vec<&str> = vec!["go"];
        full.extend(argv.iter().map(String::as_str));
        let r = self.container.exec(&full, Some(&self.inner), &[], None, COMMAND);
        if r.timed_out {
            return Ok(json!({"exit_code": null, "output": "timed out after 600s"}));
        }
        Ok(json!({"exit_code": r.exit, "output": trunc(&r.combined(), MAX_OUT)}))
    }

    /// `show_diff`.
    pub fn show_diff(&self) -> ToolOutcome {
        let d = patchlib::diff_trees(&self.baseline, &self.root, &self.lang);
        Ok(json!({"code_files": d.code_files, "test_files": d.test_files, "ignored_files": d.ignored_files,
                  "code_diff": trunc(&d.code_patch, MAX_OUT), "test_diff": trunc(&d.test_patch, MAX_OUT)}))
    }

    /// `run_analyzer`.
    pub fn run_analyzer(&self, args: &Value) -> ToolOutcome {
        let tool = arg_str(args, "tool").unwrap_or("all");
        Ok(analyzers::tool_result(tool, &self.container, &self.analyzers, &self.root))
    }

    /// `submit_report`: the root-cause report.
    pub fn submit_report(&self, args: &Value) -> ToolOutcome {
        for key in ["summary", "root_cause", "vulnerable_lines", "fix_description", "verification"] {
            required(args, key)?;
        }
        if !args.get("affected_files").is_some_and(Value::is_array) {
            return Err("TypeError: missing required argument 'affected_files'".into());
        }
        let field = |k: &str| args.get(k).cloned().unwrap_or_else(|| Value::String(String::new()));
        let rep = json!({
            "summary": field("summary"), "root_cause": field("root_cause"),
            "affected_files": field("affected_files"), "vulnerable_lines": field("vulnerable_lines"),
            "fix_description": field("fix_description"), "verification": field("verification"),
            "repro_command": field("repro_command"), "test_command": field("test_command"),
        });
        fs::create_dir_all(&self.state_dir).map_err(|e| format!("OSError: {e}"))?;
        fs::write(self.state_dir.join("agent_report.json"), to_json_pretty(&rep)).map_err(|e| format!("OSError: {e}"))?;
        Ok(json!({"ok": true, "note": "report saved; submit or grade the fix when it is final"}))
    }

    /// `submit_finding` (audit): one finding.
    pub fn submit_finding(&self, args: &Value) -> ToolOutcome {
        for key in ["title", "severity", "cwe", "file", "description", "data_flow", "reachability", "impact", "recommendation"] {
            required(args, key)?;
        }
        let line = |k: &str| arg_int(args, k).ok_or_else(|| format!("TypeError: missing required argument '{k}'"));
        let (start, end) = (line("start_line")?, line("end_line")?);
        let s = |k: &str| arg_str(args, k).unwrap_or("").to_owned();
        let rec = json!({"title": s("title"), "severity": s("severity").to_lowercase(), "cwe": s("cwe"),
            "location": {"file": s("file"), "start_line": start, "end_line": end},
            "description": s("description"), "data_flow": s("data_flow"), "reachability": s("reachability"),
            "impact": s("impact"), "recommendation": s("recommendation"), "evidence": s("evidence")});
        fs::create_dir_all(&self.state_dir).map_err(|e| format!("OSError: {e}"))?;
        let file = self.state_dir.join("findings.jsonl");
        let mut f = fs::OpenOptions::new().create(true).append(true).open(&file).map_err(|e| format!("OSError: {e}"))?;
        writeln!(f, "{rec}").map_err(|e| format!("OSError: {e}"))?;
        let n = fs::read_to_string(&file).map(|t| t.lines().count()).unwrap_or(0);
        Ok(json!({"ok": true, "recorded": n, "note": format!("finding #{n} recorded")}))
    }

    /// `submit_audit_report` (audit): the overview.
    pub fn submit_audit_report(&self, args: &Value) -> ToolOutcome {
        for key in ["summary", "scope", "methodology", "overall_risk"] {
            required(args, key)?;
        }
        let s = |k: &str| arg_str(args, k).unwrap_or("").to_owned();
        fs::create_dir_all(&self.state_dir).map_err(|e| format!("OSError: {e}"))?;
        fs::write(
            self.state_dir.join("report.json"),
            to_json_pretty(&json!({"summary": s("summary"), "scope": s("scope"),
                                   "methodology": s("methodology"), "overall_risk": s("overall_risk")})),
        )
        .map_err(|e| format!("OSError: {e}"))?;
        Ok(json!({"ok": true, "note": "audit overview recorded; you may stop"}))
    }

    /// Run one sandbox tool by name, and log it.
    pub fn call(&self, name: &str, args: &Value) -> ToolOutcome {
        let t0 = Instant::now();
        let out = match name {
            "get_task" => self.get_task(),
            "list_directory" => self.list_directory(args),
            "search_code" => self.search_code(args),
            "run_go" => self.run_go(args),
            "show_diff" => self.show_diff(),
            "run_analyzer" => self.run_analyzer(args),
            "submit_report" => self.submit_report(args),
            "submit_finding" => self.submit_finding(args),
            "submit_audit_report" => self.submit_audit_report(args),
            other => Err(format!("unknown tool {other}")),
        };
        self.log_call(name, args, &out, t0.elapsed().as_secs_f64());
        out
    }

    /// Append one call to the tool log.
    pub fn log_call(&self, name: &str, args: &Value, out: &ToolOutcome, secs: f64) {
        let short: serde_json::Map<String, Value> = args
            .as_object()
            .into_iter()
            .flatten()
            .map(|(k, v)| {
                let text = crate::report::value_text(v);
                let v = if text.chars().count() <= 300 { v.clone() } else { format!("{}...", text.chars().take(300).collect::<String>()).into() };
                (k.clone(), v)
            })
            .collect();
        let (res, err) = match out {
            Ok(v) => (v.to_string(), false),
            Err(e) => (json!({"error": e}).to_string(), true),
        };
        let res = if res.chars().count() <= 600 { res } else { format!("{}...", res.chars().take(600).collect::<String>()) };
        let line = json!({"tool": name, "args": short, "error": err, "secs": (secs * 100.0).round() / 100.0, "result": res});
        if fs::create_dir_all(&self.state_dir).is_ok() {
            if let Ok(mut f) = fs::OpenOptions::new().create(true).append(true).open(self.state_dir.join("tool_log.jsonl")) {
                let _ = writeln!(f, "{line}");
            }
        }
    }

    /// Tool calls so far, from the log.
    pub fn tool_calls(&self) -> Vec<Value> {
        tool_calls_from(&self.state_dir.join("tool_log.jsonl"))
    }

    /// The submitted root-cause report, if any.
    pub fn report(&self) -> Option<Value> {
        load_json(&self.state_dir.join("agent_report.json"))
    }
}

/// Logged tool calls (at most 5000).
pub fn tool_calls_from(log: &Path) -> Vec<Value> {
    fs::read_to_string(log)
        .map(|t| t.lines().take(5000).filter_map(|l| serde_json::from_str(l).ok()).collect())
        .unwrap_or_default()
}

/// A small JSON file that is a regular file, or `None`.
pub fn load_json(p: &Path) -> Option<Value> {
    let meta = fs::symlink_metadata(p).ok()?;
    if !meta.file_type().is_file() || meta.len() >= 200_000 {
        return None;
    }
    serde_json::from_str(&fs::read_to_string(p).ok()?).ok()
}

/// Split a command line like `shlex.split`.
pub fn split_shell(s: &str) -> Result<Vec<String>, String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut in_word = false;
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\'' => {
                in_word = true;
                loop {
                    match chars.next() {
                        Some('\'') => break,
                        Some(x) => cur.push(x),
                        None => return Err("No closing quotation".into()),
                    }
                }
            }
            '"' => {
                in_word = true;
                loop {
                    match chars.next() {
                        Some('"') => break,
                        Some('\\') => match chars.next() {
                            Some(x @ ('"' | '\\' | '$' | '`')) => cur.push(x),
                            Some(x) => {
                                cur.push('\\');
                                cur.push(x);
                            }
                            None => return Err("No closing quotation".into()),
                        },
                        Some(x) => cur.push(x),
                        None => return Err("No closing quotation".into()),
                    }
                }
            }
            '\\' => {
                in_word = true;
                match chars.next() {
                    Some(x) => cur.push(x),
                    None => return Err("No escaped character".into()),
                }
            }
            c if c.is_whitespace() => {
                if in_word {
                    out.push(std::mem::take(&mut cur));
                    in_word = false;
                }
            }
            c => {
                in_word = true;
                cur.push(c);
            }
        }
    }
    if in_word {
        out.push(cur);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shell_split_like_shlex() {
        assert_eq!(split_shell("test ./pkg -run 'TestA|TestB' -v").unwrap(), vec!["test", "./pkg", "-run", "TestA|TestB", "-v"]);
        assert_eq!(split_shell(r#"a "b c" d\ e"#).unwrap(), vec!["a", "b c", "d e"]);
        assert!(split_shell("'open").is_err());
    }

    #[test]
    fn tool_log_reader_skips_bad_lines() {
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("tool_log.jsonl");
        fs::write(&log, "{\"tool\":\"read_file\"}\nnot json\n{\"tool\":\"edit_file\"}\n").unwrap();
        let calls = tool_calls_from(&log);
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[1]["tool"], "edit_file");
    }

    #[test]
    fn load_json_refuses_symlinks() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("a.json"), "{\"x\":1}").unwrap();
        std::os::unix::fs::symlink(dir.path().join("a.json"), dir.path().join("b.json")).unwrap();
        assert_eq!(load_json(&dir.path().join("a.json")).unwrap()["x"], 1);
        assert!(load_json(&dir.path().join("b.json")).is_none());
    }
}
