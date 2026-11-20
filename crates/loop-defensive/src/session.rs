//! One `/patch` or `/audit` run inside a Loop session: the state behind the
//! run's tools.
//!
//!   prepare_task(target)  stages 0-3/4, then a sandbox container for the agent
//!   (sandbox tools)       in that container, on `/work` only
//!   grade_patch()         stage 6 in a fresh container; stage 7 once verified
//!                         or out of grading attempts
//!   finish_task()         stage 7 and stop the sandbox (when giving up)
//!
//!   prepare_audit(url)    stages 0-3, then a read-only sandbox
//!   finish_audit()        stages 4-6
//!
//! Hidden oracle material for CVEs lives in `~/.cache/defensive/hidden` and
//! never appears in a tool result.

use std::path::{Path, PathBuf};
use std::time::Duration;

use parking_lot::Mutex;
use serde_json::{json, Value};

use crate::analyzers;
use crate::audit::{self, Audit};
use crate::report::{counts_text, StageResult};
use crate::sandbox::Container;
use crate::sources::{self, Target};
use crate::task::{self, Kind, Task};
use crate::workspace::{Mode, Sandbox};

/// Dependency dirs installed in the image, put into the workspace so the
/// agent's commands find them: node_modules linked, composer's vendor/ copied
/// (its autoloader follows the real path). Neither enters a diff.
const LINK_DEPS: &str = "[ -d /opt/defensive/baseline/node_modules ] && [ ! -e /work/node_modules ] && \
                         ln -s /opt/defensive/baseline/node_modules /work/node_modules; \
                         [ -d /opt/defensive/baseline/vendor ] && [ ! -e /work/vendor ] && \
                         cp -a /opt/defensive/baseline/vendor /work/vendor; true";

fn max_grades() -> u32 {
    std::env::var("DEFENSIVE_MAX_GRADES").ok().and_then(|s| s.parse().ok()).unwrap_or(3)
}

struct PatchRun {
    task: Task,
    ws: Option<PathBuf>,
    sandbox: Option<Sandbox>,
    grades: u32,
    done: bool,
}

struct AuditRun {
    audit: Audit,
    sandbox: Option<Sandbox>,
    done: bool,
}

#[derive(Default)]
enum Active {
    #[default]
    None,
    Patch(Box<PatchRun>),
    Audit(Box<AuditRun>),
}

/// The defensive state of one Loop session.
pub struct Session {
    runs: PathBuf,
    cwd: PathBuf,
    active: Mutex<Active>,
}

impl Session {
    /// A session writing runs under `runs`; relative folders resolve against `cwd`.
    pub fn new(runs: PathBuf, cwd: PathBuf) -> Self {
        Self {
            runs,
            cwd,
            active: Mutex::new(Active::None),
        }
    }

    /// The runs directory.
    pub fn runs(&self) -> &Path {
        &self.runs
    }

    /// Whether a run is in progress (prepared and not finished).
    pub fn in_progress(&self) -> bool {
        match &*self.active.lock() {
            Active::None => false,
            Active::Patch(r) => !r.done,
            Active::Audit(r) => !r.done,
        }
    }

    /// Finish whatever is running and stop its container.
    pub fn shutdown(&self) -> String {
        let mut active = self.active.lock();
        let out = match &mut *active {
            Active::Patch(r) if !r.done => {
                r.done = true;
                r.sandbox = None;
                task::finalize(&mut r.task);
                format!("{}\nReport: {}", r.task.take_output(), r.task.rep.md.display())
            }
            Active::Audit(r) if !r.done => {
                r.done = true;
                r.sandbox = None;
                audit::finalize(&mut r.audit);
                format!("{}\nReport: {}", r.audit.rep.take_output(), r.audit.run_dir.join("report.md").display())
            }
            _ => String::new(),
        };
        out
    }

    /// The container of the run in progress, if any.
    pub fn container_name(&self) -> Option<String> {
        match &*self.active.lock() {
            Active::Patch(r) if !r.done => r.sandbox.as_ref().map(|s| s.container.name.clone()),
            Active::Audit(r) if !r.done => r.sandbox.as_ref().map(|s| s.container.name.clone()),
            _ => None,
        }
    }

