//! `/patch`: one task from target to verdict.
//!
//! A CVE id (Go, Python, JS/TS, Rust, Java, Ruby, PHP; the hidden oracle is
//! the upstream fix's tests, see [`crate::oracle`]):
//!   0 resolve   CVE record + OSV record -> description, CWE, fix commits
//!   1 fetch     clone the repo; vulnerable = the fix's parent
//!   2 split     the upstream fix -> reference patch + hidden oracle tests
//!   3 sandbox   podman build of the task image (Go toolchain + cached deps)
//!   4 confirm   the upstream fix must pass the grader -> REPRODUCIBLE
//!
//! A GitHub repository or a local folder (any language): there is no known
//! vulnerability and no hidden oracle. The agent finds one, proves it with its
//! own reproduction test, and the grader uses that test as the oracle:
//!   0 resolve   clone at a pinned commit, or copy the folder (no .git)
//!   1 scope     language profile: image, dependency install, build and test
//!   2 sandbox   podman build: source + dependencies + analyzers
//!   3 baseline  static analyzers once -> leads for the assessment
//!
//! Then, for both:
//!   5 agent     the Loop session: assess -> reproduce -> patch -> verify -> report
//!   6 grade     a fresh container (see [`crate::grader`])
//!   7 final     report.md + report.json

use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde_json::{json, Map, Value};

use crate::analyzers;
use crate::grader;
use crate::patchlib;
use crate::profiles::{self, Profile};
use crate::report::{counts_text, to_json_pretty, write_json, Reporter, StageResult, Timer};
use crate::sandbox::{self, Container};
use crate::scoring;
use crate::sources::{self, CveInfo, StopStage, Target, TaskSource};
use crate::workspace;

/// Stage facts that would reveal the hidden oracle to an agent reading the reports.
pub const HIDDEN_FACTS: &[&str] = &["code files (reference patch)", "oracle test files", "oracle tests", "oracle command"];

/// What a task was created from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// A CVE id.
    Cve,
    /// A GitHub repository.
    Github,
    /// A local folder.
    Local,
}

/// A `/patch` task and its run directory.
pub struct Task {
    /// What it was created from.
    pub kind: Kind,
    /// The target as given.
    pub target: String,
    /// `<slug>-<UTC time>`.
    pub run_id: String,
    /// Visible outputs.
    pub run_dir: PathBuf,
    /// Hidden or bulky inputs: baseline, oracle, build log.
    pub task_dir: PathBuf,
    /// Stage reports.
    pub rep: Reporter,
    /// `report.json`.
    pub summary: Map<String, Value>,
    /// `owner/name`, the folder, or the CVE id.
    pub label: String,
    /// The pinned commit.
    pub commit: Option<String>,
    /// The original source.
    pub baseline_dir: Option<PathBuf>,
    /// Go toolchain version.
    pub go_version: String,
    /// The language profile (repository targets).
    pub profile: Option<Profile>,
    /// The built image.
    pub image: Option<String>,
    /// Public task facts for the agent.
    pub task_json: Option<PathBuf>,
    /// Static-analyzer leads.
    pub leads: Vec<Value>,
    /// The best grade so far.
    pub best: Option<Map<String, Value>>,
    /// Attempts graded.
    pub attempts: u32,
    /// CVE facts.
    pub cve: Option<CveInfo>,
    /// The CVE task source.
    pub src: Option<TaskSource>,
}

/// The runs directory: `$DEFENSIVE_RUNS`, else `~/.loop/defensive/runs`.
pub fn runs_root() -> PathBuf {
    std::env::var_os("DEFENSIVE_RUNS")
        .map(PathBuf::from)
        .unwrap_or_else(|| dirs::home_dir().unwrap_or_else(|| PathBuf::from(".")).join(".loop").join("defensive").join("runs"))
}

/// Hidden material for CVE runs: `~/.cache/defensive/hidden`.
pub fn hidden_root() -> PathBuf {
    sources::cache_dir().join("hidden")
}

fn slug(s: &str) -> String {
    let re = regex::Regex::new(r"[^a-z0-9_.-]+").unwrap();
    let s = re.replace_all(&s.to_lowercase(), "-").trim_matches(|c| c == '-' || c == '.').to_owned();
    let s: String = s.chars().take(60).collect();
    if s.is_empty() {
        "repo".into()
    } else {
        s
    }
}

fn utc_stamp() -> String {
    chrono::Utc::now().format("%Y%m%dT%H%M%SZ").to_string()
}

/// Point `<dir>/latest` at `run_id`.
pub fn link_latest(dir: &Path, run_id: &str) {
    let latest = dir.join("latest");
    if fs::symlink_metadata(&latest).is_ok() {
        let _ = fs::remove_file(&latest).or_else(|_| fs::remove_dir_all(&latest));
    }
    let _ = std::os::unix::fs::symlink(run_id, &latest);
}

impl Task {
    /// Ready for the agent: the sandbox exists and the bug reproduces (CVE).
    pub fn ready(&self) -> bool {
        match self.kind {
            Kind::Cve => self.lifecycle() == "REPRODUCIBLE" && self.image.is_some() && self.task_json.is_some(),
            _ => self.image.is_some() && self.task_json.is_some(),
        }
    }

    /// The language profile name (`go` for CVE tasks).
    pub fn lang(&self) -> String {
        self.profile.as_ref().map_or_else(|| "go".into(), |p| p.lang.clone())
    }

    /// The current lifecycle.
    pub fn lifecycle(&self) -> String {
        self.summary.get("lifecycle").and_then(Value::as_str).unwrap_or("").to_owned()
    }

    fn set_lifecycle(&mut self, l: &str) {
        self.summary.insert("lifecycle".into(), l.into());
    }

    fn stop(&mut self, stage: &str, e: &StopStage) {
        self.rep.emit(StageResult::new(stage, "STOP", e.reason.clone()).lifecycle(e.lifecycle.clone()));
        let l = e.lifecycle.clone();
        self.set_lifecycle(&l);
    }

    /// Stage output printed since the last call.
    pub fn take_output(&mut self) -> String {
        self.rep.take_output()
    }
}

