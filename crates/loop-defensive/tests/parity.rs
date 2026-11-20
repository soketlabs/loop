//! Re-grade a run recorded by the Python tool with the Rust code. Needs the
//! run directory and its image:
//!
//! `DEFENSIVE_PARITY_RUN=<runs>/repo/<slug>/<run_id> DEFENSIVE_PARITY_IMAGE=<tag> \
//!   cargo test -p loop-defensive --test parity -- --ignored --nocapture`

use std::fs;
use std::path::PathBuf;

use loop_defensive::{grader, patchlib};
use serde_json::Value;

#[test]
#[ignore = "needs a recorded run and its podman image"]
fn rust_regrade_matches_the_recorded_python_grade() {
    let Some(run) = std::env::var_os("DEFENSIVE_PARITY_RUN").map(PathBuf::from) else {
        eprintln!("DEFENSIVE_PARITY_RUN not set; skipping");
        return;
    };
    let image = std::env::var("DEFENSIVE_PARITY_IMAGE").expect("DEFENSIVE_PARITY_IMAGE");
    let lang = serde_json::from_str::<Value>(&fs::read_to_string(run.join("report.json")).unwrap()).unwrap()["language"]
        .as_str()
        .unwrap()
        .to_owned();
    let attempt = run.join("attempt-1");
    let diff = patchlib::diff_trees(&run.join("task/baseline"), &attempt.join("workspace"), &lang);
    let recorded_patch = fs::read_to_string(attempt.join("candidate.patch")).unwrap();
    let recorded_tests = fs::read_to_string(attempt.join("agent_tests.patch")).unwrap();
    // The same files, and patches that apply to the same result.
    assert_eq!(patchlib::patch_paths(&diff.code_patch), patchlib::patch_paths(&recorded_patch));
    assert_eq!(patchlib::patch_paths(&diff.test_patch), patchlib::patch_paths(&recorded_tests));
    let changed = |p: &str| p.lines().filter(|l| l.starts_with(['+', '-']) && !l.starts_with("+++") && !l.starts_with("---")).count();
    assert_eq!(changed(&diff.code_patch), changed(&recorded_patch), "same added/removed lines");
    println!("diff identical to the recorded patch: {}", diff.code_patch == recorded_patch);

    let g = grader::grade_repo(&image, &diff.code_patch, &attempt.join("oracle"));
    println!("{}", serde_json::to_string_pretty(&g).unwrap());
    let recorded: Value = serde_json::from_str(&fs::read_to_string(attempt.join("grade.json")).unwrap()).unwrap();
    assert_eq!(g["final_status"], recorded["final_status"]);
    let steps = |v: &Value| -> Vec<(String, String)> {
        v["steps"]
            .as_array()
            .unwrap()
            .iter()
            .map(|s| (s["step"].as_str().unwrap().to_owned(), s["status"].as_str().unwrap().to_owned()))
            .collect()
    };
    assert_eq!(steps(&g), steps(&recorded));
    for (k, v) in recorded["dimensions"].as_object().unwrap() {
        assert_eq!(&g["dimensions"][k], v, "dimension {k}");
    }
}
