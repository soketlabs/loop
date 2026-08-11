//! Thin async wrapper around the `podman` CLI for local sandboxes.

use std::path::Path;
use std::process::Stdio;
use std::time::Duration;

use async_trait::async_trait;
use tokio::io::AsyncWriteExt;
use tokio::process::Command;

use crate::harness::sandbox::traits::SandboxError;
use crate::harness::types::{ExecutionError, ExecutionErrorCode, ShellOutput};

/// Options for `podman run`.
#[derive(Debug, Clone)]
pub struct PodmanRunOpts {
    /// Container name.
    pub name: String,
    /// OCI image.
    pub image: String,
    /// Host path bind-mounted to [`Self::guest_workdir`].
    pub host_workdir: std::path::PathBuf,
    /// Guest mount point (e.g. `/workspace`).
    pub guest_workdir: String,
    /// OCI runtime (`crun`, `runc`, `runsc`, `krun`).
    pub runtime: String,
    /// `krun.cpus` annotation value (krun only).
    pub cpus: String,
    /// `krun.ram_mib` annotation value (krun only).
    pub ram_mib: String,
}

/// Options for `podman exec`.
#[derive(Debug, Clone)]
pub struct PodmanExecOpts {
    /// Container id or name.
    pub container: String,
    /// Working directory inside the guest.
    pub workdir: Option<String>,
    /// Command argv (not a shell string).
    pub argv: Vec<String>,
    /// Optional stdin bytes.
    pub stdin: Option<Vec<u8>>,
    /// Timeout in milliseconds.
    pub timeout_ms: Option<u64>,
    /// Cancellation token.
    pub cancel: Option<tokio_util::sync::CancellationToken>,
}

/// Podman operations used by [`super::krun::KrunSandbox`].
#[async_trait]
pub trait PodmanClient: Send + Sync {
    /// Verify deps for the selected OCI runtime.
    async fn preflight(&self, runtime: &str) -> Result<(), SandboxError>;
    /// Start a detached keep-alive container; returns container id.
    async fn run(&self, opts: PodmanRunOpts) -> Result<String, SandboxError>;
    /// Exec a command in a running container.
    async fn exec(&self, opts: PodmanExecOpts) -> Result<ShellOutput, ExecutionError>;
    /// Stop a container.
    async fn stop(&self, container: &str) -> Result<(), SandboxError>;
    /// Force-remove a container.
    async fn rm(&self, container: &str) -> Result<(), SandboxError>;
}

/// Real `podman` binary client.
#[derive(Debug, Default, Clone)]
pub struct RealPodmanClient;

impl RealPodmanClient {
    /// Construct.
    pub fn new() -> Self {
        Self
    }

    async fn run_capture(args: &[&str]) -> Result<(i32, String, String), SandboxError> {
        let output = match Command::new("podman")
            .args(args)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .output()
            .await
        {
            Ok(o) => o,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Err(SandboxError::StartFailed(missing_deps_message(
                    "runc",
                    &[MissingDep::Podman],
                )));
            }
            Err(e) => {
                return Err(SandboxError::StartFailed(format!("podman: {e}")));
            }
        };
        Ok((
            output.status.code().unwrap_or(-1),
            String::from_utf8_lossy(&output.stdout).into_owned(),
            String::from_utf8_lossy(&output.stderr).into_owned(),
        ))
    }
}

/// Dependency missing for a local Podman sandbox.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MissingDep {
    Platform,
    Podman,
    Crun,
    Runc,
    Runsc,
    CrunKrun,
    KvmDevice,
    KvmAccess,
}

fn binary_on_path(name: &str) -> bool {
    let Some(paths) = std::env::var_os("PATH") else {
        return false;
    };
    for dir in std::env::split_paths(&paths) {
        let candidate = dir.join(name);
        if candidate.is_file() {
            return true;
        }
    }
    for dir in ["/usr/bin", "/usr/local/bin", "/bin"] {
        if Path::new(dir).join(name).is_file() {
            return true;
        }
    }
    false
}

fn krun_runtime_present() -> bool {
    if binary_on_path("krun") {
        return true;
    }
    const CANDIDATES: &[&str] = &[
        "/usr/libexec/podman/krun",
        "/usr/lib/podman/krun",
        "/usr/lib64/crun/handlers/krun",
        "/usr/lib/crun/handlers/krun",
        "/usr/bin/crun-krun",
    ];
    CANDIDATES.iter().any(|p| Path::new(p).is_file())
}

