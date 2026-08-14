//! MCP integration tests: bridge, server tool logic, and command policy.

#![cfg(feature = "mcp")]

use std::sync::Arc;

use loop_agent::harness::mcp::bridge::parse_mcp_tool_name;
use loop_agent::harness::mcp::LoopToolProvider;
use loop_agent::harness::tools::check_command_policy;
use loop_agent::harness::{
    create_bash_tool, create_read_tool, create_write_tool, HostExecutionEnv,
};
use loop_agent::types::AgentTool;

fn make_test_tools() -> (tempfile::TempDir, Vec<AgentTool>) {
    let dir = tempfile::tempdir().unwrap();
    let env = Arc::new(HostExecutionEnv::new(dir.path()))
        as Arc<dyn loop_agent::harness::ExecutionEnv>;
    let tools = vec![
        create_read_tool(Arc::clone(&env)),
        create_write_tool(Arc::clone(&env)),
        create_bash_tool(env),
    ];
    (dir, tools)
}

// --- Bridge tests ---

#[test]
fn parse_mcp_tool_name_valid() {
    assert_eq!(
        parse_mcp_tool_name("mcp__filesystem__read_file"),
        Some(("filesystem", "read_file"))
    );
}

#[test]
fn parse_mcp_tool_name_multi_underscore() {
    assert_eq!(
        parse_mcp_tool_name("mcp__my_server__my_tool_name"),
        Some(("my_server", "my_tool_name"))
    );
}

#[test]
fn parse_mcp_tool_name_invalid() {
    assert_eq!(parse_mcp_tool_name("read"), None);
    assert_eq!(parse_mcp_tool_name("mcp__"), None);
    assert_eq!(parse_mcp_tool_name("mcp__server"), None);
}

// --- Server tool execution tests ---

#[tokio::test]
async fn provider_set_tools_and_list() {
    let (_dir, tools) = make_test_tools();
    let provider = LoopToolProvider::new(tools);

    use loop_mcp::server::ToolProvider;
    let listed = provider.list_tools();
    assert!(!listed.is_empty());

    provider.set_tools(vec![]).await;
    let listed = provider.list_tools();
    assert!(listed.is_empty());

    let (_dir2, tools2) = make_test_tools();
    provider.set_tools(tools2).await;
    let listed = provider.list_tools();
    assert!(!listed.is_empty());
}

// --- Command policy tests ---

#[test]
fn policy_blocks_rm_rf_root() {
    let result = check_command_policy("rm -rf /", &[]);
    assert!(result.is_err());
    assert!(result.unwrap_err().contains("blocked"));
}

#[test]
fn policy_blocks_rm_rf_slash_star() {
    assert!(check_command_policy("rm -rf /*", &[]).is_err());
}

#[test]
fn policy_blocks_fork_bomb() {
    assert!(check_command_policy(":(){ :|:& };:", &[]).is_err());
}

#[test]
fn policy_blocks_mkfs() {
    assert!(check_command_policy("mkfs.ext4 /dev/sda1", &[]).is_err());
}

#[test]
fn policy_blocks_dd_dev_zero() {
    assert!(check_command_policy("dd if=/dev/zero of=/dev/sda", &[]).is_err());
}

#[test]
fn policy_blocks_chmod_777_root() {
    assert!(check_command_policy("chmod -R 777 /", &[]).is_err());
}

#[test]
fn policy_allows_normal_commands() {
    assert!(check_command_policy("ls -la", &[]).is_ok());
    assert!(check_command_policy("echo hello", &[]).is_ok());
    assert!(check_command_policy("cargo build", &[]).is_ok());
    assert!(check_command_policy("git status", &[]).is_ok());
    assert!(check_command_policy("rm -rf ./target", &[]).is_ok());
}

#[test]
fn policy_custom_blocklist() {
    let extra = vec!["dangerous_script".to_string()];
    assert!(check_command_policy("dangerous_script --flag", &extra).is_err());
    assert!(check_command_policy("safe_command", &extra).is_ok());
}

#[test]
fn policy_case_insensitive() {
    assert!(check_command_policy("RM -RF /", &[]).is_err());
    assert!(check_command_policy("Mkfs.ext4 /dev/sda", &[]).is_err());
}
