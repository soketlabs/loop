//! The conformance suite against the host driver: plain, and with reads
//! confined as under the `chat` tool lock.

mod common;

use std::collections::BTreeMap;
use std::sync::Arc;

use loop_agent::harness::sandbox::{HostWorkspaceDriver, WorkspaceSpec};
use loop_agent::harness::types::ShellExecOptions;
use loop_agent::harness::{SessionExecutionEnv, ToolEnvPolicy};
use loop_workspace_testkit::{run_all, DriverFixture};
use serde_json::json;

fn fixture(confine_reads: bool) -> DriverFixture {
    let layout = common::layout();
    DriverFixture::new(
        Arc::new(HostWorkspaceDriver),
        WorkspaceSpec {
            workdir: layout.project.clone(),
            options: json!({ "confineReads": confine_reads }),
            ..Default::default()
        },
    )
    .with_outside(layout.outside.clone())
    .with_guard(layout.root)
}

#[tokio::test]
async fn host_driver_conforms() {
    run_all(|| async { fixture(false) }).await;
}

#[tokio::test]
async fn host_driver_with_confined_reads_conforms() {
    run_all(|| async { fixture(true) }).await;
}

/// The tool env policy layered over a Workspace env removes provider keys
/// and keeps the names in `tools.passEnv`.
#[tokio::test]
async fn session_env_policy_applies_over_the_host_workspace() {
    let f = fixture(false);
    let ws = f.start().await;
    let caller = BTreeMap::from([
        ("OPENAI_API_KEY".to_owned(), "sk-kit".to_owned()),
        ("LOOP_RUNTIME_TOKEN".to_owned(), "tok".to_owned()),
        ("ANTHROPIC_API_KEY".to_owned(), "passed".to_owned()),
        ("KIT_KEEP".to_owned(), "kept".to_owned()),
    ]);
    let policy = ToolEnvPolicy::new(&[], &["ANTHROPIC_API_KEY".to_owned()]);
    let env = SessionExecutionEnv::for_tools(ws.env(), Some(&caller), &policy);
    let out = env
        .exec(
            "printf '%s|%s|%s|%s' \"${OPENAI_API_KEY-none}\" \"${LOOP_RUNTIME_TOKEN-none}\" \"$ANTHROPIC_API_KEY\" \"$KIT_KEEP\"",
            ShellExecOptions {
                timeout_ms: Some(10_000),
                ..Default::default()
            },
        )
        .await
        .expect("exec");
    assert_eq!(out.stdout, "none|none|passed|kept");
    ws.destroy().await.expect("destroy");
}
