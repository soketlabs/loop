//! `/patch CVE-…` end to end, one real CVE per language, with real podman and
//! network: stages 0-4 (the upstream fix must fail-then-pass the hidden
//! tests), then a scripted agent applies the upstream fix in its workspace,
//! submits a report, and the grader must verify it 12/12.
//!
//! `cargo test -p loop-defensive --test e2e_cve -- --ignored --nocapture`

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use loop_defensive::{patchlib, task, Session};
use serde_json::json;

fn run_dir(runs: &Path, cve: &str) -> PathBuf {
    fs::canonicalize(runs.join(cve).join("latest")).expect("run dir")
}

fn check(cve: &str) {
    let runs = tempfile::tempdir().unwrap();
    let s = Session::new(runs.path().into(), runs.path().into());
    let ready = s.prepare_task(cve, "");
    println!("{ready}");
    assert!(ready.contains("STAGE 4 CONFIRM — PASS"), "{cve}: upstream fix not confirmed:\n{ready}");
    assert!(ready.contains("Sandbox ready"), "{ready}");
    // Nothing the agent reads names the hidden tests.
    let rd = run_dir(runs.path(), cve);
    let visible = fs::read_to_string(rd.join("report.md")).unwrap();
    assert!(!visible.contains("oracle test files") && !visible.contains("oracle command"), "{visible}");

    // The scripted agent: the upstream fix, applied in the workspace.
    let run_id = rd.file_name().unwrap().to_string_lossy().into_owned();
    let reference = fs::read_to_string(task::hidden_root().join(&run_id).join("reference.patch")).unwrap();
    let ws = rd.join("attempt-1/workspace");
    let patch_file = rd.join("upstream-fix.patch");
    fs::write(&patch_file, &reference).unwrap();
    let applied = Command::new("git").args(["apply", "-p1", &patch_file.to_string_lossy()]).current_dir(&ws).output().unwrap();
    assert!(applied.status.success(), "{}", String::from_utf8_lossy(&applied.stderr));
    let files = patchlib::patch_paths(&reference);
    let (rep, err) = s.call(
        "submit_report",
        &json!({"summary": cve, "root_cause": "see the upstream fix", "affected_files": files,
                "vulnerable_lines": format!("{}:1-1", files[0]), "fix_description": "upstream fix",
                "verification": "hidden tests"}),
    );
    assert!(!err, "{rep}");
    let graded = s.grade_patch();
    println!("{graded}");
    assert!(graded.contains("verdict PATCH_VERIFIED, score 12/12"), "{cve}:\n{graded}");
    assert!(!graded.contains("oracle command"), "{graded}");
}

#[test]
#[ignore = "needs podman and network"]
fn cve_go() {
    check("CVE-2022-36009");
}

#[test]
#[ignore = "needs podman and network"]
fn cve_python() {
    check("CVE-2022-29217");
}

#[test]
#[ignore = "needs podman and network"]
fn cve_javascript() {
    check("CVE-2020-7788");
}

#[test]
#[ignore = "needs podman and network"]
fn cve_rust() {
    check("CVE-2022-24713");
}

#[test]
#[ignore = "needs podman and network"]
fn cve_java() {
    check("CVE-2022-25845");
}

#[test]
#[ignore = "needs podman and network"]
fn cve_ruby() {
    check("CVE-2022-31163");
}

#[test]
#[ignore = "needs podman and network"]
fn cve_php() {
    check("CVE-2022-31042");
}
