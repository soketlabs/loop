//! The conformance suite against the macOS Seatbelt driver. It needs only
//! `/usr/bin/sandbox-exec`, which every macOS has; elsewhere it is ignored.

#![cfg(unix)]

mod common;

use loop_agent::harness::sandbox::{seatbelt_driver, WorkspaceSource, WorkspaceSpec};
use loop_workspace_testkit::{run_all, DriverFixture};
use serde_json::json;

fn fixture() -> DriverFixture {
    let layout = common::layout();
    let home = layout.root.path().join("home");
    let agent = layout.root.path().join("agent");
    std::fs::create_dir(&home).expect("home");
    std::fs::create_dir(&agent).expect("agent");
    DriverFixture::new(
        seatbelt_driver(),
        WorkspaceSpec {
            source: WorkspaceSource::HostFolder(layout.project.clone()),
            options: json!({
                "homeDir": home,
                "agentDir": agent,
                "network": false,
                "runtimePaths": [layout.root.path().join("runtime")],
            }),
            ..Default::default()
        },
    )
    .with_outside(layout.outside.clone())
    .with_guard(layout.root)
}

#[tokio::test]
#[cfg_attr(not(target_os = "macos"), ignore = "Seatbelt exists only on macOS")]
async fn seatbelt_driver_conforms() {
    run_all(|| async { fixture() }).await;
}
