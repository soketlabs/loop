//! Static analyzers, run inside a network-less container and normalized on
//! the host to one record shape:
//!
//! `{tool, rule, severity, confidence, cwe, file, line, message}`
//!
//! `gosec`, `go vet` and Python's `bandit` run on demand. `govulncheck` and
//! `npm audit` need their online databases, so their output was captured at
//! image-build time and is read back as-is.

use std::path::Path;
use std::time::Duration;

use serde_json::{json, Map, Value};

use crate::patchlib::tail;
use crate::sandbox::{Container, Output};

/// Where the analyzers look, inside the container.
#[derive(Debug, Clone)]
pub struct Paths {
    /// The source tree (`/work` for `/patch`, `/opt/audit/source` for `/audit`).
    pub source: String,
    /// govulncheck output captured at build time.
    pub govulncheck: String,
    /// `npm audit --json` output captured at build time.
    pub npm_audit: String,
}

impl Paths {
    /// The `/patch` image layout.
    pub fn patch() -> Self {
        Self {
            source: "/work".into(),
            govulncheck: "/opt/defensive/govulncheck.json".into(),
            npm_audit: "/opt/defensive/npm-audit.json".into(),
        }
    }

    /// The `/audit` image layout.
    pub fn audit() -> Self {
        Self {
            source: "/opt/audit/source".into(),
            govulncheck: "/opt/audit/govulncheck.json".into(),
            npm_audit: "/opt/defensive/npm-audit.json".into(),
        }
    }
}

/// One analyzer's run.
#[derive(Debug, Clone, Default)]
pub struct Run {
    /// Whether the analyzer exists in the image.
    pub available: bool,
    /// Normalized findings.
    pub findings: Vec<Value>,
    /// Why it could not run, if it could not.
    pub error: Option<String>,
}

impl Run {
    fn missing(why: &str) -> Self {
        Self {
            available: false,
            findings: vec![],
            error: Some(why.into()),
        }
    }

    fn failed(why: impl Into<String>) -> Self {
        Self {
            available: true,
            findings: vec![],
            error: Some(why.into()),
        }
    }
}

const TIMEOUT: Duration = Duration::from_secs(600);

/// The analyzers `run_analyzer` accepts.
pub const CHOICES: &[&str] = &["gosec", "go_vet", "govulncheck", "bandit", "npm_audit"];

fn not_installed(r: &Output) -> bool {
    r.exit == Some(127) || r.exit == Some(126)
}

fn rel(path: &str, source: &str) -> String {
    let prefix = format!("{}/", source.trim_end_matches('/'));
    path.strip_prefix(&prefix).unwrap_or(path).to_owned()
}

fn record(tool: &str, rule: Value, severity: &str, confidence: &str, cwe: Value, file: String, line: String, message: String) -> Value {
    json!({"tool": tool, "rule": rule, "severity": severity, "confidence": confidence,
           "cwe": cwe, "file": file, "line": line, "message": message})
}

fn line_text(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Null => String::new(),
        other => other.to_string(),
    }
}

/// gosec over `./...`.
pub fn gosec(c: &Container, p: &Paths) -> Run {
    let r = c.exec(&["gosec", "-quiet", "-fmt=json", "./..."], Some(&p.source), &[], None, TIMEOUT);
    if not_installed(&r) {
        return Run::missing("gosec not installed");
    }
    if r.timed_out {
        return Run::failed("gosec timed out");
    }
    parse_gosec(&r.stdout, &r.stderr, &p.source)
}

fn parse_gosec(stdout: &str, stderr: &str, source: &str) -> Run {
    let data: Value = match serde_json::from_str(if stdout.trim().is_empty() { "{}" } else { stdout }) {
        Ok(v) => v,
        Err(_) => return Run::failed(tail(if stderr.is_empty() { "no JSON" } else { stderr }, 400)),
    };
    let findings = data["Issues"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|i| {
            record(
                "gosec",
                i["rule_id"].clone(),
                &i["severity"].as_str().unwrap_or("").to_lowercase(),
                &i["confidence"].as_str().unwrap_or("").to_lowercase(),
                i["cwe"]["id"].clone(),
                rel(i["file"].as_str().unwrap_or(""), source),
                line_text(&i["line"]),
                i["details"].as_str().unwrap_or("").to_owned(),
            )
        })
        .collect();
    Run {
        available: true,
        findings,
        error: None,
    }
}

