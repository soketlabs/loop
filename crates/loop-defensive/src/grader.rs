//! The trusted graders. Each grade runs in a FRESH container with
//! `--network none` that never saw the agent's container: only the patch
//! text, the oracle (read-only) and the image's pristine baseline.
//!
//! Steps:
//!   A validate         format, safe paths, only non-test source files
//!   B reproduce        the oracle FAILS on the baseline
//!   C build            the patched candidate builds
//!   D regression       the oracle PASSES on the candidate (run twice)
//!   E existing tests   no test that passed on the baseline fails now
//!   F security         reserved (no extra checks declared yet)
//!   G verdict          a deterministic status from A-F

use std::collections::BTreeSet;
use std::fs;
use std::path::Path;
use std::time::Duration;

use serde_json::{json, Map, Value};

use crate::patchlib::{self, tail};
use crate::sandbox::{shell_quote, Container, Output};
use crate::scoring::{empty_grader_dims, infrastructure_failure};

const BASELINE: &str = "/opt/defensive/baseline";
const WORK: &str = "/tmp/grade";
const TEST_TIMEOUT: Duration = Duration::from_secs(600);
/// Installed dependency directories linked (not copied) into scratch trees.
/// Composer's `vendor/` is copied instead: its autoloader resolves the
/// project's own classes relative to the real vendor path.
const DEP_DIRS: &[&str] = &["node_modules"];

fn test_timeout() -> Duration {
    std::env::var("DEF_TEST_TIMEOUT")
        .ok()
        .and_then(|s| s.parse().ok())
        .map(Duration::from_secs)
        .unwrap_or(TEST_TIMEOUT)
}

struct Steps(Vec<Value>);

impl Steps {
    fn add(&mut self, name: &str, status: &str, detail: impl Into<String>, extra: &[(&str, Value)]) {
        let mut s = Map::new();
        s.insert("step".into(), name.into());
        s.insert("status".into(), status.into());
        s.insert("detail".into(), detail.into().into());
        for (k, v) in extra {
            s.insert((*k).into(), v.clone());
        }
        self.0.push(Value::Object(s));
    }
}

/// The grading container and its scratch trees.
struct Bench {
    c: Container,
}

impl Bench {
    fn sh(&self, script: &str, timeout: Duration) -> Output {
        self.c.sh(script, None, &[("LC_ALL", "C")], timeout)
    }

    fn must(&self, script: &str, what: &str) -> Result<(), String> {
        let r = self.sh(script, Duration::from_secs(900));
        if r.ok() {
            Ok(())
        } else {
            Err(format!("{what}: {}", tail(r.combined().trim(), 400)))
        }
    }

    /// Copy the baseline to `dst`; with `link_deps`, installed dependency
    /// directories are linked instead of copied.
    fn copy_baseline(&self, dst: &str, link_deps: bool) -> Result<(), String> {
        let d = shell_quote(dst);
        let script = if link_deps {
            let deps = DEP_DIRS.join("|");
            format!(
                "rm -rf {d} && mkdir -p {d} && cd {BASELINE} && for f in * .[!.]* ..?*; do \
                 [ -e \"$f\" ] || [ -L \"$f\" ] || continue; case \"$f\" in {deps}) \
                 if [ -d \"$f\" ] && [ ! -L \"$f\" ]; then ln -s {BASELINE}/\"$f\" {d}/\"$f\"; else cp -a -- \"$f\" {d}/; fi ;; \
                 *) cp -a -- \"$f\" {d}/ ;; esac; done"
            )
        } else {
            format!("rm -rf {d} && mkdir -p {d} && cp -a {BASELINE}/. {d}/")
        };
        self.must(&script, "could not copy the baseline")
    }

    /// Copy the oracle's files over `dst`.
    fn overlay(&self, dst: &str) -> Result<(), String> {
        let d = shell_quote(dst);
        self.must(&format!("if [ -d /oracle/files ]; then cp -r /oracle/files/. {d}/; fi"), "could not add the oracle tests")
    }

