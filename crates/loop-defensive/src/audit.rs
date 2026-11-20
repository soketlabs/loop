//! `/audit <github-url>`: a vulnerability report, with a report per stage.
//!
//!   0 resolve      parse the URL, clone at a pinned commit, detect the language
//!   1 scope        choose analyzers
//!   2 sandbox      podman build of the audit image (source + offline analyzers)
//!   3 baseline     run the analyzers once -> leads the agent starts from
//!   4 agent        the Loop session: investigate, triage leads, submit findings
//!   5 corroborate  per finding: location verified? analyzer-corroborated?
//!   6 report       report.md + findings.json, ranked by severity x confidence
//!
//! There is NO oracle: findings are candidates for human review, never a
//! verdict that the repository is or is not vulnerable.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use serde_json::{json, Map, Value};

use crate::analyzers;
use crate::patchlib;
use crate::report::{counts_text, write_json, Reporter, StageResult, Timer};
use crate::sandbox::{self, Container};
use crate::sources::{self, RepoCheckout};
use crate::task::link_latest;

fn severity_rank(s: &str) -> i32 {
    match s {
        "critical" => 4,
        "high" => 3,
        "medium" => 2,
        "low" => 1,
        _ => 0,
    }
}

fn confidence_rank(s: &str) -> i32 {
    match s {
        "poc-demonstrated" => 3,
        "tool-corroborated" => 2,
        "cited" => 1,
        _ => 0,
    }
}

/// One audit and its run directory.
pub struct Audit {
    /// The URL given.
    pub url: String,
    /// `<owner>-<name>-<UTC time>`.
    pub run_id: String,
    /// Visible outputs.
    pub run_dir: PathBuf,
    /// Submitted findings, the overview and the tool log.
    pub out_dir: PathBuf,
    /// Stage reports.
    pub rep: Reporter,
    /// `report.json`.
    pub summary: Map<String, Value>,
    /// The checkout.
    pub repo: Option<RepoCheckout>,
    /// The built image.
    pub image: Option<String>,
    /// Static-analyzer leads.
    pub baseline: Vec<Value>,
    /// Public facts for the agent.
    pub task_json: PathBuf,
}

impl Audit {
    /// Ready for the agent.
    pub fn ready(&self) -> bool {
        self.image.is_some()
    }

    /// The `status` in `report.json`.
    pub fn status(&self) -> String {
        self.summary.get("status").and_then(Value::as_str).unwrap_or("").to_owned()
    }
}

