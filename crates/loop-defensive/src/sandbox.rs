//! Podman images and containers. Host side.
//!
//! Third-party code only ever runs inside rootless podman. The network is on
//! only during `podman build` (toolchain images and dependencies); every
//! container that runs code afterwards uses `--network none` and [`LIMITS`].
//! No helper code is copied into an image: the host drives each step with
//! `podman exec`.

use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use crate::patchlib::tail;
use crate::profiles::Profile;

/// Resource limits for every container that runs repository code.
pub const LIMITS: &[&str] = &[
    "--network", "none", "--memory", "4g", "--pids-limit", "1024", "--cpus", "4", "--cap-drop",
    "all", "--security-opt", "no-new-privileges",
];

/// The limits as one line, for reports.
pub fn limits_text() -> String {
    LIMITS.join(" ")
}

/// A finished process.
#[derive(Debug, Clone, Default)]
pub struct Output {
    /// Exit code; `None` when it was killed or timed out.
    pub exit: Option<i32>,
    /// Standard output, lossily decoded.
    pub stdout: String,
    /// Standard error, lossily decoded.
    pub stderr: String,
    /// Whether the time limit was hit.
    pub timed_out: bool,
    /// Wall time.
    pub secs: f64,
}

impl Output {
    /// Exit code 0.
    pub fn ok(&self) -> bool {
        self.exit == Some(0)
    }

    /// Standard output then standard error.
    pub fn combined(&self) -> String {
        format!("{}{}", self.stdout, self.stderr)
    }
}

/// Run `argv` with optional `stdin`, killing it after `timeout`.
pub fn run(argv: &[String], cwd: Option<&Path>, stdin: Option<&str>, timeout: Duration) -> Output {
    let t0 = Instant::now();
    let mut cmd = Command::new(&argv[0]);
    cmd.args(&argv[1..])
        .stdin(if stdin.is_some() { Stdio::piped() } else { Stdio::null() })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if let Some(cwd) = cwd {
        cmd.current_dir(cwd);
    }
    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            return Output {
                exit: None,
                stderr: format!("could not run {}: {e}", argv[0]),
                secs: t0.elapsed().as_secs_f64(),
                ..Output::default()
            }
        }
    };
    if let (Some(input), Some(mut pipe)) = (stdin, child.stdin.take()) {
        let input = input.to_owned();
        std::thread::spawn(move || {
            let _ = pipe.write_all(input.as_bytes());
        });
    }
    let mut out_pipe = child.stdout.take();
    let mut err_pipe = child.stderr.take();
    let out_t = std::thread::spawn(move || {
        let mut v = Vec::new();
        if let Some(p) = out_pipe.as_mut() {
            let _ = p.read_to_end(&mut v);
        }
        v
    });
    let err_t = std::thread::spawn(move || {
        let mut v = Vec::new();
        if let Some(p) = err_pipe.as_mut() {
            let _ = p.read_to_end(&mut v);
        }
        v
    });
    let mut timed_out = false;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Some(status),
            Ok(None) if t0.elapsed() >= timeout => {
                let _ = child.kill();
                timed_out = true;
                break child.wait().ok();
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(20)),
            Err(_) => break None,
        }
    };
    let stdout = String::from_utf8_lossy(&out_t.join().unwrap_or_default()).into_owned();
    let stderr = String::from_utf8_lossy(&err_t.join().unwrap_or_default()).into_owned();
    Output {
        exit: if timed_out { None } else { status.and_then(|s| s.code()) },
        stdout,
        stderr,
        timed_out,
        secs: t0.elapsed().as_secs_f64(),
    }
}

fn strings(argv: &[&str]) -> Vec<String> {
    argv.iter().map(|s| (*s).to_owned()).collect()
}

/// `podman <args>`.
pub fn podman(args: &[&str], timeout: Duration) -> Output {
    let mut argv = vec!["podman".to_owned()];
    argv.extend(strings(args));
    run(&argv, None, None, timeout)
}

/// Remove a container, ignoring errors.
pub fn remove(name: &str) {
    let _ = podman(&["rm", "-f", "-t", "0", name], Duration::from_secs(60));
}