/// Stages 0-3 (repository) or 0-4 (CVE) for `target`.
pub fn prepare(target: &Target, runs: &Path) -> Result<Task, String> {
    match target {
        Target::Cve(id) => prepare_cve(id, runs, &hidden_root()),
        Target::Github { url, git_ref } => prepare_repo(Kind::Github, url, git_ref.as_deref(), runs),
        Target::Local(p) => prepare_repo(Kind::Local, &p.to_string_lossy(), None, runs),
    }
}

fn new_task(kind: Kind, target: &str, label: &str, run_id: String, run_dir: PathBuf, task_dir: PathBuf, rep: Reporter) -> Task {
    Task {
        kind,
        target: target.into(),
        run_id,
        run_dir,
        task_dir,
        rep,
        summary: Map::new(),
        label: label.into(),
        commit: None,
        baseline_dir: None,
        go_version: "1.21".into(),
        profile: None,
        image: None,
        task_json: None,
        leads: vec![],
        best: None,
        attempts: 0,
        cve: None,
        src: None,
    }
}

/// Stages 0-3 for a GitHub URL or a local folder.
pub fn prepare_repo(kind: Kind, target: &str, git_ref: Option<&str>, runs: &Path) -> Result<Task, String> {
    let (label, slug) = match kind {
        Kind::Github => {
            let (owner, name) = sources::parse_github_url(target).map_err(|e| e.reason)?;
            (format!("{owner}/{name}"), slug(&format!("{owner}-{name}")))
        }
        _ => {
            let name = Path::new(target).file_name().and_then(|n| n.to_str()).unwrap_or("folder");
            let hash = &patchlib::sha256(target)[..8];
            (target.to_owned(), slug(&format!("local-{name}-{hash}")))
        }
    };
    let run_id = format!("{slug}-{}", utc_stamp());
    let base = runs.join("repo").join(&slug);
    let run_dir = base.join(&run_id);
    let task_dir = run_dir.join("task");
    fs::create_dir_all(&task_dir).map_err(|e| format!("could not create {}: {e}", task_dir.display()))?;
    link_latest(&base, &run_id);
    let mut rep = Reporter::new(&run_dir, &label).map_err(|e| e.to_string())?;
    rep.say(&format!("\n== defensive patch {run_id} ==  ({})\n", run_dir.display()));
    let mut p = new_task(kind, target, &label, run_id.clone(), run_dir.clone(), task_dir.clone(), rep);
    for (k, v) in [
        ("target", json!(target)),
        ("kind", json!(if kind == Kind::Github { "github" } else { "local" })),
        ("run_id", json!(run_id)),
        ("run_dir", json!(run_dir.to_string_lossy())),
    ] {
        p.summary.insert(k.into(), v);
    }
    p.summary.insert("lifecycle".into(), "DISCOVERED".into());

    // 0 resolve
    let baseline = task_dir.join("baseline");
    let t = Timer::start();
    let facts: Vec<(String, Value)> = match kind {
        Kind::Github => match sources::resolve_repo(target, &task_dir, git_ref) {
            Ok(co) => {
                if let Err(e) = fs::rename(&co.source_dir, &baseline) {
                    p.stop("0 resolve", &StopStage::new("UNSUPPORTED", e.to_string()));
                    return Ok(p);
                }
                p.commit = Some(co.sha.clone());
                vec![("url".into(), json!(target)), ("ref".into(), json!(co.git_ref)), ("commit".into(), json!(co.sha))]
            }
            Err(e) => {
                p.stop("0 resolve", &e);
                return Ok(p);
            }
        },
        _ => {
            if let Err(e) = sources::copy_local(Path::new(target), &baseline) {
                p.stop("0 resolve", &e);
                return Ok(p);
            }
            let (commit, dirty) = sources::local_commit(Path::new(target));
            p.commit = commit.clone();
            vec![
                ("folder".into(), json!(target)),
                ("git commit".into(), json!(commit.unwrap_or_else(|| "not a git checkout".into()))),
                ("uncommitted changes".into(), json!(if dirty { "included in the copy" } else { "none" })),
            ]
        }
    };
    p.baseline_dir = Some(baseline.clone());
    let nfiles = patchlib::list_files(&baseline).len();
    let verb = if kind == Kind::Github { "cloned" } else { "copied" };
    p.rep.emit(
        StageResult::new("0 resolve", "PASS", format!("{verb} {label} into the run directory (the original is never modified)"))
            .lifecycle("DISCOVERED")
            .secs(t.secs())
            .facts(facts)
            .fact("files", nfiles),
    );
    p.summary.insert("label".into(), label.clone().into());
    p.summary.insert("commit".into(), p.commit.clone().map_or(Value::Null, Value::from));

    // 1 scope
    let mut prof = profiles::detect(&baseline);
    let mut scope_facts: Vec<(String, Value)> = Vec::new();
    if prof.lang == "go" {
        let gomod = fs::read_to_string(baseline.join("go.mod")).unwrap_or_default();
        p.go_version = profiles::go_version(&gomod);
        prof.image = format!("docker.io/library/golang:{}-bookworm", p.go_version);
        prof.label = "Go".into();
        prof.build = "go build ./...".into();
        prof.test = "go test ./...".into();
        let module = regex::Regex::new(r"(?m)^module\s+(\S+)").unwrap().captures(&gomod).map(|c| c[1].to_owned());
        scope_facts.push(("module".into(), json!(module.unwrap_or_else(|| "?".into()))));
    } else {
        scope_facts.push(("dependency install".into(), json!(if prof.deps.is_empty() { "none" } else { &prof.deps })));
    }
    scope_facts.push(("image".into(), json!(prof.image)));
    scope_facts.push(("build command".into(), json!(if prof.build.is_empty() { "none" } else { &prof.build })));
    scope_facts.push((
        "test command".into(),
        json!(if prof.test.is_empty() { "none detected (agent's own test only)" } else { &prof.test }),
    ));
    p.rep.emit(
        StageResult::new("1 scope", "PASS", format!("{}; the agent assesses the whole project", prof.label))
            .lifecycle("CLASSIFIED")
            .facts(scope_facts),
    );
    p.set_lifecycle("CLASSIFIED");
    p.summary.insert("language".into(), prof.lang.clone().into());
    p.profile = Some(prof.clone());

    // 2 sandbox (Go: no hidden oracle; an empty one keeps the shared Containerfile)
    if let Err(e) = sandbox::podman_available() {
        p.rep.emit(StageResult::new("2 sandbox", "FAIL", e).lifecycle("FAILED"));
        p.set_lifecycle("FAILED");
        return Ok(p);
    }
    let t = Timer::start();
    let img = if prof.lang == "go" {
        let _ = fs::create_dir_all(task_dir.join("oracle").join("files"));
        sandbox::build_go_image(&format!("repo-{slug}"), &p.go_version, &task_dir, &["./...".into()], true)
    } else {
        sandbox::build_repo_image(&format!("repo-{slug}"), &prof, &task_dir)
    };
    if !img.ok {
        p.rep.emit(
            StageResult::new("2 sandbox", "FAIL", "podman build failed (see image_build.log)")
                .lifecycle("FAILED")
                .secs(t.secs())
                .fact("log tail", patchlib::tail(&img.log_tail, 600)),
        );
        p.set_lifecycle("FAILED");
        return Ok(p);
    }
    p.rep.emit(
        StageResult::new("2 sandbox", "PASS", format!("built {}", img.tag))
            .lifecycle("BUILDABLE")
            .secs(t.secs())
            .fact("image id", img.digest.clone().unwrap_or_default().chars().take(19).collect::<String>())
            .fact("runtime limits", sandbox::limits_text()),
    );
    p.set_lifecycle("BUILDABLE");
    p.image = Some(img.tag.clone());

    // 3 baseline: build check + analyzer leads, in a throwaway container
    let t = Timer::start();
    let (built, leads) = baseline_scan(&p);
    p.leads = leads;
    let findings = run_dir.join("baseline_findings.json");
    let _ = write_json(&findings, &Value::Array(p.leads.clone()));
    let by_tool = counts_text(p.leads.iter().map(|f| f["tool"].as_str().unwrap_or("?")));
    let build_text = match built {
        Some(true) => "ok",
        Some(false) => "FAILS on the original code (grading may be limited)",
        None => "no build step",
    };
    p.rep.emit(
        StageResult::new("3 baseline", "PASS", format!("{} static-analyzer lead(s); build: {build_text}", p.leads.len()))
            .secs(t.secs())
            .fact("by tool", if by_tool.is_empty() { "none".to_owned() } else { by_tool })
            .fact("saved", findings.to_string_lossy().into_owned()),
    );

    let task_json = run_dir.join("task.json");
    let repro = if prof.lang == "go" {
        "a new *_test.go file; grade_patch finds its Test functions".to_owned()
    } else {
        format!(
            "put it in a test file (e.g. under test/ or tests/, or named *.test.js, test_*.py, *_spec.rb, *Test.java) \
             and pass the command that runs ONLY it as repro_command to submit_report, e.g. `{}`; it must exit \
             non-zero before the fix and zero after",
            prof.example
        )
    };
    let _ = write_json(
        &task_json,
        &json!({"mode": "repo", "repository": label, "commit": p.commit,
                "description": "No known vulnerability is given. Assess this code base, find a real, reachable security vulnerability, prove it with a failing test, then fix it.",
                "language": prof.label, "module_root": "/work", "analyzer_leads": p.leads.len(),
                "build_command": prof.build, "test_command": prof.test, "reproduction_test": repro}),
    );
    p.task_json = Some(task_json);
    p.set_lifecycle("READY");
    Ok(p)
}