/// Stages 0-3.
pub fn prepare(url: &str, runs: &Path, git_ref: Option<&str>) -> Result<Audit, String> {
    let (owner, name) = sources::parse_github_url(url).map_err(|e| e.reason)?;
    let slug = format!("{owner}-{name}").to_lowercase();
    let run_id = format!("{slug}-{}", chrono::Utc::now().format("%Y%m%dT%H%M%SZ"));
    let base = runs.join("audit").join(&slug);
    let run_dir = base.join(&run_id);
    let out_dir = run_dir.join("out");
    fs::create_dir_all(&out_dir).map_err(|e| e.to_string())?;
    link_latest(&base, &run_id);
    let mut rep = Reporter::new(&run_dir, &slug).map_err(|e| e.to_string())?;
    rep.say(&format!("\n== defensive audit {run_id} ==  ({})\n", run_dir.display()));
    let mut summary = Map::new();
    summary.insert("url".into(), url.into());
    summary.insert("run_id".into(), run_id.clone().into());
    summary.insert("run_dir".into(), run_dir.to_string_lossy().into_owned().into());
    summary.insert("status".into(), "started".into());
    let mut a = Audit {
        url: url.into(),
        run_id,
        run_dir: run_dir.clone(),
        out_dir,
        rep,
        summary,
        repo: None,
        image: None,
        baseline: vec![],
        task_json: run_dir.join("task.json"),
    };

    // 0 resolve
    let t = Timer::start();
    let repo = match sources::resolve_repo(url, &run_dir.join("src"), git_ref) {
        Ok(r) => r,
        Err(e) => {
            a.rep.emit(StageResult::new("0 resolve", "STOP", e.reason.clone()).lifecycle(e.lifecycle.clone()).secs(t.secs()));
            a.summary.insert("status".into(), e.lifecycle.into());
            return Ok(a);
        }
    };
    let nfiles = patchlib::list_files(&repo.source_dir).len();
    a.rep.emit(
        StageResult::new("0 resolve", "PASS", format!("cloned {}/{} at {}", repo.owner, repo.name, &repo.sha[..repo.sha.len().min(12)]))
            .secs(t.secs())
            .fact("url", url)
            .fact("commit", repo.sha.clone())
            .fact("ref", repo.git_ref.clone())
            .fact("language", repo.language.clone())
            .fact("files", nfiles),
    );

    // 1 scope
    let names: &[&str] = match repo.language.as_str() {
        "Go" => &["gosec", "go_vet", "govulncheck"],
        "Python" => &["bandit"],
        _ => &[],
    };
    let summary = if names.is_empty() {
        format!("{} repo; no language analyzers (agent review + ripgrep only)", repo.language)
    } else {
        format!("{} repo; analyzers: {}", repo.language, names.join(", "))
    };
    a.rep.emit(
        StageResult::new("1 scope", "PASS", summary)
            .fact("analyzers", if names.is_empty() { "none (manual review)".to_owned() } else { names.join(", ") })
            .fact(
                "note",
                if repo.language == "Go" { "" } else { "no supported language analyzer; findings rely on agent review" },
            ),
    );

    // 2 sandbox
    if let Err(e) = sandbox::podman_available() {
        a.rep.emit(StageResult::new("2 sandbox", "FAIL", e).lifecycle("FAILED"));
        a.summary.insert("status".into(), "FAILED".into());
        a.repo = Some(repo);
        return Ok(a);
    }
    let t = Timer::start();
    let img = sandbox::build_audit_image(&slug, &repo.go_version, &repo.source_dir);
    if !img.ok {
        a.rep.emit(
            StageResult::new("2 sandbox", "FAIL", "podman build failed")
                .lifecycle("FAILED")
                .secs(t.secs())
                .fact("log tail", patchlib::tail(&img.log_tail, 600)),
        );
        a.summary.insert("status".into(), "FAILED".into());
        a.repo = Some(repo);
        return Ok(a);
    }
    a.rep.emit(
        StageResult::new("2 sandbox", "PASS", format!("built {}", img.tag))
            .secs(t.secs())
            .fact("image id", img.digest.clone().unwrap_or_default().chars().take(19).collect::<String>())
            .fact("runtime limits", sandbox::limits_text()),
    );
    a.image = Some(img.tag.clone());

    // 3 baseline scan (throwaway container)
    let t = Timer::start();
    let name = format!("defaudit-base-{}", a.run_id.to_lowercase());
    a.baseline = match Container::start(&name, &img.tag, &[(repo.source_dir.as_path(), "/opt/audit/source", "ro")], &[]) {
        Ok(c) => analyzers::tool_result("all", &c, &analyzers::Paths::audit(), &repo.source_dir)["findings"]
            .as_array()
            .cloned()
            .unwrap_or_default(),
        Err(_) => vec![],
    };
    let saved = a.run_dir.join("baseline_findings.json");
    let _ = write_json(&saved, &Value::Array(a.baseline.clone()));
    let by_tool = counts_text(a.baseline.iter().map(|f| f["tool"].as_str().unwrap_or("?")));
    a.rep.emit(
        StageResult::new("3 baseline", "PASS", format!("{} candidate hit(s) from static analyzers", a.baseline.len()))
            .secs(t.secs())
            .fact("by tool", if by_tool.is_empty() { "none".to_owned() } else { by_tool })
            .fact("saved", saved.to_string_lossy().into_owned()),
    );
    let _ = write_json(
        &a.task_json,
        &json!({"mode": "audit", "repository": format!("{}/{}", repo.owner, repo.name), "commit": repo.sha,
                "language": repo.language, "analyzer_leads": a.baseline.len(),
                "description": "Review this code base for security vulnerabilities and report them. The source is read-only."}),
    );
    a.repo = Some(repo);
    a.summary.insert("status".into(), "READY".into());
    Ok(a)
}