    /// Record a call of one of Loop's own tools in the run's tool log.
    pub fn log_builtin(&self, name: &str, args: &Value, out: &Result<Value, String>, secs: f64) {
        let active = self.active.lock();
        let sandbox = match &*active {
            Active::Patch(r) if !r.done => r.sandbox.as_ref(),
            Active::Audit(r) if !r.done => r.sandbox.as_ref(),
            _ => None,
        };
        if let Some(sandbox) = sandbox {
            sandbox.log_call(name, args, out, secs);
        }
    }

    /// Run a tool by its bare name. Returns `(text, is_error)`.
    pub fn call(&self, name: &str, args: &Value) -> (String, bool) {
        let s = |k: &str| args.get(k).and_then(Value::as_str).unwrap_or("").to_owned();
        match name {
            "prepare_task" => (self.prepare_task(&s("target"), &s("ref")), false),
            "grade_patch" => (self.grade_patch(), false),
            "finish_task" => (self.finish_task(), false),
            "prepare_audit" => (self.prepare_audit(&s("url"), &s("ref")), false),
            "finish_audit" => (self.finish_audit(), false),
            other => self.box_call(other, args),
        }
    }

    fn box_call(&self, name: &str, args: &Value) -> (String, bool) {
        let active = self.active.lock();
        let sandbox = match &*active {
            Active::Patch(r) if !r.done => r.sandbox.as_ref(),
            Active::Audit(r) if !r.done => r.sandbox.as_ref(),
            _ => None,
        };
        let Some(sandbox) = sandbox else {
            let what = if matches!(&*active, Active::Audit(_)) { "audit; call prepare_audit" } else { "task; call prepare_task" };
            return (json!({"error": format!("no active {what} first")}).to_string(), true);
        };
        match sandbox.call(name, args) {
            Ok(v) => (v.to_string(), false),
            Err(e) => (json!({"error": e}).to_string(), true),
        }
    }

    /// Stages 0-3 (repository) or 0-4 (CVE) and the agent's sandbox.
    pub fn prepare_task(&self, target: &str, git_ref: &str) -> String {
        let t = match sources::classify_target(target, &self.cwd) {
            Ok(t) => t,
            Err(e) => return format!("{}. Ask the user for a CVE id, a GitHub repo URL or a local folder path.", e.reason),
        };
        let t = match (t, git_ref.trim()) {
            (Target::Github { url, git_ref: r }, extra) => Target::Github {
                url,
                git_ref: if extra.is_empty() { r } else { Some(extra.to_owned()) },
            },
            (t, _) => t,
        };
        // A new task ends the previous one.
        self.shutdown();
        let mut p = match task::prepare(&t, &self.runs) {
            Ok(p) => p,
            Err(e) => return format!("Could not start the run: {e}"),
        };
        let what = if p.kind == Kind::Cve { "This CVE" } else { "This repository" };
        if !p.ready() {
            task::finalize(&mut p);
            let out = p.take_output();
            let lifecycle = p.lifecycle();
            *self.active.lock() = Active::Patch(Box::new(PatchRun { task: p, ws: None, sandbox: None, grades: 0, done: true }));
            return format!(
                "{out}\n{what} cannot be worked on: lifecycle {lifecycle}. Tell the user why (see the stage reports above) and stop."
            );
        }
        let ws = match task::new_workspace(&p, 1) {
            Ok(ws) => ws,
            Err(e) => return format!("{}\nCould not create the workspace: {e}", p.take_output()),
        };
        let sandbox = match start_patch_sandbox(&p, &ws) {
            Ok(s) => s,
            Err(e) => {
                task::finalize(&mut p);
                return format!("{}\nCould not start the sandbox: {e}", p.take_output());
            }
        };
        let container = sandbox.container.name.clone();
        let out = p.take_output();
        let grades = max_grades();
        let tools = "get_task, list_directory, search_code, read, write, edit, bash, show_diff, submit_report";
        let text = if p.kind == Kind::Cve {
            format!(
                "{out}\nSandbox ready: container {container} (no network). The vulnerable source is the workspace; use the \
                 sandbox tools ({tools}), then grade_patch. grade_patch may be called at most {grades} times."
            )
        } else {
            let at = p.commit.as_ref().map(|c| format!(" at {}", &c[..c.len().min(12)])).unwrap_or_default();
            let how = if p.lang() == "go" {
                "Write the test as a Test function in a *_test.go file. ".to_owned()
            } else {
                let prof = p.profile.clone().unwrap_or_default();
                format!(
                    "Language: {}. Put the test in a test file and give submit_report the command that runs only it as \
                     repro_command (e.g. `{}`); get_task shows the build and test commands the grader uses. ",
                    prof.label, prof.example
                )
            };
            format!(
                "{out}\nSandbox ready: container {container} (no network) with a copy of {}{at}. No vulnerability is known in \
                 advance: assess the code (run_analyzer gives {} static-analyzer leads to confirm or dismiss), pick the most \
                 severe real, reachable issue, prove it with a failing reproduction test, then fix it. {how}Tools: {tools}, \
                 run_analyzer; then grade_patch, which uses YOUR reproduction test as the oracle (it must fail before your fix \
                 and pass after), plus the existing tests. grade_patch may be called at most {grades} times.",
                p.label,
                p.leads.len()
            )
        };
        *self.active.lock() = Active::Patch(Box::new(PatchRun { task: p, ws: Some(ws), sandbox: Some(sandbox), grades: 0, done: false }));
        text
    }