fn baseline_scan(p: &Task) -> (Option<bool>, Vec<Value>) {
    let (Some(image), Some(baseline), Some(prof)) = (&p.image, &p.baseline_dir, &p.profile) else {
        return (Some(false), vec![]);
    };
    let name = format!("defensive-base-{}", p.run_id.to_lowercase());
    let Ok(c) = Container::start(&name, image, &[(baseline.as_path(), "/work", "ro")], &[]) else {
        return (Some(false), vec![]);
    };
    // The build runs in the image's copy, where the dependencies are installed.
    let built = (!prof.build.is_empty()).then(|| {
        c.sh(&format!("cd /opt/defensive/baseline && {}", prof.build), None, &[], Duration::from_secs(900)).ok()
    });
    let res = analyzers::tool_result("all", &c, &analyzers::Paths::patch(), baseline);
    let leads = res["findings"].as_array().cloned().unwrap_or_default();
    (built, leads)
}

/// A fresh copy of the baseline for attempt `n`.
pub fn new_workspace(p: &Task, n: u32) -> Result<PathBuf, String> {
    let ws = p.run_dir.join(format!("attempt-{n}")).join("workspace");
    let base = match p.kind {
        Kind::Cve => p.src.as_ref().map(|s| s.baseline_dir.clone()),
        _ => p.baseline_dir.clone(),
    }
    .ok_or("the task has no baseline")?;
    sandbox::copy_tree(&base, &ws, &|_| false).map_err(|e| e.to_string())?;
    Ok(ws)
}

/// The original source of the task.
pub fn baseline_of(p: &Task) -> PathBuf {
    match p.kind {
        Kind::Cve => p.src.as_ref().map(|s| s.baseline_dir.clone()).unwrap_or_default(),
        _ => p.baseline_dir.clone().unwrap_or_default(),
    }
}

/// Copy `ws/rel` to `dst` when it is a regular file that resolves inside `ws`.
fn safe_copy(ws: &Path, rel: &str, dst: &Path) -> bool {
    let src = ws.join(rel);
    let (Ok(real), Ok(root)) = (fs::canonicalize(&src), fs::canonicalize(ws)) else {
        return false;
    };
    let is_file = fs::symlink_metadata(&src).map(|m| m.file_type().is_file()).unwrap_or(false);
    if !is_file || !real.starts_with(&root) {
        return false;
    }
    if let Some(parent) = dst.parent() {
        let _ = fs::create_dir_all(parent);
    }
    fs::copy(&real, dst).is_ok()
}