/// Stage 5: attach a confidence tier to each finding and rank them.
pub fn corroborate(a: &Audit, findings: &[Value]) -> Vec<Value> {
    let Some(repo) = &a.repo else { return findings.to_vec() };
    let base = &repo.source_dir;
    let mut out: Vec<Value> = findings
        .iter()
        .map(|f| {
            let loc = &f["location"];
            let file = loc["file"].as_str().unwrap_or("").trim().trim_start_matches(['.', '/']).to_owned();
            let rel = patchlib::safe_relpath(&file);
            let start = match &loc["start_line"] {
                Value::Number(n) => n.as_i64().unwrap_or(0),
                Value::String(s) => s.parse().unwrap_or(0),
                _ => 0,
            };
            let cited = rel.as_ref().is_some_and(|r| {
                let p = base.join(r);
                p.is_file() && {
                    let n = String::from_utf8_lossy(&fs::read(&p).unwrap_or_default()).lines().count() as i64;
                    1 <= start && start <= n
                }
            });
            let last = rel.as_deref().and_then(|r| r.rsplit('/').next()).unwrap_or("").to_owned();
            let tool_hit = a.baseline.iter().find_map(|b| {
                let bf = b["file"].as_str().unwrap_or("");
                let bl: i64 = b["line"].as_str().and_then(|l| l.parse().ok()).unwrap_or(0);
                (!bf.is_empty() && rel.is_some() && bf.ends_with(&last) && (bl - start).abs() <= 5)
                    .then(|| b["tool"].as_str().unwrap_or("").to_owned())
            });
            let conf = if f["evidence"].as_str().unwrap_or("").to_lowercase().starts_with("poc:") {
                "poc-demonstrated"
            } else if tool_hit.is_some() {
                "tool-corroborated"
            } else if cited {
                "cited"
            } else {
                "uncited"
            };
            let mut g = f.clone();
            g["confidence"] = conf.into();
            g["location_verified"] = cited.into();
            g["corroborating_tool"] = tool_hit.map_or(Value::Null, Value::from);
            g
        })
        .collect();
    // Uncited findings sink below everything else regardless of claimed severity.
    out.sort_by_key(|x| {
        let c = x["confidence"].as_str().unwrap_or("");
        std::cmp::Reverse((c != "uncited", severity_rank(x["severity"].as_str().unwrap_or("info")), confidence_rank(c)))
    });
    out
}

fn counts(items: &[Value], key: &str) -> Map<String, Value> {
    let mut m = Map::new();
    for it in items {
        let k = it[key].as_str().unwrap_or("?").to_owned();
        let n = m.get(&k).and_then(Value::as_u64).unwrap_or(0) + 1;
        m.insert(k, n.into());
    }
    m
}

fn py_dict(m: &Map<String, Value>) -> String {
    format!("{{{}}}", m.iter().map(|(k, v)| format!("'{k}': {v}")).collect::<Vec<_>>().join(", "))
}

