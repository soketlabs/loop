//! A file review reads its snapshot back, and removes it, through the
//! Session env (task C2b). The write tool writes the snapshot through the
//! env too, so under the Seatbelt env (the macOS default) both steps must
//! work in the jailed file system.
#![cfg(unix)]

use std::sync::Arc;

use loop_agent::harness::sandbox::{seatbelt_driver, WorkspaceSource, WorkspaceSpec};
use loop_agent::harness::tools::create_write_tool;
use serde_json::json;

#[tokio::test]
#[cfg_attr(not(target_os = "macos"), ignore = "Seatbelt exists only on macOS")]
async fn the_snapshot_is_read_and_removed_through_the_seatbelt_env() {
    // Under the current folder: Seatbelt lets commands write the temp dir.
    let root = tempfile::Builder::new()
        .prefix(".review-snapshot-")
        .tempdir_in(std::env::current_dir().unwrap())
        .unwrap();
    let project = root.path().join("project");
    let home = root.path().join("home");
    let agent = root.path().join("agent");
    for dir in [&project, &home, &agent] {
        std::fs::create_dir(dir).unwrap();
    }
    std::fs::write(project.join("a.txt"), "before").unwrap();
    let workspace = seatbelt_driver()
        .create(WorkspaceSpec {
            source: WorkspaceSource::HostFolder(project.clone()),
            options: json!({ "homeDir": home, "agentDir": agent, "network": false }),
            ..Default::default()
        })
        .await
        .unwrap();
    workspace.start().await.unwrap();
    let env = workspace.env();
    let tool = create_write_tool(Arc::clone(&env));
    let details = (tool.execute)(
        "w".into(),
        json!({"path": "a.txt", "content": "after"}),
        None,
        None,
    )
    .await
    .unwrap()
    .details;
    let prev = std::path::PathBuf::from(details["previousPath"].as_str().unwrap());
    // What `revert_change` and `cleanup_snapshot` do.
    assert_eq!(env.read_binary_file(&prev).await.unwrap(), b"before");
    env.remove(&prev).await.unwrap();
    assert!(!env.exists(&prev).await.unwrap());
    workspace.destroy().await.unwrap();
}