/// The agent's Go test files as an oracle directory; returns its test names.
fn agent_go_oracle(ws: &Path, diff: &patchlib::TreeDiff, dst: &Path) -> Vec<String> {
    let funcs = patchlib::changed_test_funcs(&diff.test_patch);
    let _ = fs::remove_dir_all(dst);
    let mut pkgs: Vec<String> = Vec::new();
    for rel in &diff.test_files {
        if !safe_copy(ws, rel, &dst.join("files").join(rel)) {
            continue; // deleted test files are not part of the oracle
        }
        if rel.ends_with("_test.go") {
            let pkg = rel.rsplit_once('/').map_or_else(|| ".".to_owned(), |(d, _)| format!("./{d}"));
            if !pkgs.contains(&pkg) {
                pkgs.push(pkg);
            }
        }
    }
    let _ = fs::create_dir_all(dst.join("files"));
    let _ = write_json(
        &dst.join("oracle.json"),
        &json!({"run": format!("^({})$", funcs.join("|")), "tests": funcs, "packages": pkgs}),
    );
    if pkgs.is_empty() {
        vec![]
    } else {
        funcs
    }
}

/// The oracle for the any-language grader: the agent's test files and its
/// repro command. Returns the repro command (`""` when none was submitted).
fn command_oracle(p: &Task, ws: &Path, diff: &patchlib::TreeDiff, report: Option<&Value>, dst: &Path) -> String {
    let _ = fs::remove_dir_all(dst);
    let _ = fs::create_dir_all(dst.join("files"));
    for rel in &diff.test_files {
        safe_copy(ws, rel, &dst.join("files").join(rel));
    }
    let field = |k: &str| report.and_then(|r| r.get(k)).and_then(Value::as_str).unwrap_or("").trim().to_owned();
    let repro = field("repro_command");
    let prof = p.profile.clone().unwrap_or_default();
    let test = Some(field("test_command")).filter(|t| !t.is_empty()).unwrap_or(prof.test.clone());
    let _ = write_json(
        &dst.join("oracle.json"),
        &json!({"lang": p.lang(), "repro_command": repro, "build_command": prof.build, "test_command": test}),
    );
    repro
}

/// `{step: "status: detail"}` for a stage report.
pub fn step_facts(g: &Value) -> Vec<(String, Value)> {
    g["steps"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|s| {
            (
                s["step"].as_str().unwrap_or("").to_owned(),
                json!(format!("{}: {}", s["status"].as_str().unwrap_or(""), s["detail"].as_str().unwrap_or(""))),
            )
        })
        .collect()
}

/// Stage 6 for attempt `n`. Returns the grade, with `feedback` when not verified.
pub fn grade_attempt(p: &mut Task, n: u32, ws: &Path, report: Option<&Value>) -> Value {
    match p.kind {
        Kind::Cve => grade_cve_attempt(p, n, ws, report),
        _ => grade_repo_attempt(p, n, ws, report),
    }
}

fn grade_repo_attempt(p: &mut Task, n: u32, ws: &Path, report: Option<&Value>) -> Value {
    let adir = ws.parent().unwrap_or(ws).to_path_buf();
    let lang = p.lang();
    let baseline = baseline_of(p);
    let diff = patchlib::diff_trees(&baseline, ws, &lang);
    let _ = fs::write(adir.join("candidate.patch"), &diff.code_patch);
    let _ = fs::write(adir.join("agent_tests.patch"), &diff.test_patch);
    let rc = scoring::check_report(report, &baseline, &diff.code_files);
    let rc_detail = rc["detail"].as_str().unwrap_or("").replace("matches upstream fix files", "matches the patched files");
    let t = Timer::start();
    let oracle = adir.join("oracle");
    let (funcs, missing) = if lang == "go" {
        let funcs = agent_go_oracle(ws, &diff, &oracle);
        let missing = if funcs.is_empty() { "no reproduction test: add a Test function in a *_test.go file" } else { "" };
        (funcs, missing)
    } else {
        let repro = command_oracle(p, ws, &diff, report, &oracle);
        let missing = if repro.is_empty() {
            "no repro_command: call submit_report with repro_command, the command that runs only your reproduction test"
        } else {
            ""
        };
        (if repro.is_empty() { vec![] } else { vec![repro] }, missing)
    };
    let image = p.image.clone().unwrap_or_default();
    let g = if !missing.is_empty() {
        json!({"final_status": "INCONCLUSIVE", "dimensions": {}, "steps": [{"step": "B reproduce", "status": "fail", "detail": missing}]})
    } else if lang == "go" {
        grader::grade_go(&image, &diff.code_patch, &oracle, "")
    } else {
        grader::grade_repo(&image, &diff.code_patch, &oracle)
    };
    let status = g["final_status"].as_str().unwrap_or("FAILED").to_owned();
    let verified = status == "PATCH_VERIFIED";
    let mut grade = g.clone();
    grade["report_check"] = rc_detail.clone().into();
    grade["changed_code_files"] = json!(diff.code_files);
    grade["reproduction_tests"] = json!(funcs);
    grade["oracle"] = "agent-written reproduction test".into();
    grade["patch_sha256"] = if diff.code_patch.is_empty() { Value::Null } else { patchlib::sha256(&diff.code_patch).into() };
    let _ = fs::write(adir.join("grade.json"), to_json_pretty(&grade));
    let mut r = StageResult::new(
        format!("6 grade (attempt {n})"),
        if verified { "PASS" } else { "FAIL" },
        format!("verdict {status} (oracle: the agent's own reproduction test)"),
    )
    .secs(t.secs())
    .facts(step_facts(&g))
    .fact("reproduction tests", if funcs.is_empty() { "none".to_owned() } else { funcs.join(", ") })
    .fact("agent test files", if diff.test_files.is_empty() { "none".to_owned() } else { diff.test_files.join(", ") })
    .fact("report check", rc_detail)
    .fact("note", "the test is agent-written; review the finding and the test before trusting the verdict");
    if verified {
        r = r.lifecycle("PATCH_VERIFIED");
    }
    p.rep.emit(r);
    p.attempts = p.attempts.max(n);
    let rank = |s: &str| match s {
        "PATCH_VERIFIED" => 3,
        "REPRODUCIBLE" => 2,
        "INCONCLUSIVE" => 1,
        _ => 0,
    };
    let best_rank = p.best.as_ref().map_or(-1, |b| rank(b["final_status"].as_str().unwrap_or("")));
    if rank(&status) > best_rank {
        let mut b = Map::new();
        b.insert("attempt".into(), n.into());
        b.insert("final_status".into(), status.clone().into());
        p.best = Some(b);
    }
    if verified {
        p.set_lifecycle("PATCH_VERIFIED");
    }
    grade["feedback"] = if verified { "".into() } else { repo_feedback(&g, report.is_some()).into() };
    grade
}