    /// Apply `/in/<name>` to `dst`: `git apply`, else `patch`.
    fn apply(&self, name: &str, dst: &str) -> (bool, String) {
        let d = shell_quote(dst);
        let p = shell_quote(&format!("/in/{name}"));
        let script = format!(
            "cd {d} && if git apply --check -p1 {p} >/dev/null 2>&1; then git apply -p1 {p}; \
             else patch -p1 --forward --batch --no-backup-if-mismatch -i {p}; fi"
        );
        let r = self.sh(&script, Duration::from_secs(120));
        (r.ok(), tail(&r.combined(), 600))
    }

    /// Run a shell command in `dir`.
    fn run(&self, command: &str, dir: &str) -> Value {
        let pythonpath = format!("{dir}:{dir}/src");
        let r = self.c.exec(
            &["bash", "-c", command],
            Some(dir),
            &[("PYTHONPATH", &pythonpath), ("CI", "true")],
            None,
            test_timeout(),
        );
        json!({"exit": r.exit, "timed_out": r.timed_out, "secs": (r.secs * 10.0).round() / 10.0,
               "log_tail": tail(&format!("{}\n{}", r.stdout, r.stderr), 1500)})
    }

    /// `go test -count=1 -json [-run R] pkgs`, summarised per test.
    fn go_test(&self, dir: &str, pkgs: &[String], run: Option<&str>) -> GoTest {
        let mut argv = vec!["go", "test", "-count=1", "-json"];
        if let Some(r) = run {
            argv.extend(["-run", r]);
        }
        argv.extend(pkgs.iter().map(String::as_str));
        let r = self.c.exec(&argv, Some(dir), &[], None, test_timeout());
        GoTest::parse(&r)
    }
}

fn is(dims: &Map<String, Value>, key: &str, value: bool) -> bool {
    dims.get(key) == Some(&Value::Bool(value))
}

fn outcome(r: &Value) -> &'static str {
    if r["timed_out"] == true {
        "timeout"
    } else if r["exit"] == 0 {
        "pass"
    } else {
        "fail"
    }
}

/// One `go test -json` run.
#[derive(Debug, Default, Clone)]
pub struct GoTest {
    timed_out: bool,
    passed: BTreeSet<String>,
    failed: BTreeSet<String>,
    build_error: bool,
    log_tail: String,
}

impl GoTest {
    fn parse(r: &Output) -> Self {
        let mut t = GoTest {
            timed_out: r.timed_out,
            ..Self::default()
        };
        let mut text = Vec::new();
        for line in r.stdout.lines() {
            let Ok(ev) = serde_json::from_str::<Value>(line) else {
                text.push(line.to_owned());
                continue;
            };
            let action = ev["Action"].as_str().unwrap_or("");
            let test = ev["Test"].as_str();
            match (action, test) {
                ("output", _) => {
                    let o = ev["Output"].as_str().unwrap_or("");
                    text.push(o.trim_end_matches('\n').to_owned());
                    if o.contains("[build failed]") || o.contains("[setup failed]") {
                        t.build_error = true;
                    }
                }
                ("build-fail", _) => t.build_error = true,
                ("pass", Some(name)) => {
                    t.passed.insert(name.to_owned());
                }
                ("fail", Some(name)) => {
                    t.failed.insert(name.to_owned());
                }
                _ => {}
            }
        }
        let exited_badly = !matches!(r.exit, Some(0) | None);
        if r.stderr.contains("build failed") || r.stderr.contains("setup failed") || (exited_badly && t.passed.is_empty() && t.failed.is_empty()) {
            t.build_error = t.build_error || !r.stderr.trim().is_empty();
        }
        t.log_tail = tail(&format!("{}\n{}", text.join("\n"), r.stderr), 1500);
        t
    }

