//! Standalone Surface with the sandbox off: provider keys and Runtime tokens
//! are not in the environment of the `bash` tool or `!shell`.
//!
//! Each scenario runs in a child copy of this test binary with its own
//! environment, so no test changes the environment of this process.

use loop_app_core::{bootstrap, BootstrapOpts, Runtime};
use serde_json::json;

const SCENARIO: &str = "LOOP_TOOLENV_SCENARIO";

/// Run `scenario` in a child process. `agent` lays out the agent directory.
fn run_child(scenario: &str, agent: impl FnOnce(&std::path::Path, &std::path::Path)) {
    let dir = tempfile::tempdir().unwrap();
    let agent_dir = dir.path().join("agent");
    let project = dir.path().join("project");
    std::fs::create_dir_all(&agent_dir).unwrap();
    std::fs::create_dir_all(&project).unwrap();
    agent(&agent_dir, &project);
    let status = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "child", "--nocapture"])
        .env(SCENARIO, scenario)
        .env("LOOP_CODING_AGENT_DIR", &agent_dir)
        .env("LOOP_CODING_AGENT_SESSION_DIR", dir.path().join("sessions"))
        .env("OPENAI_API_KEY", "canary-openai")
        .env("ANTHROPIC_API_KEY", "canary-anthropic")
        .env("SOKET_API_KEY", "canary-soket")
        .env("GOOGLE_API_KEY", "canary-google")
        .env("LOOP_RUNTIME_TOKEN", "canary-token")
        .env("MY_CUSTOM_KEY", "canary-custom")
        .env("GITHUB_TOKEN", "canary-github")
        .env("NPM_TOKEN", "canary-npm")
        .env("AWS_SECRET_ACCESS_KEY", "canary-aws")
        .current_dir(&project)
        .status()
        .unwrap();
    assert!(status.success(), "scenario {scenario} failed");
}

fn custom_provider(agent: &std::path::Path) {
    std::fs::write(
        agent.join("models.json"),
        json!({"providers": [{
            "id": "mine", "baseUrl": "http://127.0.0.1:9/v1",
            "apiKeyEnv": ["MY_CUSTOM_KEY"], "models": ["m"]
        }]})
        .to_string(),
    )
    .unwrap();
}

#[test]
fn default_removes_provider_keys_and_keeps_other_secrets() {
    run_child("default", |agent, _| custom_provider(agent));
}

#[test]
fn global_pass_env_keeps_the_named_variable() {
    run_child("pass", |agent, _| {
        custom_provider(agent);
        std::fs::write(
            agent.join("settings.json"),
            json!({"tools": {"passEnv": ["OPENAI_API_KEY", "MY_CUSTOM_KEY"]}}).to_string(),
        )
        .unwrap();
    });
}

#[test]
fn project_pass_env_is_ignored_with_a_notice() {
    run_child("project", |agent, project| {
        custom_provider(agent);
        std::fs::write(
            agent.join("settings.json"),
            json!({"defaultProjectTrust": "always"}).to_string(),
        )
        .unwrap();
        std::fs::create_dir_all(project.join(".loop")).unwrap();
        std::fs::write(
            project.join(".loop/settings.json"),
            json!({"tools": {"passEnv": ["OPENAI_API_KEY"]}}).to_string(),
        )
        .unwrap();
    });
}

#[test]
fn a_failed_command_names_a_removed_variable_once_per_turn() {
    run_child("note", |agent, _| custom_provider(agent));
}

async fn start() -> Runtime {
    bootstrap(BootstrapOpts {
        cwd: std::env::current_dir().unwrap(),
        provider: None,
        model: None,
        theme: None,
        system_prompt: None,
        append_system_prompt: None,
        no_context_files: true,
        interactive: false,
        session_id: None,
        no_web: true,
        permission_mode: Some("full-access".into()),
        agent: Some("external".into()),
    })
    .await
    .unwrap()
}

async fn bash(runtime: &Runtime, command: &str) -> String {
    let tools = runtime.harness.get_tools().await;
    let tool = tools.iter().find(|tool| tool.name == "bash").unwrap();
    let result = (tool.execute)("c".into(), json!({ "command": command }), None, None)
        .await
        .unwrap();
    serde_json::to_string(&result.content).unwrap()
}