/// What failed, for the agent's next try (repository targets).
pub fn repo_feedback(g: &Value, had_report: bool) -> String {
    let names = |step: &str| match step {
        "A validate" => Some("the patch was empty or touched files it may not (only non-test source files; no test files or dependency manifests/lockfiles)"),
        "B reproduce" => Some("your reproduction test does not fail on the original code, so it does not demonstrate the vulnerability"),
        "C build" => Some("the patched code did not build"),
        "D regression" => Some("your reproduction test still fails on the patched code"),
        "E existing tests" => Some("existing tests broke"),
        _ => None,
    };
    let failed: Vec<String> = g["steps"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|s| s["status"] == "fail")
        .filter_map(|s| {
            let text = names(s["step"].as_str()?)?;
            let mut m = format!("{text} ({})", s["detail"].as_str().unwrap_or(""));
            if let Some(log) = s["log_tail"].as_str().filter(|l| !l.is_empty()) {
                m.push_str(&format!("; output: {}", patchlib::tail(log, 400)));
            }
            Some(m)
        })
        .collect();
    let mut msg = if failed.is_empty() { g["final_status"].as_str().unwrap_or("").to_owned() } else { failed.join("; ") };
    if !had_report {
        msg.push_str("; no report was submitted");
    }
    msg + "."
}