    /// `pass`, `fail`, `build_error`, `no_tests` or `timeout`.
    fn oracle_outcome(&self, expected: &[String]) -> &'static str {
        if self.timed_out {
            "timeout"
        } else if self.build_error {
            "build_error"
        } else if !expected.iter().any(|t| self.passed.contains(t) || self.failed.contains(t)) {
            "no_tests"
        } else if !self.failed.is_empty() {
            "fail"
        } else {
            "pass"
        }
    }

    fn failed_list(&self) -> Value {
        json!(self.failed.iter().collect::<Vec<_>>())
    }
}

fn pkgs_for(paths: &[String]) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for p in paths {
        let pkg = match p.rsplit_once('/') {
            Some((d, _)) => format!("./{d}"),
            None => ".".into(),
        };
        if !out.contains(&pkg) {
            out.push(pkg);
        }
    }
    out
}

/// Start the grading container with the patch files and the oracle mounted.
fn bench(tag: &str, inbox: &Path, oracle: &Path) -> Result<Bench, String> {
    let name = format!("defensive-grade-{}", uuid_like());
    let c = Container::start(&name, tag, &[(inbox, "/in", "ro"), (oracle, "/oracle", "ro")], &[])?;
    Ok(Bench { c })
}

fn uuid_like() -> String {
    format!(
        "{}-{}",
        std::process::id(),
        std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0)
    )
}

fn inbox_with(files: &[(&str, &str)]) -> Result<tempfile::TempDir, String> {
    let dir = tempfile::Builder::new().prefix("defgrade-").tempdir().map_err(|e| e.to_string())?;
    for (name, body) in files {
        fs::write(dir.path().join(name), body).map_err(|e| e.to_string())?;
    }
    Ok(dir)
}