async fn shell(runtime: &Runtime, command: &str) -> String {
    let env = runtime.harness.tool_env().await.unwrap();
    let out = env.exec(command, Default::default()).await.unwrap();
    out.stdout
}

const ALL: [&str; 9] = [
    "OPENAI_API_KEY",
    "ANTHROPIC_API_KEY",
    "SOKET_API_KEY",
    "GOOGLE_API_KEY",
    "LOOP_RUNTIME_TOKEN",
    "MY_CUSTOM_KEY",
    "GITHUB_TOKEN",
    "NPM_TOKEN",
    "AWS_SECRET_ACCESS_KEY",
];

/// The variables from `ALL` that the command sees.
fn seen(text: &str) -> Vec<&'static str> {
    ALL.into_iter()
        .filter(|name| {
            text.contains(&format!("{name}=canary")) || text.contains(&format!("{name}:canary"))
        })
        .collect()
}

#[tokio::test]
async fn child() {
    let Ok(scenario) = std::env::var(SCENARIO) else {
        return;
    };
    let runtime = start().await;
    assert_eq!(runtime.settings.sandbox.mode, "off");
    let print_all = ALL
        .map(|name| format!("echo {name}:${{{name}}}"))
        .join("; ");
    let kept = ["GITHUB_TOKEN", "NPM_TOKEN", "AWS_SECRET_ACCESS_KEY"];
    match scenario.as_str() {
        "default" => {
            for text in [
                bash(&runtime, &print_all).await,
                shell(&runtime, &print_all).await,
            ] {
                assert_eq!(seen(&text), kept, "{text}");
            }
            let env_dump = bash(&runtime, "env").await;
            assert_eq!(seen(&env_dump), kept, "{env_dump}");
        }
        "pass" => {
            let expected = [
                "OPENAI_API_KEY",
                "MY_CUSTOM_KEY",
                "GITHUB_TOKEN",
                "NPM_TOKEN",
                "AWS_SECRET_ACCESS_KEY",
            ];
            for text in [
                bash(&runtime, &print_all).await,
                shell(&runtime, &print_all).await,
            ] {
                assert_eq!(seen(&text), expected, "{text}");
            }
        }
        "project" => {
            assert!(
                runtime
                    .startup_notices
                    .iter()
                    .any(|n| n.contains("tools.passEnv") && n.contains("global")),
                "{:?}",
                runtime.startup_notices
            );
            for text in [
                bash(&runtime, &print_all).await,
                shell(&runtime, &print_all).await,
            ] {
                assert_eq!(seen(&text), kept, "{text}");
            }
        }
        "note" => {
            let fail = "echo 'OPENAI_API_KEY is not set' >&2; echo MY_CUSTOM_KEY; exit 1";
            let first = bash(&runtime, fail).await;
            assert_eq!(first.matches("Note: ").count(), 1, "{first}");
            assert!(
                first.contains("MY_CUSTOM_KEY, OPENAI_API_KEY were removed"),
                "{first}"
            );
            assert!(first.contains("tools.passEnv"), "{first}");
            // Same turn: no second note for the same names.
            let second = bash(&runtime, fail).await;
            assert!(!second.contains("Note: "), "{second}");
            // A variable that was not removed, or a success, gets no note.
            let other = bash(&runtime, "echo GITHUB_TOKEN; exit 1").await;
            assert!(!other.contains("Note: "), "{other}");
            let ok = bash(&runtime, "echo LOOP_RUNTIME_TOKEN").await;
            assert!(!ok.contains("Note: "), "{ok}");
            // The next turn reports again, and only the newly mentioned name.
            runtime.harness.tool_env_policy().begin_turn();
            let next = bash(&runtime, "echo 'LOOP_RUNTIME_TOKEN missing' >&2; exit 2").await;
            assert!(next.contains("Note: LOOP_RUNTIME_TOKEN was removed from the command environment by Loop; allow it with tools.passEnv in global settings."), "{next}");
            let again = bash(&runtime, "echo 'OPENAI_API_KEY' >&2; exit 2").await;
            assert!(
                again.contains("Note: OPENAI_API_KEY was removed"),
                "{again}"
            );
        }
        other => panic!("unknown scenario {other}"),
    }
}