/// Stages 0-4 for a CVE. Hidden material goes to `hidden/<run_id>`.
pub fn prepare_cve(cve: &str, runs: &Path, hidden: &Path) -> Result<Task, String> {
    let cve = cve.trim().to_uppercase();
    let run_id = format!("{cve}-{}", utc_stamp());
    let base = runs.join(&cve);
    let run_dir = base.join(&run_id);
    fs::create_dir_all(&run_dir).map_err(|e| e.to_string())?;
    let task_dir = hidden.join(&run_id);
    fs::create_dir_all(&task_dir).map_err(|e| e.to_string())?;
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = fs::set_permissions(&task_dir, fs::Permissions::from_mode(0o700));
    }
    link_latest(&base, &run_id);
    let mut rep = Reporter::new(&run_dir, &cve).map_err(|e| e.to_string())?.hide(HIDDEN_FACTS, &task_dir);
    rep.say(&format!("\n== defensive run {run_id} ==  ({})\n", run_dir.display()));
    let mut p = new_task(Kind::Cve, &cve, &cve, run_id.clone(), run_dir.clone(), task_dir.clone(), rep);
    p.summary.insert("cve".into(), cve.clone().into());
    p.summary.insert("run_id".into(), run_id.into());
    p.summary.insert("run_dir".into(), run_dir.to_string_lossy().into_owned().into());
    p.summary.insert("lifecycle".into(), "DISCOVERED".into());

    // 0 resolve
    let t = Timer::start();
    let info = match sources::resolve_cve(&cve) {
        Ok(i) => i,
        Err(e) => {
            p.stop("0 resolve", &e);
            return Ok(p);
        }
    };
    let commits = info
        .commits
        .iter()
        .map(|(o, r, s)| format!("{o}/{r}@{}", &s[..s.len().min(10)]))
        .collect::<Vec<_>>()
        .join(", ");
    let summary: String = info.description.chars().take(300).collect();
    p.rep.emit(
        StageResult::new("0 resolve", "PASS", if summary.is_empty() { "(no description)".into() } else { summary })
            .lifecycle("DISCOVERED")
            .secs(t.secs())
            .fact("sources", info.sources.join(", "))
            .fact("CWE", if info.cwes.is_empty() { "none".into() } else { info.cwes.join(", ") })
            .fact("published", info.published.clone().map_or(Value::Null, Value::from))
            .fact("fix commits", if commits.is_empty() { "none".into() } else { commits })
            .data("description", info.description.clone())
            .data("commits", json!(info.commits.iter().map(|(o, r, s)| json!([o, r, s])).collect::<Vec<_>>())),
    );
    p.cve = Some(info.clone());

    // 1-4, once per upstream fix commit until one confirms (a CVE fixed on
    // several branches lists one commit per branch).
    let mut tried: Vec<String> = Vec::new();
    loop {
        // 1 fetch + 2 split
        let t = Timer::start();
        let src = match sources::fetch_and_split_except(&info, &task_dir, &tried) {
            Ok(s) => s,
            Err(e) => {
                if tried.is_empty() {
                    p.stop("1 fetch+split", &e);
                }
                return Ok(p);
            }
        };
        tried.push(src.fix_commit.clone());
        p.rep.emit(
            StageResult::new("1 fetch", "PASS", format!("cloned {}; vulnerable commit is the fix's parent", src.repo))
                .lifecycle("CLASSIFIED")
                .secs(t.secs())
                .fact("fix commit", src.fix_commit.clone())
                .fact("vulnerable commit", src.vulnerable_commit.clone())
                .fact("baseline files", patchlib::list_files(&src.baseline_dir).len()),
        );
        let go = src.profile.lang == "go";
        let ecosystem = if go {
            format!("Go (image golang:{})", src.go_version)
        } else {
            format!("{} (image {})", src.profile.label, src.profile.image)
        };
        let mut split = StageResult::new(
            "2 split",
            "PASS",
            format!("{} task; upstream fix split into a reference patch and hidden oracle tests", if go { "Go module" } else { src.profile.label.as_str() }),
        )
        .lifecycle("CLASSIFIED")
        .fact("ecosystem", ecosystem)
        .fact("code files (reference patch)", src.code_files.join(", "))
        .fact("oracle test files", src.test_files.join(", "));
        split = if go {
            split.fact("oracle tests", src.oracle_tests.join(", ")).fact("oracle packages", src.oracle_packages.join(", "))
        } else {
            split.fact("oracle command", src.oracle_command.clone())
        };
        p.rep.emit(split);
        if !go {
            p.profile = Some(src.profile.clone());
        }
        p.summary.insert("repo".into(), src.repo.clone().into());
        p.summary.insert("fix_commit".into(), src.fix_commit.clone().into());
        p.set_lifecycle("CLASSIFIED");
        p.go_version = src.go_version.clone();
        p.src = Some(src.clone());

        // 3 sandbox
        if let Err(e) = sandbox::podman_available() {
            p.rep.emit(StageResult::new("3 sandbox", "FAIL", e).lifecycle("FAILED"));
            p.set_lifecycle("FAILED");
            return Ok(p);
        }
        let t = Timer::start();
        let mut pkgs: Vec<String> = src.oracle_packages.clone();
        for f in &src.code_files {
            let pkg = f.rsplit_once('/').map_or_else(|| ".".to_owned(), |(d, _)| format!("./{d}"));
            if !pkgs.contains(&pkg) {
                pkgs.push(pkg);
            }
        }
        pkgs.sort();
        let img = if go {
            sandbox::build_go_image(&cve, &src.go_version, &task_dir, &pkgs, false)
        } else {
            sandbox::build_repo_image(&cve, &src.profile, &task_dir)
        };
        if !img.ok {
            p.rep.emit(
                StageResult::new("3 sandbox", "FAIL", "podman build failed")
                    .lifecycle("FAILED")
                    .secs(t.secs())
                    .fact("log tail", patchlib::tail(&img.log_tail, 600)),
            );
            p.set_lifecycle("FAILED");
            return Ok(p);
        }
        p.rep.emit(
            StageResult::new("3 sandbox", "PASS", format!("built {}", img.tag))
                .lifecycle("BUILDABLE")
                .secs(t.secs())
                .fact("image id", img.digest.clone().unwrap_or_default().chars().take(19).collect::<String>())
                .fact("runtime limits", sandbox::limits_text()),
        );
        p.set_lifecycle("BUILDABLE");

        // 4 confirm: the upstream fix must fail-then-pass the oracle
        let t = Timer::start();
        let conf = if go {
            grader::grade_go(&img.tag, &src.reference_patch, &task_dir.join("oracle"), "")
        } else {
            grader::grade_repo(&img.tag, &src.reference_patch, &task_dir.join("oracle"))
        };
        let _ = fs::write(task_dir.join("confirm_grade.json"), to_json_pretty(&conf));
        let conf = redact(&conf, &src.oracle_command);
        let step = |name: &str| {
            conf["steps"].as_array().into_iter().flatten().find(|s| s["step"] == name).map(|s| s["status"].as_str().unwrap_or("").to_owned())
        };
        // The upstream fix itself breaks the existing suite: the hidden tests are
        // still a fair oracle, but that suite cannot tell regressions apart.
        let suite_unusable = !go
            && conf["final_status"] == "REPRODUCIBLE"
            && step("B reproduce").as_deref() == Some("pass")
            && step("D regression").as_deref() == Some("pass")
            && step("E existing tests").as_deref() == Some("fail");
        if suite_unusable {
            let oracle_json = task_dir.join("oracle").join("oracle.json");
            if let Some(mut o) = workspace::load_json(&oracle_json) {
                o["test_command"] = "".into();
                o["existing_tests_note"] = "the upstream fix already fails the existing test suite".into();
                let _ = write_json(&oracle_json, &o);
            }
        }
        let ok = conf["final_status"] == "PATCH_VERIFIED" || suite_unusable;
        let summary = if suite_unusable {
            "upstream fix verified against the hidden tests (they fail on the vulnerable code and pass with the fix); \
             the upstream fix already fails the existing test suite, so regressions are not graded"
                .to_owned()
        } else if ok {
            "upstream fix verified: the oracle fails on the vulnerable code and passes with the fix".to_owned()
        } else {
            format!(
                "upstream fix not verified ({}); this CVE cannot be graded fairly",
                conf["final_status"].as_str().unwrap_or("?")
            )
        };
        p.rep.emit(
            StageResult::new("4 confirm", if ok { "PASS" } else { "STOP" }, summary)
                .lifecycle(if ok { "REPRODUCIBLE" } else { "INCONCLUSIVE" })
                .secs(t.secs())
                .facts(step_facts(&conf)),
        );
        p.set_lifecycle(if ok { "REPRODUCIBLE" } else { "INCONCLUSIVE" });
        if !ok {
            let more = info.commits.iter().any(|(_, _, sha)| !tried.iter().any(|t| t.starts_with(sha.as_str())));
            if more {
                p.rep.say("\nTrying the next upstream fix commit for this CVE.\n");
                continue;
            }
            return Ok(p);
        }
        p.image = Some(img.tag);
        let task_json = run_dir.join("task.json");
        let _ = write_json(
            &task_json,
            &json!({"cve": cve, "repository": src.repo, "description": info.description,
                    "cwe": info.cwes, "language": if go { "Go".to_owned() } else { src.profile.label.clone() },
                    "module_root": "/work", "build_command": src.profile.build, "test_command": src.profile.test,
                    "reproduction_test": "write your own reproduction test to prove the bug; the grader also runs hidden security tests"}),
        );
        p.task_json = Some(task_json);
        return Ok(p);
    }
}

