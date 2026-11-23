//! Shared fixture setup for the in-repository driver runs.

#![allow(dead_code)]

use std::path::PathBuf;

use loop_workspace_testkit::{OUTSIDE_SECRET, OUTSIDE_SECRET_TEXT};

/// A temporary root with `project/` (the Workspace) and `outside/` (holds
/// the secret file). The root is under the current folder, not the system
/// temporary folder, because Seatbelt lets commands write there.
pub struct Layout {
    pub root: tempfile::TempDir,
    pub project: PathBuf,
    pub outside: PathBuf,
}

pub fn layout() -> Layout {
    let root = tempfile::Builder::new()
        .prefix(".ws-kit-")
        .tempdir_in(std::env::current_dir().expect("current dir"))
        .expect("temp root");
    let project = root.path().join("project");
    let outside = root.path().join("outside");
    std::fs::create_dir(&project).expect("project");
    std::fs::create_dir(&outside).expect("outside");
    std::fs::write(outside.join(OUTSIDE_SECRET), OUTSIDE_SECRET_TEXT).expect("secret");
    Layout {
        root,
        project,
        outside,
    }
}
