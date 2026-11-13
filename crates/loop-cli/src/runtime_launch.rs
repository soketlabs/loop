//! Discovery and detached startup for the default interactive Runtime client.
use loop_protocol::AgentBackend;
use loop_wire::RemoteBackend;
use std::{
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

const CONNECT_WAIT: Duration = Duration::from_secs(15);

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Target {
    Unix(PathBuf),
    WebSocket(String),
}
impl Target {
    pub fn parse(value: &str) -> Self {
        if value.starts_with("ws://") || value.starts_with("wss://") {
            Self::WebSocket(value.into())
        } else {
            Self::Unix(value.into())
        }
    }
    pub fn label(&self) -> String {
        match self {
            Self::Unix(p) => p.display().to_string(),
            Self::WebSocket(s) => s.clone(),
        }
    }
}

/// Current discovery has one location. Keep selection independent of that
/// inventory so additional registries can supply the same reachable targets.
pub async fn discover(agent_dir: &Path) -> anyhow::Result<Vec<Target>> {
    let socket = crate::config::paths::runtime_socket_path(agent_dir)?;
    if tokio::net::UnixStream::connect(&socket).await.is_err() {
        return Ok(vec![]);
    }
    let target = Target::Unix(socket);
    let remote = tokio::time::timeout(
        Duration::from_secs(2),
        connect(
            &target,
            None,
            Arc::new(crate::surface_client::RemoteHost::default()),
        ),
    )
    .await
    .map_err(|_| {
        anyhow::anyhow!("Runtime discovery handshake timed out; check the socket or use --connect")
    })??;
    remote.disconnect();
    Ok(vec![target])
}

pub fn select(
    targets: &[Target],
    mut pick: impl FnMut(&[Target]) -> anyhow::Result<usize>,
) -> anyhow::Result<Option<Target>> {
    match targets {
        [] => Ok(None),
        [one] => Ok(Some(one.clone())),
        many => many
            .get(pick(many)?)
            .cloned()
            .map(Some)
            .ok_or_else(|| anyhow::anyhow!("invalid Runtime selection")),
    }
}

/// Read one bounded numeric selection; EOF and invalid input are explicit errors.
pub fn read_choice(mut input: impl std::io::BufRead, count: usize) -> anyhow::Result<usize> {
    let mut answer = String::new();
    if input.read_line(&mut answer)? == 0 {
        anyhow::bail!("selection input closed");
    }
    let choice = answer
        .trim()
        .parse::<usize>()
        .ok()
        .filter(|n| *n >= 1 && *n <= count)
        .ok_or_else(|| anyhow::anyhow!("invalid selection; enter a number from 1 to {count}"))?;
    Ok(choice - 1)
}

pub fn pick(targets: &[Target]) -> anyhow::Result<usize> {
    use std::io::{IsTerminal, Write};
    if !std::io::stdin().is_terminal() {
        return Ok(0);
    }
    eprintln!("Choose a Runtime:");
    for (i, target) in targets.iter().enumerate() {
        eprintln!("  {}. {}", i + 1, target.label());
    }
    eprint!("> ");
    std::io::stderr().flush()?;
    read_choice(std::io::stdin().lock(), targets.len())
}

/// `--token-file` wins over `LOOP_RUNTIME_TOKEN`; one of them is required.
fn websocket_token(token_file: Option<&Path>, env_token: Option<&str>) -> anyhow::Result<String> {
    match token_file {
        Some(path) => Ok(loop_wire::token::read(path)?),
        None => env_token
            .filter(|v| !v.trim().is_empty())
            .map(str::to_owned)
            .ok_or_else(|| {
                anyhow::anyhow!("WebSocket connections require LOOP_RUNTIME_TOKEN or --token-file")
            }),
    }
}

/// One plain sentence for an explicit `--connect` target that cannot be used.
pub fn explicit_connect_error(target: &Target, error: &anyhow::Error) -> anyhow::Error {
    let reason = error.to_string();
    let reason = if reason.contains("LOOP_RUNTIME_TOKEN") {
        "missing token, pass --token-file or set LOOP_RUNTIME_TOKEN".to_owned()
    } else {
        reason
    };
    anyhow::anyhow!("could not connect to {}: {reason}", target.label())
}

pub async fn connect(
    target: &Target,
    token_file: Option<&Path>,
    host: Arc<dyn loop_protocol::HostServices>,
) -> anyhow::Result<Arc<RemoteBackend>> {
    tokio::time::timeout(CONNECT_WAIT, async {
        let transport = match target {
            Target::Unix(socket) => {
                loop_wire::socket::unix(tokio::net::UnixStream::connect(socket).await?)
            }
            Target::WebSocket(url) => {
                let token = websocket_token(
                    token_file,
                    std::env::var("LOOP_RUNTIME_TOKEN").ok().as_deref(),
                )?;
                loop_wire::socket::websocket_connect(url, token.trim()).await?
            }
        };
        Ok::<_, anyhow::Error>(crate::surface_client::connect(transport, host).await?)
    })
    .await
    .map_err(|_| anyhow::anyhow!("Runtime connection timed out"))?
}

/// Limit WebSocket environment forwarding to portable context and explicit names.
pub fn caller_environment(
    websocket: bool,
    forward: &[String],
    vars: impl IntoIterator<Item = (String, String)>,
) -> std::collections::BTreeMap<String, String> {
    vars.into_iter()
        .filter(|(key, _)| {
            !websocket
                || matches!(
                    key.as_str(),
                    "PATH"
                        | "HOME"
                        | "USER"
                        | "SHELL"
                        | "LANG"
                        | "TERM"
                        | "TMPDIR"
                        | "VIRTUAL_ENV"
                        | "CONDA_PREFIX"
                )
                || key.starts_with("LC_")
                || forward.contains(key)
        })
        .collect()
}

/// Names a Surface forwards over WebSocket beyond the built-in allowlist:
/// `runtime.forwardEnv`, and `tools.passEnv`, since a name there is an explicit
/// choice to keep that variable in tool environments.
fn forward_names(settings: &crate::config::Settings) -> Vec<String> {
    settings
        .runtime
        .forward_env
        .iter()
        .chain(&settings.tools.pass_env)
        .cloned()
        .collect()
}

/// Capture this Surface's environment under its local forwarding policy.
pub fn session_environment(
    remote: &RemoteBackend,
    agent_dir: &Path,
) -> anyhow::Result<std::collections::BTreeMap<String, String>> {
    let settings =
        crate::config::Settings::load_file(&crate::config::paths::settings_path(agent_dir))?;
    Ok(caller_environment(
        remote.is_websocket(),
        &forward_names(&settings),
        std::env::vars(),
    ))
}

/// `setsid` plus closed stdin separates the owner from this terminal's lifetime.
pub fn spawn(
    executable: &Path,
    agent_dir: &Path,
    cwd: &Path,
    socket: &Path,
) -> anyhow::Result<std::process::Child> {
    spawn_owner(executable, agent_dir, cwd, socket, true)
}

fn spawn_owner(
    executable: &Path,
    agent_dir: &Path,
    cwd: &Path,
    socket: &Path,
    auto_started: bool,
) -> anyhow::Result<std::process::Child> {
    use std::os::unix::process::CommandExt;
    crate::config::paths::ensure_agent_dirs(agent_dir)?;
    let (log, _) = crate::config::paths::open_runtime_log(agent_dir)?;
    let mut command = std::process::Command::new(executable);
    command
        .args(["runtime", "start", "--socket"])
        .arg(socket)
        .arg("--cwd")
        .arg(cwd)
        .env("LOOP_CODING_AGENT_DIR", agent_dir)
        .stdin(std::process::Stdio::null())
        .stdout(log.try_clone()?)
        .stderr(log);
    if auto_started {
        command.arg("--auto-started");
    }
    // SAFETY: setsid is async-signal-safe and uses no Rust locks after fork.
    unsafe {
        command.pre_exec(|| {
            if libc::setsid() == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    Ok(command.spawn()?)
}

pub async fn start_and_connect(
    executable: &Path,
    agent_dir: &Path,
    cwd: &Path,
    socket: &Path,
    host: Arc<dyn loop_protocol::HostServices>,
) -> anyhow::Result<Arc<RemoteBackend>> {
    start_owner(executable, agent_dir, cwd, socket, host, true).await
}

async fn start_owner(
    executable: &Path,
    agent_dir: &Path,
    cwd: &Path,
    socket: &Path,
    host: Arc<dyn loop_protocol::HostServices>,
    auto_started: bool,
) -> anyhow::Result<Arc<RemoteBackend>> {
    let mut child = if loop_app_core::runtime_instance::RuntimeInstanceLock::is_held(agent_dir)? {
        None
    } else {
        Some(
            spawn_owner(executable, agent_dir, cwd, socket, auto_started).map_err(|error| {
                anyhow::anyhow!(
                    "could not start Runtime: {error}; see {}",
                    agent_dir.join("logs/runtime.log").display()
                )
            })?,
        )
    };
    let result = tokio::time::timeout(CONNECT_WAIT, async {
        loop {
            if let Ok(remote) = connect(&Target::Unix(socket.into()), None, host.clone()).await {
                break Ok(remote);
            }
            // Even a cleanly exited contender may have lost to a starting
            // owner. Keep waiting for that owner's socket under one deadline.
            if let Some(child) = &mut child {
                let _ = child.try_wait()?;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .map_err(|_| anyhow::anyhow!("Runtime did not start; see logs/runtime.log"))
    .and_then(|result| result);
    // Reap a child owned by this client without making it a Tokio child that
    // could be killed when the Surface exits.
    if let Some(mut child) = child {
        std::thread::spawn(move || {
            let _ = child.wait();
        });
    }
    result
}

/// An explicit restart creates a detached owner without an idle deadline.
pub async fn restart(socket: Option<PathBuf>, cwd: &Path) -> anyhow::Result<()> {
    let agent_dir = crate::config::paths::get_agent_dir();
    let socket = match socket {
        Some(socket) => socket,
        None => crate::config::paths::runtime_socket_path(&agent_dir)?,
    };
    crate::runtime_cmd::stop(Some(socket.clone())).await?;
    let remote = start_owner(
        &std::env::current_exe()?,
        &agent_dir,
        cwd,
        &socket,
        Arc::new(crate::surface_client::RemoteHost::default()),
        false,
    )
    .await?;
    eprintln!("Runtime restarted (pid {}).", remote.status().await?.pid);
    Ok(())
}

/// Check before opening or attaching any Session. Never stop a foreign host.
pub async fn compatible(remote: &RemoteBackend) -> anyhow::Result<bool> {
    if remote.server().version == env!("CARGO_PKG_VERSION") {
        return Ok(true);
    }
    if replaceable(&remote.status().await?) {
        return Ok(false);
    }
    anyhow::bail!(
        "Runtime is v{}, this loop is v{}; run `loop runtime restart` when its Sessions are done",
        remote.server().version,
        env!("CARGO_PKG_VERSION")
    )
}

fn replaceable(status: &loop_protocol::rpc::RuntimeStatus) -> bool {
    status.auto_started && status.loaded == 0
}

pub async fn launch(
    explicit: Option<&str>,
    token_file: Option<&Path>,
    agent_dir: &Path,
    cwd: &Path,
    host: Arc<dyn loop_protocol::HostServices>,
) -> anyhow::Result<Arc<RemoteBackend>> {
    let target = match explicit {
        Some(value) => Some(Target::parse(value)),
        None => {
            let targets = discover(agent_dir).await?;
            tokio::task::spawn_blocking(move || select(&targets, pick)).await??
        }
    };
    let executable = std::env::current_exe()?;
    let remote = match &target {
        Some(target) => connect(target, token_file, host.clone())
            .await
            .map_err(|error| explicit_connect_error(target, &error))?,
        None => {
            start_and_connect(
                &executable,
                agent_dir,
                cwd,
                &crate::config::paths::runtime_socket_path(agent_dir)?,
                host.clone(),
            )
            .await?
        }
    };
    if compatible(&remote).await? {
        if !remote.capabilities().session_environment {
            anyhow::bail!("Runtime does not accept per-Session environments");
        }
        return Ok(remote);
    }
    let socket = match target {
        Some(Target::WebSocket(_)) => anyhow::bail!(
            "Runtime is v{}, this loop is v{}; restart the Runtime on its host",
            remote.server().version,
            env!("CARGO_PKG_VERSION")
        ),
        Some(Target::Unix(socket)) => socket,
        None => crate::config::paths::runtime_socket_path(agent_dir)?,
    };
    // Only restart the local owner belonging to this agent directory.
    if socket != crate::config::paths::runtime_socket_path(agent_dir)? {
        anyhow::bail!("Runtime version differs; restart it on its host");
    }
    let stopped = if remote.capabilities().runtime_stop_empty {
        remote.stop_empty().await
    } else {
        remote.stop().await
    };
    match stopped {
        Ok(()) => {}
        Err(loop_protocol::BackendError::Failed(info))
            if info.category == loop_protocol::ErrorCategory::Network => {}
        Err(error) => return Err(error.into()),
    }
    tokio::time::timeout(CONNECT_WAIT, async {
        while tokio::net::UnixStream::connect(&socket).await.is_ok() {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .map_err(|_| anyhow::anyhow!("Runtime did not stop"))?;
    let previous_version = remote.server().version.clone();
    let replacement = start_and_connect(&executable, agent_dir, cwd, &socket, host).await?;
    replacement.add_connection_notice(format!(
        "Restarted an auto-started Runtime from v{previous_version} to v{}.",
        env!("CARGO_PKG_VERSION")
    ));
    Ok(replacement)
}

/// Resolve an attach request before creating anything. Prefix ambiguity is an
/// error; `--continue` chooses the latest root Session in this cwd.
pub fn resume_session(
    infos: &[loop_protocol::rpc::SessionInfo],
    cwd: &Path,
    id: Option<&str>,
    latest: bool,
    mut picker: impl FnMut(&[loop_protocol::rpc::SessionInfo]) -> anyhow::Result<usize>,
) -> anyhow::Result<Option<loop_protocol::SessionRef>> {
    if id.is_none() && !latest {
        return Ok(None);
    }
    let mut choices: Vec<_> = infos
        .iter()
        .filter(|info| {
            if let Some(id) = id.filter(|id| !id.is_empty()) {
                info.r#ref.native_id.starts_with(id)
            } else {
                info.cwd == cwd && info.parent.is_none()
            }
        })
        .cloned()
        .collect();
    choices.sort_by_key(|info| std::cmp::Reverse(info.last_active_ms));
    if choices.is_empty() {
        if id.is_some_and(|id| !id.is_empty()) {
            anyhow::bail!("no Session matches that id; use --resume to choose one");
        }
        return Ok(None);
    }
    if latest {
        return Ok(Some(choices[0].r#ref.clone()));
    }
    if id.is_some_and(|id| !id.is_empty()) {
        if choices.len() != 1 {
            anyhow::bail!("Session id is ambiguous; use a longer id");
        }
        return Ok(Some(choices[0].r#ref.clone()));
    }
    choices
        .get(picker(&choices)?)
        .map(|info| Some(info.r#ref.clone()))
        .ok_or_else(|| anyhow::anyhow!("invalid Session selection"))
}

pub async fn open(
    remote: Arc<RemoteBackend>,
    host: Arc<crate::surface_client::RemoteHost>,
    opts: &crate::BootstrapOpts,
    resume: Option<&str>,
    latest: bool,
) -> anyhow::Result<crate::SurfaceClient> {
    let requested_resume = resume.is_some() || latest;
    let selected = if requested_resume {
        let infos = remote.list().await?;
        let cwd = opts.cwd.clone();
        let resume = resume.map(str::to_owned);
        tokio::task::spawn_blocking(move || {
            resume_session(&infos, &cwd, resume.as_deref(), latest, |choices| {
                use std::io::{IsTerminal, Write};
                if !std::io::stdin().is_terminal() {
                    return Ok(0);
                }
                eprintln!("Choose a Session:");
                for (i, info) in choices.iter().enumerate() {
                    eprintln!(
                        "  {}. {} · {}",
                        i + 1,
                        info.r#ref.native_id,
                        info.name
                            .as_deref()
                            .or(info.summary.first_prompt.as_deref())
                            .unwrap_or("(empty)")
                    );
                }
                eprint!("> ");
                std::io::stderr().flush()?;
                read_choice(std::io::stdin().lock(), choices.len())
            })
        })
        .await??
    } else {
        None
    };
    let resumed = selected.is_some();
    if requested_resume && !resumed {
        remote.add_connection_notice(
            "No previous Sessions in this folder; started a new Session.".into(),
        );
    }
    let session = if let Some(session) = selected {
        remote
            .load_session(
                &session,
                loop_protocol::SessionSpec {
                    env: Some(session_environment(
                        &remote,
                        &crate::config::paths::get_agent_dir(),
                    )?),
                    ..Default::default()
                },
            )
            .await?;
        session
    } else {
        if !remote.capabilities().session_environment {
            anyhow::bail!("Runtime does not accept per-Session environments");
        }
        remote
            .new_session(loop_protocol::SessionSpec {
                cwd: Some(opts.cwd.clone()),
                env: Some(session_environment(
                    &remote,
                    &crate::config::paths::get_agent_dir(),
                )?),
                name: None,
                options: Some(loop_protocol::SessionOptions {
                    provider: opts.provider.clone(),
                    model: opts.model.clone(),
                    system_prompt: opts.system_prompt.clone(),
                    append_system_prompt: opts.append_system_prompt.clone(),
                    no_context_files: opts.no_context_files,
                    no_web: opts.no_web,
                    permission_mode: opts.permission_mode.clone(),
                }),
            })
            .await?
    };
    crate::SurfaceClient::connected(remote, host, opts, session, resumed).await
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn picker_uses_the_discovered_inventory() {
        let one = Target::Unix("/tmp/one.sock".into());
        let two = Target::Unix("/tmp/two.sock".into());
        assert_eq!(
            select(std::slice::from_ref(&one), |_| panic!("no picker for one")).unwrap(),
            Some(one.clone())
        );
        assert_eq!(
            select(&[one, two.clone()], |targets| {
                assert_eq!(targets.len(), 2);
                Ok(1)
            })
            .unwrap(),
            Some(two)
        );
        assert!(select(&[], |_| panic!("no picker for none"))
            .unwrap()
            .is_none());
    }
    #[test]
    fn resume_latest_prefix_and_picker_choose_existing_sessions() {
        use loop_protocol::rpc::SessionInfo;
        let make = |id: &str, time| SessionInfo {
            r#ref: loop_protocol::SessionRef {
                instance: "native".into(),
                key: id.into(),
                native_id: id.into(),
            },
            cwd: "/work".into(),
            name: None,
            parent: None,
            live: true,
            busy: false,
            created_ms: 0,
            last_active_ms: time,
            summary: Default::default(),
        };
        let infos = vec![make("abc-one", 1), make("abc-two", 2)];
        assert_eq!(
            resume_session(&infos, Path::new("/work"), None, true, |_| panic!())
                .unwrap()
                .unwrap()
                .native_id,
            "abc-two"
        );
        assert_eq!(
            resume_session(
                &infos,
                Path::new("/work"),
                Some("abc-o"),
                false,
                |_| panic!()
            )
            .unwrap()
            .unwrap()
            .native_id,
            "abc-one"
        );
        assert!(
            resume_session(&infos, Path::new("/work"), Some("abc"), false, |_| panic!()).is_err()
        );
        assert_eq!(
            resume_session(&infos, Path::new("/work"), Some(""), false, |_| Ok(1))
                .unwrap()
                .unwrap()
                .native_id,
            "abc-one"
        );
        assert!(
            resume_session(&infos, Path::new("/work"), None, false, |_| panic!())
                .unwrap()
                .is_none()
        );
    }
    #[test]
    fn websocket_environment_uses_the_allowlist_and_explicit_forward_env_names() {
        let vars = [
            ("PATH", "/bin"),
            ("HOME", "/home/test"),
            ("USER", "test"),
            ("SHELL", "/bin/sh"),
            ("LANG", "en"),
            ("LC_CTYPE", "en"),
            ("TERM", "xterm"),
            ("TMPDIR", "/tmp"),
            ("VIRTUAL_ENV", "/venv"),
            ("CONDA_PREFIX", "/conda"),
            ("TASK_VALUE", "approved"),
            ("SENTINEL_SECRET", "do-not-forward"),
            ("OPENAI_API_KEY", "do-not-forward"),
            ("LOOP_RUNTIME_TOKEN", "do-not-forward"),
        ];
        let vars = || {
            vars.iter()
                .map(|(key, value)| (key.to_string(), value.to_string()))
        };
        let remote = caller_environment(true, &["TASK_VALUE".into()], vars());
        assert_eq!(remote.len(), 11);
        assert_eq!(remote["TASK_VALUE"], "approved");
        assert!(!remote.contains_key("SENTINEL_SECRET"));
        assert!(!remote.contains_key("OPENAI_API_KEY"));
        assert!(!remote.contains_key("LOOP_RUNTIME_TOKEN"));
        assert_eq!(caller_environment(false, &[], vars()).len(), 14);
    }
    #[test]
    fn pass_env_names_are_forwarded_with_the_forward_env_names() {
        let settings: crate::config::Settings = serde_json::from_str(
            r#"{"runtime":{"forwardEnv":["TASK_VALUE"]},"tools":{"passEnv":["OPENAI_API_KEY"]}}"#,
        )
        .unwrap();
        let names = forward_names(&settings);
        assert_eq!(names, ["TASK_VALUE", "OPENAI_API_KEY"]);
        let vars = [
            ("OPENAI_API_KEY", "k"),
            ("OTHER_KEY", "x"),
            ("TASK_VALUE", "v"),
        ]
        .map(|(k, v)| (k.to_string(), v.to_string()));
        let sent = caller_environment(true, &names, vars);
        assert_eq!(sent.len(), 2);
        assert!(sent.contains_key("OPENAI_API_KEY") && sent.contains_key("TASK_VALUE"));
    }
    #[test]
    fn missing_history_and_picker_input_have_friendly_results() {
        assert!(
            resume_session(&[], Path::new("/work"), None, true, |_| panic!())
                .unwrap()
                .is_none()
        );
        assert_eq!(read_choice(std::io::Cursor::new("2\n"), 2).unwrap(), 1);
        for input in ["", "0\n", "9\n", "abc\n"] {
            let error = read_choice(std::io::Cursor::new(input), 2)
                .unwrap_err()
                .to_string();
            assert!(
                error.contains("selection") || error.contains("closed"),
                "{error}"
            );
        }
    }

    #[test]
    fn websocket_token_comes_from_the_file_or_the_environment() {
        let error = websocket_token(None, None).unwrap_err().to_string();
        assert!(error.contains("LOOP_RUNTIME_TOKEN"), "{error}");
        assert!(websocket_token(None, Some("  ")).is_err());
        assert_eq!(
            websocket_token(None, Some("env-token")).unwrap(),
            "env-token"
        );
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("token");
        std::fs::write(&file, "file-token\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
            std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o600)).unwrap();
        }
        assert_eq!(
            websocket_token(Some(&file), Some("env-token"))
                .unwrap()
                .trim(),
            "file-token"
        );
    }
    #[test]
    fn explicit_connect_errors_name_the_target_and_the_reason() {
        let target = Target::parse("ws://127.0.0.1:9");
        let missing = websocket_token(None, None).unwrap_err();
        assert_eq!(
            explicit_connect_error(&target, &missing).to_string(),
            "could not connect to ws://127.0.0.1:9: missing token, pass --token-file or set LOOP_RUNTIME_TOKEN"
        );
        let denied = anyhow::anyhow!("unauthorized");
        assert_eq!(
            explicit_connect_error(&target, &denied).to_string(),
            "could not connect to ws://127.0.0.1:9: unauthorized"
        );
    }

    #[test]
    fn only_a_known_auto_started_empty_owner_can_be_version_replaced() {
        let mut status: loop_protocol::rpc::RuntimeStatus = serde_json::from_value(serde_json::json!({
            "pid":1,"socket":"/tmp/runtime.sock","generation":1,"loaded":0,"busy":0,"max_sessions":16,"max_active_turns":8
        })).unwrap();
        assert!(!replaceable(&status));
        status.auto_started = true;
        assert!(replaceable(&status));
        status.loaded = 1;
        assert!(!replaceable(&status));
    }
}