fn missing_deps_message(runtime: &str, missing: &[MissingDep]) -> String {
    let mut lines = vec![format!(
        "cannot enable local sandbox (--{runtime}) — required tools are missing:"
    )];
    for dep in missing {
        match dep {
            MissingDep::Platform => {
                lines.push("  • platform: local sandbox requires Linux".into());
            }
            MissingDep::Podman => {
                lines.push("  • podman — not found in PATH".into());
                lines.push("      install: sudo dnf install podman   (Fedora/RHEL)".into());
                lines.push("               sudo apt install podman   (Debian/Ubuntu)".into());
            }
            MissingDep::Crun => {
                lines.push("  • crun — not found in PATH".into());
                lines.push("      install: sudo dnf install crun   (Fedora/RHEL)".into());
                lines.push("               sudo apt install crun   (Debian/Ubuntu)".into());
            }
            MissingDep::Runc => {
                lines.push("  • runc — not found in PATH".into());
                lines.push("      install: sudo dnf install runc   (Fedora/RHEL)".into());
                lines.push("               sudo apt install runc   (Debian/Ubuntu)".into());
            }
            MissingDep::Runsc => {
                lines.push("  • runsc (gVisor) — not found in PATH".into());
                lines.push(
                    "      install: see https://gvisor.dev/docs/user_guide/install/".into(),
                );
                lines.push(
                    "               then use: /sandbox local --runsc".into(),
                );
            }
            MissingDep::CrunKrun => {
                lines.push("  • crun-krun / krun runtime — not found".into());
                lines.push("      install: sudo dnf install crun-krun   (Fedora)".into());
                lines.push(
                    "               (provides `krun` for `podman --runtime=krun`)".into(),
                );
            }
            MissingDep::KvmDevice => {
                lines.push("  • /dev/kvm — not found (required for --krun)".into());
                lines.push(
                    "      enable virtualization in BIOS/firmware and load the kvm module".into(),
                );
            }
            MissingDep::KvmAccess => {
                lines.push("  • /dev/kvm — present but not accessible".into());
                lines.push("      fix: sudo usermod -aG kvm $USER   (then log out/in)".into());
            }
        }
    }
    lines.push(format!(
        "Install the missing tools, then run `/sandbox local --{runtime}` again (add --partial or --full as needed)."
    ));
    lines.push(
        "Runtimes: --runc (default, rootless containers) | --crun | --runsc (gVisor) | --krun (microVM)"
            .into(),
    );
    lines.join("\n")
}

/// Check host deps for a local Podman sandbox with the given OCI runtime.
pub async fn check_local_sandbox_deps(runtime: &str) -> Result<(), SandboxError> {
    let runtime_owned = runtime.trim().to_ascii_lowercase();
    let runtime = match runtime_owned.as_str() {
        "gvisor" => "runsc",
        other => other,
    };

    if !cfg!(target_os = "linux") {
        return Err(SandboxError::StartFailed(missing_deps_message(
            runtime,
            &[MissingDep::Platform],
        )));
    }

    let mut missing = Vec::new();

    if !binary_on_path("podman") {
        missing.push(MissingDep::Podman);
    }

    match runtime {
        "crun" => {
            if !binary_on_path("crun") {
                missing.push(MissingDep::Crun);
            }
        }
        "runc" => {
            if !binary_on_path("runc") {
                missing.push(MissingDep::Runc);
            }
        }
        "runsc" => {
            if !binary_on_path("runsc") {
                missing.push(MissingDep::Runsc);
            }
        }
        "krun" => {
            if !krun_runtime_present() {
                missing.push(MissingDep::CrunKrun);
            }
            let kvm = Path::new("/dev/kvm");
            if !kvm.exists() {
                missing.push(MissingDep::KvmDevice);
            } else if tokio::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .open(kvm)
                .await
                .is_err()
            {
                missing.push(MissingDep::KvmAccess);
            }
        }
        other => {
            return Err(SandboxError::StartFailed(format!(
                "unknown sandbox runtime '{other}' (crun|runc|runsc|krun)"
            )));
        }
    }

    if !missing.is_empty() {
        return Err(SandboxError::StartFailed(missing_deps_message(
            runtime, &missing,
        )));
    }

    let (code, _, stderr) =
        RealPodmanClient::run_capture(&["version", "--format", "{{.Client.Version}}"]).await?;
    if code != 0 {
        return Err(SandboxError::StartFailed(format!(
            "podman is installed but failed to run: {}\n{}",
            stderr.trim(),
            missing_deps_message(runtime, &[MissingDep::Podman])
        )));
    }

    Ok(())
}

/// Check deps for the krun microVM runtime.
pub async fn check_krun_deps() -> Result<(), SandboxError> {
    check_local_sandbox_deps("krun").await
}

#[async_trait]
impl PodmanClient for RealPodmanClient {
    async fn preflight(&self, runtime: &str) -> Result<(), SandboxError> {
        check_local_sandbox_deps(runtime).await
    }