/// An image-name component: lower-case alphanumerics joined by single dashes
/// (a run slug like `local-.tmp` is not a valid reference otherwise).
fn image_component(name: &str) -> String {
    let mut out = String::new();
    for c in name.to_lowercase().chars() {
        if c.is_ascii_alphanumeric() {
            out.push(c);
        } else if !out.is_empty() && !out.ends_with('-') {
            out.push('-');
        }
    }
    let out = out.trim_end_matches('-').to_owned();
    if out.is_empty() {
        "task".into()
    } else {
        out
    }
}

/// `localhost/defensive-<name>:latest`.
pub fn image_tag(name: &str) -> String {
    format!("localhost/defensive-{}:latest", image_component(name))
}

/// `localhost/defensive-audit-<slug>:latest`.
pub fn audit_image_tag(slug: &str) -> String {
    format!("localhost/defensive-audit-{}:latest", image_component(slug))
}

/// The outcome of `podman build`.
#[derive(Debug, Clone)]
pub struct Built {
    /// Whether the build succeeded.
    pub ok: bool,
    /// The image tag.
    pub tag: String,
    /// The image id, when built.
    pub digest: Option<String>,
    /// The end of the build log.
    pub log_tail: String,
}

/// Copy a tree, keeping symlinks as links; `skip(rel)` leaves entries out.
pub fn copy_tree(src: &Path, dst: &Path, skip: &dyn Fn(&Path) -> bool) -> std::io::Result<()> {
    fs::create_dir_all(dst)?;
    for entry in walkdir::WalkDir::new(src).follow_links(false).min_depth(1).into_iter().filter_entry(|e| {
        e.path().strip_prefix(src).map(|rel| !skip(rel)).unwrap_or(true)
    }) {
        let entry = entry.map_err(std::io::Error::other)?;
        let rel = entry.path().strip_prefix(src).map_err(std::io::Error::other)?;
        let target = dst.join(rel);
        let ft = entry.file_type();
        if ft.is_symlink() {
            let link = fs::read_link(entry.path())?;
            let _ = fs::remove_file(&target);
            std::os::unix::fs::symlink(link, &target)?;
        } else if ft.is_dir() {
            fs::create_dir_all(&target)?;
        } else if ft.is_file() {
            fs::copy(entry.path(), &target)?;
        }
    }
    Ok(())
}

fn no_git(rel: &Path) -> bool {
    rel.file_name().is_some_and(|n| n == ".git")
}

fn build_context(ctx: &Path, tag: &str, log_file: Option<&Path>) -> Built {
    fs::write(ctx.join(".containerignore"), "**/.git\n").ok();
    let argv = strings(&["podman", "build", "--pull=missing", "-t", tag, "-f", "Containerfile", "."]);
    let r = run(&argv, Some(ctx), None, Duration::from_secs(3600));
    let log = r.combined();
    if let Some(f) = log_file {
        let _ = fs::write(f, &log);
    }
    let digest = r.ok().then(|| {
        podman(&["image", "inspect", tag, "--format", "{{.Id}}"], Duration::from_secs(60))
            .stdout
            .trim()
            .to_owned()
    });
    Built {
        ok: r.ok(),
        tag: tag.to_owned(),
        digest,
        log_tail: tail(&log, 2000),
    }
}