fn grade_cve_attempt(p: &mut Task, n: u32, ws: &Path, report: Option<&Value>) -> Value {
    let adir = ws.parent().unwrap_or(ws).to_path_buf();
    let Some(src) = p.src.clone() else {
        return scoring::infrastructure_failure("no task source");
    };
    let lang = src.profile.lang.clone();
    let diff = patchlib::diff_trees(&src.baseline_dir, ws, &lang);
    let _ = fs::write(adir.join("candidate.patch"), &diff.code_patch);
    let _ = fs::write(adir.join("agent_tests.patch"), &diff.test_patch);
    let t = Timer::start();
    let image = p.image.clone().unwrap_or_default();
    let g = if lang == "go" {
        grader::grade_go(&image, &diff.code_patch, &p.task_dir.join("oracle"), &diff.test_patch)
    } else {
        grader::grade_repo(&image, &diff.code_patch, &p.task_dir.join("oracle"))
    };
    let rc = scoring::check_report(report, &src.baseline_dir, &src.code_files);
    let mut dims = Map::new();
    for d in scoring::DIMENSIONS {
        dims.insert((*d).into(), g["dimensions"].get(*d).cloned().unwrap_or(Value::Null));
    }
    dims.insert("detection_correct".into(), rc["detection_correct"].clone());
    dims.insert("root_cause_supported".into(), rc["root_cause_supported"].clone());
    // The fix commit comes from the official references; the patch is recorded by hash.
    let provenance = !diff.code_patch.is_empty()
        && p.cve.as_ref().is_some_and(|i| i.commits.iter().any(|(_, _, sha)| src.fix_commit.starts_with(sha.as_str())));
    dims.insert("patch_provenance_verified".into(), provenance.into());
    let status = g["final_status"].as_str().unwrap_or("FAILED").to_owned();
    let sc = scoring::score(&dims, &status);
    let shown = redact(&g, &src.oracle_command);
    let mut grade = g.clone();
    grade["dimensions"] = Value::Object(dims.clone());
    grade["score"] = sc.into();
    grade["max_score"] = scoring::MAX_SCORE.into();
    grade["report_check"] = rc["detail"].clone();
    grade["changed_code_files"] = json!(diff.code_files);
    grade["patch_sha256"] = if diff.code_patch.is_empty() { Value::Null } else { patchlib::sha256(&diff.code_patch).into() };
    let _ = fs::write(p.task_dir.join(format!("grade-attempt-{n}.json")), to_json_pretty(&grade));
    // The visible copy leaves out the oracle's test names, command and logs.
    let mut visible = redact(&grade, &src.oracle_command);
    if let Some(o) = visible.as_object_mut() {
        o.remove("oracle_tests");
    }
    if let Some(steps) = visible["steps"].as_array_mut() {
        for s in steps {
            if let Some(o) = s.as_object_mut() {
                o.remove("failed");
                o.remove("log_tail");
            }
        }
    }
    let _ = fs::write(adir.join("grade.json"), to_json_pretty(&visible));
    let verified = status == "PATCH_VERIFIED";
    let ar = &g["agent_repro"];
    let own = if ar["tests"].as_array().is_some_and(|t| !t.is_empty()) {
        format!("before fix: {}, after fix: {}", crate::report::value_text(&ar["before_fix"]), crate::report::value_text(&ar["after_fix"]))
    } else {
        ar["detail"].as_str().unwrap_or(if lang == "go" { "none" } else { "not run outside Go" }).to_owned()
    };
    let checks = dims.iter().map(|(k, v)| format!("{k}={}", scoring::mark(v))).collect::<Vec<_>>().join(" ");
    let mut r = StageResult::new(
        format!("6 grade (attempt {n})"),
        if verified { "PASS" } else { "FAIL" },
        format!("verdict {status}, score {}/{}", fmt_score(sc), scoring::MAX_SCORE),
    )
    .secs(t.secs())
    .facts(step_facts(&shown))
    .fact("report check", rc["detail"].clone())
    .fact("agent's own repro test", own)
    .fact("checks", checks);
    if verified {
        r = r.lifecycle("PATCH_VERIFIED");
    }
    p.rep.emit(r);
    p.attempts = p.attempts.max(n);
    let best_score = p.best.as_ref().and_then(|b| b["score"].as_f64()).unwrap_or(f64::NEG_INFINITY);
    if sc > best_score {
        let mut b = Map::new();
        b.insert("attempt".into(), n.into());
        b.insert("final_status".into(), status.clone().into());
        b.insert("score".into(), sc.into());
        p.best = Some(b);
    }
    if verified {
        p.set_lifecycle("PATCH_VERIFIED");
    }
    grade["feedback"] = if verified { "".into() } else { cve_feedback(&g, report.is_some()).into() };
    grade
}

/// A grade with the hidden oracle command replaced in every step detail, for
/// the outputs the agent can read.
pub fn redact(g: &Value, oracle_command: &str) -> Value {
    let mut g = g.clone();
    if oracle_command.is_empty() {
        return g;
    }
    if let Some(steps) = g["steps"].as_array_mut() {
        for s in steps {
            if let Some(d) = s["detail"].as_str() {
                s["detail"] = d.replace(oracle_command, "hidden security tests").into();
            }
            if let Some(o) = s.as_object_mut() {
                o.remove("log_tail");
                o.remove("failed");
            }
        }
    }
    g
}

/// What failed, without revealing hidden tests or the upstream fix.
pub fn cve_feedback(g: &Value, had_report: bool) -> String {
    let names = |step: &str| match step {
        "A validate" => Some("the patch was empty or touched files it may not"),
        "C build" => Some("the patched code did not build"),
        "D regression" => Some("the vulnerability is still present (hidden security tests fail)"),
        "E existing tests" => Some("existing tests broke"),
        _ => None,
    };
    let failed: Vec<&str> = g["steps"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|s| s["status"] == "fail")
        .filter_map(|s| names(s["step"].as_str()?))
        .collect();
    let mut msg = if failed.is_empty() { g["final_status"].as_str().unwrap_or("").to_owned() } else { failed.join("; ") };
    if !had_report {
        msg.push_str("; no report was submitted");
    }
    msg + ". Investigate again and submit a corrected fix."
}

fn fmt_score(s: f64) -> String {
    if s.fract() == 0.0 {
        format!("{s:.0}")
    } else {
        format!("{s}")
    }
}

/// Python's `repr` of a small JSON map, for `report.md` parity.
fn py_repr(v: &Value) -> String {
    match v {
        Value::String(s) => format!("'{s}'"),
        Value::Bool(b) => if *b { "True" } else { "False" }.into(),
        Value::Null => "None".into(),
        Value::Number(n) => n.as_f64().filter(|f| f.fract() == 0.0 && n.is_f64()).map_or_else(|| n.to_string(), |f| format!("{f:.1}")),
        Value::Array(a) => format!("[{}]", a.iter().map(py_repr).collect::<Vec<_>>().join(", ")),
        Value::Object(o) => format!("{{{}}}", o.iter().map(|(k, v)| format!("'{k}': {}", py_repr(v))).collect::<Vec<_>>().join(", ")),
    }
}