    async fn run(&self, opts: PodmanRunOpts) -> Result<String, SandboxError> {
        let volume = format!(
            "{}:{}:U,z",
            opts.host_workdir.display(),
            opts.guest_workdir
        );
        let mut args: Vec<String> = vec![
            "run".into(),
            "-d".into(),
            "--runtime".into(),
            opts.runtime.clone(),
            "--name".into(),
            opts.name.clone(),
            "-v".into(),
            volume,
            "-w".into(),
            opts.guest_workdir.clone(),
        ];
        if opts.runtime == "krun" {
            args.push("--annotation".into());
            args.push(format!("krun.cpus={}", opts.cpus));
            args.push("--annotation".into());
            args.push(format!("krun.ram_mib={}", opts.ram_mib));
        }
        args.push(opts.image.clone());
        args.push("sleep".into());
        args.push("infinity".into());

        let arg_refs: Vec<&str> = args.iter().map(|s| s.as_str()).collect();
        let (code, stdout, stderr) = Self::run_capture(&arg_refs).await?;
        if code != 0 {
            return Err(SandboxError::StartFailed(format!(
                "podman run failed ({code}): {stderr}"
            )));
        }
        let id = stdout.trim().to_string();
        if id.is_empty() {
            return Err(SandboxError::StartFailed(
                "podman run returned empty container id".into(),
            ));
        }
        Ok(id)
    }

    async fn exec(&self, opts: PodmanExecOpts) -> Result<ShellOutput, ExecutionError> {
        let mut cmd = Command::new("podman");
        cmd.arg("exec").arg("-i");
        if let Some(wd) = &opts.workdir {
            cmd.arg("-w").arg(wd);
        }
        cmd.arg(&opts.container);
        for a in &opts.argv {
            cmd.arg(a);
        }
        cmd.stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);

        let mut child = cmd.spawn().map_err(|e| {
            ExecutionError::new(ExecutionErrorCode::SpawnFailed, format!("podman exec: {e}"))
        })?;

        if let Some(data) = &opts.stdin {
            if let Some(mut stdin) = child.stdin.take() {
                stdin.write_all(data).await.map_err(|e| {
                    ExecutionError::new(ExecutionErrorCode::Io, format!("stdin: {e}"))
                })?;
                drop(stdin);
            }
        } else {
            drop(child.stdin.take());
        }

        let output_fut = child.wait_with_output();
        let output = if let Some(cancel) = &opts.cancel {
            tokio::select! {
                _ = cancel.cancelled() => {
                    return Err(ExecutionError::new(
                        ExecutionErrorCode::Aborted,
                        "operation aborted",
                    ));
                }
                res = output_fut => res.map_err(|e| {
                    ExecutionError::new(ExecutionErrorCode::Io, e.to_string())
                })?,
            }
        } else if let Some(ms) = opts.timeout_ms {
            match tokio::time::timeout(Duration::from_millis(ms), output_fut).await {
                Ok(res) => res.map_err(|e| {
                    ExecutionError::new(ExecutionErrorCode::Io, e.to_string())
                })?,
                Err(_) => {
                    return Err(ExecutionError::new(
                        ExecutionErrorCode::TimedOut,
                        "command timed out",
                    ));
                }
            }
        } else {
            output_fut
                .await
                .map_err(|e| ExecutionError::new(ExecutionErrorCode::Io, e.to_string()))?
        };

        Ok(ShellOutput {
            stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
            exit_code: output.status.code().unwrap_or(-1),
        })
    }

    async fn stop(&self, container: &str) -> Result<(), SandboxError> {
        let (code, _, stderr) = Self::run_capture(&["stop", "-t", "5", container]).await?;
        if code != 0 && !stderr.contains("no such container") {
            return Err(SandboxError::Other(format!("podman stop: {stderr}")));
        }
        Ok(())
    }

    async fn rm(&self, container: &str) -> Result<(), SandboxError> {
        let (code, _, stderr) = Self::run_capture(&["rm", "-f", container]).await?;
        if code != 0 && !stderr.contains("no such container") {
            return Err(SandboxError::Other(format!("podman rm: {stderr}")));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_deps_message_includes_install_hints() {
        let msg = missing_deps_message("krun", &[MissingDep::Podman, MissingDep::CrunKrun]);
        assert!(msg.contains("podman"));
        assert!(msg.contains("dnf install podman") || msg.contains("apt install podman"));
        assert!(msg.contains("crun-krun"));
        assert!(msg.contains("/sandbox local"));
        assert!(msg.contains("--krun"));
    }

    #[test]
    fn missing_deps_message_for_crun_and_runsc() {
        let crun = missing_deps_message("crun", &[MissingDep::Crun]);
        assert!(crun.contains("crun"));
        let runsc = missing_deps_message("runsc", &[MissingDep::Runsc]);
        assert!(runsc.contains("gVisor") || runsc.contains("runsc"));
    }

    #[tokio::test]
    async fn check_krun_deps_reports_clearly_when_podman_missing() {
        let msg = missing_deps_message("runc", &[MissingDep::Podman]);
        assert!(msg.starts_with("cannot enable local sandbox"));
        assert!(msg.contains("Install the missing tools"));
    }
}