    /// Stage 6, and stage 7 once verified or out of attempts.
    pub fn grade_patch(&self) -> String {
        let mut active = self.active.lock();
        let Active::Patch(r) = &mut *active else {
            return "No active task; call prepare_task first.".into();
        };
        let (Some(ws), false) = (r.ws.clone(), r.done) else {
            return "No active task; call prepare_task first.".into();
        };
        let max = max_grades();
        if r.grades >= max {
            return "No grading attempts left. Call finish_task.".into();
        }
        r.grades += 1;
        let adir = ws.parent().map(Path::to_path_buf).unwrap_or_else(|| ws.clone());
        let calls = r.sandbox.as_ref().map(Sandbox::tool_calls).unwrap_or_default();
        let report = task::attempt_report(&adir);
        task::emit_agent_stage(&mut r.task, r.grades, &ws, &calls, report.as_ref());
        let g = task::grade_attempt(&mut r.task, r.grades, &ws, report.as_ref());
        let verified = g["final_status"] == "PATCH_VERIFIED";
        let tail = if verified || r.grades >= max {
            r.done = true;
            r.sandbox = None;
            task::finalize(&mut r.task);
            let lead = if verified {
                "\nThe task is finished. Summarize the stage reports for the user: the bug, your fix, your verification, and the verdict."
            } else {
                "\nNo grading attempts left; the task is finished. Summarize for the user."
            };
            format!("{lead}\nPatch: {}  Report: {}", adir.join("candidate.patch").display(), r.task.rep.md.display())
        } else {
            format!(
                "\nNot verified: {} You have {} grading attempt(s) left. Keep working in the same workspace, then call grade_patch again.",
                g["feedback"].as_str().unwrap_or(""),
                max - r.grades
            )
        };
        format!("{}{tail}", r.task.take_output())
    }

    /// Stage 7 and stop the sandbox.
    pub fn finish_task(&self) -> String {
        let mut active = self.active.lock();
        let Active::Patch(r) = &mut *active else {
            return "No task.".into();
        };
        if !r.done {
            r.done = true;
            r.sandbox = None;
            task::finalize(&mut r.task);
        }
        format!("{}\nReport: {}", r.task.take_output(), r.task.rep.md.display())
    }

