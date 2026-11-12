//! Thin async wrapper around the `podman` CLI for local sandboxes.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use async_trait::async_trait;
use tokio::io::AsyncWriteExt;
use tokio::process::Command;

use crate::harness::sandbox::protect::ProtectedBind;
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
    /// Guest mount point (same absolute path as the host workdir).
    pub guest_workdir: String,
    /// OCI runtime (`crun`, `runc`, `runsc`, `krun`).
    pub runtime: String,
    /// `krun.cpus` annotation value (krun only).
    pub cpus: String,
    /// `krun.ram_mib` annotation value (krun only).
    pub ram_mib: String,
    /// Extra same-path bind mounts below the workdir, parents first: the
    /// write-protected paths ([`super::protect::plan`]).
    pub binds: Vec<ProtectedBind>,
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
#[derive(Debug, Clone)]
pub struct RealPodmanClient {
    program: PathBuf,
}

impl Default for RealPodmanClient {
    fn default() -> Self {
        Self::new()
    }
}

impl RealPodmanClient {
    /// Construct a client that runs `podman` from `PATH`.
    pub fn new() -> Self {
        Self {
            program: PathBuf::from("podman"),
        }
    }

    /// Construct a client that runs `program` in place of `podman` (tests).
    pub fn with_program(program: impl Into<PathBuf>) -> Self {
        Self {
            program: program.into(),
        }
    }

    async fn run_capture(&self, args: &[&str]) -> Result<(i32, String, String), SandboxError> {
        run_capture(&self.program, args).await
    }

    /// Kill every process in `container` that carries `marker` in its
    /// environment, and every descendant of those processes. Bounded: it
    /// never waits longer than [`KILL_EXEC_TIMEOUT`]. With `look_again`, an
    /// empty first scan is repeated once after a short wait, for a stop that
    /// came so early that the command may not have started yet.
    async fn kill_marked(&self, container: &str, marker: &str, look_again: bool) {
        let mut cmd = Command::new(&self.program);
        cmd.args([
            "exec",
            container,
            "sh",
            "-c",
            KILL_MARKED_SCRIPT,
            "loop-kill",
            marker,
        ])
        .arg(if look_again { "again" } else { "once" })
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
        match tokio::time::timeout(KILL_EXEC_TIMEOUT, cmd.output()).await {
            Ok(Ok(out)) if out.status.success() => {}
            Ok(Ok(out)) => tracing::warn!(
                container,
                code = out.status.code().unwrap_or(-1),
                stderr = %String::from_utf8_lossy(&out.stderr).trim(),
                "sandbox: could not stop every process of a cancelled command"
            ),
            Ok(Err(error)) => {
                tracing::warn!(container, %error, "sandbox: could not run the kill command")
            }
            Err(_) => tracing::warn!(container, "sandbox: the kill command timed out"),
        }
    }
}