/// Stages 5-6: corroborate, then write `findings.json`, `report.md` and `report.json`.
pub fn finalize(a: &mut Audit) -> Map<String, Value> {
    let findings: Vec<Value> = fs::read_to_string(a.out_dir.join("findings.jsonl"))
        .map(|t| t.lines().filter_map(|l| serde_json::from_str(l).ok()).collect())
        .unwrap_or_default();
    let ranked = corroborate(a, &findings);
    let overview: Map<String, Value> = fs::read_to_string(a.out_dir.join("report.json"))
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_default();
    let (kept, dropped): (Vec<Value>, Vec<Value>) = ranked.into_iter().partition(|f| f["confidence"] != "uncited");
    let by_sev = counts(&kept, "severity");
    a.summary.insert(
        "findings".into(),
        json!({"total_submitted": findings.len(), "reported": kept.len(), "dropped_uncited": dropped.len(),
               "by_severity": by_sev, "by_confidence": counts(&kept, "confidence")}),
    );
    a.summary.insert("status".into(), "AUDITED".into());
    let commit = a.repo.as_ref().map_or(Value::Null, |r| r.sha.clone().into());
    let _ = write_json(
        &a.run_dir.join("findings.json"),
        &json!({"repo": a.url, "commit": commit, "overview": overview, "findings": kept, "dropped_uncited": dropped}),
    );
    write_markdown(a, &overview, &kept, &dropped);
    let md = a.run_dir.join("report.md").to_string_lossy().into_owned();
    let fj = a.run_dir.join("findings.json").to_string_lossy().into_owned();
    a.rep.emit(
        StageResult::new(
            "6 report",
            "PASS",
            format!("{} finding(s) reported ({}); {} dropped as uncited", kept.len(), py_dict(&by_sev), dropped.len()),
        )
        .fact("report", md)
        .fact("json", fj)
        .fact("note", "findings are candidates for human review, not a verdict"),
    );
    let _ = write_json(&a.run_dir.join("report.json"), &Value::Object(a.summary.clone()));
    a.summary.clone()
}

fn s(v: &Value) -> String {
    crate::report::value_text(v)
}

