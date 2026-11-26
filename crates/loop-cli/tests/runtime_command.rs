//! `loop runtime start|stop|status` end to end: spawn the built binary with an isolated agent dir.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;
use tokio::process::{Child, Command};

fn serve(agent: &Path, args: &[&str]) -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_loop"));
    cmd.args(["runtime", "start"])
        .args(args)
        .env("LOOP_CODING_AGENT_DIR", agent)
        .env("LOOP_CODING_AGENT_SESSION_DIR", agent.join("sessions"))
        .env_remove("LOOP_DEBUG")
        .env_remove("RUST_LOG")
        .env_remove("LOOP_TOOL_LOCK")
        .env_remove("LOOP_TOOL_LOCK_ROOT")
        .current_dir(agent)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    cmd
}

/// Wait until the Runtime accepts connections on `path`. The socket file
/// appears when it is bound, a moment before the Runtime listens.
async fn wait_for(path: &Path, child: &mut Child) {
    let wait = async {
        while tokio::net::UnixStream::connect(path).await.is_err() {
            if let Some(status) = child.try_wait().unwrap() {
                panic!("loop runtime start exited early: {status}");
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    };
    tokio::time::timeout(Duration::from_secs(30), wait)
        .await
        .expect("socket never appeared");
}

fn signal(child: &Child, name: &str) {
    assert!(std::process::Command::new("kill")
        .args([name, &child.id().unwrap().to_string()])
        .status()
        .unwrap()
        .success());
}

fn agent_dir() -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::Builder::new()
        .prefix("lsv")
        .tempdir_in("/tmp")
        .unwrap();
    let agent = dir.path().join("agent");
    std::fs::create_dir_all(&agent).unwrap();
    (dir, agent)
}

#[tokio::test]
async fn websocket_refuses_non_loopback_before_starting() {
    tokio::time::timeout(Duration::from_secs(30), async {
        for address in ["0.0.0.0:0", "[::]:0", "192.0.2.1:0"] {
            let (_dir, agent) = agent_dir();
            let output = serve(&agent, &["--listen", address])
                .output()
                .await
                .unwrap();
            assert!(!output.status.success());
            let error = String::from_utf8_lossy(&output.stderr);
            assert!(
                error.contains("non-loopback") && error.contains("TLS"),
                "{error}"
            );
            assert!(!agent.join("run/runtime.pid").exists());
        }
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn websocket_publishes_only_its_url_and_keeps_a_private_fresh_token() {
    use std::os::unix::fs::PermissionsExt;
    tokio::time::timeout(Duration::from_secs(60), async {
        let (_dir, agent) = agent_dir();
        let socket = agent.join("run/runtime.sock");
        let token_file = agent.join("run/runtime.token");
        let mut previous = String::new();
        for _ in 0..2 {
            let mut child = serve(&agent, &["--listen", "127.0.0.1:0"]).spawn().unwrap();
            wait_for(&socket, &mut child).await;
            let token = std::fs::read_to_string(&token_file).expect("WebSocket token file");
            assert!(token.trim().len() >= 43);
            assert_ne!(token, previous, "each start generates a fresh token");
            for (path, mode) in [(&token_file, 0o600), (&agent.join("run"), 0o700)] {
                assert_eq!(
                    std::fs::metadata(path).unwrap().permissions().mode() & 0o777,
                    mode
                );
            }
            let output = Command::new(env!("CARGO_BIN_EXE_loop"))
                .args([
                    "runtime",
                    "status",
                    "--json",
                    "--socket",
                    socket.to_str().unwrap(),
                ])
                .output()
                .await
                .unwrap();
            assert!(output.status.success());
            let text = String::from_utf8(output.stdout).unwrap();
            assert!(!text.contains(token.trim()), "status leaks the token");
            let status: serde_json::Value = serde_json::from_str(&text).unwrap();
            let url = status["listen_url"].as_str().expect("listen URL");
            assert!(url.starts_with("ws://127.0.0.1:"), "{url}");
            let transport = loop_wire::socket::websocket_connect(url, token.trim())
                .await
                .unwrap();
            drop(transport);
            let remote = loop_cli::runtime_launch::connect(
                &loop_cli::runtime_launch::Target::WebSocket(url.into()),
                Some(&token_file),
                std::sync::Arc::new(loop_cli::surface_client::RemoteHost::default()),
            )
            .await
            .unwrap();
            assert!(remote.is_websocket());
            assert!(!remote.status().await.unwrap().auto_started);
            let mut env_token = Command::new(env!("CARGO_BIN_EXE_loop"));
            env_token
                .args(["session", "list", "--url", url, "--json"])
                .env("LOOP_RUNTIME_TOKEN", token.trim());
            let output = env_token.output().await.unwrap();
            assert!(output.status.success(), "{output:?}");
            let output = Command::new(env!("CARGO_BIN_EXE_loop"))
                .args(["session", "list", "--url", url, "--json"])
                .env_remove("LOOP_RUNTIME_TOKEN")
                .output()
                .await
                .unwrap();
            assert!(!output.status.success());
            assert!(String::from_utf8_lossy(&output.stderr)
                .contains("require LOOP_RUNTIME_TOKEN or --token-file"));
            assert_eq!(remote.server().version, env!("CARGO_PKG_VERSION"));
            assert_eq!(remote.status().await.unwrap().pid, child.id().unwrap());
            drop(remote);
            signal(&child, "-TERM");
            assert!(child.wait().await.unwrap().success());
            assert!(!token_file.exists(), "remove the token on clean shutdown");
            previous = token;
        }
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn serve_starts_without_stdin_then_cleans_up_on_sigterm() {
    let (_dir, agent) = agent_dir();
    let socket = agent.join("run/runtime.sock");
    let mut child = serve(&agent, &[]).spawn().unwrap();
    wait_for(&socket, &mut child).await;
    assert!(agent.join("run/runtime.pid").exists());
    assert!(agent.join("events.db").exists());
    signal(&child, "-TERM");
    let status = tokio::time::timeout(Duration::from_secs(30), child.wait())
        .await
        .unwrap()
        .unwrap();
    assert!(status.success(), "{status}");
    assert!(!socket.exists(), "socket removed");
    assert!(!agent.join("run/runtime.pid").exists(), "pid file removed");
    let log = std::fs::read_to_string(agent.join("logs/runtime.log")).unwrap();
    assert!(log.contains("Runtime logging enabled"));
}

#[tokio::test]
async fn second_serve_with_same_agent_dir_refuses() {
    let (_dir, agent) = agent_dir();
    let socket = agent.join("run/runtime.sock");
    let mut first = serve(&agent, &[]).spawn().unwrap();
    wait_for(&socket, &mut first).await;
    let output = tokio::time::timeout(Duration::from_secs(30), serve(&agent, &[]).output())
        .await
        .unwrap()
        .unwrap();
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("already running"), "{stderr}");
    // The first Runtime is untouched.
    assert!(socket.exists());
    signal(&first, "-INT");
    let status = tokio::time::timeout(Duration::from_secs(30), first.wait())
        .await
        .unwrap()
        .unwrap();
    assert!(status.success(), "{status}");
}

#[tokio::test]
async fn explicit_socket_and_invalid_permission_mode() {
    let (_dir, agent) = agent_dir();
    let output = serve(&agent, &["--permission-mode", "yolo"])
        .output()
        .await
        .unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("permission-mode"));

    let socket = agent.join("custom.sock");
    let socket_arg = socket.to_str().unwrap();
    let mut child = serve(&agent, &["--socket", socket_arg]).spawn().unwrap();
    wait_for(&socket, &mut child).await;
    signal(&child, "-TERM");
    let status = tokio::time::timeout(Duration::from_secs(30), child.wait())
        .await
        .unwrap()
        .unwrap();
    assert!(status.success());
    assert!(!socket.exists());
}

#[tokio::test]
async fn a_client_connects_through_remote_backend() {
    use loop_protocol::{AgentBackend, HostServices};
    let (_dir, agent) = agent_dir();
    let socket = agent.join("run/runtime.sock");
    let mut child = serve(&agent, &[]).spawn().unwrap();
    wait_for(&socket, &mut child).await;
    let stream = tokio::net::UnixStream::connect(&socket).await.unwrap();
    let backend = loop_wire::RemoteBackend::connect(
        loop_wire::socket::unix(stream),
        loop_protocol::rpc::PeerInfo {
            name: "test".into(),
            version: "0".into(),
        },
        std::sync::Arc::new(NoHost) as std::sync::Arc<dyn HostServices>,
    )
    .await
    .unwrap();
    assert!(backend.capabilities().multi_session);
    assert_eq!(backend.server().name, "loop");
    drop(backend);
    signal(&child, "-TERM");
    let status = tokio::time::timeout(Duration::from_secs(30), child.wait())
        .await
        .unwrap()
        .unwrap();
    assert!(status.success());
}

#[tokio::test]
async fn an_unknown_tool_lock_stops_startup() {
    tokio::time::timeout(Duration::from_secs(30), async {
        let cases: [(Option<&str>, Option<&str>, bool, &str); 5] = [
            (Some("chatt"), None, true, "unknown tool lock"),
            (None, Some("CHAT"), true, "unknown tool lock"),
            (None, Some(""), true, "is empty"),
            (Some("chat"), None, false, "needs a root folder"),
            (None, None, true, "without a tool lock"),
        ];
        for (flag, env, root, want) in cases {
            let (_dir, agent) = agent_dir();
            let mut cmd = serve(&agent, &[]);
            if let Some(flag) = flag {
                cmd.args(["--tool-lock", flag]);
            }
            if let Some(env) = env {
                cmd.env("LOOP_TOOL_LOCK", env);
            }
            if root {
                cmd.env("LOOP_TOOL_LOCK_ROOT", &agent);
            }
            let output = cmd.output().await.unwrap();
            assert!(!output.status.success());
            let error = String::from_utf8_lossy(&output.stderr);
            assert!(error.contains(want), "{want}: {error}");
            assert!(!agent.join("run/runtime.pid").exists());
        }
    })
    .await
    .unwrap();
}

/// `LOOP_TOOL_LOCK=chat` locks the Runtime: `initialize` advertises it and
/// hides the shell and workflows, and `loop runtime status` shows it.
#[tokio::test]
async fn the_tool_lock_from_the_environment_is_advertised() {
    use loop_protocol::{AgentBackend, HostServices};
    tokio::time::timeout(Duration::from_secs(60), async {
        let (_dir, agent) = agent_dir();
        let socket = agent.join("run/runtime.sock");
        let mut child = serve(&agent, &[])
            .env("LOOP_TOOL_LOCK", "chat")
            .env("LOOP_TOOL_LOCK_ROOT", &agent)
            .spawn()
            .unwrap();
        wait_for(&socket, &mut child).await;
        let stream = tokio::net::UnixStream::connect(&socket).await.unwrap();
        let backend = loop_wire::RemoteBackend::connect(
            loop_wire::socket::unix(stream),
            loop_protocol::rpc::PeerInfo {
                name: "test".into(),
                version: "0".into(),
            },
            std::sync::Arc::new(NoHost) as std::sync::Arc<dyn HostServices>,
        )
        .await
        .unwrap();
        let offered = backend.capabilities();
        assert_eq!(offered.tool_lock, Some(loop_protocol::ToolLock::Chat));
        assert!(!offered.local_shell && !offered.workflow);
        drop(backend);
        let output = run(&agent, &["runtime", "status"]).await;
        let text = String::from_utf8_lossy(&output.stdout);
        assert!(text.contains("tool lock: chat"), "{text}");
        let output = run(&agent, &["runtime", "status", "--json"]).await;
        let json: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(json["tool_lock"], "chat");
        signal(&child, "-TERM");
        child.wait().await.unwrap();
    })
    .await
    .expect("test timed out");
}

struct NoHost;
#[async_trait::async_trait]
impl loop_protocol::HostServices for NoHost {
    async fn interact(
        &self,
        _: loop_protocol::InteractionRequest,
    ) -> loop_protocol::InteractionOutcome {
        loop_protocol::InteractionOutcome::Rejected {
            reason: "test host".into(),
        }
    }
}

#[tokio::test]
async fn shared_session_database_is_owned_by_one_runtime() {
    let (_dir, first_agent) = agent_dir();
    let second_agent = first_agent.parent().unwrap().join("agent2");
    std::fs::create_dir_all(&second_agent).unwrap();
    let shared = first_agent.join("shared-sessions");
    let socket = first_agent.join("run/runtime.sock");
    let mut first = serve(&first_agent, &[])
        .env("LOOP_CODING_AGENT_SESSION_DIR", &shared)
        .spawn()
        .unwrap();
    wait_for(&socket, &mut first).await;
    let output = tokio::time::timeout(
        Duration::from_secs(30),
        serve(&second_agent, &[])
            .env("LOOP_CODING_AGENT_SESSION_DIR", &shared)
            .output(),
    )
    .await
    .unwrap()
    .unwrap();
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("Session database"), "{stderr}");
    assert!(socket.exists(), "first Runtime untouched");
    signal(&first, "-TERM");
    first.wait().await.unwrap();
}

fn loop_cmd(agent: &Path, args: &[&str]) -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_loop"));
    cmd.args(args)
        .env("LOOP_CODING_AGENT_DIR", agent)
        .env("LOOP_CODING_AGENT_SESSION_DIR", agent.join("sessions"))
        .env_remove("LOOP_DEBUG")
        .env_remove("RUST_LOG")
        .env_remove("LOOP_TOOL_LOCK")
        .env_remove("LOOP_TOOL_LOCK_ROOT")
        .current_dir(agent)
        .stdin(Stdio::null())
        .kill_on_drop(true);
    cmd
}

async fn run(agent: &Path, args: &[&str]) -> std::process::Output {
    tokio::time::timeout(Duration::from_secs(30), loop_cmd(agent, args).output())
        .await
        .expect("command hung")
        .unwrap()
}

#[tokio::test]
async fn status_reports_a_running_runtime_and_exits_1_without_one() {
    tokio::time::timeout(Duration::from_secs(60), async {
        let (_dir, agent) = agent_dir();
        let output = run(&agent, &["runtime", "status"]).await;
        assert_eq!(output.status.code(), Some(1), "{output:?}");
        assert!(String::from_utf8_lossy(&output.stderr).contains("No Runtime running"));
        let output = run(&agent, &["runtime", "status", "--json"]).await;
        assert_eq!(output.status.code(), Some(1));
        assert!(output.stdout.is_empty());

        let socket = agent.join("run/runtime.sock");
        let mut child = serve(&agent, &["--max-sessions", "3"]).spawn().unwrap();
        wait_for(&socket, &mut child).await;
        let output = run(&agent, &["runtime", "status", "--json"]).await;
        assert_eq!(output.status.code(), Some(0), "{output:?}");
        let status: loop_protocol::rpc::RuntimeStatus =
            serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(status.pid, child.id().unwrap());
        assert_eq!(status.socket, socket);
        assert_eq!((status.loaded, status.busy), (0, 0));
        assert_eq!((status.max_sessions, status.max_active_turns), (3, 8));
        let output = run(&agent, &["runtime", "status"]).await;
        assert_eq!(output.status.code(), Some(0));
        let text = String::from_utf8_lossy(&output.stdout);
        assert!(text.contains("0 loaded of 3"), "{text}");
        signal(&child, "-TERM");
        child.wait().await.unwrap();
    })
    .await
    .expect("test timed out");
}

#[tokio::test]
async fn stop_shuts_the_runtime_down_through_its_socket() {
    tokio::time::timeout(Duration::from_secs(60), async {
        let (_dir, agent) = agent_dir();
        let socket = agent.join("run/runtime.sock");
        let mut child = serve(&agent, &[]).spawn().unwrap();
        wait_for(&socket, &mut child).await;
        let output = run(&agent, &["runtime", "stop"]).await;
        assert!(output.status.success(), "{output:?}");
        assert!(String::from_utf8_lossy(&output.stdout).contains("stopped"));
        assert!(!socket.exists(), "socket removed");
        let status = tokio::time::timeout(Duration::from_secs(30), child.wait())
            .await
            .unwrap()
            .unwrap();
        assert!(status.success(), "{status}");
        assert!(!agent.join("run/runtime.pid").exists(), "pid file removed");
        // Stopping again finds nothing and touches nothing.
        let output = run(&agent, &["runtime", "stop"]).await;
        assert!(output.status.success());
        assert!(String::from_utf8_lossy(&output.stdout).contains("No Runtime running"));
    })
    .await
    .expect("test timed out");
}

#[tokio::test]
async fn stop_never_signals_the_pid_in_a_stale_pid_file() {
    tokio::time::timeout(Duration::from_secs(60), async {
        let (_dir, agent) = agent_dir();
        let mut bystander = Command::new("sleep")
            .arg("30")
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        std::fs::create_dir_all(agent.join("run")).unwrap();
        std::fs::write(
            agent.join("run/runtime.pid"),
            bystander.id().unwrap().to_string(),
        )
        .unwrap();
        let output = run(&agent, &["runtime", "stop"]).await;
        assert!(output.status.success(), "{output:?}");
        assert!(String::from_utf8_lossy(&output.stdout).contains("No Runtime running"));
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert!(
            bystander.try_wait().unwrap().is_none(),
            "the pid in a stale pid file was signalled"
        );
        bystander.kill().await.unwrap();
    })
    .await
    .expect("test timed out");
}

fn write_settings(agent: &Path, json: &str) {
    std::fs::write(agent.join("settings.json"), json).unwrap();
}

async fn status_of(agent: &Path) -> loop_protocol::rpc::RuntimeStatus {
    let output = run(agent, &["runtime", "status", "--json"]).await;
    assert_eq!(output.status.code(), Some(0), "{output:?}");
    serde_json::from_slice(&output.stdout).unwrap()
}

#[tokio::test]
async fn limits_come_from_flags_then_settings_then_defaults() {
    tokio::time::timeout(Duration::from_secs(90), async {
        let (_dir, agent) = agent_dir();
        let socket = agent.join("run/runtime.sock");
        write_settings(&agent, r#"{"runtime":{"maxSessions":3}}"#);
        let mut child = serve(&agent, &[]).spawn().unwrap();
        wait_for(&socket, &mut child).await;
        let status = status_of(&agent).await;
        assert_eq!((status.max_sessions, status.max_active_turns), (3, 8));
        assert_eq!(status.max_sessions_from.as_deref(), Some("settings"));
        assert_eq!(status.max_active_turns_from.as_deref(), Some("default"));
        let text = run(&agent, &["runtime", "status"]).await;
        let text = String::from_utf8_lossy(&text.stdout);
        assert!(text.contains("0 loaded of 3 (settings)"), "{text}");
        assert!(text.contains("8 turns at once (default)"), "{text}");
        signal(&child, "-TERM");
        child.wait().await.unwrap();

        // A flag wins over settings.
        write_settings(
            &agent,
            r#"{"runtime":{"maxSessions":3,"maxActiveTurns":4}}"#,
        );
        let mut child = serve(&agent, &["--max-sessions", "2"]).spawn().unwrap();
        wait_for(&socket, &mut child).await;
        let status = status_of(&agent).await;
        assert_eq!((status.max_sessions, status.max_active_turns), (2, 4));
        assert_eq!(status.max_sessions_from.as_deref(), Some("flag"));
        assert_eq!(status.max_active_turns_from.as_deref(), Some("settings"));
        signal(&child, "-TERM");
        child.wait().await.unwrap();
    })
    .await
    .expect("test timed out");
}

#[tokio::test]
async fn an_invalid_limit_in_settings_stops_startup_and_names_its_key() {
    tokio::time::timeout(Duration::from_secs(60), async {
        let (_dir, agent) = agent_dir();
        for (json, key) in [
            (r#"{"runtime":{"maxSessions":0}}"#, "runtime.maxSessions"),
            (
                r#"{"runtime":{"maxActiveTurns":null}}"#,
                "runtime.maxActiveTurns",
            ),
            (
                r#"{"runtime":{"maxActiveTurns":-1}}"#,
                "runtime.maxActiveTurns",
            ),
        ] {
            write_settings(&agent, json);
            let output = tokio::time::timeout(Duration::from_secs(30), serve(&agent, &[]).output())
                .await
                .unwrap()
                .unwrap();
            assert!(!output.status.success(), "{json}");
            let stderr = String::from_utf8_lossy(&output.stderr);
            assert!(stderr.contains(key), "{json}: {stderr}");
            assert!(!agent.join("run/runtime.sock").exists());
        }
    })
    .await
    .expect("test timed out");
}

/// Settings with two custom models, `alpha` the saved default, served by a
/// local fake `/v1/models` so an unknown model fails with a plain answer.
fn model_settings(base_url: &str) -> String {
    format!(
        r#"{{
  "defaultProvider": "fake",
  "defaultModel": "alpha",
  "providers": [{{
    "id": "fake", "name": "Fake", "baseUrl": "{base_url}/v1",
    "models": [{{"id": "alpha"}}, {{"id": "beta"}}]
  }}]
}}"#
    )
}

fn models_server() -> loop_test_support::FakeHttpServer {
    loop_test_support::FakeHttpServer::start(|request| match request.path.as_str() {
        "/v1/models" => {
            loop_test_support::FakeResponse::json(r#"{"data":[{"id":"alpha"},{"id":"beta"}]}"#)
        }
        _ => loop_test_support::FakeResponse::new(404, "text/plain", "nope"),
    })
}

/// The model of the newest Session, read over the socket.
async fn newest_model(agent: &Path) -> Option<String> {
    use loop_protocol::ModelControl;
    let remote = loop_cli::runtime_launch::connect(
        &loop_cli::runtime_launch::Target::Unix(agent.join("run/runtime.sock")),
        None,
        std::sync::Arc::new(loop_cli::surface_client::RemoteHost::default()),
    )
    .await
    .unwrap();
    let mut sessions = remote.list().await.unwrap();
    sessions.sort_by_key(|s| s.created_ms);
    let newest = sessions.pop().unwrap();
    let choices = remote.models(&newest.r#ref).await.unwrap();
    remote.disconnect();
    choices.current
}

async fn new_session(agent: &Path, extra: &[&str]) -> std::process::Output {
    let mut args = vec!["session", "new"];
    args.extend_from_slice(extra);
    run(agent, &args).await
}

#[tokio::test]
async fn runtime_flags_set_the_default_and_session_flags_override_it() {
    tokio::time::timeout(Duration::from_secs(120), async {
        let (_dir, agent) = agent_dir();
        let socket = agent.join("run/runtime.sock");
        let server = models_server();
        write_settings(&agent, &model_settings(server.base_url()));

        // No flags: the saved default.
        let mut child = serve(&agent, &[]).spawn().unwrap();
        wait_for(&socket, &mut child).await;
        let output = new_session(&agent, &[]).await;
        assert!(output.status.success(), "{output:?}");
        assert_eq!(newest_model(&agent).await.as_deref(), Some("fake/alpha"));
        signal(&child, "-TERM");
        child.wait().await.unwrap();

        // Runtime flags replace the saved default for its new Sessions.
        let mut child = serve(&agent, &["--provider", "fake", "--model", "beta"])
            .spawn()
            .unwrap();
        wait_for(&socket, &mut child).await;
        let output = new_session(&agent, &[]).await;
        assert!(output.status.success(), "{output:?}");
        assert_eq!(newest_model(&agent).await.as_deref(), Some("fake/beta"));

        // A Session's own flags win over the Runtime's; a bare `--model`
        // resolves like it does in the terminal UI.
        let output = new_session(&agent, &["--provider", "fake", "--model", "alpha"]).await;
        assert!(output.status.success(), "{output:?}");
        assert_eq!(newest_model(&agent).await.as_deref(), Some("fake/alpha"));
        let output = new_session(&agent, &["--model", "fake/alpha"]).await;
        assert!(output.status.success(), "{output:?}");
        assert_eq!(newest_model(&agent).await.as_deref(), Some("fake/alpha"));

        // An unknown choice is an error, not a fallback, and creates nothing.
        let before = run(&agent, &["session", "list", "--json"]).await.stdout;
        for extra in [
            &["--provider", "fake", "--model", "nope"][..],
            &["--model", "nope"][..],
            &["--provider", "fake"][..],
        ] {
            let output = new_session(&agent, extra).await;
            assert!(!output.status.success(), "{extra:?}: {output:?}");
            let error = String::from_utf8_lossy(&output.stderr);
            assert!(
                error.contains("unknown model") || error.contains("needs --model"),
                "{extra:?}: {error}"
            );
        }
        assert_eq!(
            run(&agent, &["session", "list", "--json"]).await.stdout,
            before
        );
        signal(&child, "-TERM");
        child.wait().await.unwrap();
    })
    .await
    .expect("test timed out");
}

#[tokio::test]
async fn runtime_start_refuses_an_unknown_provider_or_model() {
    tokio::time::timeout(Duration::from_secs(60), async {
        let (_dir, agent) = agent_dir();
        let server = models_server();
        write_settings(&agent, &model_settings(server.base_url()));
        for (args, wanted) in [
            (&["--model", "nope"][..], "unknown model"),
            (
                &["--provider", "nowhere", "--model", "alpha"][..],
                "unknown model",
            ),
            (&["--provider", "fake"][..], "needs --model"),
            (
                &["--provider", "chatgpt", "--model", "gpt"][..],
                "subscription",
            ),
        ] {
            let output = serve(&agent, args).output().await.unwrap();
            assert!(!output.status.success(), "{args:?}");
            let error = String::from_utf8_lossy(&output.stderr);
            assert!(error.contains(wanted), "{args:?}: {error}");
            assert!(!agent.join("run/runtime.sock").exists(), "{args:?}");
        }
    })
    .await
    .expect("test timed out");
}