async fn run_capture(program: &Path, args: &[&str]) -> Result<(i32, String, String), SandboxError> {
    let output = match Command::new(program)
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

/// Environment variable that tags the top process of one `podman exec`, so
/// a timeout or cancel can find it inside the container. Children inherit it
/// unless they replace their environment (`env -i`, `sudo`, a program that
/// sets its own); the kill therefore also follows the parent links from the
/// tagged processes.
pub const EXEC_MARKER_VAR: &str = "LOOP_SANDBOX_EXEC";

/// Upper bound for the in-container kill after a timeout or cancel. It stays
/// below the 5 s shutdown grace.
const KILL_EXEC_TIMEOUT: Duration = Duration::from_secs(3);

/// A stop this soon after the spawn may come before the command runs in the
/// container; the kill then looks a second time if it finds nothing.
const EARLY_STOP: Duration = Duration::from_secs(1);

/// Runs in the container as `sh -c SCRIPT loop-kill VAR=ID once|again`.
/// Needs only `/proc`, `grep` and `sed` (no `ps` or `pkill`).
///
/// 1. Targets: the processes whose environment holds `VAR=ID`, then every
///    descendant of them through the `PPid:` lines of `/proc/*/status`,
///    repeated until no pid is added. This is done before any signal: a
///    killed parent hands its children to PID 1 and the link is lost.
/// 2. TERM to the targets; wait up to 0.5 s.
/// 3. KILL to what is still alive (old targets and new ones) until none is
///    left. A zombie is not alive: it has nothing left to stop.
const KILL_MARKED_SCRIPT: &str = r#"m="$1"
if printf 'a\0' | grep -qzxF a 2>/dev/null; then
  marked() { grep -lzxsF -- "$m" /proc/[0-9]*/environ 2>/dev/null | sed -n 's#^/proc/\([0-9]*\)/environ$#\1#p'; }
else
  marked() {
    for d in /proc/[0-9]*; do
      tr '\0' '\n' < "$d/environ" 2>/dev/null | grep -qxF -- "$m" && echo "${d#/proc/}"
    done
  }
fi
targets() {
  set_=" $(marked | tr '\n' ' ')"
  [ "$set_" = " " ] && return 0
  tree=$(grep -s '^PPid:' /proc/[0-9]*/status | sed -n 's#^/proc/\([0-9]*\)/status:PPid:[[:space:]]*\([0-9]*\)$#\1 \2#p')
  while :; do
    added=
    while read -r pid ppid; do
      case "$set_" in *" $ppid "*) case "$set_" in *" $pid "*) ;; *) set_="$set_$pid "; added=1 ;; esac ;; esac
    done <<EOF
$tree
EOF
    [ -n "$added" ] || break
  done
  echo $set_
}
alive() {
  for x in $1; do
    [ -r "/proc/$x/status" ] || continue
    grep -qs '^State:[[:space:]]*Z' "/proc/$x/status" && continue
    printf '%s ' "$x"
  done
}
p=$(targets)
if [ -z "$p" ] && [ "$2" = again ]; then sleep 0.3; p=$(targets); fi
[ -n "$p" ] || exit 0
kill -TERM $p 2>/dev/null
i=0
while [ $i -lt 5 ]; do
  sleep 0.1
  p=$(alive "$p $(targets)")
  [ -n "$p" ] || exit 0
  i=$((i+1))
done
i=0
while [ $i -lt 30 ]; do
  kill -KILL $p 2>/dev/null
  sleep 0.05
  p=$(alive "$p $(targets)")
  [ -n "$p" ] || exit 0
  i=$((i+1))
done
echo "still running: $p" >&2
exit 1
"#;

/// Why [`wait_or_stop`] stopped waiting.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Stopped {
    Cancelled,
    TimedOut,
}

impl Stopped {
    fn into_error(self) -> ExecutionError {
        match self {
            Self::Cancelled => {
                ExecutionError::new(ExecutionErrorCode::Aborted, "operation aborted")
            }
            Self::TimedOut => {
                ExecutionError::new(ExecutionErrorCode::TimedOut, "command timed out")
            }
        }
    }
}