/// Grade a non-Go repository patch. The oracle is the agent's own
/// reproduction test: `oracle/files` and `oracle/oracle.json`
/// (`repro_command`, `build_command`, `test_command`, `lang`).
pub fn grade_repo(tag: &str, patch: &str, oracle_dir: &Path) -> Value {
    let o: Value = match fs::read_to_string(oracle_dir.join("oracle.json")).ok().and_then(|t| serde_json::from_str(&t).ok()) {
        Some(v) => v,
        None => return infrastructure_failure("grader error: no oracle.json"),
    };
    let s = |k: &str| o[k].as_str().unwrap_or("").trim().to_owned();
    // Upstream tests (a CVE's hidden oracle) replace the files they change, so
    // the existing suite runs with them; an agent's own test is kept apart.
    let tests_include_oracle = o["existing_tests_include_oracle"] == true;
    // Upstream tests for a denial-of-service bug hang on the vulnerable code.
    let timeout_reproduces = o["timeout_reproduces"] == true;
    let (lang, repro, build, tests) = (
        o["lang"].as_str().unwrap_or("generic").to_owned(),
        s("repro_command"),
        s("build_command"),
        s("test_command"),
    );
    let (ok, detail, _) = patchlib::validate_patch(patch, |p| patchlib::is_code_file(p, &lang));
    let added: String = if ok {
        patchlib::split_patch(patch).into_iter().filter(|(_, _, new)| *new).map(|(_, c, _)| c).collect()
    } else {
        String::new()
    };
    let inbox = match inbox_with(&[("candidate.patch", patch), ("added.patch", &added)]) {
        Ok(d) => d,
        Err(e) => return infrastructure_failure(format!("grader error: {e}")),
    };
    let mut steps = Steps(Vec::new());
    let mut dims = empty_grader_dims();
    let result = (|| -> Result<String, String> {
        let b = bench(tag, inbox.path(), oracle_dir)?;
        // A validate
        dims.insert("patch_format_valid".into(), ok.into());
        let sha = if patch.is_empty() { Value::Null } else { patchlib::sha256(patch).into() };
        steps.add("A validate", if ok { "pass" } else { "fail" }, detail.clone(), &[("sha256", sha)]);

        // B reproduce: the baseline + files the patch ADDS + the agent's tests must
        // fail, so a test cannot "fail" only because a new helper file is missing.
        let base = format!("{WORK}/baseline");
        b.copy_baseline(&base, true)?;
        let base_tests = (!tests.is_empty()).then(|| b.run(&tests, &base));
        if !added.is_empty() && !b.apply("added.patch", &base).0 {
            return Err("could not add the patch's new files to the baseline".into());
        }
        b.overlay(&base)?;
        let br = b.run(&repro, &base);
        let b_out = outcome(&br);
        let reproduced = b_out == "fail" || (timeout_reproduces && b_out == "timeout");
        dims.insert("baseline_reproduced".into(), reproduced.into());
        let note = if reproduced && b_out == "timeout" { " (the vulnerable code hangs: counts as reproduced)" } else { "" };
        steps.add(
            "B reproduce",
            if reproduced { "pass" } else { "fail" },
            format!("`{repro}` on the original code: {b_out} (expected fail){note}"),
            &[("log_tail", tail(br["log_tail"].as_str().unwrap_or(""), 800).into())],
        );

        if ok {
            let cand = format!("{WORK}/candidate");
            b.copy_baseline(&cand, true)?;
            let (applied, msg) = b.apply("candidate.patch", &cand);
            if !applied {
                dims.insert("candidate_build_passed".into(), false.into());
                steps.add("C build", "fail", format!("patch did not apply: {msg}"), &[]);
            } else {
                let built = if build.is_empty() {
                    steps.add("C build", "skip", "no build step for this project", &[]);
                    true
                } else {
                    let r = b.run(&build, &cand);
                    let built = r["exit"] == 0;
                    let log = if built { String::new() } else { r["log_tail"].as_str().unwrap_or("").to_owned() };
                    steps.add("C build", if built { "pass" } else { "fail" }, format!("`{build}`"), &[("log_tail", log.into())]);
                    built
                };
                dims.insert("candidate_build_passed".into(), built.into());
                if built {
                    // E existing tests first: WITHOUT the agent's own tests, or WITH
                    // the upstream ones they replace.
                    if tests_include_oracle {
                        b.overlay(&cand)?;
                    }
                    if tests.is_empty() {
                        let why = o["existing_tests_note"].as_str().unwrap_or("no test suite detected for this project");
                        steps.add("E existing tests", "skip", why, &[]);
                    } else {
                        let e = b.run(&tests, &cand);
                        let base_out = base_tests.as_ref().map(outcome).unwrap_or("fail");
                        if outcome(&e) == "pass" {
                            dims.insert("existing_tests_passed".into(), true.into());
                            steps.add("E existing tests", "pass", format!("`{tests}` passes on the patched code"), &[]);
                        } else if base_out != "pass" {
                            steps.add(
                                "E existing tests",
                                "skip",
                                format!("`{tests}` already {base_out} on the original code, so regressions cannot be told apart"),
                                &[("log_tail", tail(e["log_tail"].as_str().unwrap_or(""), 600).into())],
                            );
                        } else {
                            dims.insert("existing_tests_passed".into(), false.into());
                            steps.add(
                                "E existing tests",
                                "fail",
                                format!("`{tests}` passed on the original code but {} after the patch", outcome(&e)),
                                &[("log_tail", tail(e["log_tail"].as_str().unwrap_or(""), 800).into())],
                            );
                        }
                    }
                    // D regression (twice)
                    b.overlay(&cand)?;
                    let (d1, d2) = (b.run(&repro, &cand), b.run(&repro, &cand));
                    let (o1, o2) = (outcome(&d1), outcome(&d2));
                    dims.insert("regression_test_passed".into(), (o1 == "pass").into());
                    dims.insert("validation_reproducible".into(), (o1 == o2).into());
                    let log = if o1 != "pass" { tail(d1["log_tail"].as_str().unwrap_or(""), 800) } else { String::new() };
                    steps.add(
                        "D regression",
                        if o1 == "pass" { "pass" } else { "fail" },
                        format!("`{repro}` on the patched code: {o1} / rerun: {o2} (expected pass)"),
                        &[("log_tail", log.into())],
                    );
                }
            }
        }
        steps.add("F security", "skip", "no extra security checks declared", &[]);
        let fp = ok && is(&dims, "candidate_build_passed", true) && (is(&dims, "regression_test_passed", false) || is(&dims, "existing_tests_passed", false));
        dims.insert("false_positive".into(), fp.into());
        dims.insert("infrastructure_failure".into(), false.into());
        let status = if !is(&dims, "baseline_reproduced", true) {
            "INCONCLUSIVE"
        } else if ok && is(&dims, "candidate_build_passed", true) && is(&dims, "regression_test_passed", true) && !is(&dims, "existing_tests_passed", false) {
            "PATCH_VERIFIED"
        } else {
            "REPRODUCIBLE"
        };
        steps.add("G verdict", if status == "PATCH_VERIFIED" { "pass" } else { "fail" }, status, &[]);
        Ok(status.to_owned())
    })();
    finish(result, dims, steps, &[])
}