/// `go vet ./...`.
pub fn go_vet(c: &Container, p: &Paths) -> Run {
    let r = c.exec(&["go", "vet", "./..."], Some(&p.source), &[], None, TIMEOUT);
    if not_installed(&r) {
        return Run::missing("go not installed");
    }
    if r.timed_out {
        return Run::failed("go vet timed out");
    }
    Run {
        available: true,
        findings: parse_go_vet(&r.stderr, &p.source),
        error: None,
    }
}

fn parse_go_vet(stderr: &str, source: &str) -> Vec<Value> {
    stderr
        .lines()
        .filter_map(|ln| {
            let parts: Vec<&str> = ln.splitn(4, ':').collect();
            (parts.len() >= 4 && !parts[1].trim().is_empty() && parts[1].trim().chars().all(|c| c.is_ascii_digit())).then(|| {
                record("go vet", "vet".into(), "low", "low", Value::Null, rel(parts[0], source), parts[1].to_owned(), parts[3].trim().to_owned())
            })
        })
        .collect()
}

/// bandit over the source tree.
pub fn bandit(c: &Container, p: &Paths) -> Run {
    let r = c.exec(&["bandit", "-q", "-f", "json", "-r", "."], Some(&p.source), &[], None, TIMEOUT);
    if not_installed(&r) {
        return Run::missing("bandit not installed");
    }
    if r.timed_out {
        return Run::failed("bandit timed out");
    }
    parse_bandit(&r.stdout, &r.stderr, &p.source)
}

fn parse_bandit(stdout: &str, stderr: &str, source: &str) -> Run {
    let data: Value = match serde_json::from_str(if stdout.trim().is_empty() { "{}" } else { stdout }) {
        Ok(v) => v,
        Err(_) => return Run::failed(tail(if stderr.is_empty() { "no JSON" } else { stderr }, 400)),
    };
    let findings = data["results"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|i| {
            let file = i["filename"].as_str().unwrap_or("");
            let file = rel(file.strip_prefix("./").unwrap_or(file), source);
            record(
                "bandit",
                i["test_id"].clone(),
                &i["issue_severity"].as_str().unwrap_or("").to_lowercase(),
                &i["issue_confidence"].as_str().unwrap_or("").to_lowercase(),
                i["issue_cwe"]["id"].clone(),
                file,
                line_text(&i["line_number"]),
                i["issue_text"].as_str().unwrap_or("").to_owned(),
            )
        })
        .collect();
    Run {
        available: true,
        findings,
        error: None,
    }
}

fn read_artifact(c: &Container, path: &str) -> Option<String> {
    let r = c.exec(&["cat", path], None, &[], None, Duration::from_secs(60));
    r.ok().then_some(r.stdout)
}

/// govulncheck output captured at build time.
pub fn govulncheck(c: &Container, p: &Paths) -> Run {
    match read_artifact(c, &p.govulncheck) {
        None => Run::missing("no govulncheck artifact"),
        Some(text) => Run {
            available: true,
            findings: parse_govulncheck(&text, &p.source),
            error: None,
        },
    }
}

