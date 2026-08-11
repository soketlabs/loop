//! Sandbox trait + KrunSandbox tests (fake Podman client).

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use loop_agent::harness::types::{
    ExecutionError, ExecutionErrorCode, ShellExecOptions, ShellOutput,
};
use loop_agent::harness::{
    create_read_tool, create_write_tool, KrunIsolation, KrunSandbox, KrunSandboxFactory,
    LocalSandboxRuntime, PodmanClient, PodmanExecOpts, PodmanRunOpts, Sandbox, SandboxConfig,
    SandboxError, SandboxMode, SandboxRegistry, SandboxStatus,
};
use parking_lot::Mutex;
use serde_json::json;

/// In-memory Podman stand-in for unit tests.
struct FakePodman {
    files: Mutex<HashMap<String, Vec<u8>>>,
    dirs: Mutex<std::collections::HashSet<String>>,
    running: Mutex<Option<String>>,
    shell_log: Mutex<Vec<String>>,
}

impl FakePodman {
    fn new() -> Self {
        Self {
            files: Mutex::new(HashMap::new()),
            dirs: Mutex::new(std::collections::HashSet::new()),
            running: Mutex::new(None),
            shell_log: Mutex::new(Vec::new()),
        }
    }

    fn ensure_parent(dirs: &mut std::collections::HashSet<String>, path: &str) {
        let p = Path::new(path);
        let mut cur = PathBuf::new();
        for c in p.components() {
            cur.push(c);
            let s = cur.to_string_lossy().to_string();
            if s != "/" {
                dirs.insert(s);
            }
        }
        if let Some(parent) = p.parent() {
            dirs.insert(parent.to_string_lossy().into_owned());
        }
    }
}

fn unquote(s: &str) -> String {
    let s = s.trim();
    if s.starts_with('\'') && s.ends_with('\'') && s.len() >= 2 {
        s[1..s.len() - 1].replace("'\\''", "'")
    } else {
        s.to_string()
    }
}

/// Extract quoted path after a marker like `cat > ` or `cat -- `.
fn path_after(script: &str, marker: &str) -> Option<String> {
    let idx = script.find(marker)?;
    Some(unquote(script[idx + marker.len()..].trim()))
}

#[async_trait]
impl PodmanClient for FakePodman {
    async fn preflight(&self, _runtime: &str) -> Result<(), SandboxError> {
        Ok(())
    }

    async fn run(&self, opts: PodmanRunOpts) -> Result<String, SandboxError> {
        // Same-path mount: seed the guest workdir (equals host workdir).
        Self::ensure_parent(&mut self.dirs.lock(), &opts.guest_workdir);
        self.dirs.lock().insert(opts.guest_workdir.clone());
        let id = format!("fake-{}", opts.name);
        *self.running.lock() = Some(id.clone());
        Ok(id)
    }

