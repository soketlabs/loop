//! Sandbox trait + LocalShellSandbox tests.

use std::path::Path;
use std::sync::Arc;

use loop_agent::harness::{
    create_read_tool, create_write_tool, LocalShellSandbox, LocalShellSandboxFactory, Sandbox,
    SandboxConfig, SandboxMode, SandboxRegistry, SandboxStatus,
};
use serde_json::json;

#[tokio::test]
async fn local_shell_sandbox_isolates_workdir() {
    let sb = LocalShellSandbox::new(SandboxConfig {
        workdir: Default::default(),
        options: json!({"temp": true}),
        labels: Default::default(),
    });
    sb.start().await.unwrap();
    assert_eq!(sb.status(), SandboxStatus::Ready);
    let env = sb.env();
    env.write_file(Path::new("hello.txt"), b"hi")
        .await
        .unwrap();
    let text = env.read_text_file(Path::new("hello.txt")).await.unwrap();
    assert_eq!(text, "hi");

    // Path escape rejected
    let err = env.read_text_file(Path::new("../outside.txt")).await;
    assert!(err.is_err());

    let tool = create_write_tool(Arc::clone(&env));
    let result = (tool.execute)(
        "1".into(),
        json!({"path": "a.txt", "content": "x"}),
        None,
        None,
    )
    .await
    .unwrap();
    assert!(!result.content.is_empty());

    let read = create_read_tool(env);
    let result = (read.execute)("2".into(), json!({"path": "a.txt"}), None, None)
        .await
        .unwrap();
    assert!(matches!(
        &result.content[0],
        loop_ai::ToolResultContent::Text(t) if t.text == "x"
    ));

    sb.destroy().await.unwrap();
}

#[tokio::test]
async fn registry_creates_by_kind() {
    let reg = SandboxRegistry::new();
    reg.register(Arc::new(LocalShellSandboxFactory));
    assert!(reg.kinds().contains(&"local-shell".to_string()));
    let sb = reg
        .create(
            "local-shell",
            SandboxConfig {
                workdir: Default::default(),
                options: json!({"temp": true}),
                labels: Default::default(),
            },
        )
        .await
        .unwrap();
    sb.start().await.unwrap();
    assert_eq!(sb.kind(), "local-shell");
    sb.destroy().await.unwrap();
}

#[tokio::test]
async fn sandbox_mode_enum() {
    let _ = SandboxMode::Disabled;
}