fn finish(result: Result<String, String>, mut dims: Map<String, Value>, mut steps: Steps, extra: &[(&str, Value)]) -> Value {
    let status = match result {
        Ok(status) => status,
        Err(e) => {
            dims.insert("infrastructure_failure".into(), true.into());
            steps.add("G verdict", "error", format!("grader error: {e}"), &[]);
            "FAILED".to_owned()
        }
    };
    let mut out = json!({"final_status": status, "dimensions": dims, "steps": steps.0});
    for (k, v) in extra {
        out[*k] = v.clone();
    }
    out
}

/// Grade a Go patch against hidden oracle tests (`oracle/oracle.json`:
/// `run`, `tests`, `packages`). `agent_tests` is the agent's own test diff,
/// run for information only.
pub fn grade_go(tag: &str, patch: &str, oracle_dir: &Path, agent_tests: &str) -> Value {
    let o: Value = match fs::read_to_string(oracle_dir.join("oracle.json")).ok().and_then(|t| serde_json::from_str(&t).ok()) {
        Some(v) => v,
        None => return infrastructure_failure("grader error: no oracle.json"),
    };
    let strs = |v: &Value| v.as_array().into_iter().flatten().filter_map(Value::as_str).map(str::to_owned).collect::<Vec<_>>();
    let (o_run, o_tests, o_pkgs) = (o["run"].as_str().unwrap_or("").to_owned(), strs(&o["tests"]), strs(&o["packages"]));
    let inbox = match inbox_with(&[("candidate.patch", patch), ("agent_tests.patch", agent_tests)]) {
        Ok(d) => d,
        Err(e) => return infrastructure_failure(format!("grader error: {e}")),
    };
    let mut steps = Steps(Vec::new());
    let mut dims = empty_grader_dims();
    let mut agent_repro = Value::Null;
    let result = (|| -> Result<String, String> {
        let b = bench(tag, inbox.path(), oracle_dir)?;
        let (ok, detail, paths) = patchlib::validate_patch(patch, |p| patchlib::is_code_file(p, "go"));
        dims.insert("patch_format_valid".into(), ok.into());
        let sha = if patch.is_empty() { Value::Null } else { patchlib::sha256(patch).into() };
        steps.add("A validate", if ok { "pass" } else { "fail" }, detail, &[("sha256", sha)]);
        let mut test_pkgs = o_pkgs.clone();
        if ok {
            for p in pkgs_for(&paths) {
                if !test_pkgs.contains(&p) {
                    test_pkgs.push(p);
                }
            }
        }

        let base = format!("{WORK}/baseline");
        b.copy_baseline(&base, false)?;
        let base_existing = b.go_test(&base, &test_pkgs, None);
        b.overlay(&base)?;
        let bt = b.go_test(&base, &o_pkgs, Some(&o_run));
        let b_out = bt.oracle_outcome(&o_tests);
        dims.insert("baseline_reproduced".into(), (b_out == "fail").into());
        let log = if b_out != "fail" { bt.log_tail.clone() } else { tail(&bt.log_tail, 600) };
        steps.add(
            "B reproduce",
            if b_out == "fail" { "pass" } else { "fail" },
            format!("oracle on baseline: {b_out} (expected fail)"),
            &[("failed", bt.failed_list()), ("log_tail", log.into())],
        );

        if ok {
            let cand = format!("{WORK}/candidate");
            b.copy_baseline(&cand, false)?;
            let (applied, msg) = b.apply("candidate.patch", &cand);
            if !applied {
                dims.insert("candidate_build_passed".into(), false.into());
                steps.add("C build", "fail", format!("patch did not apply: {msg}"), &[]);
            } else {
                let mut argv = vec!["go".to_owned(), "build".to_owned()];
                argv.extend(test_pkgs.iter().cloned());
                let argv_ref: Vec<&str> = argv.iter().map(String::as_str).collect();
                let br = b.c.exec(&argv_ref, Some(&cand), &[], None, test_timeout());
                let built = br.ok();
                dims.insert("candidate_build_passed".into(), built.into());
                let log = if built { String::new() } else { tail(&br.stderr, 1500) };
                steps.add("C build", if built { "pass" } else { "fail" }, format!("go build {}", test_pkgs.join(" ")), &[("log_tail", log.into())]);
                if built {
                    b.overlay(&cand)?;
                    let (d1, d2) = (b.go_test(&cand, &o_pkgs, Some(&o_run)), b.go_test(&cand, &o_pkgs, Some(&o_run)));
                    let (o1, o2) = (d1.oracle_outcome(&o_tests), d2.oracle_outcome(&o_tests));
                    dims.insert("regression_test_passed".into(), (o1 == "pass").into());
                    dims.insert("validation_reproducible".into(), (o1 == o2).into());
                    let log = if o1 != "pass" { tail(&d1.log_tail, 800) } else { String::new() };
                    steps.add(
                        "D regression",
                        if o1 == "pass" { "pass" } else { "fail" },
                        format!("oracle on candidate: {o1} / rerun: {o2} (expected pass)"),
                        &[("failed", d1.failed_list()), ("log_tail", log.into())],
                    );
                    let e = b.go_test(&cand, &test_pkgs, None);
                    let newly: Vec<String> = e
                        .failed
                        .iter()
                        .filter(|t| !base_existing.failed.contains(*t) && !o_tests.contains(t))
                        .cloned()
                        .collect();
                    let e_ok = !e.build_error && !e.timed_out && newly.is_empty();
                    dims.insert("existing_tests_passed".into(), e_ok.into());
                    let newly_text = if newly.is_empty() {
                        "none".to_owned()
                    } else {
                        format!("[{}]", newly.iter().map(|t| format!("'{t}'")).collect::<Vec<_>>().join(", "))
                    };
                    steps.add(
                        "E existing tests",
                        if e_ok { "pass" } else { "fail" },
                        format!(
                            "{} passed; newly failing: {newly_text}; already failing on baseline: {}",
                            e.passed.len(),
                            base_existing.failed.len()
                        ),
                        &[("newly_failing", json!(newly))],
                    );
                    if !agent_tests.trim().is_empty() {
                        agent_repro = run_agent_tests(&b, agent_tests);
                    }
                }
            }
        }
        steps.add("F security", "skip", "no extra security checks declared", &[]);
        let fp = ok && is(&dims, "candidate_build_passed", true) && (is(&dims, "regression_test_passed", false) || is(&dims, "existing_tests_passed", false));
        dims.insert("false_positive".into(), fp.into());
        dims.insert("infrastructure_failure".into(), false.into());
        let status = if matches!(b_out, "build_error" | "no_tests" | "timeout") || !is(&dims, "baseline_reproduced", true) {
            "INCONCLUSIVE"
        } else if is(&dims, "patch_format_valid", true)
            && is(&dims, "candidate_build_passed", true)
            && is(&dims, "regression_test_passed", true)
            && is(&dims, "existing_tests_passed", true)
            && !is(&dims, "security_checks_passed", false)
        {
            "PATCH_VERIFIED"
        } else {
            "REPRODUCIBLE"
        };
        steps.add("G verdict", if status == "PATCH_VERIFIED" { "pass" } else { "fail" }, status, &[]);
        Ok(status.to_owned())
    })();
    let repro = agent_repro.clone();
    finish(result, dims, steps, &[("agent_repro", repro), ("oracle_tests", json!(o_tests))])
}