    async fn exec(&self, opts: PodmanExecOpts) -> Result<ShellOutput, ExecutionError> {
        if opts.argv.len() >= 3 && opts.argv[0] == "sh" && opts.argv[1] == "-c" {
            let script = opts.argv[2].clone();
            self.shell_log.lock().push(script.clone());

            if let Some(path) = path_after(&script, "cat > ") {
                if let Some(data) = opts.stdin {
                    Self::ensure_parent(&mut self.dirs.lock(), &path);
                    self.files.lock().insert(path, data);
                }
                return Ok(ShellOutput {
                    stdout: String::new(),
                    stderr: String::new(),
                    exit_code: 0,
                });
            }
            if let Some(path) = path_after(&script, "cat >> ") {
                if let Some(data) = opts.stdin {
                    Self::ensure_parent(&mut self.dirs.lock(), &path);
                    self.files
                        .lock()
                        .entry(path)
                        .or_default()
                        .extend_from_slice(&data);
                }
                return Ok(ShellOutput {
                    stdout: String::new(),
                    stderr: String::new(),
                    exit_code: 0,
                });
            }
            if let Some(path) = path_after(&script, "cat -- ") {
                return match self.files.lock().get(&path) {
                    Some(data) => Ok(ShellOutput {
                        stdout: String::from_utf8_lossy(data).into_owned(),
                        stderr: String::new(),
                        exit_code: 0,
                    }),
                    None => Ok(ShellOutput {
                        stdout: String::new(),
                        stderr: "No such file".into(),
                        exit_code: 1,
                    }),
                };
            }
            if let Some(path) = path_after(&script, "base64 -w0 -- ") {
                return match self.files.lock().get(&path) {
                    Some(data) => {
                        use base64::Engine;
                        Ok(ShellOutput {
                            stdout: base64::engine::general_purpose::STANDARD.encode(data),
                            stderr: String::new(),
                            exit_code: 0,
                        })
                    }
                    None => Ok(ShellOutput {
                        stdout: String::new(),
                        stderr: "No such file".into(),
                        exit_code: 1,
                    }),
                };
            }
            if script.contains("if [ ! -e ") && script.contains("echo DIR") {
                let path = script
                    .split('\'')
                    .nth(1)
                    .unwrap_or("")
                    .to_string();
                if let Some(data) = self.files.lock().get(&path) {
                    return Ok(ShellOutput {
                        stdout: format!("FILE\n{}\n", data.len()),
                        stderr: String::new(),
                        exit_code: 0,
                    });
                }
                if self.dirs.lock().contains(&path) {
                    return Ok(ShellOutput {
                        stdout: "DIR\n0\n".into(),
                        stderr: String::new(),
                        exit_code: 0,
                    });
                }
                return Ok(ShellOutput {
                    stdout: "MISSING\n".into(),
                    stderr: String::new(),
                    exit_code: 1,
                });
            }
            if script.contains("if [ -e ") {
                let path = script
                    .split('\'')
                    .nth(1)
                    .unwrap_or("")
                    .to_string();
                let exists =
                    self.files.lock().contains_key(&path) || self.dirs.lock().contains(&path);
                return Ok(ShellOutput {
                    stdout: if exists { "1\n" } else { "0\n" }.into(),
                    stderr: String::new(),
                    exit_code: 0,
                });
            }
            if let Some(path) = path_after(&script, "mkdir -p -- ") {
                Self::ensure_parent(&mut self.dirs.lock(), &path);
                self.dirs.lock().insert(path);
                return Ok(ShellOutput {
                    stdout: String::new(),
                    stderr: String::new(),
                    exit_code: 0,
                });
            }
            if let Some(path) = path_after(&script, "rm -rf -- ") {
                self.files.lock().remove(&path);
                self.dirs.lock().remove(&path);
                return Ok(ShellOutput {
                    stdout: String::new(),
                    stderr: String::new(),
                    exit_code: 0,
                });
            }
            return Ok(ShellOutput {
                stdout: format!("ok:{script}"),
                stderr: String::new(),
                exit_code: 0,
            });
        }

        Err(ExecutionError::new(
            ExecutionErrorCode::Other,
            format!("unsupported argv: {:?}", opts.argv),
        ))
    }

    async fn stop(&self, _container: &str) -> Result<(), SandboxError> {
        *self.running.lock() = None;
        Ok(())
    }

    async fn rm(&self, _container: &str) -> Result<(), SandboxError> {
        *self.running.lock() = None;
        Ok(())
    }
}

