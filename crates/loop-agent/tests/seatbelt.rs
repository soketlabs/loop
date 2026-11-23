#![cfg(target_os = "macos")]

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use loop_agent::harness::sandbox::seatbelt::SeatbeltSandboxFactory;
use loop_agent::harness::types::{ExecutionErrorCode, ShellExecOptions, ShellOutput};
use loop_agent::harness::{
    create_edit_tool, create_glob_tool, create_grep_tool, create_ls_tool, create_read_tool,
    create_write_tool, ExecutionEnv, Sandbox, SandboxConfig, SandboxFactory,
};
use serde_json::{json, Value};

struct Fixture {
    _root: tempfile::TempDir,
    project: PathBuf,
    home: PathBuf,
    agent: PathBuf,
    outside: PathBuf,
    sandbox: Arc<dyn Sandbox>,
}

impl Fixture {
    async fn new(network: bool) -> Self {
        // Outside-project tests must also be outside the system TMPDIR grant.
        let root = tempfile::Builder::new()
            .prefix(".seatbelt-test-")
            .tempdir_in(std::env::current_dir().unwrap())
            .unwrap();
        let project = root.path().join("project \" (allow default)");
        let home = root.path().join("home");
        let agent = root.path().join("actual-agent");
        let outside = root.path().join("outside");
        for path in [&project, &home, &agent, &outside] {
            std::fs::create_dir(path).unwrap();
        }
        let alias = root.path().join("agent-alias");
        std::os::unix::fs::symlink(&agent, &alias).unwrap();
        for name in [".ssh", ".aws", ".gnupg", ".config/gh", "Library/Keychains"] {
            std::fs::create_dir_all(home.join(name)).unwrap();
            std::fs::write(home.join(name).join("secret"), "sensitive-sentinel").unwrap();
        }
        std::fs::create_dir(agent.join("run")).unwrap();
        std::fs::write(agent.join("auth.json"), "sensitive-sentinel").unwrap();
        std::fs::create_dir(agent.join("audit")).unwrap();
        std::fs::write(agent.join("audit/decisions.jsonl"), "sensitive-sentinel").unwrap();
        std::fs::write(agent.join("run/token"), "sensitive-sentinel").unwrap();
        let sandbox = SeatbeltSandboxFactory.create(SandboxConfig {
            workdir: project.clone(),
            options: json!({"homeDir": home, "agentDir": alias, "network": network, "allowWrite": [home, alias], "runtimePaths": [root.path().join("runtime-fallback")]}),
            ..Default::default()
        }).await.unwrap();
        sandbox.start().await.unwrap();
        Self {
            _root: root,
            project,
            home,
            agent,
            outside,
            sandbox,
        }
    }
    fn env(&self) -> Arc<dyn ExecutionEnv> {
        self.sandbox.env()
    }
    async fn sh(&self, command: &str) -> ShellOutput {
        self.env()
            .exec(
                command,
                ShellExecOptions {
                    timeout_ms: Some(10_000),
                    ..Default::default()
                },
            )
            .await
            .unwrap()
    }
}

fn q(path: &Path) -> String {
    format!("'{}'", path.display().to_string().replace('\'', "'\\''"))
}

