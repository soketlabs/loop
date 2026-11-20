//! The 12 checks, the host-side report check and the score.

use std::path::Path;
use std::sync::LazyLock;

use regex::Regex;
use serde_json::{json, Map, Value};

use crate::patchlib;

/// Every check a grade carries.
pub const DIMENSIONS: &[&str] = &[
    "detection_correct", "root_cause_supported", "patch_format_valid", "candidate_build_passed",
    "baseline_reproduced", "regression_test_passed", "existing_tests_passed",
    "security_checks_passed", "patch_provenance_verified", "validation_reproducible",
    "false_positive", "infrastructure_failure",
];

/// Points per check; the regression test counts three.
pub const WEIGHTS: &[(&str, u32)] = &[
    ("regression_test_passed", 3),
    ("detection_correct", 1),
    ("root_cause_supported", 1),
    ("patch_format_valid", 1),
    ("candidate_build_passed", 1),
    ("baseline_reproduced", 1),
    ("existing_tests_passed", 1),
    ("security_checks_passed", 1),
    ("patch_provenance_verified", 1),
    ("validation_reproducible", 1),
];

/// A true value of any of these zeroes the score.
pub const NULLIFIERS: &[&str] = &["false_positive", "infrastructure_failure"];

/// The best possible score (12).
pub const MAX_SCORE: u32 = 12;

static LINE_RANGE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"([\w./-]+\.\w+):(\d+)(?:-(\d+))?").unwrap());

/// Mechanical checks of the agent's root-cause report.
///
/// Returns `{detection_correct, root_cause_supported, detail}`; `reference_files`
/// are the files the fix should touch (the upstream fix's, or the patch's own).
pub fn check_report(report: Option<&Value>, baseline: &Path, reference_files: &[String]) -> Map<String, Value> {
    let mut out = Map::new();
    let Some(report) = report.filter(|r| r.as_object().is_some_and(|o| !o.is_empty())) else {
        out.insert("detection_correct".into(), Value::Null);
        out.insert("root_cause_supported".into(), Value::Null);
        out.insert("detail".into(), "no report submitted".into());
        return out;
    };
    let named: Vec<String> = report
        .get("affected_files")
        .and_then(Value::as_array)
        .map(|files| {
            files
                .iter()
                .filter_map(|f| {
                    let s = match f {
                        Value::String(s) => s.clone(),
                        other => other.to_string(),
                    };
                    let s = s.trim().trim_start_matches(['.', '/']).to_owned();
                    patchlib::safe_relpath(&s)
                })
                .collect()
        })
        .unwrap_or_default();
    let exists = named.iter().filter(|f| baseline.join(f).is_file()).count();
    let detection = named.iter().any(|f| reference_files.contains(f));
    let lines = match report.get("vulnerable_lines") {
        Some(Value::String(s)) => s.clone(),
        Some(other) => other.to_string(),
        None => String::new(),
    };
    let ranges: Vec<_> = LINE_RANGE.captures_iter(&lines).collect();
    let mut ranges_ok = true;
    for c in &ranges {
        let p = baseline.join(&c[1]);
        let n = if p.is_file() {
            String::from_utf8_lossy(&std::fs::read(&p).unwrap_or_default()).lines().count()
        } else {
            0
        };
        let s: usize = c[2].parse().unwrap_or(0);
        let e: Option<usize> = c.get(3).and_then(|m| m.as_str().parse().ok());
        if !(1 <= s && s <= n && e.is_none_or(|e| s <= e && e <= n)) {
            ranges_ok = false;
        }
    }
    let supported = !named.is_empty() && exists == named.len() && !ranges.is_empty() && ranges_ok;
    let named_repr = format!(
        "[{}]",
        named.iter().map(|f| format!("'{f}'")).collect::<Vec<_>>().join(", ")
    );
    out.insert("detection_correct".into(), detection.into());
    out.insert("root_cause_supported".into(), supported.into());
    out.insert(
        "detail".into(),
        format!(
            "files named {named_repr}, exist {exists}/{}, line ranges {}, matches upstream fix files: {}",
            named.len(),
            if !ranges.is_empty() && ranges_ok { "valid" } else { "missing/invalid" },
            if detection { "True" } else { "False" }
        )
        .into(),
    );
    out
}

/// The score for `dims` (check name → true/false/null) and the verdict.
pub fn score(dims: &Map<String, Value>, final_status: &str) -> f64 {
    if NULLIFIERS.iter().any(|n| dims.get(*n) == Some(&Value::Bool(true))) {
        return 0.0;
    }
    if final_status == "PATCH_VERIFIED" {
        return f64::from(MAX_SCORE);
    }
    WEIGHTS
        .iter()
        .filter(|(d, _)| dims.get(*d) == Some(&Value::Bool(true)))
        .map(|(_, w)| f64::from(*w))
        .sum()
}

/// `✔`, `✘` or `–` for a check value.
pub fn mark(v: &Value) -> String {
    match v {
        Value::Bool(true) => "✔".into(),
        Value::Bool(false) => "✘".into(),
        Value::Null => "–".into(),
        other => other.to_string(),
    }
}

/// An all-null set of the grader's own checks.
pub fn empty_grader_dims() -> Map<String, Value> {
    let mut m = Map::new();
    for k in [
        "patch_format_valid", "baseline_reproduced", "candidate_build_passed",
        "regression_test_passed", "existing_tests_passed", "security_checks_passed",
        "validation_reproducible", "false_positive", "infrastructure_failure",
    ] {
        m.insert(k.into(), Value::Null);
    }
    m
}

/// A grade for a failure of the grading machinery itself.
pub fn infrastructure_failure(detail: impl Into<String>) -> Value {
    json!({
        "final_status": "FAILED",
        "dimensions": {"infrastructure_failure": true},
        "steps": [{"step": "G verdict", "status": "error", "detail": detail.into()}],
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dims(pairs: &[(&str, Value)]) -> Map<String, Value> {
        pairs.iter().map(|(k, v)| ((*k).to_owned(), v.clone())).collect()
    }

    #[test]
    fn weights_sum_to_max() {
        assert_eq!(WEIGHTS.iter().map(|(_, w)| w).sum::<u32>(), MAX_SCORE);
    }

    #[test]
    fn score_rules() {
        assert_eq!(score(&dims(&[]), "PATCH_VERIFIED"), 12.0);
        assert_eq!(score(&dims(&[("false_positive", true.into())]), "PATCH_VERIFIED"), 0.0);
        let d = dims(&[("regression_test_passed", true.into()), ("baseline_reproduced", true.into())]);
        assert_eq!(score(&d, "REPRODUCIBLE"), 4.0);
    }

    #[test]
    fn report_check_validates_files_and_ranges() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("src")).unwrap();
        std::fs::write(dir.path().join("src/a.js"), "1\n2\n3\n").unwrap();
        let report = json!({"affected_files": ["./src/a.js"], "vulnerable_lines": "src/a.js:1-3"});
        let rc = check_report(Some(&report), dir.path(), &["src/a.js".into()]);
        assert_eq!(rc["detection_correct"], true);
        assert_eq!(rc["root_cause_supported"], true);
        assert_eq!(
            rc["detail"],
            "files named ['src/a.js'], exist 1/1, line ranges valid, matches upstream fix files: True"
        );
        let bad = json!({"affected_files": ["src/a.js"], "vulnerable_lines": "src/a.js:2-9"});
        assert_eq!(check_report(Some(&bad), dir.path(), &[])["root_cause_supported"], false);
        assert_eq!(check_report(None, dir.path(), &[])["detail"], "no report submitted");
    }
}