/// Stage 7: `report.json`, the final table in `report.md`, and the stage box.
pub fn finalize(p: &mut Task) -> Map<String, Value> {
    let t = Timer::start();
    if let Some(best) = &p.best {
        p.summary.insert("best".into(), Value::Object(best.clone()));
    }
    let rows: Vec<(String, String, String)> = p
        .rep
        .results
        .iter()
        .map(|r| (r.stage.clone(), r.status.clone(), r.summary.chars().take(90).collect()))
        .collect();
    p.summary.insert(
        "stages".into(),
        json!(rows.iter().map(|(s, st, sm)| json!({"stage": s, "status": st, "summary": sm})).collect::<Vec<_>>()),
    );
    let _ = write_json(&p.run_dir.join("report.json"), &Value::Object(p.summary.clone()));
    let mut md = format!("## Final\n\n- **lifecycle**: {}\n", p.lifecycle());
    if let Some(best) = p.summary.get("best") {
        md.push_str(&format!("- **best attempt**: {}\n", py_repr(best)));
    }
    md.push_str("\n| Stage | Status | Summary |\n|---|---|---|\n");
    for (s, st, sm) in &rows {
        md.push_str(&format!("| {s} | {st} | {sm} |\n"));
    }
    p.rep.append_md(&md);
    let mut summary = format!("lifecycle {}", p.lifecycle());
    if let Some(best) = p.summary.get("best") {
        summary.push_str(&format!("; best: {}", best["final_status"].as_str().unwrap_or("")));
        if let Some(sc) = best.get("score").and_then(Value::as_f64) {
            summary.push_str(&format!(" {}/{}", fmt_score(sc), scoring::MAX_SCORE));
        }
    }
    let md_path = p.rep.md.to_string_lossy().into_owned();
    let json_path = p.run_dir.join("report.json").to_string_lossy().into_owned();
    p.rep.emit(StageResult::new("7 final", "PASS", summary).secs(t.secs()).fact("report", md_path).fact("json", json_path));
    p.summary.clone()
}

/// Stage 5 for an interactive session: what the agent did so far.
pub fn emit_agent_stage(p: &mut Task, grades: u32, ws: &Path, calls: &[Value], report: Option<&Value>) {
    let d = patchlib::diff_trees(&baseline_of(p), ws, &p.lang());
    let tools = counts_text(calls.iter().map(|c| c["tool"].as_str().unwrap_or("?")));
    let root_cause: String = report.and_then(|r| r["root_cause"].as_str()).unwrap_or("").chars().take(400).collect();
    let has_code = !d.code_patch.is_empty();
    p.rep.emit(
        StageResult::new(
            format!("5 agent (before grade {grades})"),
            if has_code { "PASS" } else { "FAIL" },
            format!("loop session: {}", if has_code { "changed code is ready for grading" } else { "no code changes" }),
        )
        .fact("tool calls", if tools.is_empty() { "none".to_owned() } else { tools })
        .fact("changed code files", if d.code_files.is_empty() { "none".to_owned() } else { d.code_files.join(", ") })
        .fact("agent test files", if d.test_files.is_empty() { "none".to_owned() } else { d.test_files.join(", ") })
        .fact("report submitted", report.is_some())
        .fact("root cause (agent)", root_cause),
    );
}

/// The submitted report of attempt `adir`.
pub fn attempt_report(adir: &Path) -> Option<Value> {
    workspace::load_json(&adir.join("agent_report.json"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slugs() {
        assert_eq!(slug("soketlabs-tensorstudio-platform-backend"), "soketlabs-tensorstudio-platform-backend");
        assert_eq!(slug("Local My Repo!"), "local-my-repo");
        assert_eq!(slug("..."), "repo");
    }

    #[test]
    fn python_repr_of_best() {
        assert_eq!(py_repr(&json!({"attempt": 1, "final_status": "PATCH_VERIFIED"})), "{'attempt': 1, 'final_status': 'PATCH_VERIFIED'}");
        assert_eq!(py_repr(&json!({"score": 12.0})), "{'score': 12.0}");
    }

    #[test]
    fn feedback_names_failed_steps_only() {
        let g = json!({"final_status": "REPRODUCIBLE", "steps": [
            {"step": "B reproduce", "status": "pass", "detail": "x"},
            {"step": "D regression", "status": "fail", "detail": "`t` on the patched code: fail", "log_tail": "boom"}]});
        assert_eq!(
            repo_feedback(&g, true),
            "your reproduction test still fails on the patched code (`t` on the patched code: fail); output: boom."
        );
        assert_eq!(cve_feedback(&g, false), "the vulnerability is still present (hidden security tests fail); no report was submitted. Investigate again and submit a corrected fix.");
    }

    #[test]
    fn finalize_writes_reports() {
        let dir = tempfile::tempdir().unwrap();
        let rep = Reporter::new(dir.path(), "x").unwrap();
        let mut t = new_task(Kind::Local, "/x", "/x", "r".into(), dir.path().to_path_buf(), dir.path().join("task"), rep);
        t.rep.emit(StageResult::new("0 resolve", "PASS", "copied"));
        let mut b = Map::new();
        b.insert("attempt".into(), 1.into());
        b.insert("final_status".into(), "PATCH_VERIFIED".into());
        t.best = Some(b);
        t.summary.insert("lifecycle".into(), "PATCH_VERIFIED".into());
        finalize(&mut t);
        let md = fs::read_to_string(dir.path().join("report.md")).unwrap();
        assert!(md.contains("- **best attempt**: {'attempt': 1, 'final_status': 'PATCH_VERIFIED'}"));
        assert!(md.contains("| 0 resolve | PASS | copied |"));
        assert!(md.contains("## Stage 7 final: PASS\n\nlifecycle PATCH_VERIFIED; best: PATCH_VERIFIED"));
        let rj: Value = serde_json::from_str(&fs::read_to_string(dir.path().join("report.json")).unwrap()).unwrap();
        assert_eq!(rj["best"]["attempt"], 1);
    }
}
