//! The conformance suite against the Podman driver. It needs Podman and a
//! container image, so it runs only on Linux with
//! `LOOP_WORKSPACE_KIT_PODMAN=1`. Optional: `LOOP_WORKSPACE_KIT_RUNTIME`
//! (`runc`, `crun`, `runsc` or `krun`; default `runc`) and
//! `LOOP_WORKSPACE_KIT_ISOLATION` (`full` or `partial`; default `full`).

#![cfg(target_os = "linux")]

mod common;

use loop_agent::harness::sandbox::{podman_driver, WorkspaceSpec};
use loop_workspace_testkit::{run_all, DriverFixture};
use serde_json::json;

fn enabled() -> bool {
    std::env::var("LOOP_WORKSPACE_KIT_PODMAN").as_deref() == Ok("1")
}

fn fixture() -> DriverFixture {
    let layout = common::layout();
    let runtime = std::env::var("LOOP_WORKSPACE_KIT_RUNTIME").unwrap_or_else(|_| "runc".into());
    let isolation = std::env::var("LOOP_WORKSPACE_KIT_ISOLATION").unwrap_or_else(|_| "full".into());
    DriverFixture::new(
        podman_driver(),
        WorkspaceSpec {
            workdir: layout.project.clone(),
            options: json!({ "runtime": runtime, "isolation": isolation }),
            ..Default::default()
        },
    )
    .with_outside(layout.outside.clone())
    .with_guard(layout.root)
}

#[tokio::test]
async fn podman_driver_conforms() {
    if !enabled() {
        eprintln!("skipped: set LOOP_WORKSPACE_KIT_PODMAN=1 on Linux with Podman to run");
        return;
    }
    run_all(|| async { fixture() }).await;
}