async fn call(tool: loop_agent::AgentTool, args: Value) -> Result<String, String> {
    let result = (tool.execute)("test".into(), args, None, None).await?;
    Ok(result
        .content
        .iter()
        .filter_map(|v| match v {
            loop_ai::ToolResultContent::Text(t) => Some(t.text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n"))
}

#[tokio::test]
async fn shell_and_file_tools_enforce_write_and_secret_boundaries() {
    tokio::time::timeout(Duration::from_secs(60), async {
        let f = Fixture::new(false).await;
        let outside = f.outside.join("created");
        assert_ne!(
            f.sh(&format!("echo escape > {}", q(&outside)))
                .await
                .exit_code,
            0
        );
        assert!(call(
            create_write_tool(f.env()),
            json!({"path": outside, "content": "escape"})
        )
        .await
        .is_err());
        assert!(!outside.exists());
        call(
            create_write_tool(f.env()),
            json!({"path": "nested/ok", "content": "before"}),
        )
        .await
        .unwrap();
        call(
            create_edit_tool(f.env()),
            json!({"path": "nested/ok", "oldText": "before", "newText": "after"}),
        )
        .await
        .unwrap();
        assert_eq!(
            std::fs::read_to_string(f.project.join("nested/ok")).unwrap(),
            "after"
        );
        for secret in [
            f.home.join(".ssh/secret"),
            f.home.join(".aws/secret"),
            f.home.join(".gnupg/secret"),
            f.home.join(".config/gh/secret"),
            f.home.join("Library/Keychains/secret"),
            f.agent.join("auth.json"),
            f.agent.join("audit/decisions.jsonl"),
            f.agent.join("run/token"),
        ] {
            let out = f.sh(&format!("cat {}", q(&secret))).await;
            assert_ne!(out.exit_code, 0, "{}: {out:?}", secret.display());
            assert!(!out.stdout.contains("sensitive-sentinel"));
            assert!(call(create_read_tool(f.env()), json!({"path": secret}))
                .await
                .is_err());
            assert!(call(
                create_write_tool(f.env()),
                json!({"path": secret, "content": "oops"})
            )
            .await
            .is_err());
            assert!(call(
                create_edit_tool(f.env()),
                json!({"path": secret, "oldText": "sensitive-sentinel", "newText": "oops"})
            )
            .await
            .is_err());
            assert!(call(
                create_grep_tool(f.env()),
                json!({"path": secret, "pattern": "."})
            )
            .await
            .is_err());
            assert!(call(
                create_glob_tool(f.env()),
                json!({"path": secret, "pattern": "*"})
            )
            .await
            .is_err());
            assert!(call(create_ls_tool(f.env()), json!({"path": secret}))
                .await
                .is_err());
        }
        std::os::unix::fs::symlink(&f.outside, f.project.join("escape")).unwrap();
        assert_ne!(f.sh("echo escape > escape/new").await.exit_code, 0);
        assert!(f
            .env()
            .write_file(Path::new("escape/new/deep/file"), b"escape")
            .await
            .is_err());
        std::os::unix::fs::symlink(&f.agent, f.project.join("secrets")).unwrap();
        assert_ne!(f.sh("cat secrets/auth.json").await.exit_code, 0);
        assert!(call(
            create_read_tool(f.env()),
            json!({"path": "secrets/auth.json"})
        )
        .await
        .is_err());
        for tool in [
            create_grep_tool(f.env()),
            create_glob_tool(f.env()),
            create_ls_tool(f.env()),
        ] {
            let text = call(
                tool,
                json!({"path": ".", "pattern": "sensitive-sentinel", "hidden": true}),
            )
            .await
            .unwrap();
            assert!(!text.contains("sensitive-sentinel"));
            assert!(!text.contains("auth.json"));
        }
        let link = f.home.join("linked-auth");
        let out = f
            .sh(&format!(
                "ln {} {} && cat {}",
                q(&f.agent.join("auth.json")),
                q(&link),
                q(&link)
            ))
            .await;
        assert_ne!(out.exit_code, 0, "{out:?}");
        assert!(!out.stdout.contains("sensitive-sentinel"));
        // An allowed parent must not let a command rename secrets out from
        // underneath a pathname deny rule.
        let renamed = f.home.join("moved-agent");
        let out = f
            .sh(&format!(
                "mv {} {} && cat {}/auth.json",
                q(&f.agent),
                q(&renamed),
                q(&renamed)
            ))
            .await;
        assert_ne!(out.exit_code, 0, "{out:?}");
        assert!(!out.stdout.contains("sensitive-sentinel"));
        let card = f.sandbox.info().format_box();
        for label in ["seatbelt", "partial", "off", "Writable paths"] {
            assert!(card.contains(label), "{card}");
        }
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn network_and_runtime_unix_socket_are_denied() {
    tokio::time::timeout(Duration::from_secs(60), async {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let off = Fixture::new(false).await;
        let command = format!("/usr/bin/nc -z -w 1 127.0.0.1 {port}");
        assert_ne!(off.sh(&command).await.exit_code, 0);
        let on = Fixture::new(true).await;
        assert_eq!(on.sh(&command).await.exit_code, 0);
        let socket = on.agent.join("run/runtime.sock");
        let _unix = std::os::unix::net::UnixListener::bind(
            socket
                .strip_prefix(std::env::current_dir().unwrap())
                .unwrap(),
        )
        .unwrap();
        assert_ne!(
            on.sh(&format!(
                "/usr/bin/nc -w 1 -U {}",
                q(Path::new("../actual-agent/run/runtime.sock"))
            ))
            .await
            .exit_code,
            0
        );
        // Also cover a custom socket outside run/, as used by Runtime overrides.
        let socket = on.project.join("custom.sock");
        let _custom = std::os::unix::net::UnixListener::bind(
            socket
                .strip_prefix(std::env::current_dir().unwrap())
                .unwrap(),
        )
        .unwrap();
        let host_probe = tokio::process::Command::new("/usr/bin/nc")
            .current_dir(&on.project)
            .args(["-w", "1", "-U", "custom.sock"])
            .stdin(std::process::Stdio::null())
            .output()
            .await
            .unwrap();
        assert!(host_probe.status.success(), "{host_probe:?}");
        assert_ne!(
            on.sh(&format!(
                "/usr/bin/nc -w 1 -U {}",
                q(Path::new("custom.sock"))
            ))
            .await
            .exit_code,
            0
        );
        let socket = off.project.join("custom.sock");
        let _off_socket = std::os::unix::net::UnixListener::bind(
            socket
                .strip_prefix(std::env::current_dir().unwrap())
                .unwrap(),
        )
        .unwrap();
        assert_ne!(off.sh("/usr/bin/nc -w 1 -U custom.sock").await.exit_code, 0);
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn timeout_kills_descendants_and_environment_drops_secrets() {
    tokio::time::timeout(Duration::from_secs(30), async {
        let f = Fixture::new(false).await;
        let options = ShellExecOptions {
            env: Some(
                [
                    ("OPENAI_API_KEY".into(), "sentinel".into()),
                    ("LOOP_RUNTIME_TOKEN".into(), "sentinel".into()),
                    ("PATH".into(), "/usr/bin:/bin".into()),
                ]
                .into(),
            ),
            timeout_ms: Some(10_000),
            ..Default::default()
        };
        let out = f.env().exec("env", options).await.unwrap();
        assert!(!out.stdout.contains("sentinel"));
        assert!(out.stdout.contains("PATH=/usr/bin:/bin"));
        let error = f
            .env()
            .exec(
                "sleep 60 & echo $! > child.pid; wait",
                ShellExecOptions {
                    timeout_ms: Some(500),
                    ..Default::default()
                },
            )
            .await
            .unwrap_err();
        assert_eq!(error.code, ExecutionErrorCode::TimedOut);
        let pid = std::fs::read_to_string(f.project.join("child.pid")).unwrap();
        // Wait for the outcome (reaping), rather than assert a wall-clock bound.
        loop {
            let output = tokio::process::Command::new("/bin/ps")
                .args(["-p", pid.trim(), "-o", "stat="])
                .output()
                .await
                .unwrap();
            if !output.status.success()
                || String::from_utf8_lossy(&output.stdout)
                    .trim()
                    .starts_with('Z')
            {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn git_commit_and_cargo_build_work() {
    tokio::time::timeout(Duration::from_secs(120), async {
        let f = Fixture::new(false).await;
        let init = tokio::process::Command::new("git").args(["init", "-q"]).current_dir(&f.project).output().await.unwrap();
        assert!(init.status.success());
        let out = f.sh("git -c user.name=Sandbox -c user.email=sandbox@example.invalid -c commit.gpgsign=false commit --allow-empty -qm sandbox").await;
        assert_eq!(out.exit_code, 0, "{out:?}");
        std::fs::create_dir_all(f.project.join("src")).unwrap();
        std::fs::write(f.project.join("Cargo.toml"), "[package]\nname='seatbelt-smoke'\nversion='0.1.0'\nedition='2021'\n[workspace]\n").unwrap();
        std::fs::write(f.project.join("src/main.rs"), "fn main() {}\n").unwrap();
        let home = PathBuf::from(std::env::var_os("HOME").unwrap());
        let cargo_home = std::env::var_os("CARGO_HOME").map(PathBuf::from).unwrap_or_else(|| home.join(".cargo"));
        let rustup_home = std::env::var_os("RUSTUP_HOME").map(PathBuf::from).unwrap_or_else(|| home.join(".rustup"));
        let sandbox = SeatbeltSandboxFactory.create(SandboxConfig { workdir: f.project.clone(), options: json!({"homeDir": home, "agentDir": f.agent, "allowWrite": [cargo_home, rustup_home]}), ..Default::default() }).await.unwrap();
        sandbox.start().await.unwrap();
        let out = sandbox.env().exec("cargo build --offline --target-dir target", ShellExecOptions { timeout_ms: Some(60_000), ..Default::default() }).await.unwrap();
        assert_eq!(out.exit_code, 0, "{out:?}");
        assert!(f.project.join("target/debug/seatbelt-smoke").is_file());
    }).await.unwrap();
}

#[tokio::test]
async fn configuration_credentials_and_descriptor_aliases_are_confined() {
    tokio::time::timeout(Duration::from_secs(60), async {
        let f = Fixture::new(false).await;
        for path in [
            f.project.join(".loop/settings.json"),
            f.project.join(".git/hooks/pre-commit"),
            f.project.join(".git/config"),
            f.agent.join("settings.json"),
            f.agent.join("trust.json"),
            f.agent.join("bin/evil"),
        ] {
            assert!(
                f.env().write_file(&path, b"evil").await.is_err(),
                "{}",
                path.display()
            );
            assert_ne!(
                f.sh(&format!(
                    "mkdir -p {} && echo evil > {}",
                    q(path.parent().unwrap()),
                    q(&path)
                ))
                .await
                .exit_code,
                0,
                "{}",
                path.display()
            );
        }
        for name in [
            ".netrc",
            ".docker/config.json",
            ".kube/config",
            ".npmrc",
            ".pypirc",
            ".config/gcloud/token",
            "Library/Cookies/Cookies.binarycookies",
            ".codex/auth.json",
            ".claude/.credentials.json",
            ".gemini/oauth_creds.json",
            ".git-credentials",
            ".config/git/credentials",
            ".cargo/credentials",
            ".cargo/credentials.toml",
            ".gem/credentials",
            ".terraform.d/credentials.tfrc.json",
            ".azure/accessTokens.json",
            ".config/hub",
            ".zsh_history",
            ".bash_history",
            ".local/share/fish/fish_history",
            ".python_history",
        ] {
            let path = f.home.join(name);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(&path, "private-token").unwrap();
            assert!(f.env().read_text_file(&path).await.is_err(), "{name}");
            assert_ne!(
                f.sh(&format!("cat {}", q(&path))).await.exit_code,
                0,
                "{name}"
            );
        }
        for name in [
            "mcp-auth/token",
            "agents/chatgpt/home/auth.json",
            "sessions.db",
            "sessions.db-wal",
            "events.db-shm",
            "history.json",
            "logs/runtime.log",
            "usage/2026-11.jsonl",
        ] {
            let path = f.agent.join(name);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(&path, "private-token").unwrap();
            assert!(f.env().read_text_file(&path).await.is_err(), "{name}");
            assert_ne!(
                f.sh(&format!("cat {}", q(&path))).await.exit_code,
                0,
                "{name}"
            );
        }
        let fallback = f._root.path().join("runtime-fallback/token");
        std::fs::create_dir_all(fallback.parent().unwrap()).unwrap();
        std::fs::write(&fallback, "runtime-token").unwrap();
        assert!(f.env().read_text_file(&fallback).await.is_err());
        assert_ne!(f.sh(&format!("cat {}", q(&fallback))).await.exit_code, 0);
        use std::os::fd::AsRawFd;
        let secret = std::fs::File::open(f.agent.join("auth.json")).unwrap();
        assert!(f
            .env()
            .read_text_file(Path::new(&format!("/dev/fd/{}", secret.as_raw_fd())))
            .await
            .is_err());
        assert!(f
            .env()
            .read_binary_file(Path::new("/dev/null"))
            .await
            .is_err());
        let renamed = f.project.join("renamed-auth");
        std::fs::rename(f.agent.join("auth.json"), &renamed).unwrap();
        assert!(f.env().read_text_file(&renamed).await.is_err());
        assert!(f.env().write_file(&renamed, b"evil").await.is_err());
        std::fs::rename(&renamed, f.agent.join("auth.json")).unwrap();
        let outside = f.outside.join("ordinary");
        std::fs::write(&outside, "outside").unwrap();
        let alias = f.project.join("hardlink");
        assert_ne!(
            f.sh(&format!("ln {} {}", q(&outside), q(&alias)))
                .await
                .exit_code,
            0
        );
        std::fs::hard_link(&outside, &alias).unwrap();
        assert!(f.env().read_text_file(&alias).await.is_err());
        assert!(f.env().write_file(&alias, b"evil").await.is_err());
        assert_eq!(std::fs::read_to_string(outside).unwrap(), "outside");
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn temporary_directory_is_private_and_cannot_be_overridden() {
    tokio::time::timeout(Duration::from_secs(30), async {
        let f = Fixture::new(false).await;
        let other = Fixture::new(false).await;
        let out = f
            .env()
            .exec(
                "printf '%s' \"$TMPDIR\"",
                ShellExecOptions {
                    env: Some([("TMPDIR".into(), "/".into())].into()),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        let path = PathBuf::from(out.stdout);
        assert_ne!(path, Path::new("/"));
        assert_ne!(path, std::env::temp_dir());
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o700
        );
        assert_ne!(f.sh("chmod 777 \"$TMPDIR\"").await.exit_code, 0);
        assert_eq!(f.sh("echo ok > \"$TMPDIR/test\"").await.exit_code, 0);
        assert_ne!(
            other
                .sh(&format!("echo evil > {}", q(&path.join("other"))))
                .await
                .exit_code,
            0
        );
        assert_ne!(f.sh("echo injected >/dev/tty").await.exit_code, 0);
        f.sandbox.destroy().await.unwrap();
        assert!(!path.exists());
        assert!(f.env().exec("true", Default::default()).await.is_err());
    })
    .await
    .unwrap();
}

async fn gone(pid: &str) {
    loop {
        let output = tokio::process::Command::new("/bin/ps")
            .args(["-p", pid.trim(), "-o", "stat="])
            .output()
            .await
            .unwrap();
        if !output.status.success()
            || String::from_utf8_lossy(&output.stdout)
                .trim()
                .starts_with('Z')
        {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

#[tokio::test]
async fn detached_descendants_are_killed_on_return_timeout_and_destroy() {
    tokio::time::timeout(Duration::from_secs(45), async {
        for finish in ["return", "timeout", "cancel", "drop", "stop", "destroy"] {
            let f = Fixture::new(false).await;
            // Clear the environment too: cleanup cannot rely on a removable
            // environment variable or on the child's process group/session.
            let script = r#"/usr/bin/perl -MPOSIX -e 'setsid() >= 0 or die; my $pid = fork(); defined $pid or die; exit if $pid; setsid() >= 0 or die; exec "/usr/bin/env", "-i", "/bin/sh", "-c", "echo $$ > escaped.pid; exec /bin/sleep 60"' & while [ ! -s escaped.pid ]; do sleep 0.01; done"#;
            let script = if finish == "return" { script.to_owned() } else { format!("{script}; /bin/sleep 60") };
            let env = f.env();
            let cancel = tokio_util::sync::CancellationToken::new();
            let trigger = cancel.clone();
            let command = tokio::spawn(async move { env.exec(&script, ShellExecOptions { timeout_ms: Some(700), cancel: Some(cancel), ..Default::default() }).await });
            let pid = loop {
                if let Ok(pid) = std::fs::read_to_string(f.project.join("escaped.pid")) {
                    if pid.trim().parse::<u32>().is_ok_and(|p| p > 1) { break pid; }
                }
                tokio::task::yield_now().await;
            };
            if finish == "cancel" { trigger.cancel(); }
            if finish == "drop" { command.abort(); }
            if finish == "stop" { f.sandbox.stop().await.unwrap(); }
            if finish == "destroy" { f.sandbox.destroy().await.unwrap(); }
            let result = command.await;
            if finish == "timeout" { assert_eq!(result.unwrap().unwrap_err().code, ExecutionErrorCode::TimedOut); }
            else if finish == "drop" { assert!(result.unwrap_err().is_cancelled()); }
            else if finish == "return" { assert_eq!(result.unwrap().unwrap().exit_code, 0); }
            else { assert_eq!(result.unwrap().unwrap_err().code, ExecutionErrorCode::Aborted); }
            gone(&pid).await;
        }
    }).await.unwrap();
}

#[tokio::test]
async fn ambient_tmpdir_cannot_widen_writes() {
    tokio::time::timeout(Duration::from_secs(30), async {
        if std::env::var_os("LOOP_SEATBELT_TMPDIR_TEST").is_some() {
            let f = Fixture::new(false).await;
            let actual = f.sh("printf '%s' \"$TMPDIR\"").await.stdout;
            assert!(actual.starts_with("/private/tmp/loop-seatbelt-"));
            assert_ne!(
                f.sh(&format!("echo escape > {}", q(&f.outside.join("escape"))))
                    .await
                    .exit_code,
                0
            );
            return;
        }
        use std::os::unix::ffi::OsStringExt;
        for value in [
            std::ffi::OsString::from("/"),
            std::ffi::OsString::from_vec(b"/tmp/non-utf8-\xff".to_vec()),
        ] {
            let child = tokio::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "ambient_tmpdir_cannot_widen_writes",
                    "--nocapture",
                ])
                .env("LOOP_SEATBELT_TMPDIR_TEST", "1")
                .env("TMPDIR", value)
                .output()
                .await
                .unwrap();
            assert!(
                child.status.success(),
                "{}",
                String::from_utf8_lossy(&child.stderr)
            );
        }
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn nested_profiles_cannot_hide_cleanup_identity() {
    tokio::time::timeout(Duration::from_secs(15), async {
        let f = Fixture::new(false).await;
        let out = f
            .sh("/usr/bin/sandbox-exec -p '(version 1)(allow default)' /usr/bin/true")
            .await;
        assert_ne!(
            out.exit_code, 0,
            "a nested profile must not replace the cleanup marker"
        );
        assert!(out.stderr.contains("sandbox_apply"), "{out:?}");
    })
    .await
    .unwrap();
}

fn git(dir: &Path, args: &[&str]) {
    let out = std::process::Command::new("git")
        .current_dir(dir)
        .args([
            "-c",
            "protocol.file.allow=always",
            "-c",
            "user.name=Host",
            "-c",
            "user.email=host@example.invalid",
            "-c",
            "commit.gpgsign=false",
        ])
        .args(args)
        .output()
        .unwrap();
    assert!(out.status.success(), "git {args:?}: {out:?}");
}

async fn seatbelt(project: &Path, root: &Path, extra: Value) -> Result<Arc<dyn Sandbox>, String> {
    let mut options =
        json!({"homeDir": root.join("home"), "agentDir": root.join("agent"), "network": false});
    if let (Some(options), Some(extra)) = (options.as_object_mut(), extra.as_object()) {
        options.extend(extra.clone());
    }
    let sandbox = SeatbeltSandboxFactory
        .create(SandboxConfig {
            workdir: project.to_path_buf(),
            options,
            ..Default::default()
        })
        .await
        .map_err(|e| e.to_string())?;
    sandbox.start().await.map_err(|e| e.to_string())?;
    Ok(sandbox)
}

async fn run(sandbox: &Arc<dyn Sandbox>, command: &str) -> ShellOutput {
    sandbox
        .env()
        .exec(
            command,
            ShellExecOptions {
                timeout_ms: Some(20_000),
                ..Default::default()
            },
        )
        .await
        .unwrap()
}

fn scratch() -> (tempfile::TempDir, PathBuf) {
    let root = tempfile::Builder::new()
        .prefix(".seatbelt-git-")
        .tempdir_in(std::env::current_dir().unwrap())
        .unwrap();
    let path = std::fs::canonicalize(root.path()).unwrap();
    for dir in ["home", "agent", "project"] {
        std::fs::create_dir_all(path.join(dir)).unwrap();
    }
    (root, path)
}

/// The system git shim can discover its cache directory without Xcode warnings.
#[tokio::test]
async fn system_git_status_is_quiet_inside_the_sandbox() {
    tokio::time::timeout(Duration::from_secs(30), async {
        let (_guard, root) = scratch();
        let project = root.join("project");
        git(&project, &["init", "-q"]);
        let sandbox = seatbelt(&project, &root, json!({})).await.unwrap();
        let out = run(&sandbox, "/usr/bin/git status --porcelain").await;
        assert_eq!(out.exit_code, 0, "{out:?}");
        assert!(out.stdout.is_empty(), "{out:?}");
        assert!(out.stderr.is_empty(), "{out:?}");
        sandbox.destroy().await.unwrap();
    })
    .await
    .unwrap();
}

/// The xcrun cache grant is two file names in the user temp dir, not the dir.
#[tokio::test]
async fn only_the_xcrun_cache_files_are_writable_in_the_user_temp_dir() {
    tokio::time::timeout(Duration::from_secs(30), async {
        let (_guard, root) = scratch();
        let project = root.join("project");
        let sandbox = seatbelt(&project, &root, json!({})).await.unwrap();
        let mut buf = [0 as libc::c_char; libc::PATH_MAX as usize];
        // SAFETY: `buf` is valid for `buf.len()` bytes.
        let n =
            unsafe { libc::confstr(libc::_CS_DARWIN_USER_TEMP_DIR, buf.as_mut_ptr(), buf.len()) };
        assert!(n > 0 && n <= buf.len());
        // SAFETY: confstr NUL-terminated the buffer.
        let dir = unsafe { std::ffi::CStr::from_ptr(buf.as_ptr()) }
            .to_str()
            .unwrap();
        let dir = std::fs::canonicalize(dir).unwrap();
        let pid = std::process::id();
        let other = dir.join(format!("loop-seatbelt-neighbour-{pid}"));
        let nested = dir.join(format!("loop-seatbelt-dir-{pid}"));
        let cache = dir.join(format!("xcrun_db-Probe{pid}"));
        std::fs::create_dir(&nested).unwrap();
        let nested_cache = nested.join("xcrun_db");
        for (path, writable) in [(&other, false), (&nested_cache, false), (&cache, true)] {
            let out = run(&sandbox, &format!("echo probe > '{}'", path.display())).await;
            assert_eq!(out.exit_code == 0, writable, "{path:?}: {out:?}");
            assert_eq!(path.exists(), writable, "{path:?}");
            let _ = std::fs::remove_file(path);
        }
        let _ = std::fs::remove_dir(&nested);
        sandbox.destroy().await.unwrap();
    })
    .await
    .unwrap();
}

/// Git metadata that runs commands is protected in every repository the
/// sandbox can write: the project, its submodules and nested repositories,
/// including metadata created later. No `.git` entry can be created or
/// replaced. Ordinary commits still work.
#[tokio::test]
async fn git_metadata_in_every_repository_is_protected() {
    tokio::time::timeout(Duration::from_secs(120), async {
        let (_guard, root) = scratch();
        let project = root.join("project");
        let upstream = root.join("upstream");
        std::fs::create_dir(&upstream).unwrap();
        git(&upstream, &["init", "-q"]);
        git(&upstream, &["commit", "-q", "--allow-empty", "-m", "upstream"]);
        git(&project, &["init", "-q"]);
        git(&project, &["submodule", "add", "-q", upstream.to_str().unwrap(), "sub"]);
        git(&project, &["commit", "-q", "-m", "project"]);
        let nested = project.join("vendor/lib");
        std::fs::create_dir_all(&nested).unwrap();
        git(&nested, &["init", "-q"]);
        git(&nested, &["commit", "-q", "--allow-empty", "-m", "nested"]);
        let sandbox = seatbelt(&project, &root, json!({})).await.unwrap();
        let pwned = root.join("PWNED");
        let payload = format!("[core]\n\tfsmonitor = \"touch {}; false\"\n", pwned.display());
        for target in [
            ".git/config",
            ".git/config.worktree",
            ".git/hooks/pre-commit",
            ".git/info/attributes",
            ".git/commondir",
            ".git/modules/sub/config",
            ".git/modules/sub/config.worktree",
            ".git/modules/sub/hooks/post-checkout",
            ".git/worktrees/w/gitdir",
            ".git/worktrees/w/commondir",
            "vendor/lib/.git/config",
            "vendor/lib/.git/hooks/pre-commit",
        ] {
            let path = project.join(target);
            let out = run(
                &sandbox,
                &format!(
                    "mkdir -p {} && printf '%s' {} >> {}",
                    q(path.parent().unwrap()),
                    q(Path::new(&payload)),
                    q(&path)
                ),
            )
            .await;
            assert_ne!(out.exit_code, 0, "{target}: {out:?}");
            assert!(
                sandbox.env().write_file(&path, payload.as_bytes()).await.is_err(),
                "{target}"
            );
        }
        // Replacing protected metadata by rename, or a .git entry itself.
        for command in [
            "printf x > new-config && mv new-config .git/config",
            "printf x > new-config && mv new-config .git/modules/sub/config",
            "mv .git .git-old",
            "mv sub/.git sub/.git-old",
            "rm -f sub/.git",
            "printf 'gitdir: ../evil' > sub/.git",
            "printf 'gitdir: ../evil' >> sub/.git",
            "touch sub/.git",
            "mkdir -p fresh && cd fresh && git init -q",
            "git clone -q ./vendor/lib cloned",
            "mkdir -p pointer && printf 'gitdir: ../evil' > pointer/.git",
            "mkdir -p evil && mkdir -p linked && ln -s ../evil linked/.git",
            "mkdir -p made/.git",
        ] {
            let out = run(&sandbox, command).await;
            assert_ne!(out.exit_code, 0, "{command}: {out:?}");
        }
        for path in ["file-tool/.git", "file-tool/deep/.git/config"] {
            assert!(
                sandbox
                    .env()
                    .write_file(Path::new(path), b"gitdir: ../evil")
                    .await
                    .is_err(),
                "{path}"
            );
        }
        assert!(sandbox
            .env()
            .create_dir(Path::new("file-tool-dir/.git"))
            .await
            .is_err());
        assert!(project.join(".git").is_dir());
        assert!(project.join("sub/.git").is_file());
        // Git run by the person afterwards finds nothing planted.
        for dir in [&project, &project.join("sub"), &nested] {
            let _ = std::process::Command::new("git")
                .current_dir(dir)
                .args(["status"])
                .output();
        }
        assert!(!pwned.exists(), "a sandboxed command got git to run on the host");
        // Ordinary work in existing repositories still succeeds.
        let commit = "git -c user.name=Sandbox -c user.email=sandbox@example.invalid -c commit.gpgsign=false commit -q --allow-empty -m sandbox";
        for dir in [".", "sub", "vendor/lib"] {
            let out = run(&sandbox, &format!("cd {dir} && {commit} && git branch -q topic")).await;
            assert_eq!(out.exit_code, 0, "{dir}: {out:?}");
        }
        sandbox.destroy().await.unwrap();
    })
    .await
    .unwrap();
}

/// Refusal explains the protected ancestor, rather than just the project path.
#[tokio::test]
async fn a_project_in_a_protected_folder_explains_why_it_cannot_start() {
    tokio::time::timeout(Duration::from_secs(30), async {
        let (_guard, root) = scratch();
        let protected = root.join("home/.claude");
        let project = protected.join("projects/example");
        std::fs::create_dir_all(&project).unwrap();
        let error = seatbelt(&project, &root, json!({})).await.err().unwrap();
        assert!(error.contains(protected.to_str().unwrap()), "{error}");
        assert!(error.contains("protected folder"), "{error}");
        assert!(error.contains("credentials"), "{error}");
    })
    .await
    .unwrap();
}

/// A project whose `.git` the sandbox cannot cover does not start.
#[tokio::test]
async fn a_git_symlink_or_outside_pointer_refuses_to_start() {
    tokio::time::timeout(Duration::from_secs(60), async {
        let (_guard, root) = scratch();
        let project = root.join("project");
        let elsewhere = root.join("elsewhere");
        std::fs::create_dir(&elsewhere).unwrap();
        git(&elsewhere, &["init", "-q"]);
        std::os::unix::fs::symlink(elsewhere.join(".git"), project.join(".git")).unwrap();
        let error = seatbelt(&project, &root, json!({})).await.err().unwrap();
        assert!(error.contains("symlink"), "{error}");

        std::fs::remove_file(project.join(".git")).unwrap();
        std::fs::write(
            project.join(".git"),
            format!("gitdir: {}\n", elsewhere.join(".git").display()),
        )
        .unwrap();
        let error = seatbelt(&project, &root, json!({})).await.err().unwrap();
        assert!(error.contains("points to"), "{error}");

        // A pointer to a git directory inside the project but outside any
        // `.git` path is refused too: the metadata rule would not cover it.
        std::fs::create_dir(project.join("gitdata")).unwrap();
        std::fs::write(project.join(".git"), "gitdir: gitdata\n").unwrap();
        assert!(seatbelt(&project, &root, json!({})).await.is_err());

        // Inside a `.git` directory (as a submodule checkout has) is covered.
        std::fs::create_dir_all(project.join("inner/.git")).unwrap();
        std::fs::write(project.join(".git"), "gitdir: inner/.git\n").unwrap();
        let sandbox = seatbelt(&project, &root, json!({})).await.unwrap();
        sandbox.destroy().await.unwrap();
    })
    .await
    .unwrap();
}

/// With denyHomeReads, only the project, allowWrite, allowRead and toolchain
/// settings are readable under $HOME.
#[tokio::test]
async fn deny_home_reads_keeps_only_listed_home_paths_readable() {
    tokio::time::timeout(Duration::from_secs(60), async {
        let (_guard, root) = scratch();
        let home = root.join("home");
        let project = home.join("work/project");
        std::fs::create_dir_all(&project).unwrap();
        std::fs::write(project.join("main.rs"), "project-file").unwrap();
        std::fs::create_dir_all(home.join("allowed")).unwrap();
        std::fs::write(home.join("allowed/notes"), "allowed-file").unwrap();
        std::fs::create_dir_all(home.join("cache")).unwrap();
        std::fs::write(home.join("private.txt"), "private-file").unwrap();
        std::fs::write(home.join(".gitconfig"), "[user]\n").unwrap();
        std::fs::create_dir_all(home.join(".cargo")).unwrap();
        std::fs::write(home.join(".cargo/config.toml"), "[build]\n").unwrap();
        std::fs::write(home.join(".cargo/credentials.toml"), "private-token").unwrap();
        let sandbox = seatbelt(
            &project,
            &root,
            json!({"denyHomeReads": true, "allowRead": [home.join("allowed")], "allowWrite": [home.join("cache")]}),
        )
        .await
        .unwrap();
        for (path, readable) in [
            (project.join("main.rs"), true),
            (home.join("allowed/notes"), true),
            (home.join(".gitconfig"), true),
            (home.join(".cargo/config.toml"), true),
            (home.join("private.txt"), false),
            (home.join(".cargo/credentials.toml"), false),
        ] {
            let out = run(&sandbox, &format!("cat {}", q(&path))).await;
            assert_eq!(out.exit_code == 0, readable, "{}: {out:?}", path.display());
            assert_eq!(
                sandbox.env().read_text_file(&path).await.is_ok(),
                readable,
                "{}",
                path.display()
            );
        }
        assert_eq!(run(&sandbox, &format!("echo ok > {}/f && cat {0}/f", q(&home.join("cache")))).await.exit_code, 0);
        assert_ne!(run(&sandbox, &format!("ls {}", q(&home))).await.exit_code, 0);
        assert_eq!(run(&sandbox, "ls && pwd -P").await.exit_code, 0);
        sandbox.destroy().await.unwrap();
        // Off by default: the same home file is readable.
        let open = seatbelt(&project, &root, json!({})).await.unwrap();
        assert_eq!(run(&open, &format!("cat {}", q(&home.join("private.txt")))).await.exit_code, 0);
        open.destroy().await.unwrap();
    })
    .await
    .unwrap();
}

/// A concurrent collector must never mistake a starting sandbox for a stale one.
#[tokio::test]
async fn sandbox_start_survives_concurrent_stale_cleanup() {
    struct Cleaners {
        stop: Arc<std::sync::atomic::AtomicBool>,
        threads: Vec<std::thread::JoinHandle<()>>,
    }
    impl Drop for Cleaners {
        fn drop(&mut self) {
            self.stop.store(true, std::sync::atomic::Ordering::Release);
            for thread in self.threads.drain(..) {
                thread.join().unwrap();
            }
        }
    }

    tokio::time::timeout(Duration::from_secs(60), async {
        let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let _cleaners = Cleaners {
            threads: (0..4)
                .map(|_| {
                    let stop = Arc::clone(&stop);
                    std::thread::spawn(move || {
                        while !stop.load(std::sync::atomic::Ordering::Acquire) {
                            loop_agent::harness::sandbox::seatbelt::clean_stale_temp_dirs();
                            std::thread::yield_now();
                        }
                    })
                })
                .collect(),
            stop,
        };
        let (_guard, root) = scratch();
        for _ in 0..50 {
            let sandbox = seatbelt(&root.join("project"), &root, json!({}))
                .await
                .unwrap();
            assert_eq!(run(&sandbox, "true").await.exit_code, 0);
            sandbox.destroy().await.unwrap();
        }
    })
    .await
    .unwrap();
}

/// Sandbox start removes this user's abandoned temporary directories and
/// keeps those still in use.
#[tokio::test]
async fn stale_temporary_directories_are_removed_at_start() {
    tokio::time::timeout(Duration::from_secs(60), async {
        let id = std::process::id();
        let stale = PathBuf::from(format!("/private/tmp/loop-seatbelt-test-stale-{id}"));
        let held = PathBuf::from(format!("/private/tmp/loop-command-test-held-{id}"));
        let fresh = PathBuf::from(format!("/private/tmp/loop-seatbelt-test-fresh-{id}"));
        for dir in [&stale, &held, &fresh] {
            let _ = std::fs::remove_dir_all(dir);
            std::fs::create_dir(dir).unwrap();
            std::fs::write(dir.join("left"), "x").unwrap();
        }
        std::fs::write(stale.join(".loop-lock"), "").unwrap();
        let lock = tempfile::NamedTempFile::new_in(&held).unwrap();
        rustix::fs::flock(
            lock.as_file(),
            rustix::fs::FlockOperation::NonBlockingLockExclusive,
        )
        .unwrap();
        let lock = lock.persist_noclobber(held.join(".loop-lock")).unwrap();
        let (_guard, root) = scratch();
        let sandbox = seatbelt(&root.join("project"), &root, json!({}))
            .await
            .unwrap();
        assert!(!stale.exists(), "an unlocked directory is stale");
        assert!(held.exists(), "a locked directory is in use");
        assert!(fresh.exists(), "a recent directory without a lock is kept");
        sandbox.destroy().await.unwrap();
        for dir in [&held, &fresh] {
            std::fs::remove_dir_all(dir).unwrap();
        }
        drop(lock);
    })
    .await
    .unwrap();
}

const COMMIT: &str = "git -c user.name=Sandbox -c user.email=sandbox@example.invalid -c commit.gpgsign=false commit -q --allow-empty -m sandbox";

/// Plant a git payload at each of `targets` (shell and file tools); every
/// attempt must fail. Returns the marker a planted payload would create.
async fn plant_everywhere(sandbox: &Arc<dyn Sandbox>, root: &Path, targets: &[PathBuf]) -> PathBuf {
    let pwned = root.join("PWNED");
    let payload = format!(
        "[core]\n\tfsmonitor = \"touch {}; false\"\n",
        pwned.display()
    );
    for path in targets {
        let out = run(
            sandbox,
            &format!(
                "mkdir -p {} ; printf '%s' {} >> {}",
                q(path.parent().unwrap()),
                q(Path::new(&payload)),
                q(path)
            ),
        )
        .await;
        assert_ne!(out.exit_code, 0, "{}: {out:?}", path.display());
        assert!(
            sandbox
                .env()
                .write_file(path, payload.as_bytes())
                .await
                .is_err(),
            "{}",
            path.display()
        );
    }
    pwned
}

fn host_git_status(dirs: &[&Path]) {
    for dir in dirs {
        let _ = std::process::Command::new("git")
            .current_dir(dir)
            .args(["status"])
            .output();
    }
}

/// A Session's git worktree: commits and branches work; the shared git
/// directory's executable metadata stays protected.
#[tokio::test]
async fn a_linked_worktree_commits_and_its_shared_metadata_is_protected() {
    tokio::time::timeout(Duration::from_secs(120), async {
        let (_guard, root) = scratch();
        std::fs::remove_dir(root.join("project")).unwrap();
        let main = root.join("main");
        std::fs::create_dir(&main).unwrap();
        git(&main, &["init", "-q"]);
        git(&main, &["commit", "-q", "--allow-empty", "-m", "main"]);
        git(
            &main,
            &["worktree", "add", "-q", "-b", "session", "../project"],
        );
        let project = root.join("project");
        let sandbox = seatbelt(&project, &root, json!({})).await.unwrap();
        let out = run(
            &sandbox,
            &format!("{COMMIT} && git branch -q topic && git switch -q topic && {COMMIT}"),
        )
        .await;
        assert_eq!(out.exit_code, 0, "{out:?}");
        let common = main.join(".git");
        let admin = common.join("worktrees/project");
        let pwned = plant_everywhere(
            &sandbox,
            &root,
            &[
                common.join("config"),
                common.join("config.worktree"),
                common.join("hooks/pre-commit"),
                common.join("info/attributes"),
                common.join("HEAD"),
                admin.join("config.worktree"),
                admin.join("commondir"),
                admin.join("gitdir"),
                main.join("README"),
            ],
        )
        .await;
        for command in [
            "printf x > c && mv c ../main/.git/config",
            "rm -f .git",
            "printf 'gitdir: ../evil' > .git",
        ] {
            assert_ne!(run(&sandbox, command).await.exit_code, 0, "{command}");
        }
        host_git_status(&[&project, &main]);
        assert!(
            !pwned.exists(),
            "a sandboxed command got git to run on the host"
        );
        let branches = std::process::Command::new("git")
            .current_dir(&main)
            .args(["branch", "--list", "topic"])
            .output()
            .unwrap();
        assert!(String::from_utf8_lossy(&branches.stdout).contains("topic"));
        sandbox.destroy().await.unwrap();
    })
    .await
    .unwrap();
}

/// A submodule checkout as the project: commits and branches work; the
/// superproject's and the submodule's metadata stay protected.
#[tokio::test]
async fn a_submodule_checkout_commits_and_its_metadata_is_protected() {
    tokio::time::timeout(Duration::from_secs(120), async {
        let (_guard, root) = scratch();
        let upstream = root.join("upstream");
        let superproject = root.join("project");
        std::fs::create_dir(&upstream).unwrap();
        git(&upstream, &["init", "-q"]);
        git(
            &upstream,
            &["commit", "-q", "--allow-empty", "-m", "upstream"],
        );
        git(&superproject, &["init", "-q"]);
        git(
            &superproject,
            &["submodule", "add", "-q", upstream.to_str().unwrap(), "sub"],
        );
        git(&superproject, &["commit", "-q", "-m", "super"]);
        let project = superproject.join("sub");
        let sandbox = seatbelt(&project, &root, json!({})).await.unwrap();
        let out = run(&sandbox, &format!("{COMMIT} && git branch -q topic")).await;
        assert_eq!(out.exit_code, 0, "{out:?}");
        let module = superproject.join(".git/modules/sub");
        let pwned = plant_everywhere(
            &sandbox,
            &root,
            &[
                module.join("config"),
                module.join("hooks/post-checkout"),
                module.join("info/attributes"),
                superproject.join(".git/config"),
                superproject.join(".git/hooks/pre-commit"),
                superproject.join(".gitmodules"),
            ],
        )
        .await;
        host_git_status(&[&project, &superproject]);
        assert!(
            !pwned.exists(),
            "a sandboxed command got git to run on the host"
        );
        sandbox.destroy().await.unwrap();
    })
    .await
    .unwrap();
}

/// Pointers that only look like a worktree are refused.
#[tokio::test]
async fn forged_worktree_pointers_refuse_to_start() {
    tokio::time::timeout(Duration::from_secs(60), async {
        let (_guard, root) = scratch();
        let project = root.join("project");
        let main = root.join("main");
        let other = root.join("other");
        for dir in [&main, &other] {
            std::fs::create_dir(dir).unwrap();
            git(dir, &["init", "-q"]);
            git(dir, &["commit", "-q", "--allow-empty", "-m", "x"]);
        }
        let admin = main.join(".git/worktrees/fake");
        std::fs::create_dir_all(&admin).unwrap();
        std::fs::write(admin.join("HEAD"), "ref: refs/heads/master\n").unwrap();
        std::fs::write(
            project.join(".git"),
            format!("gitdir: {}\n", admin.display()),
        )
        .unwrap();
        // commondir names another repository.
        std::fs::write(
            admin.join("commondir"),
            format!("{}\n", other.join(".git").display()),
        )
        .unwrap();
        std::fs::write(
            admin.join("gitdir"),
            format!("{}\n", project.join(".git").display()),
        )
        .unwrap();
        assert!(seatbelt(&project, &root, json!({})).await.is_err());
        // gitdir names another checkout.
        std::fs::write(admin.join("commondir"), "../..\n").unwrap();
        std::fs::write(
            admin.join("gitdir"),
            format!("{}\n", other.join(".git").display()),
        )
        .unwrap();
        assert!(seatbelt(&project, &root, json!({})).await.is_err());
        // Consistent: accepted.
        std::fs::write(
            admin.join("gitdir"),
            format!("{}\n", project.join(".git").display()),
        )
        .unwrap();
        let sandbox = seatbelt(&project, &root, json!({})).await.unwrap();
        sandbox.destroy().await.unwrap();
    })
    .await
    .unwrap();
}
