//! Parity of the permission gate's path decisions (task C2b).
//!
//! A fixed tree with links, a link loop, a dangling link, a hard link, a
//! literal `~` folder, a folder nobody may read, a name that is not UTF-8,
//! a path deeper than the gate checks, and protected names. Every case
//! (rule set, mode, cwd, tool, path) gives one line: the decision, the step
//! that decided, the rule key and the refusal text, with the fixture root
//! written `$ROOT` (the matrix is in `support/gate_matrix.rs`).
//!
//! The expected lines (`tests/data/gate_parity.<os>.golden`) were recorded
//! on staging, before the gate moved onto the execution env. Set
//! `LOOP_GATE_GOLDEN_WRITE=1` to record them again. This file and the
//! matrix use only the API staging has, so they record there too.
#![cfg(unix)]

#[path = "support/gate_matrix.rs"]
mod gate_matrix;

/// The only lines that may differ from staging, and only to a stricter
/// decision: a path deeper than the gate checks ([`gate_matrix::deep`]).
const STRICTER_OK: &[&str] = &["|$DEEP"];

#[tokio::test]
async fn gate_path_decisions_match_the_recorded_ones() {
    let tree = gate_matrix::tree();
    let lines = gate_matrix::all_lines(&tree).await;
    assert!(lines.len() > 5000, "{} lines", lines.len());
    let changed = gate_matrix::check_golden(&lines, STRICTER_OK);
    eprintln!("{} lines stricter than staging:", changed.len());
    for line in &changed {
        eprintln!("{line}");
    }
}