fn parse_govulncheck(text: &str, source: &str) -> Vec<Value> {
    let mut osvs: Map<String, Value> = Map::new();
    let mut findings: Vec<Value> = Vec::new();
    // govulncheck -json writes a stream of JSON objects, often one per line.
    let stream = serde_json::Deserializer::from_str(text).into_iter::<Value>();
    for msg in stream.flatten() {
        if let Some(o) = msg.get("osv") {
            let summary = o["summary"]
                .as_str()
                .filter(|s| !s.is_empty())
                .map(str::to_owned)
                .unwrap_or_else(|| o["details"].as_str().unwrap_or("").chars().take(160).collect());
            osvs.insert(o["id"].as_str().unwrap_or("").to_owned(), summary.into());
        }
        if let Some(f) = msg.get("finding") {
            let osv_id = f["osv"].as_str().unwrap_or("").to_owned();
            let top = f["trace"].as_array().and_then(|t| t.first()).cloned().unwrap_or(json!({}));
            let pos = &top["position"];
            let module = top["module"].as_str().unwrap_or("?");
            findings.push(json!({"tool": "govulncheck", "rule": osv_id, "severity": "high",
                "confidence": "high", "cwe": null,
                "file": rel(pos["filename"].as_str().unwrap_or(""), source),
                "line": line_text(&pos["line"]),
                "message": format!("{osv_id}: {{}} (module {module})")}));
        }
    }
    let mut seen = std::collections::BTreeSet::new();
    findings
        .into_iter()
        .map(|mut f| {
            let id = f["rule"].as_str().unwrap_or("").to_owned();
            let summary = osvs.get(&id).and_then(Value::as_str).unwrap_or("").to_owned();
            let msg = f["message"].as_str().unwrap_or("").replace("{}", &summary);
            f["message"] = msg.into();
            f
        })
        .filter(|f| seen.insert((f["rule"].to_string(), f["file"].to_string(), f["line"].to_string())))
        .collect()
}

/// `npm audit --json` captured at build time.
pub fn npm_audit(c: &Container, p: &Paths) -> Run {
    match read_artifact(c, &p.npm_audit) {
        None => Run::missing("no npm audit artifact"),
        Some(text) => parse_npm_audit(&text),
    }
}

fn parse_npm_audit(text: &str) -> Run {
    let data: Value = match serde_json::from_str(if text.trim().is_empty() { "{}" } else { text }) {
        Ok(v) => v,
        Err(_) => return Run::failed("npm audit output is not JSON"),
    };
    let mut findings = Vec::new();
    for (name, v) in data["vulnerabilities"].as_object().into_iter().flatten() {
        let via_all: Vec<Value> = v["via"].as_array().cloned().unwrap_or_default();
        let via: Vec<&Value> = via_all.iter().filter(|x| x.is_object()).collect();
        let title = if via.is_empty() {
            format!(
                "via {}",
                via_all.iter().map(|x| x.as_str().map(str::to_owned).unwrap_or_else(|| x.to_string())).collect::<Vec<_>>().join(", ")
            )
        } else {
            via.iter()
                .map(|x| format!("{} ({})", x["title"].as_str().unwrap_or(""), x["url"].as_str().unwrap_or("")))
                .collect::<Vec<_>>()
                .join("; ")
        };
        let cwe = via.iter().find_map(|x| x["cwe"].as_array().and_then(|c| c.first().cloned())).unwrap_or(Value::Null);
        let message: String = format!("{name} {}: {title}", v["range"].as_str().unwrap_or("")).chars().take(400).collect();
        findings.push(record(
            "npm audit",
            name.clone().into(),
            v["severity"].as_str().unwrap_or(""),
            "high",
            cwe,
            "package.json".into(),
            String::new(),
            message,
        ));
    }
    Run {
        available: true,
        findings,
        error: None,
    }
}

fn has_ext(root: &Path, ext: &str) -> bool {
    walkdir::WalkDir::new(root)
        .follow_links(false)
        .into_iter()
        .filter_entry(|e| e.file_name() != "node_modules" && e.file_name() != ".git")
        .flatten()
        .any(|e| e.file_type().is_file() && e.path().extension().is_some_and(|x| x == ext))
}

/// One analyzer by name.
pub fn run_one(tool: &str, c: &Container, p: &Paths) -> Option<Run> {
    Some(match tool {
        "gosec" => gosec(c, p),
        "go_vet" => go_vet(c, p),
        "govulncheck" => govulncheck(c, p),
        "bandit" => bandit(c, p),
        "npm_audit" => npm_audit(c, p),
        _ => return None,
    })
}