/// Do the agent's own Go tests fail before its fix and pass after?
fn run_agent_tests(b: &Bench, tests_patch: &str) -> Value {
    let funcs = patchlib::changed_test_funcs(tests_patch);
    let pkgs = pkgs_for(&patchlib::patch_paths(tests_patch));
    if funcs.is_empty() {
        return json!({"tests": [], "detail": "agent tests add no Test functions"});
    }
    let run = format!("^({})$", funcs.join("|"));
    let mut out = json!({"tests": funcs});
    for (label, with_fix) in [("before_fix", false), ("after_fix", true)] {
        let t = format!("{WORK}/agent-{label}");
        if b.copy_baseline(&t, false).is_err() {
            out[label] = "copy_failed".into();
            continue;
        }
        if with_fix && !b.apply("candidate.patch", &t).0 {
            out[label] = "patch_failed".into();
            continue;
        }
        if !b.apply("agent_tests.patch", &t).0 {
            out[label] = "tests_did_not_apply".into();
            continue;
        }
        out[label] = b.go_test(&t, &pkgs, Some(&run)).oracle_outcome(&funcs).into();
    }
    out["confirms_fix"] = (out["before_fix"] == "fail" && out["after_fix"] == "pass").into();
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn output(stdout: &str, stderr: &str, exit: Option<i32>) -> Output {
        Output {
            exit,
            stdout: stdout.into(),
            stderr: stderr.into(),
            ..Output::default()
        }
    }

    #[test]
    fn go_test_json_outcomes() {
        let run = concat!(
            r#"{"Action":"run","Test":"TestRepro"}"#, "\n",
            r#"{"Action":"output","Test":"TestRepro","Output":"--- FAIL: TestRepro\n"}"#, "\n",
            r#"{"Action":"fail","Test":"TestRepro"}"#, "\n",
            r#"{"Action":"pass","Test":"TestOther"}"#, "\n"
        );
        let t = GoTest::parse(&output(run, "", Some(1)));
        assert_eq!(t.oracle_outcome(&["TestRepro".into()]), "fail");
        assert_eq!(t.oracle_outcome(&["TestMissing".into()]), "no_tests");
        let build = GoTest::parse(&output("", "# pkg\n./a.go:1: syntax error\nFAIL pkg [build failed]\n", Some(2)));
        assert_eq!(build.oracle_outcome(&["TestRepro".into()]), "build_error");
        let ok = GoTest::parse(&output(r#"{"Action":"pass","Test":"TestRepro"}"#, "", Some(0)));
        assert_eq!(ok.oracle_outcome(&["TestRepro".into()]), "pass");
    }

    #[test]
    fn packages_of_paths() {
        assert_eq!(pkgs_for(&["a.go".into(), "x/y/b.go".into(), "x/y/c.go".into()]), vec![".", "./x/y"]);
    }

    #[test]
    fn outcome_of_runs() {
        assert_eq!(outcome(&json!({"exit": 0, "timed_out": false})), "pass");
        assert_eq!(outcome(&json!({"exit": 1, "timed_out": false})), "fail");
        assert_eq!(outcome(&json!({"exit": null, "timed_out": true})), "timeout");
    }
}