/// Wait for `fut`, or stop at the first of `cancel` and `timeout_ms`. Both
/// apply together. On a stop, `fut` is left as it is, so the caller decides
/// when to drop it.
async fn wait_or_stop<F: std::future::Future>(
    fut: F,
    cancel: Option<&tokio_util::sync::CancellationToken>,
    timeout_ms: Option<u64>,
) -> Result<F::Output, Stopped> {
    let cancelled = async {
        match cancel {
            Some(token) => token.cancelled().await,
            None => std::future::pending().await,
        }
    };
    let timed_out = async {
        match timeout_ms {
            Some(ms) => tokio::time::sleep(Duration::from_millis(ms)).await,
            None => std::future::pending().await,
        }
    };
    tokio::select! {
        // A cancel wins over a timeout that is due at the same time.
        biased;
        _ = cancelled => Err(Stopped::Cancelled),
        _ = timed_out => Err(Stopped::TimedOut),
        out = fut => Ok(out),
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
    if missing.contains(&MissingDep::Platform) {
        let message = format!(
            "cannot enable local sandbox (--{runtime}) — Linux only (macOS support is planned)."
        );
        #[cfg(target_os = "macos")]
        let message = format!(
            "{message} On macOS, use `--permission-mode read-only` or `--permission-mode ask` to control tool permissions."
        );
        return message;
    }

    let mut lines = vec![format!(
        "cannot enable local sandbox (--{runtime}) — required tools are missing:"
    )];
    for dep in missing {
        match dep {
            MissingDep::Platform => unreachable!("unsupported platforms return above"),
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
                lines.push("      install: see https://gvisor.dev/docs/user_guide/install/".into());
                lines.push("               then use: /sandbox local --runsc".into());
            }
            MissingDep::CrunKrun => {
                lines.push("  • crun-krun / krun runtime — not found".into());
                lines.push("      install: sudo dnf install crun-krun   (Fedora)".into());
                lines.push("               (provides `krun` for `podman --runtime=krun`)".into());
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

/// Message for a `podman` that is installed but does not run. It names the
/// real cause and gives no "not found" or install hints.
fn podman_failed_message(runtime: &str, code: i32, stderr: &str) -> String {
    let detail = stderr.trim();
    let detail = if detail.is_empty() {
        format!("`podman version` exited with code {code} and printed no error")
    } else {
        detail.to_string()
    };
    [
        format!(
            "cannot enable local sandbox (--{runtime}) — podman is installed but failed to run:"
        ),
        format!("  {}", detail.replace('\n', "\n  ")),
        "Check that rootless Podman works for this user: run `podman info`.".into(),
        format!("Then run `/sandbox local --{runtime}` again."),
    ]
    .join("\n")
}

/// Check host deps for a local Podman sandbox with the given OCI runtime.
pub async fn check_local_sandbox_deps(runtime: &str) -> Result<(), SandboxError> {
    check_local_sandbox_deps_with(Path::new("podman"), runtime).await
}

/// [`check_local_sandbox_deps`] with `program` in place of `podman`.
async fn check_local_sandbox_deps_with(program: &Path, runtime: &str) -> Result<(), SandboxError> {
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

    let podman_present = if program.components().count() > 1 {
        program.is_file()
    } else {
        binary_on_path(&program.to_string_lossy())
    };
    if !podman_present {
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
        run_capture(program, &["version", "--format", "{{.Client.Version}}"]).await?;
    if code != 0 {
        return Err(SandboxError::StartFailed(podman_failed_message(
            runtime, code, &stderr,
        )));
    }

    Ok(())
}

/// Check deps for the krun microVM runtime.
pub async fn check_krun_deps() -> Result<(), SandboxError> {
    check_local_sandbox_deps("krun").await
}

/// Arguments of `podman run` for `opts`.
fn run_args(opts: &PodmanRunOpts) -> Result<Vec<String>, SandboxError> {
    let volume = format!("{}:{}:U,z", opts.host_workdir.display(), opts.guest_workdir);
    let mut args: Vec<String> = vec![
        "run".into(),
        "-d".into(),
        "--replace".into(),
        "--rm".into(),
        "--runtime".into(),
        opts.runtime.clone(),
        "--name".into(),
        opts.name.clone(),
        "-v".into(),
        volume,
    ];
    // Same path in the guest. `U` (chown) stays on the workdir mount only.
    for bind in &opts.binds {
        let path = bind
            .path
            .to_str()
            .filter(|p| !p.contains(':'))
            .ok_or_else(|| {
                SandboxError::StartFailed(format!(
                    "cannot start the sandbox: cannot mount {} (the path must be UTF-8 without ':')",
                    bind.path.display()
                ))
            })?;
        args.push("-v".into());
        args.push(if bind.read_only {
            format!("{path}:{path}:ro")
        } else {
            format!("{path}:{path}")
        });
    }
    args.push("-w".into());
    args.push(opts.guest_workdir.clone());
    if opts.runtime == "krun" {
        args.push("--annotation".into());
        args.push(format!("krun.cpus={}", opts.cpus));
        args.push("--annotation".into());
        args.push(format!("krun.ram_mib={}", opts.ram_mib));
    }
    args.push(opts.image.clone());
    args.push("sleep".into());
    args.push("infinity".into());
    Ok(args)
}

#[async_trait]
impl PodmanClient for RealPodmanClient {
    async fn preflight(&self, runtime: &str) -> Result<(), SandboxError> {
        check_local_sandbox_deps_with(&self.program, runtime).await
    }

    async fn run(&self, opts: PodmanRunOpts) -> Result<String, SandboxError> {
        let args = run_args(&opts)?;
        let arg_refs: Vec<&str> = args.iter().map(|s| s.as_str()).collect();
        let (code, stdout, stderr) = self.run_capture(&arg_refs).await?;
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
        // A command that can be stopped gets a tag, so a stop can find all
        // its processes in the container: killing the host `podman exec`
        // client does not stop them.
        let marker = (opts.timeout_ms.is_some() || opts.cancel.is_some())
            .then(|| format!("{EXEC_MARKER_VAR}={}", loop_ai::new_id().replace('-', "")));
        let mut cmd = Command::new(&self.program);
        cmd.arg("exec").arg("-i");
        if let Some(marker) = &marker {
            cmd.arg("-e").arg(marker);
        }
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

        let spawned = std::time::Instant::now();
        let stdin = child.stdin.take();
        let output = async move {
            if let (Some(data), Some(mut stdin)) = (&opts.stdin, stdin) {
                stdin.write_all(data).await.map_err(|e| {
                    ExecutionError::new(ExecutionErrorCode::Io, format!("stdin: {e}"))
                })?;
            }
            child
                .wait_with_output()
                .await
                .map_err(|e| ExecutionError::new(ExecutionErrorCode::Io, e.to_string()))
        };
        tokio::pin!(output);
        let output = match wait_or_stop(&mut output, opts.cancel.as_ref(), opts.timeout_ms).await {
            Ok(output) => output?,
            Err(stopped) => {
                // Stop the command in the container first, while the host
                // client still runs: no process loses its parent link before
                // the kill has collected it. The client is dropped (killed)
                // when this returns.
                if let Some(marker) = &marker {
                    let early = spawned.elapsed() < EARLY_STOP;
                    self.kill_marked(&opts.container, marker, early).await;
                }
                return Err(stopped.into_error());
            }
        };

        Ok(ShellOutput {
            stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
            exit_code: output.status.code().unwrap_or(-1),
            truncated: false,
        })
    }

    async fn stop(&self, container: &str) -> Result<(), SandboxError> {
        // `-t 0`: the keep-alive `sleep infinity` is PID 1 and ignores
        // SIGTERM, so any grace period is only waited out.
        let (code, _, stderr) = self.run_capture(&["stop", "-t", "0", container]).await?;
        if code != 0 && !stderr.contains("no such container") {
            return Err(SandboxError::Other(format!("podman stop: {stderr}")));
        }
        Ok(())
    }

    async fn rm(&self, container: &str) -> Result<(), SandboxError> {
        // `-t 0`: the keep-alive `sleep infinity` is PID 1 and ignores
        // SIGTERM, so a graceful stop only waits for the timeout.
        let (mut code, _, mut stderr) = self
            .run_capture(&["rm", "-f", "-t", "0", container])
            .await?;
        if code != 0 && stderr.contains("unknown shorthand flag") {
            // Podman before 3.2 has no `rm --time`.
            let _ = self.run_capture(&["kill", container]).await;
            (code, _, stderr) = self.run_capture(&["rm", "-f", container]).await?;
        }
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
    fn unsupported_platform_message_is_actionable() {
        let msg = missing_deps_message("runc", &[MissingDep::Platform]);
        assert!(msg.contains("Linux only (macOS support is planned)"));
        assert!(!msg.contains("required tools are missing"));
        assert!(!msg.contains("Install the missing tools"));
        assert!(!msg.contains("/sandbox local --runc"));
        #[cfg(target_os = "macos")]
        {
            assert!(msg.contains("--permission-mode read-only"));
            assert!(msg.contains("--permission-mode ask"));
        }
    }

    #[cfg(not(target_os = "linux"))]
    #[tokio::test]
    async fn local_sandbox_preflight_reports_unsupported_platform() {
        let err = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            check_local_sandbox_deps("runc"),
        )
        .await
        .expect("preflight timed out")
        .expect_err("local sandbox requires Linux");
        assert!(err
            .to_string()
            .contains(&missing_deps_message("runc", &[MissingDep::Platform],)));
    }

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

    #[test]
    fn podman_that_fails_to_run_is_not_reported_as_missing() {
        let msg = podman_failed_message("runc", 3, "stub: podman is broken\n");
        assert!(
            msg.contains("podman is installed but failed to run"),
            "{msg}"
        );
        assert!(msg.contains("stub: podman is broken"), "{msg}");
        assert!(!msg.contains("not found"), "{msg}");
        assert!(!msg.contains("required tools are missing"), "{msg}");
        assert!(!msg.contains("install podman"), "{msg}");
        let silent = podman_failed_message("crun", 125, "  ");
        assert!(silent.contains("exited with code 125"), "{silent}");
        assert!(silent.contains("--crun"), "{silent}");
    }

    #[tokio::test]
    async fn wait_or_stop_applies_the_timeout_when_a_cancel_token_is_set() {
        let token = tokio_util::sync::CancellationToken::new();
        let started = std::time::Instant::now();
        let out = wait_or_stop(
            tokio::time::sleep(Duration::from_secs(30)),
            Some(&token),
            Some(100),
        )
        .await;
        assert_eq!(out.unwrap_err(), Stopped::TimedOut);
        assert!(started.elapsed() < Duration::from_secs(5));
    }

    #[tokio::test]
    async fn wait_or_stop_applies_a_cancel_when_a_timeout_is_set() {
        let token = tokio_util::sync::CancellationToken::new();
        let later = token.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(50)).await;
            later.cancel();
        });
        let out = wait_or_stop(
            tokio::time::sleep(Duration::from_secs(30)),
            Some(&token),
            Some(20_000),
        )
        .await;
        assert_eq!(out.unwrap_err(), Stopped::Cancelled);
        assert_eq!(
            Stopped::Cancelled.into_error().code,
            ExecutionErrorCode::Aborted
        );
        // The bash tool reports a timeout by matching this text.
        assert!(Stopped::TimedOut
            .into_error()
            .to_string()
            .contains("timed out"));
    }

    #[tokio::test]
    async fn wait_or_stop_returns_the_output_when_it_finishes_first() {
        let token = tokio_util::sync::CancellationToken::new();
        assert_eq!(
            wait_or_stop(async { 7 }, Some(&token), Some(1)).await,
            Ok(7)
        );
        assert_eq!(wait_or_stop(async { 8 }, None, None).await, Ok(8));
    }

    /// A stand-in `podman`: `exec` runs the command on the host with the
    /// `-e` variables set, so the in-container kill script runs on the host
    /// too. Every call is logged.
    #[cfg(unix)]
    fn fake_podman(dir: &Path) -> (PathBuf, PathBuf) {
        use std::os::unix::fs::PermissionsExt;
        let log = dir.join("calls.log");
        let program = dir.join("podman");
        let script = format!(
            r#"#!/bin/sh
printf '%s\n' "$*" >> '{log}'
[ "$1" = exec ] || exit 0
shift
while :; do
  case "$1" in
    -i) shift ;;
    -e) export "$2"; shift 2 ;;
    -w) cd "$2" || exit 125; shift 2 ;;
    *) break ;;
  esac
done
shift
exec "$@"
"#,
            log = log.display()
        );
        std::fs::write(&program, script).unwrap();
        std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o755)).unwrap();
        (program, log)
    }

    #[cfg(unix)]
    fn shell(command: &str, timeout_ms: Option<u64>) -> PodmanExecOpts {
        PodmanExecOpts {
            container: "c1".into(),
            workdir: None,
            argv: vec!["sh".into(), "-c".into(), command.into()],
            stdin: None,
            timeout_ms,
            cancel: Some(tokio_util::sync::CancellationToken::new()),
        }
    }

    /// The marker a stoppable exec ran with, from the fake's call log.
    #[cfg(unix)]
    fn exec_marker(log: &Path) -> String {
        let calls = std::fs::read_to_string(log).unwrap();
        let first = calls.lines().next().unwrap();
        let marker = first
            .split(' ')
            .find(|w| w.starts_with(EXEC_MARKER_VAR))
            .unwrap_or_else(|| panic!("no marker in {first:?}"));
        marker.to_string()
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn real_client_exec_times_out_with_a_cancel_token() {
        let dir = tempfile::tempdir().unwrap();
        let (program, log) = fake_podman(dir.path());
        let client = RealPodmanClient::with_program(&program);
        let started = std::time::Instant::now();
        let err = client
            // `exec`: the host client is the `sleep` itself, so dropping it
            // leaves nothing running where the kill script finds no `/proc`.
            .exec(shell("exec sleep 25", Some(1000)))
            .await
            .expect_err("the command must time out");
        assert_eq!(err.code, ExecutionErrorCode::TimedOut);
        assert!(
            started.elapsed() < Duration::from_secs(8),
            "took {:?}",
            started.elapsed()
        );
        // The stop also ran the kill for this command's marker in the container.
        let marker = exec_marker(&log);
        let calls = std::fs::read_to_string(&log).unwrap();
        // The kill script spans lines in the log; its last argument is the marker.
        let kill = calls
            .find("exec c1 sh -c ")
            .unwrap_or_else(|| panic!("no kill exec in {calls}"));
        assert!(
            calls[kill..].contains(&format!("loop-kill {marker}")),
            "{calls}"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn real_client_exec_without_stop_has_no_marker_and_keeps_output() {
        let dir = tempfile::tempdir().unwrap();
        let (program, log) = fake_podman(dir.path());
        let client = RealPodmanClient::with_program(&program);
        let out = client
            .exec(PodmanExecOpts {
                container: "c1".into(),
                workdir: None,
                argv: vec!["sh".into(), "-c".into(), "cat; exit 4".into()],
                stdin: Some(b"piped".to_vec()),
                timeout_ms: None,
                cancel: None,
            })
            .await
            .unwrap();
        assert_eq!(out.stdout, "piped");
        assert_eq!(out.exit_code, 4);
        let calls = std::fs::read_to_string(&log).unwrap();
        assert!(!calls.contains(EXEC_MARKER_VAR), "{calls}");
    }

    /// Processes on this machine whose environment holds `marker`.
    #[cfg(target_os = "linux")]
    fn marked_processes(marker: &str) -> Vec<String> {
        let mut found = Vec::new();
        for entry in std::fs::read_dir("/proc").unwrap().flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            if !name.chars().all(|c| c.is_ascii_digit()) {
                continue;
            }
            let Ok(env) = std::fs::read(entry.path().join("environ")) else {
                continue;
            };
            if env.split(|b| *b == 0).any(|v| v == marker.as_bytes()) {
                found.push(name);
            }
        }
        found
    }

    /// Live processes on this machine whose command line is `cmdline`.
    #[cfg(target_os = "linux")]
    fn processes_named(cmdline: &str) -> Vec<String> {
        let mut found = Vec::new();
        for entry in std::fs::read_dir("/proc").unwrap().flatten() {
            let Ok(raw) = std::fs::read(entry.path().join("cmdline")) else {
                continue;
            };
            let text = String::from_utf8_lossy(&raw).replace('\0', " ");
            if text.trim_end() == cmdline {
                found.push(entry.file_name().to_string_lossy().into_owned());
            }
        }
        found
    }

    /// On Linux the fake runs the real kill script against `/proc`: after a
    /// timeout and after a cancel, no process of the command is left, also a
    /// background child, one in its own session and one that ignores TERM.
    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn real_client_stop_leaves_no_process_of_the_command() {
        let command = "sleep 301 & (trap '' TERM; sleep 302) & \
                       if command -v setsid >/dev/null; then setsid sleep 303 & fi; \
                       env -i sleep 305 & sleep 304";
        for cancel in [false, true] {
            let dir = tempfile::tempdir().unwrap();
            let (program, log) = fake_podman(dir.path());
            let client = RealPodmanClient::with_program(&program);
            let mut opts = shell(command, (!cancel).then_some(1500));
            if cancel {
                let token = opts.cancel.clone().unwrap();
                tokio::spawn(async move {
                    tokio::time::sleep(Duration::from_millis(1500)).await;
                    token.cancel();
                });
            }
            let err = client.exec(opts).await.expect_err("stopped");
            let expected = if cancel {
                ExecutionErrorCode::Aborted
            } else {
                ExecutionErrorCode::TimedOut
            };
            assert_eq!(err.code, expected);
            let marker = exec_marker(&log);
            let left = marked_processes(&marker);
            assert!(left.is_empty(), "cancel={cancel}: still running: {left:?}");
            // `env -i` drops the marker; the kill found it as a descendant.
            let unmarked = processes_named("sleep 305");
            assert!(
                unmarked.is_empty(),
                "cancel={cancel}: env -i child left: {unmarked:?}"
            );
        }
    }

    fn run_opts(binds: Vec<ProtectedBind>) -> PodmanRunOpts {
        PodmanRunOpts {
            name: "loop-sandbox-x".into(),
            image: "img".into(),
            host_workdir: PathBuf::from("/p"),
            guest_workdir: "/p".into(),
            runtime: "runc".into(),
            cpus: "2".into(),
            ram_mib: "2048".into(),
            binds,
        }
    }

    #[test]
    fn run_mounts_protected_paths_at_the_same_path_after_the_workdir() {
        let args = run_args(&run_opts(vec![
            ProtectedBind {
                path: PathBuf::from("/p/.git"),
                read_only: false,
            },
            ProtectedBind {
                path: PathBuf::from("/p/.git/hooks"),
                read_only: true,
            },
        ]))
        .unwrap();
        let line = args.join(" ");
        assert!(
            line.contains(
                "-v /p:/p:U,z -v /p/.git:/p/.git -v /p/.git/hooks:/p/.git/hooks:ro -w /p"
            ),
            "{line}"
        );
        assert!(line.ends_with("img sleep infinity"), "{line}");
    }

    #[test]
    fn run_refuses_a_protected_path_podman_cannot_parse() {
        let err = run_args(&run_opts(vec![ProtectedBind {
            path: PathBuf::from("/p/.git/modules/a:b"),
            read_only: false,
        }]))
        .unwrap_err();
        assert!(err.to_string().contains("without ':'"), "{err}");
    }

    #[tokio::test]
    async fn check_krun_deps_reports_clearly_when_podman_missing() {
        let msg = missing_deps_message("runc", &[MissingDep::Podman]);
        assert!(msg.starts_with("cannot enable local sandbox"));
        assert!(msg.contains("Install the missing tools"));
    }
}