    /// Audit stages 0-3 and the read-only sandbox.
    pub fn prepare_audit(&self, url: &str, git_ref: &str) -> String {
        self.shutdown();
        let git_ref = Some(git_ref.trim()).filter(|r| !r.is_empty());
        let mut a = match audit::prepare(url, &self.runs, git_ref) {
            Ok(a) => a,
            Err(e) => return format!("{e}. Ask the user for a GitHub repository URL."),
        };
        if !a.ready() {
            if a.repo.is_some() {
                audit::finalize(&mut a);
            }
            let out = a.rep.take_output();
            let status = a.status();
            *self.active.lock() = Active::Audit(Box::new(AuditRun { audit: a, sandbox: None, done: true }));
            return format!("{out}\nCannot audit this repo (status {status}). Stop.");
        }
        let repo = a.repo.clone().expect("ready audits have a checkout");
        let name = format!("defaudit-{}", a.run_id.to_lowercase());
        let image = a.image.clone().unwrap_or_default();
        let container = match Container::start(&name, &image, &[(repo.source_dir.as_path(), "/opt/audit/source", "ro")], &[]) {
            Ok(c) => c,
            Err(e) => {
                let out = a.rep.take_output();
                *self.active.lock() = Active::Audit(Box::new(AuditRun { audit: a, sandbox: None, done: true }));
                return format!("{out}\nCould not start sandbox: {e}");
            }
        };
        let sandbox = Sandbox {
            container,
            root: repo.source_dir.clone(),
            inner: "/opt/audit/source".into(),
            mode: Mode::Audit,
            lang: "generic".into(),
            baseline: repo.source_dir.clone(),
            task_json: a.task_json.clone(),
            state_dir: a.out_dir.clone(),
            analyzers: analyzers::Paths::audit(),
        };
        let out = a.rep.take_output();
        let text = format!(
            "{out}\nSandbox ready (no network): {}/{} at {}, language {}, {} static-analyzer leads to triage. Use \
             list_directory, read, search_code, run_analyzer; then submit_finding per issue and submit_audit_report; \
             then finish_audit.",
            repo.owner,
            repo.name,
            &repo.sha[..repo.sha.len().min(12)],
            repo.language,
            a.baseline.len()
        );
        *self.active.lock() = Active::Audit(Box::new(AuditRun { audit: a, sandbox: Some(sandbox), done: false }));
        text
    }

    /// Audit stages 4-6.
    pub fn finish_audit(&self) -> String {
        let mut active = self.active.lock();
        let Active::Audit(r) = &mut *active else {
            return "No active audit.".into();
        };
        if r.done {
            return format!("Already finished. Report: {}", r.audit.run_dir.join("report.md").display());
        }
        let calls = r.sandbox.as_ref().map(Sandbox::tool_calls).unwrap_or_default();
        let n_sub = std::fs::read_to_string(r.audit.out_dir.join("findings.jsonl")).map(|t| t.lines().count()).unwrap_or(0);
        let tools = counts_text(calls.iter().map(|c| c["tool"].as_str().unwrap_or("?")));
        r.audit.rep.emit(
            StageResult::new("4 agent", if n_sub > 0 { "PASS" } else { "FAIL" }, format!("loop session: {n_sub} finding(s) submitted"))
                .fact("tool calls", if tools.is_empty() { "none".to_owned() } else { tools }),
        );
        r.done = true;
        r.sandbox = None;
        audit::finalize(&mut r.audit);
        format!(
            "{}\nReport: {}\nSummarize the findings for the user: the highest-severity issues, their reachability, and the \
             overall risk. Note that these are candidates for human review.",
            r.audit.rep.take_output(),
            r.audit.run_dir.join("report.md").display()
        )
    }
}

fn start_patch_sandbox(p: &Task, ws: &Path) -> Result<Sandbox, String> {
    let name = format!("defensive-{}", p.run_id.to_lowercase());
    let image = p.image.clone().ok_or("no image")?;
    let container = Container::start(&name, &image, &[(ws, "/work", "")], &[])?;
    if p.lang() != "go" {
        let _ = container.sh(LINK_DEPS, None, &[], Duration::from_secs(60));
    }
    Ok(Sandbox {
        container,
        root: ws.to_path_buf(),
        inner: "/work".into(),
        mode: Mode::Patch,
        lang: p.lang(),
        baseline: task::baseline_of(p),
        task_json: p.task_json.clone().ok_or("no task.json")?,
        state_dir: ws.parent().map(Path::to_path_buf).unwrap_or_else(|| ws.to_path_buf()),
        analyzers: analyzers::Paths::patch(),
    })
}

impl Drop for Session {
    fn drop(&mut self) {
        let _ = self.shutdown();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tools_without_a_task_say_so() {
        let dir = tempfile::tempdir().unwrap();
        let s = Session::new(dir.path().into(), dir.path().into());
        assert!(!s.in_progress());
        let (text, err) = s.call("list_directory", &json!({"path": "a"}));
        assert!(err && text.contains("call prepare_task first"), "{text}");
        assert_eq!(s.grade_patch(), "No active task; call prepare_task first.");
        assert_eq!(s.finish_audit(), "No active audit.");
        let (text, err) = s.call("prepare_task", &json!({"target": "not a thing"}));
        assert!(!err && text.starts_with("not a CVE id, a GitHub repo URL or an existing folder"), "{text}");
    }
}