#[tokio::test]
async fn krun_full_fs_via_exec() {
    let dir = tempfile::tempdir().unwrap();
    let client = Arc::new(FakePodman::new());
    let sb = KrunSandbox::with_client(
        SandboxConfig {
            workdir: dir.path().to_path_buf(),
            options: json!({"isolation": "full"}),
            labels: Default::default(),
        },
        client.clone(),
    );
    sb.start().await.unwrap();
    assert_eq!(sb.status(), SandboxStatus::Ready);
    assert_eq!(sb.kind(), "local");
    assert_eq!(sb.isolation(), KrunIsolation::Full);

    let env = sb.env();
    env.write_file(Path::new("hello.txt"), b"hi").await.unwrap();
    let text = env.read_text_file(Path::new("hello.txt")).await.unwrap();
    assert_eq!(text, "hi");

    // Absolute host path must map 1:1 into the guest (same path, no /workspace rewrite).
    let abs = dir.path().join("hello.txt");
    let abs_text = env.read_text_file(&abs).await.unwrap();
    assert_eq!(abs_text, "hi");
    let abs_s = abs.to_string_lossy().into_owned();
    let guest = client
        .shell_log
        .lock()
        .iter()
        .rev()
        .find_map(|s| path_after(s, "cat -- "));
    assert_eq!(guest.as_deref(), Some(abs_s.as_str()));

    let err = env.read_text_file(Path::new("../outside.txt")).await;
    assert!(err.is_err());

    let out = env
        .exec("echo hi", ShellExecOptions::default())
        .await
        .unwrap();
    assert_eq!(out.exit_code, 0);
    assert!(client.shell_log.lock().iter().any(|s| s.contains("echo hi")));

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
async fn krun_partial_uses_host_fs_and_exec_shell() {
    let dir = tempfile::tempdir().unwrap();
    let client = Arc::new(FakePodman::new());
    let sb = KrunSandbox::with_client(
        SandboxConfig {
            workdir: dir.path().to_path_buf(),
            options: json!({"isolation": "partial"}),
            labels: Default::default(),
        },
        client.clone(),
    );
    sb.start().await.unwrap();
    assert_eq!(sb.isolation(), KrunIsolation::Partial);

    let env = sb.env();
    env.write_file(Path::new("host.txt"), b"on-host").await.unwrap();
    let host_text = std::fs::read_to_string(dir.path().join("host.txt")).unwrap();
    assert_eq!(host_text, "on-host");

    let err = env.read_text_file(Path::new("../outside.txt")).await;
    assert!(err.is_err());

    let out = env
        .exec("uname", ShellExecOptions::default())
        .await
        .unwrap();
    assert_eq!(out.exit_code, 0);
    assert!(client.shell_log.lock().iter().any(|s| s == "uname"));

    sb.destroy().await.unwrap();
}

#[tokio::test]
async fn registry_creates_local_kind() {
    let reg = SandboxRegistry::new();
    reg.register(Arc::new(KrunSandboxFactory));
    assert!(reg.kinds().contains(&"local".to_string()));
    let dir = tempfile::tempdir().unwrap();
    let client = Arc::new(FakePodman::new());
    let sb = KrunSandbox::with_client(
        SandboxConfig {
            workdir: dir.path().to_path_buf(),
            options: json!({"isolation": "full"}),
            labels: Default::default(),
        },
        client,
    );
    assert_eq!(sb.kind(), "local");
    let _ = reg;
}

#[tokio::test]
async fn default_runtime_is_runc() {
    let dir = tempfile::tempdir().unwrap();
    let client = Arc::new(FakePodman::new());
    let sb = KrunSandbox::with_client(
        SandboxConfig {
            workdir: dir.path().to_path_buf(),
            options: json!({"isolation": "partial"}),
            labels: Default::default(),
        },
        client,
    );
    sb.start().await.unwrap();
    assert_eq!(sb.runtime(), "runc");
    sb.destroy().await.unwrap();
}

#[tokio::test]
async fn sandbox_mode_enum() {
    let _ = SandboxMode::Disabled;
}

#[tokio::test]
#[ignore = "requires podman + crun-krun + /dev/kvm; set LOOP_TEST_KRUN=1 to run manually"]
async fn krun_integration_real_podman() {
    if std::env::var("LOOP_TEST_KRUN").ok().as_deref() != Some("1") {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("seed.txt"), b"seed").unwrap();
    let sb = KrunSandbox::new(KrunSandbox::config_for(
        dir.path().to_path_buf(),
        KrunIsolation::Partial,
        LocalSandboxRuntime::Runc,
    ));
    sb.start().await.expect("start krun sandbox");
    let env = sb.env();
    let text = env.read_text_file(Path::new("seed.txt")).await.unwrap();
    assert_eq!(text, "seed");
    let out = env
        .exec("cat seed.txt", ShellExecOptions::default())
        .await
        .unwrap();
    assert!(out.stdout.contains("seed"));
    sb.destroy().await.unwrap();
}