fn write_markdown(a: &Audit, overview: &Map<String, Value>, kept: &[Value], dropped: &[Value]) {
    let Some(repo) = &a.repo else { return };
    let mut f = String::new();
    f.push_str(&format!("# Vulnerability audit: {}/{}\n\n", repo.owner, repo.name));
    f.push_str(&format!("- **Repository**: {}\n- **Commit**: `{}`\n", a.url, repo.sha));
    f.push_str(&format!("- **Language**: {}\n- **Run**: {}\n\n", repo.language, a.run_id));
    f.push_str(
        "> Findings are candidates for human review, produced by static analysis and an LLM agent. \
         They are **not** a verdict that the code is or is not vulnerable. Each is labelled by how far it was corroborated.\n\n",
    );
    if !overview.is_empty() {
        let o = |k: &str| overview.get(k).map(s).unwrap_or_default();
        f.push_str(&format!("## Summary\n\n{}\n\n", o("summary")));
        f.push_str(&format!("- **Overall risk (agent)**: {}\n", o("overall_risk")));
        f.push_str(&format!("- **Scope**: {}\n", o("scope")));
        f.push_str(&format!("- **Methodology**: {}\n\n", o("methodology")));
    }
    f.push_str(&format!("## Findings ({})\n\n", kept.len()));
    f.push_str("Confidence: **poc-demonstrated** > **tool-corroborated** > **cited** (location verified only).\n\n");
    for (i, x) in kept.iter().enumerate() {
        let loc = &x["location"];
        f.push_str(&format!("### {}. {} \n\n", i + 1, s(&x["title"])));
        let via = x["corroborating_tool"].as_str().map(|t| format!(" (via {t})")).unwrap_or_default();
        f.push_str(&format!("- **Severity**: {}  |  **Confidence**: {}{via}\n", s(&x["severity"]), s(&x["confidence"])));
        f.push_str(&format!("- **CWE**: {}\n", s(&x["cwe"])));
        let verified = if x["location_verified"] == true { "✓ verified" } else { "✗ not found" };
        f.push_str(&format!(
            "- **Location**: `{}:{}-{}` {verified}\n",
            s(&loc["file"]),
            s(&loc["start_line"]),
            s(&loc["end_line"])
        ));
        f.push_str(&format!("- **Description**: {}\n", s(&x["description"])));
        f.push_str(&format!("- **Data flow**: {}\n", s(&x["data_flow"])));
        f.push_str(&format!("- **Reachability / exploitability**: {}\n", s(&x["reachability"])));
        f.push_str(&format!("- **Impact**: {}\n", s(&x["impact"])));
        if x["evidence"].as_str().is_some_and(|e| !e.is_empty()) {
            f.push_str(&format!("- **Evidence**: {}\n", s(&x["evidence"])));
        }
        f.push_str(&format!("- **Recommendation**: {}\n\n", s(&x["recommendation"])));
    }
    if !dropped.is_empty() {
        f.push_str(&format!(
            "## Dropped ({})\n\nSubmitted but their cited location could not be verified, so they are not reported as findings:\n\n",
            dropped.len()
        ));
        for x in dropped {
            f.push_str(&format!("- {} (`{}`)\n", s(&x["title"]), s(&x["location"]["file"])));
        }
    }
    if let Ok(mut file) = fs::File::create(a.run_dir.join("report.md")) {
        let _ = file.write_all(f.as_bytes());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn audit(dir: &Path, baseline: Vec<Value>) -> Audit {
        let src = dir.join("src/source");
        fs::create_dir_all(src.join("pkg")).unwrap();
        fs::write(src.join("pkg/a.go"), "1\n2\n3\n4\n5\n6\n7\n8\n9\n10\n").unwrap();
        Audit {
            url: "https://github.com/o/r".into(),
            run_id: "o-r-1".into(),
            run_dir: dir.to_path_buf(),
            out_dir: dir.join("out"),
            rep: Reporter::new(dir, "o-r").unwrap(),
            summary: Map::new(),
            repo: Some(RepoCheckout {
                url: "https://github.com/o/r".into(),
                owner: "o".into(),
                name: "r".into(),
                sha: "abc".into(),
                git_ref: "default branch".into(),
                source_dir: src,
                go_version: "1.21".into(),
                language: "Go".into(),
            }),
            image: None,
            baseline,
            task_json: dir.join("task.json"),
        }
    }

    #[test]
    fn corroboration_tiers_and_ranking() {
        let dir = tempfile::tempdir().unwrap();
        let a = audit(dir.path(), vec![json!({"tool": "gosec", "file": "pkg/a.go", "line": "4"})]);
        let f = |title: &str, sev: &str, file: &str, line: i64, ev: &str| {
            json!({"title": title, "severity": sev, "location": {"file": file, "start_line": line, "end_line": line}, "evidence": ev})
        };
        let ranked = corroborate(
            &a,
            &[
                f("ghost", "critical", "nope.go", 1, ""),
                f("cited", "high", "pkg/a.go", 10, ""),
                f("tool", "high", "./pkg/a.go", 2, ""),
                f("poc", "low", "pkg/a.go", 1, "PoC: curl ..."),
            ],
        );
        let order: Vec<&str> = ranked.iter().map(|x| x["title"].as_str().unwrap()).collect();
        assert_eq!(order, vec!["tool", "cited", "poc", "ghost"]);
        assert_eq!(ranked[0]["confidence"], "tool-corroborated");
        assert_eq!(ranked[0]["corroborating_tool"], "gosec");
        assert_eq!(ranked[3]["confidence"], "uncited");
    }

    #[test]
    fn finalize_drops_uncited_and_writes_reports() {
        let dir = tempfile::tempdir().unwrap();
        let mut a = audit(dir.path(), vec![]);
        fs::create_dir_all(&a.out_dir).unwrap();
        fs::write(
            a.out_dir.join("findings.jsonl"),
            concat!(
                r#"{"title":"real","severity":"high","location":{"file":"pkg/a.go","start_line":3,"end_line":4}}"#, "\n",
                r#"{"title":"fake","severity":"high","location":{"file":"x.go","start_line":3,"end_line":4}}"#, "\n"
            ),
        )
        .unwrap();
        let summary = finalize(&mut a);
        assert_eq!(summary["findings"]["reported"], 1);
        assert_eq!(summary["findings"]["dropped_uncited"], 1);
        let md = fs::read_to_string(dir.path().join("report.md")).unwrap();
        assert!(md.contains("### 1. real") && md.contains("## Dropped (1)"));
        assert!(md.contains("`pkg/a.go:3-4` ✓ verified"));
    }
}