/// Every analyzer that fits the source at `host_source` (the host copy of
/// the tree the container sees at `p.source`).
pub fn run_all(c: &Container, p: &Paths, host_source: &Path) -> Vec<(String, Run)> {
    let mut results = Vec::new();
    if host_source.join("go.mod").is_file() || has_ext(host_source, "go") {
        results.push(("gosec".to_owned(), gosec(c, p)));
        results.push(("go_vet".to_owned(), go_vet(c, p)));
        results.push(("govulncheck".to_owned(), govulncheck(c, p)));
    }
    if has_ext(host_source, "py") {
        results.push(("bandit".to_owned(), bandit(c, p)));
    }
    if host_source.join("package.json").is_file() {
        results.push(("npm_audit".to_owned(), npm_audit(c, p)));
    }
    results
}

/// The `run_analyzer` tool result for `tool` (`all` or one of [`CHOICES`]).
pub fn tool_result(tool: &str, c: &Container, p: &Paths, host_source: &Path) -> Value {
    if tool.is_empty() || tool == "all" {
        let res = run_all(c, p, host_source);
        let findings: Vec<Value> = res.iter().flat_map(|(_, r)| r.findings.clone()).collect();
        let tools: Map<String, Value> = res
            .iter()
            .map(|(k, r)| (k.clone(), json!({"available": r.available, "count": r.findings.len(), "error": r.error})))
            .collect();
        return json!({"findings": findings, "tools": tools});
    }
    match run_one(tool, c, p) {
        Some(r) => json!({"findings": r.findings, "available": r.available, "error": r.error}),
        None => json!({"error": format!(
            "unknown analyzer '{tool}'; choices: gosec, go_vet, govulncheck, bandit, npm_audit, all")}),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gosec_json_normalizes() {
        let out = r#"{"Issues":[{"rule_id":"G107","severity":"MEDIUM","confidence":"HIGH","cwe":{"id":"88"},"file":"/work/a/b.go","line":"12","details":"Url provided"}]}"#;
        let r = parse_gosec(out, "", "/work");
        assert_eq!(r.findings[0]["file"], "a/b.go");
        assert_eq!(r.findings[0]["severity"], "medium");
        assert_eq!(r.findings[0]["cwe"], "88");
        assert!(parse_gosec("garbage", "boom", "/work").error.is_some());
    }

    #[test]
    fn go_vet_lines() {
        let f = parse_go_vet("# pkg\n/work/x.go:3:2: unreachable code\nnoise\n", "/work");
        assert_eq!(f.len(), 1);
        assert_eq!(f[0]["file"], "x.go");
        assert_eq!(f[0]["message"], "unreachable code");
    }

    #[test]
    fn npm_audit_shape_matches_recorded_baseline() {
        let text = r#"{"vulnerabilities":{"axios":{"severity":"high","range":"<1.8.2","via":[{"title":"SSRF","url":"https://x","cwe":["CWE-918"]}]},
                       "dep":{"severity":"low","range":"*","via":["axios"]}}}"#;
        let r = parse_npm_audit(text);
        assert_eq!(r.findings.len(), 2);
        assert_eq!(r.findings[0]["tool"], "npm audit");
        assert_eq!(r.findings[0]["message"], "axios <1.8.2: SSRF (https://x)");
        assert_eq!(r.findings[0]["cwe"], "CWE-918");
        assert_eq!(r.findings[1]["message"], "dep *: via axios");
    }

    #[test]
    fn govulncheck_stream_dedups() {
        let text = concat!(
            r#"{"osv":{"id":"GO-1","summary":"bad"}}"#, "\n",
            r#"{"finding":{"osv":"GO-1","trace":[{"module":"m","position":{"filename":"/work/a.go","line":3}}]}}"#, "\n",
            r#"{"finding":{"osv":"GO-1","trace":[{"module":"m","position":{"filename":"/work/a.go","line":3}}]}}"#, "\n"
        );
        let f = parse_govulncheck(text, "/work");
        assert_eq!(f.len(), 1);
        assert_eq!(f[0]["message"], "GO-1: bad (module m)");
        assert_eq!(f[0]["file"], "a.go");
    }
}