/// The Go task image (CVE targets and Go repositories). The oracle is
/// bind-mounted only to warm the build cache, never stored in a layer.
const GO_CONTAINERFILE: &str = r#"FROM docker.io/library/golang:{go}-bookworm
RUN printf 'Acquire::ForceIPv4 "true";\n' > /etc/apt/apt.conf.d/99force-ipv4 \
 && apt-get update && apt-get install -y --no-install-recommends patch \
 && rm -rf /var/lib/apt/lists/*
ENV GOTOOLCHAIN=local GOFLAGS=-buildvcs=false GOPATH=/go GOCACHE=/opt/gocache PATH=/go/bin:$PATH
COPY baseline/ /opt/defensive/baseline/
RUN --mount=type=bind,source=oracle,target=/tmp/oracle,ro \
    cd /opt/defensive/baseline && go mod download && (go build {pkgs} || true) \
 && mkdir -p /tmp/warm && cp -a /opt/defensive/baseline/. /tmp/warm/ \
 && cp -a /tmp/oracle/files/. /tmp/warm/ \
 && cd /tmp/warm && (go test -count=1 -run '^$' {pkgs} || true) \
 && cd / && rm -rf /tmp/warm
{analyzers}ENV GOPROXY=off GOSUMDB=off GOFLAGS="-buildvcs=false -mod=readonly"
WORKDIR /work
"#;

/// Go analyzers for repository targets; the govulncheck DB is read now,
/// because the build has network.
const GO_ANALYZERS: &str = r#"RUN (go install github.com/securego/gosec/v2/cmd/gosec@v2.21.4 || true) \
 && (go install golang.org/x/vuln/cmd/govulncheck@latest || true) \
 && cd /opt/defensive/baseline \
 && (govulncheck -json ./... > /opt/defensive/govulncheck.json 2>/dev/null || true)
"#;

/// Build the Go image from `task_dir/baseline` and `task_dir/oracle`.
pub fn build_go_image(name: &str, go_version: &str, task_dir: &Path, pkgs: &[String], analyzers: bool) -> Built {
    let tag = image_tag(name);
    let Ok(ctx) = tempfile::Builder::new().prefix("defctx-").tempdir() else {
        return failed_build(&tag, "could not create a build context");
    };
    let ctx = ctx.path();
    let staged = copy_tree(&task_dir.join("baseline"), &ctx.join("baseline"), &no_git)
        .and_then(|_| copy_tree(&task_dir.join("oracle"), &ctx.join("oracle"), &|_| false))
        .and_then(|_| {
            fs::write(
                ctx.join("Containerfile"),
                GO_CONTAINERFILE
                    .replace("{go}", go_version)
                    .replace("{pkgs}", &pkgs.join(" "))
                    .replace("{analyzers}", if analyzers { GO_ANALYZERS } else { "" }),
            )
        });
    if let Err(e) = staged {
        return failed_build(&tag, &format!("could not stage the build context: {e}"));
    }
    build_context(ctx, &tag, Some(&task_dir.join("image_build.log")))
}

const REPO_CONTAINERFILE: &str = r#"FROM {image}
{pre}
RUN printf 'Acquire::ForceIPv4 "true";\n' > /etc/apt/apt.conf.d/99force-ipv4 \
 && apt-get update && DEBIAN_FRONTEND=noninteractive apt-get install -y --no-install-recommends \
    patch git ca-certificates {apt} && rm -rf /var/lib/apt/lists/*
COPY baseline/ /opt/defensive/baseline/
# Dependencies (network on only here). Best effort: a failure is logged, and the
# grader reports what then cannot build or run.
RUN cd /opt/defensive/baseline && ( {deps} ) > /opt/defensive/deps.log 2>&1 \
    || echo "dependency install failed; see /opt/defensive/deps.log" | tee -a /opt/defensive/deps.log
RUN cd /opt/defensive/baseline && ( {analyzers} ) || true
ENV {env}
WORKDIR /work
"#;

/// The image for a non-Go repository target, from its language profile.
pub fn build_repo_image(name: &str, profile: &Profile, task_dir: &Path) -> Built {
    let tag = image_tag(name);
    let Ok(ctx) = tempfile::Builder::new().prefix("defctx-").tempdir() else {
        return failed_build(&tag, "could not create a build context");
    };
    let ctx = ctx.path();
    let mut env = vec![format!("DEF_PROFILE={}", json_str(&profile.lang))];
    env.extend(profile.env.iter().map(|(k, v)| format!("{k}={}", json_str(v))));
    let file = REPO_CONTAINERFILE
        .replace("{image}", &profile.image)
        .replace("{pre}", &profile.pre)
        .replace("{apt}", &profile.apt)
        .replace("{deps}", if profile.deps.is_empty() { "true" } else { &profile.deps })
        .replace("{analyzers}", if profile.analyzers.is_empty() { "true" } else { &profile.analyzers })
        .replace("{env}", &env.join(" "));
    let staged = copy_tree(&task_dir.join("baseline"), &ctx.join("baseline"), &no_git)
        .and_then(|_| fs::write(ctx.join("Containerfile"), file));
    if let Err(e) = staged {
        return failed_build(&tag, &format!("could not stage the build context: {e}"));
    }
    build_context(ctx, &tag, Some(&task_dir.join("image_build.log")))
}

const AUDIT_CONTAINERFILE: &str = r#"FROM docker.io/library/golang:{go}-bookworm
RUN printf 'Acquire::ForceIPv4 "true";\n' > /etc/apt/apt.conf.d/99force-ipv4 \
 && apt-get update && apt-get install -y --no-install-recommends python3-bandit ripgrep git \
 && rm -rf /var/lib/apt/lists/*
ENV GOTOOLCHAIN=local GOFLAGS=-buildvcs=false GOPATH=/go GOCACHE=/opt/gocache PATH=/go/bin:$PATH
COPY source/ /opt/audit/source/
# Static-analysis tools (network on only during build). Best effort: a tool that
# fails to install is skipped at audit time.
RUN (go install github.com/securego/gosec/v2/cmd/gosec@v2.21.4 || true) \
 && (go install golang.org/x/vuln/cmd/govulncheck@latest || true)
# govulncheck needs its online DB, so its output is captured now.
RUN cd /opt/audit/source && (test -f go.mod && go mod download || true) \
 && (test -f go.mod && govulncheck -json ./... > /opt/audit/govulncheck.json 2>/dev/null || true)
WORKDIR /opt/audit/source
"#;

/// The audit image, from a source tree staged at `src`.
pub fn build_audit_image(slug: &str, go_version: &str, src: &Path) -> Built {
    let tag = audit_image_tag(slug);
    let Ok(ctx) = tempfile::Builder::new().prefix("defaudit-").tempdir() else {
        return failed_build(&tag, "could not create a build context");
    };
    let ctx = ctx.path();
    let staged = copy_tree(src, &ctx.join("source"), &no_git)
        .and_then(|_| fs::write(ctx.join("Containerfile"), AUDIT_CONTAINERFILE.replace("{go}", go_version)));
    if let Err(e) = staged {
        return failed_build(&tag, &format!("could not stage the build context: {e}"));
    }
    build_context(ctx, &tag, None)
}

fn failed_build(tag: &str, why: &str) -> Built {
    Built {
        ok: false,
        tag: tag.to_owned(),
        digest: None,
        log_tail: why.to_owned(),
    }
}

fn json_str(s: &str) -> String {
    serde_json::to_string(s).unwrap_or_default()
}

/// A bind mount: host path, container path, `ro`/`rw`/``.
pub type Mount<'a> = (&'a Path, &'a str, &'a str);

/// A detached, network-less container; removed on drop.
#[derive(Debug)]
pub struct Container {
    /// Container name.
    pub name: String,
}

impl Container {
    /// Start `tag` running `sleep infinity` with `mounts` and `env`.
    pub fn start(name: &str, tag: &str, mounts: &[Mount<'_>], env: &[(&str, &str)]) -> Result<Self, String> {
        remove(name);
        let mut argv = strings(&["podman", "run", "-d", "--rm", "--name", name]);
        argv.extend(strings(LIMITS));
        for (k, v) in env {
            argv.push("-e".into());
            argv.push(format!("{k}={v}"));
        }
        for (host, dest, mode) in mounts {
            let host = fs::canonicalize(host).unwrap_or_else(|_| host.to_path_buf());
            argv.push("-v".into());
            argv.push(if mode.is_empty() {
                format!("{}:{dest}", host.display())
            } else {
                format!("{}:{dest}:{mode}", host.display())
            });
        }
        argv.extend(strings(&[tag, "sleep", "infinity"]));
        let r = run(&argv, None, None, Duration::from_secs(120));
        if !r.ok() {
            return Err(format!("could not start container: {}", tail(&r.stderr, 400)));
        }
        Ok(Self { name: name.to_owned() })
    }

    /// Run `argv` in `workdir` with `env`, killed inside the container after
    /// `timeout` (exit code 124 or 137 then reads as a timeout).
    pub fn exec(&self, argv: &[&str], workdir: Option<&str>, env: &[(&str, &str)], stdin: Option<&str>, timeout: Duration) -> Output {
        let mut full = strings(&["podman", "exec", "-i"]);
        if let Some(w) = workdir {
            full.push("-w".into());
            full.push(w.into());
        }
        for (k, v) in env {
            full.push("-e".into());
            full.push(format!("{k}={v}"));
        }
        full.push(self.name.clone());
        let secs = timeout.as_secs().max(1).to_string();
        full.extend(strings(&["timeout", "-k", "5", &secs]));
        full.extend(strings(argv));
        let mut r = run(&full, None, stdin, timeout + Duration::from_secs(60));
        if matches!(r.exit, Some(124) | Some(137)) {
            r.timed_out = true;
            r.exit = None;
        }
        r
    }

    /// `bash -c script` in `workdir`.
    pub fn sh(&self, script: &str, workdir: Option<&str>, env: &[(&str, &str)], timeout: Duration) -> Output {
        self.exec(&["bash", "-c", script], workdir, env, None, timeout)
    }
}

impl Drop for Container {
    fn drop(&mut self) {
        remove(&self.name);
    }
}

/// Single-quote `s` for a POSIX shell.
pub fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

/// Whether rootless podman answers.
pub fn podman_available() -> Result<(), String> {
    let r = podman(&["version", "--format", "{{.Client.Version}}"], Duration::from_secs(30));
    if r.ok() {
        Ok(())
    } else if r.stderr.contains("could not run") {
        Err("podman is not installed; /patch and /audit need rootless podman".into())
    } else {
        Err(format!("podman does not work: {}", tail(&r.stderr, 300)))
    }
}

/// The host directory as an absolute path, for mounts.
pub fn abs(p: &Path) -> PathBuf {
    fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn run_captures_exit_and_times_out() {
        let ok = run(&strings(&["sh", "-c", "echo hi; echo err >&2; exit 3"]), None, None, Duration::from_secs(10));
        assert_eq!(ok.exit, Some(3));
        assert_eq!(ok.stdout, "hi\n");
        assert_eq!(ok.stderr, "err\n");
        let slow = run(&strings(&["sleep", "5"]), None, None, Duration::from_millis(200));
        assert!(slow.timed_out && slow.exit.is_none());
        let fed = run(&strings(&["cat"]), None, Some("in"), Duration::from_secs(10));
        assert_eq!(fed.stdout, "in");
    }

    #[test]
    fn copy_tree_keeps_symlinks_and_skips() {
        let src = tempfile::tempdir().unwrap();
        fs::create_dir_all(src.path().join("a/.git")).unwrap();
        fs::write(src.path().join("a/f.txt"), "x").unwrap();
        fs::write(src.path().join("a/.git/HEAD"), "x").unwrap();
        std::os::unix::fs::symlink("/etc/passwd", src.path().join("a/link")).unwrap();
        let dst = tempfile::tempdir().unwrap();
        copy_tree(src.path(), &dst.path().join("out"), &no_git).unwrap();
        assert_eq!(fs::read_to_string(dst.path().join("out/a/f.txt")).unwrap(), "x");
        assert!(!dst.path().join("out/a/.git").exists());
        let link = dst.path().join("out/a/link");
        assert!(fs::symlink_metadata(&link).unwrap().file_type().is_symlink());
    }

    #[test]
    fn image_tags_are_valid_references() {
        assert_eq!(image_tag("repo-local-.tmp2erstg-625b6ce4"), "localhost/defensive-repo-local-tmp2erstg-625b6ce4:latest");
        assert_eq!(image_tag("CVE-2022-36009"), "localhost/defensive-cve-2022-36009:latest");
        assert_eq!(
            image_tag("repo-soketlabs-tensorstudio-platform-backend"),
            "localhost/defensive-repo-soketlabs-tensorstudio-platform-backend:latest"
        );
        assert_eq!(audit_image_tag("socket.io-x__y"), "localhost/defensive-audit-socket-io-x-y:latest");
    }

    #[test]
    fn shell_quote_escapes() {
        assert_eq!(shell_quote("a'b"), r"'a'\''b'");
    }

    #[test]
    fn repo_containerfile_has_no_python_helpers() {
        assert!(!REPO_CONTAINERFILE.contains("python3"));
        assert!(!GO_CONTAINERFILE.contains("PYTHONPATH"));
    }
}
