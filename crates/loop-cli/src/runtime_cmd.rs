//! `loop runtime start|stop|status`. `start` runs the Runtime: it never
//! reads stdin, never prompts and never starts a TUI. `stop` and `status`
//! talk to it only through its socket, never through the pid file: a pid
//! file can be stale and a pid can be reused by another process.

use std::path::PathBuf;
use std::time::Duration;

/// How long `loop runtime stop` waits for the socket to disappear.
pub const STOP_WAIT: Duration = Duration::from_secs(10);

/// Options of `loop runtime start`.
#[derive(Debug, Clone)]
pub struct StartArgs {
    /// Started by an interactive client; enables idle shutdown.
    pub auto_started: bool,
    /// Optional loopback WebSocket listener, in addition to the Unix socket.
    pub listen: Option<std::net::SocketAddr>,
    /// Socket path; default is the agent directory's Runtime socket.
    pub socket: Option<PathBuf>,
    /// `--max-sessions`: most Sessions the Runtime holds at once.
    pub max_sessions: Option<usize>,
    /// Most turns running at once; the rest queue.
    pub max_active_turns: Option<usize>,
    /// `--permission-mode`; `full-access` only takes effect through this flag.
    pub permission_mode: Option<String>,
    /// Folder for Sessions that name none.
    pub cwd: PathBuf,
    /// `--tool-lock`; without it, the `LOOP_TOOL_LOCK` environment variable.
    pub tool_lock: Option<String>,
    /// `--tool-lock-root`; without it, `LOOP_TOOL_LOCK_ROOT`.
    pub tool_lock_root: Option<PathBuf>,
    /// `--unstable-edge-listen`: a loopback listener for a trusted Edge
    /// Gateway that names each connection's end user at the handshake. Off
    /// by default; unstable, not for real users yet.
    pub edge_listen: Option<std::net::SocketAddr>,
    /// `--unstable-edge-token-file`: the gateway's bearer token, in a
    /// private file.
    pub edge_token_file: Option<PathBuf>,
}

/// Shortest edge token, in bytes: as long as the Runtime's own (256 bits in
/// hex is 64; a 32-byte minimum refuses a short, guessable secret).
const MIN_EDGE_TOKEN_BYTES: usize = 32;

/// The checked edge flags: both or neither, a loopback address, and a tool
/// lock. Refused before anything starts. The token file is read later
/// ([`edge_token`]), once the Runtime token exists to compare it with.
///
/// An Edge Gateway user is outside the host, so the Runtime serves one only
/// under a tool lock: without it a Session runs shell commands as the host's
/// OS user. The server refuses such a connection too; this names the
/// mistake at start.
fn edge_options(
    listen: Option<std::net::SocketAddr>,
    token_file: Option<&std::path::Path>,
    locked: bool,
) -> anyhow::Result<Option<(std::net::SocketAddr, PathBuf)>> {
    match (listen, token_file) {
        (None, None) => Ok(None),
        (Some(_), None) | (None, Some(_)) => anyhow::bail!(
            "--unstable-edge-listen and --unstable-edge-token-file are used together"
        ),
        (Some(address), Some(_)) if !address.ip().is_loopback() => {
            anyhow::bail!("refusing a non-loopback --unstable-edge-listen bind: TLS is not yet supported; run the Edge Gateway on this host")
        }
        (Some(_), Some(_)) if !locked => anyhow::bail!(
            "--unstable-edge-listen needs --tool-lock chat: without a tool lock an Edge Gateway user could run commands as this host's user"
        ),
        (Some(address), Some(path)) => Ok(Some((address, path.to_path_buf()))),
    }
}

/// Refuse a tool-lock root from which a locked Session could read the
/// Runtime's own secrets: the agent directory (credentials, the Runtime
/// token, the Unix socket's folder), the Session database, or the edge token
/// file. The root and a secret must not overlap in either direction: a root
/// inside the agent directory (such as its `run/` folder) is refused too.
/// A path that does not exist yet is resolved through its nearest existing
/// parent, so a symbolic link cannot hide it.
///
/// The user's own secrets count too: a root that contains `home` (the
/// home directory itself or a folder above it) is refused, and so is a root
/// that overlaps any path the Seatbelt profile denies to sandboxed commands
/// (`~/.ssh`, `~/.aws`, `~/.config/gh`, shell histories and the like;
/// [`loop_agent::harness::sandbox::seatbelt::sensitive_home_paths`]). A
/// root below the home directory, such as `~/projects`, is allowed.
fn edge_reach(
    lock_root: &std::path::Path,
    secrets: &[(&str, &std::path::Path)],
    home: Option<&std::path::Path>,
) -> anyhow::Result<()> {
    fn resolve(path: &std::path::Path) -> PathBuf {
        let path = std::path::absolute(path).unwrap_or_else(|_| path.to_path_buf());
        let mut missing = Vec::new();
        let mut existing = path.as_path();
        loop {
            if let Ok(real) = existing.canonicalize() {
                return missing.iter().rev().fold(real, |acc, part| acc.join(part));
            }
            match (existing.parent(), existing.file_name()) {
                (Some(parent), Some(name)) => {
                    missing.push(name.to_owned());
                    existing = parent;
                }
                _ => return path,
            }
        }
    }
    let root = resolve(lock_root);
    for (what, path) in secrets {
        let secret = resolve(path);
        if secret.starts_with(&root) || root.starts_with(&secret) {
            anyhow::bail!(
                "--unstable-edge-listen: the --tool-lock-root {} overlaps the {what}; an Edge Gateway user could read it. Use a root outside it",
                root.display()
            );
        }
    }
    let Some(home) = home else {
        anyhow::bail!(
            "--unstable-edge-listen: Loop cannot find your home directory, so it cannot check that the --tool-lock-root {} keeps your secrets out of reach",
            root.display()
        );
    };
    // The configured and the resolved home: either name can be the one
    // above the root.
    for home_form in [
        std::path::absolute(home).unwrap_or_else(|_| home.to_path_buf()),
        resolve(home),
    ] {
        if home_form.starts_with(&root) {
            anyhow::bail!(
                "--unstable-edge-listen: the --tool-lock-root {} contains your home directory {}; an Edge Gateway user could read your files. Use a folder below it, such as one project folder",
                root.display(),
                home_form.display()
            );
        }
    }
    for path in loop_agent::harness::sandbox::seatbelt::sensitive_home_paths(home) {
        let secret = resolve(&path);
        if secret.starts_with(&root) || root.starts_with(&secret) {
            anyhow::bail!(
                "--unstable-edge-listen: the --tool-lock-root {} overlaps {}, which holds credentials or history; an Edge Gateway user could read it. Use a root outside it",
                root.display(),
                path.display()
            );
        }
    }
    Ok(())
}

/// Read the edge token from `path`, after the Runtime token was made. It
/// must be a private file (as [`loop_wire::token::read`] checks), at least
/// [`MIN_EDGE_TOKEN_BYTES`] long, outside `run_dir` (the Runtime's token
/// folder), not the Runtime token file itself (same device and inode, so a
/// hard link or a second name is refused too), and not equal to the Runtime
/// token. `runtime_token` is the token this start made, or else the one a
/// previous start left in `run_dir`, so the check runs with or without
/// `--listen`.
fn edge_token(
    path: &std::path::Path,
    run_dir: &std::path::Path,
    runtime_token: Option<&str>,
) -> anyhow::Result<String> {
    use anyhow::Context;
    use std::os::unix::fs::MetadataExt;
    let named = || {
        format!(
            "could not read the edge token file {}; it must be a 0600 file you own in a 0700 directory",
            path.display()
        )
    };
    let file = path.canonicalize().with_context(named)?;
    if let Ok(run) = run_dir.canonicalize() {
        if file.starts_with(&run) {
            anyhow::bail!(
                "the edge token file {} is in the Runtime's token folder {}; keep it outside the agent directory",
                path.display(),
                run.display()
            );
        }
    }
    let runtime_file = run_dir.join(RUNTIME_TOKEN_FILE);
    if let (Ok(edge), Ok(runtime)) = (std::fs::metadata(&file), std::fs::metadata(&runtime_file)) {
        if (edge.dev(), edge.ino()) == (runtime.dev(), runtime.ino()) {
            anyhow::bail!("the edge token file is the Runtime token file; use a separate file");
        }
    }
    let token = loop_wire::token::read(path).with_context(named)?;
    if token.len() < MIN_EDGE_TOKEN_BYTES {
        anyhow::bail!(
            "the edge token in {} is shorter than {MIN_EDGE_TOKEN_BYTES} bytes",
            path.display()
        );
    }
    let stored = match runtime_token {
        Some(token) => Some(token.to_owned()),
        None => loop_wire::token::read(&runtime_file).ok(),
    };
    if stored.is_some_and(|runtime| {
        loop_wire::socket::constant_time_eq(runtime.as_bytes(), token.as_bytes())
    }) {
        anyhow::bail!("the edge token must differ from the Runtime token");
    }
    Ok(token)
}

/// The Runtime token's file name in `run/`.
const RUNTIME_TOKEN_FILE: &str = "runtime.token";

/// Run the Runtime until SIGINT, SIGTERM or `loop runtime stop`.
pub async fn start(args: StartArgs) -> anyhow::Result<()> {
    use anyhow::Context;
    use loop_app_core::{
        config::{paths, Settings},
        permissions::PermissionMode,
        runtime::{BootstrapOpts, GlobalResources},
        runtime_instance::RuntimeInstanceLock,
    };
    use loop_protocol::{rpc::PeerInfo, AgentBackend};
    use loop_runtime::{log::SqliteEventLog, Runtime, RuntimeLimits};
    use loop_wire::server::{serve_with_caller, serve_with_control, ServeOptions};

    if args.listen.is_some_and(|addr| !addr.ip().is_loopback()) {
        anyhow::bail!("refusing a non-loopback WebSocket bind: TLS is required and is not yet supported; use a loopback address and an SSH tunnel");
    }

    if let Some(mode) = &args.permission_mode {
        if PermissionMode::parse(mode).is_none() {
            anyhow::bail!(
                "unknown --permission-mode `{mode}`: use read-only, ask, accept-edits, full-access or plan"
            );
        }
    }
    // Refused before anything starts: a typo must never start a Runtime
    // without the lock it asked for.
    use loop_app_core::permissions::tool_lock;
    let tool_lock = tool_lock::resolve(
        args.tool_lock.as_deref(),
        std::env::var(tool_lock::ENV).ok().as_deref(),
        args.tool_lock_root.as_deref(),
        std::env::var(tool_lock::ROOT_ENV).ok().as_deref(),
    )?;
    let edge = edge_options(
        args.edge_listen,
        args.edge_token_file.as_deref(),
        tool_lock.is_some(),
    )?;
    let lock_name = tool_lock.as_ref().map(|lock| lock.lock());
    let lock_root = tool_lock.as_ref().map(|lock| lock.root().to_path_buf());
    let agent_dir = paths::get_agent_dir();
    let settings = Settings::load_file(&paths::settings_path(&agent_dir)).unwrap_or_default();
    // Refused before anything starts, naming the flag or settings key.
    let limits = Limits::resolve(args.max_sessions, args.max_active_turns, &settings.runtime)?;
    paths::ensure_agent_dirs(&agent_dir)?;
    // The Lock is the ownership proof; everything below runs while holding it.
    let lock = RuntimeInstanceLock::acquire(&agent_dir).map_err(|e| {
        if e.kind() == std::io::ErrorKind::WouldBlock {
            anyhow::anyhow!("{e}")
        } else {
            anyhow::Error::new(e).context("could not take the Runtime lock")
        }
    })?;
    crate::debug_log::init_tracing(
        false,
        &args.cwd,
        crate::debug_log::TracingMode::Daemon {
            agent_dir: &agent_dir,
        },
        None,
    )?;
    // Register handlers before the socket appears so an early signal is not lost.
    let (mut signal, mut shutdown_ready) =
        match register(Box::pin(crate::runtime_signals::shutdown_signal()))
            .await
            .context("could not register shutdown signal handlers")?
        {
            Registered::Pending(signal) => (Some(signal), false),
            Registered::Done => (None, true),
        };

    // The Session database may live outside this agent directory and be shared
    // with another Runtime; its ownership domain needs its own Lock.
    let sessions_db = paths::sessions_db_path(&agent_dir);
    if let (Some((_, token_file)), Some(root)) = (&edge, &lock_root) {
        edge_reach(
            root,
            &[
                ("agent directory", agent_dir.as_path()),
                ("Session database", sessions_db.as_path()),
                ("edge token file", token_file.as_path()),
            ],
            dirs::home_dir().as_deref(),
        )?;
    }
    let sessions_lock = loop_app_core::runtime_instance::SessionDatabaseLock::acquire(&sessions_db)
        .map_err(|e| anyhow::anyhow!("{e}"))?;

    // Fixed here for the life of the process: no setting or request can
    // change it later.
    let globals = GlobalResources::load(agent_dir.clone(), &settings.providers, true)
        .await?
        .with_tool_lock(tool_lock);
    let log = Arc::new(
        SqliteEventLog::open(&agent_dir)
            .await
            .context("could not open the Event Log")?,
    );
    // The Runtime opened this store itself before; keep its error text.
    let store = globals
        .open_runtime_session_store()
        .map_err(|e| anyhow::anyhow!("could not start the Runtime: {e}"))?;
    let defaults = BootstrapOpts {
        cwd: args.cwd.clone(),
        provider: None,
        model: None,
        theme: None,
        system_prompt: None,
        append_system_prompt: None,
        no_context_files: false,
        interactive: false,
        session_id: None,
        no_web: false,
        permission_mode: args.permission_mode.clone(),
        agent: None,
    };
    let runtime = Runtime::new(
        globals,
        defaults,
        log,
        store,
        RuntimeLimits {
            max_sessions: limits.max_sessions.0,
            max_active_turns: limits.max_active_turns.0,
        },
    )
    .await
    .map_err(|e| anyhow::anyhow!("could not start the Runtime: {e}"))?;
    // Shared, so its Sessions are offered the `subagent` tool.
    let runtime = runtime.into_shared();

    // Recover before accepting clients. Holding runtime.lock (and the Session
    // database lock) proves the previous Runtime is dead.
    let recovered = runtime
        .recover(Some(&sessions_lock))
        .await
        .map_err(|e| anyhow::anyhow!("Runtime recovery failed: {e}"))?;
    if recovered != loop_runtime::Recovery::default() {
        eprintln!(
            "loop runtime: recovered {} interrupted turn(s), withdrew {} Escalation(s), reclaimed {} lease(s), removed {} stale lease(s) of exited processes",
            recovered.turns_ended,
            recovered.escalations_withdrawn,
            recovered.leases_reclaimed,
            recovered.leases_swept
        );
    }

    let socket = match &args.socket {
        Some(path) => path.clone(),
        None => paths::runtime_socket_path(&agent_dir)?,
    };
    let websocket = match args.listen {
        Some(address) => {
            let listener = tokio::net::TcpListener::bind(address)
                .await
                .context("could not bind the loopback WebSocket listener")?;
            let (token, path) = loop_wire::token::create(&agent_dir)
                .context("could not create the private Runtime token file")?;
            Some((listener, token, TokenFile(path)))
        }
        None => None,
    };
    // After the Runtime token exists, so the two are always compared.
    let edge = match edge {
        Some((address, path)) => Some((address, {
            let user = websocket.as_ref().map(|(_, token, _)| token.as_str());
            edge_token(&path, &agent_dir.join("run"), user)?
        })),
        None => None,
    };
    let edge = match edge {
        Some((address, token)) => Some((
            tokio::net::TcpListener::bind(address)
                .await
                .context("could not bind the loopback edge listener")?,
            token,
        )),
        None => None,
    };
    let listen_url = websocket
        .as_ref()
        .map(|(listener, _, _)| listener.local_addr().map(|addr| format!("ws://{addr}/")))
        .transpose()?;
    // `unix_listen` probes, and replaces only a socket proven stale.
    let listener = loop_wire::socket::unix_listen(&socket)
        .with_context(|| format!("could not listen on {}", socket.display()))?;
    tracing::info!(
        socket = %socket.display(),
        generation = runtime.generation(),
        turns_ended = recovered.turns_ended,
        escalations_withdrawn = recovered.escalations_withdrawn,
        leases_reclaimed = recovered.leases_reclaimed,
        leases_swept = recovered.leases_swept,
        "Runtime listening"
    );
    eprintln!("loop runtime: listening on {}", socket.display());
    if let (Some(lock), Some(root)) = (lock_name, &lock_root) {
        eprintln!(
            "loop runtime: tool lock `{lock}`: every Session can only read files in its \
             folder and use the web tools; Session folders must be in {}",
            root.display()
        );
    }
    if let Some(url) = &listen_url {
        eprintln!(
            "loop runtime: listening on {url}; token file: {}",
            agent_dir.join("run").join(RUNTIME_TOKEN_FILE).display()
        );
    }
    if let Some((listener, _)) = &edge {
        eprintln!(
            "loop runtime: Edge Gateway listener on ws://{}/; the gateway names each connection's user",
            listener.local_addr()?
        );
    }
    let stop = tokio_util::sync::CancellationToken::new();
    let control: Arc<dyn loop_wire::RuntimeControl> = Arc::new(Control {
        runtime: runtime.clone(),
        socket: std::path::absolute(&socket).unwrap_or_else(|_| socket.clone()),
        stop: stop.clone(),
        limits,
        #[cfg(feature = "usage")]
        agent_dir: agent_dir.clone(),
        listen_url,
        auto_started: args.auto_started,
        tool_lock: lock_name,
    });

    let info = PeerInfo {
        name: "loop".into(),
        version: env!("CARGO_PKG_VERSION").into(),
    };
    let idle_timeout = idle_timeout(args.auto_started, &settings.runtime);
    let mut idle_since = tokio::time::Instant::now();
    let mut idle_epoch = runtime.idle_activity();
    let mut idle_check = tokio::time::interval(Duration::from_secs(1));
    let mut connections = tokio::task::JoinSet::new();
    while !shutdown_ready {
        let Some(pending) = signal.as_mut() else {
            break;
        };
        tokio::select! {
            _ = idle_check.tick(), if idle_timeout.is_some() => {
                let activity = runtime.idle_activity();
                if activity != idle_epoch || !runtime.is_idle().await { idle_since = tokio::time::Instant::now(); }
                idle_epoch = activity;
                if idle_since.elapsed() >= idle_timeout.unwrap() && runtime.shutdown_if_idle().await {
                    tracing::info!("Auto-started Runtime idle timeout reached");
                    shutdown_ready = true;
                }
            }
            result = pending => {
                if let Err(e) = result {
                    tracing::error!(error = %e, "shutdown signal failed");
                }
                shutdown_ready = true;
            }
            _ = stop.cancelled() => {
                tracing::info!("Runtime stop requested over the socket");
                shutdown_ready = true;
            }
            accepted = listener.accept() => match accepted {
                Ok((stream, _)) => {
                    let backend: Arc<dyn AgentBackend> = runtime.clone();
                    let control = control.clone();
                    let served = serve_with_control(
                        backend,
                        control,
                        loop_wire::socket::unix(stream),
                        info.clone(),
                        ServeOptions { owns_backend: false, authority: loop_wire::Authority::HostLocal, },
                    );
                    connections.spawn(async move {
                        let _ = served.done.await;
                    });
                }
                Err(e) => {
                    tracing::warn!(error = %e, "accept failed");
                    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                }
            },
            accepted = async {
                match &websocket {
                    Some((listener, _, _)) => listener.accept().await,
                    None => std::future::pending().await,
                }
            } => match accepted {
                Ok((stream, _)) => {
                    let token = websocket.as_ref().expect("WebSocket listener").1.clone();
                    let backend: Arc<dyn AgentBackend> = runtime.clone();
                    let control = control.clone();
                    let info = info.clone();
                    connections.spawn(async move {
                        if let Ok(transport) = loop_wire::socket::websocket_accept(stream, &token).await {
                            // The token proves the host's user (ADR-0009).
                            let caller = loop_protocol::Caller::local_token(loop_wire::local_principal());
                            let served = serve_with_caller(backend, Some(control), transport, info,
                                ServeOptions { owns_backend: false, authority: loop_wire::Authority::Remote }, caller);
                            let _ = served.done.await;
                        }
                    });
                }
                Err(error) => {
                    tracing::warn!(%error, "WebSocket accept failed");
                    tokio::time::sleep(Duration::from_millis(100)).await;
                }
            },
            accepted = async {
                match &edge {
                    Some((listener, token)) => listener.accept().await.map(|(stream, _)| (stream, token.clone())),
                    None => std::future::pending().await,
                }
            } => match accepted {
                Ok((stream, token)) => {
                    let backend: Arc<dyn AgentBackend> = runtime.clone();
                    let control = control.clone();
                    let info = info.clone();
                    connections.spawn(async move {
                        // The identity is asserted once, at the handshake,
                        // and fixed for the connection. Edge connections
                        // never have host authority.
                        match loop_wire::socket::websocket_accept_edge(stream, &token).await {
                            Ok((transport, caller)) => {
                                tracing::info!(principal = %caller.principal(), tenant = %caller.tenant(), "edge connection");
                                let served = serve_with_caller(backend, Some(control), transport, info,
                                    ServeOptions { owns_backend: false, authority: loop_wire::Authority::Remote }, caller);
                                let _ = served.done.await;
                            }
                            Err(error) => tracing::warn!(%error, "edge connection refused"),
                        }
                    });
                }
                Err(error) => {
                    tracing::warn!(%error, "edge accept failed");
                    tokio::time::sleep(Duration::from_millis(100)).await;
                }
            },
            Some(_) = connections.join_next(), if !connections.is_empty() => {}
        }
    }
    tracing::info!("Runtime shutting down");
    drop(listener);
    drop(websocket);
    drop(edge);
    runtime.shutdown().await;
    connections.abort_all();
    // The Lock is still held, so the socket is ours to remove.
    if let Err(e) = std::fs::remove_file(&socket) {
        if e.kind() != std::io::ErrorKind::NotFound {
            tracing::warn!(error = %e, "could not remove the Runtime socket");
        }
    }
    drop(lock);
    Ok(())
}
fn idle_timeout(auto_started: bool, settings: &crate::config::RuntimeSettings) -> Option<Duration> {
    auto_started
        .then(|| {
            settings
                .idle_shutdown_minutes
                .unwrap_or(10)
                .saturating_mul(60)
        })
        .filter(|seconds| *seconds != 0)
        .map(Duration::from_secs)
}

use std::sync::Arc;
struct TokenFile(PathBuf);
impl Drop for TokenFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

/// The Runtime's limits and where each came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limits {
    /// Most Sessions loaded at once, and its source.
    pub max_sessions: (usize, &'static str),
    /// Most turns running at once, and its source.
    pub max_active_turns: (usize, &'static str),
}

impl Limits {
    /// Default `max_sessions`.
    pub const MAX_SESSIONS: usize = 16;
    /// Default `max_active_turns`.
    pub const MAX_ACTIVE_TURNS: usize = 8;

    /// A flag wins over `settings.json`'s `runtime` key, which wins over the
    /// defaults. Settings values are checked even when a flag overrides them.
    pub fn resolve(
        max_sessions: Option<usize>,
        max_active_turns: Option<usize>,
        settings: &crate::config::RuntimeSettings,
    ) -> anyhow::Result<Self> {
        let pick = |flag: Option<usize>,
                    name: &str,
                    set: Option<usize>,
                    default: usize|
         -> anyhow::Result<(usize, &'static str)> {
            match (flag, set) {
                (Some(0), _) => anyhow::bail!("--{name} must be at least 1"),
                (Some(n), _) => Ok((n, "flag")),
                (None, Some(n)) => Ok((n, "settings")),
                (None, None) => Ok((default, "default")),
            }
        };
        Ok(Self {
            max_sessions: pick(
                max_sessions,
                "max-sessions",
                settings.max_sessions()?,
                Self::MAX_SESSIONS,
            )?,
            max_active_turns: pick(
                max_active_turns,
                "max-active-turns",
                settings.max_active_turns()?,
                Self::MAX_ACTIVE_TURNS,
            )?,
        })
    }
}

/// The Runtime as one socket serves it: `runtime/status` names this socket
/// and `runtime/stop` ends the accept loop like a signal does.
struct Control {
    auto_started: bool,
    runtime: Arc<loop_runtime::Runtime>,
    socket: PathBuf,
    stop: tokio_util::sync::CancellationToken,
    limits: Limits,
    /// The host's usage log directory.
    #[cfg(feature = "usage")]
    agent_dir: PathBuf,
    listen_url: Option<String>,
    /// Advertised in `initialize` so a client can hide what it turns off.
    tool_lock: Option<loop_protocol::ToolLock>,
}

/// Workspace admin is the Runtime's; the host-only usage history and
/// new-Session defaults are the daemon's, served only host-locally (the
/// wire refuses them on any other connection).
#[async_trait::async_trait]
impl loop_wire::RuntimeAdmin for Control {
    async fn settings(&self, p: admin::SettingsParams) -> BackendResult<admin::SettingsResult> {
        self.runtime.settings(p).await
    }

    fn offered(&self) -> loop_protocol::Capabilities {
        loop_protocol::Capabilities {
            host_usage: cfg!(feature = "usage"),
            host_defaults: true,
            runtime_stop_empty: true,
            tool_lock: self.tool_lock,
            ..self.runtime.offered()
        }
    }
    async fn mcp(&self, p: admin::McpParams) -> BackendResult<admin::McpResult> {
        self.runtime.mcp(p).await
    }
    async fn sessions(&self) -> BackendResult<admin::RuntimeSessionsResult> {
        loop_wire::RuntimeAdmin::sessions(self.runtime.as_ref()).await
    }
    async fn web_search(&self, p: admin::WebSearchParams) -> BackendResult<admin::WebSearchResult> {
        self.runtime.web_search(p).await
    }
    async fn skills(&self, p: admin::SkillsParams) -> BackendResult<admin::SkillsResult> {
        self.runtime.skills(p).await
    }
    async fn templates(&self, p: admin::TemplatesParams) -> BackendResult<admin::TemplatesResult> {
        self.runtime.templates(p).await
    }
    async fn reload(&self, p: admin::ReloadParams) -> BackendResult<admin::ReloadResult> {
        loop_wire::RuntimeAdmin::reload(self.runtime.as_ref(), p).await
    }
    async fn trust(&self, p: admin::TrustParams) -> BackendResult<admin::TrustResult> {
        self.runtime.trust(p).await
    }
    async fn sandbox(&self, p: admin::SandboxParams) -> BackendResult<admin::SandboxResult> {
        self.runtime.sandbox(p).await
    }
    async fn permission_mode(
        &self,
        p: admin::PermissionModeParams,
    ) -> BackendResult<admin::PermissionModeResult> {
        self.runtime.permission_mode(p).await
    }
    #[cfg(feature = "usage")]
    async fn usage(&self, p: admin::UsageParams) -> BackendResult<admin::UsageResult> {
        // The host's usage log covers every Session; not under a tool lock.
        if let Some(lock) = self.tool_lock {
            return Err(loop_protocol::BackendError::ToolLocked {
                what: "runtime/usage".into(),
                lock,
            });
        }
        let dir = loop_app_core::config::paths::usage_dir(&self.agent_dir);
        tokio::task::spawn_blocking(move || usage_page(&dir, &p))
            .await
            .map_err(failed)?
    }
    async fn defaults(&self, p: admin::DefaultsParams) -> BackendResult<admin::DefaultsResult> {
        self.runtime.update_defaults(p).await
    }
}
use loop_protocol::{admin, BackendResult};

#[cfg(feature = "usage")]
fn failed(error: impl std::fmt::Display) -> loop_protocol::BackendError {
    loop_protocol::BackendError::failed(loop_protocol::ErrorCategory::Internal, error.to_string())
}

/// One page of the host's usage log, one row per Session, most recently
/// active first. The cursor is the next row's offset.
#[cfg(feature = "usage")]
pub(crate) fn usage_page(
    dir: &std::path::Path,
    params: &admin::UsageParams,
) -> BackendResult<admin::UsageResult> {
    let bad = || {
        loop_protocol::BackendError::failed(
            loop_protocol::ErrorCategory::Protocol,
            "invalid page cursor or limit (use 1 to 1000)",
        )
    };
    let start: usize = match &params.cursor {
        Some(cursor) => cursor.parse().map_err(|_| bad())?,
        None => 0,
    };
    let limit = params.limit.unwrap_or(100) as usize;
    if limit == 0 || limit > 1000 {
        return Err(bad());
    }
    let summary = loop_dashboard::summarize_dir(dir, params.days);
    let dashboard_summary = serde_json::to_value(&summary).map_err(failed)?;
    let mut sessions = summary.sessions;
    sessions.sort_by_key(|row| std::cmp::Reverse(row.last_active));
    if start > sessions.len() {
        return Err(bad());
    }
    let end = start.saturating_add(limit).min(sessions.len());
    let rows = sessions[start..end]
        .iter()
        .map(|row| serde_json::to_value(row).unwrap_or_default())
        .collect();
    let log_status = if summary.files == 0 {
        "no usage logged yet".to_owned()
    } else {
        format!(
            "read {} log file(s); skipped {} unreadable line(s)",
            summary.files, summary.skipped_lines
        )
    };
    Ok(admin::UsageResult {
        summary: Some(dashboard_summary),
        rows,
        totals: serde_json::to_value(&summary.totals).unwrap_or_default(),
        next_cursor: (end < sessions.len()).then(|| end.to_string()),
        taken_at_ms: std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_millis() as u64),
        log_status,
    })
}
#[async_trait::async_trait]
impl loop_wire::RuntimeControl for Control {
    async fn config(
        &self,
        p: loop_protocol::rpc::SessionParams,
    ) -> BackendResult<loop_protocol::rpc::SessionSnapshot> {
        self.runtime.config(p).await
    }

    async fn skill(
        &self,
        p: loop_protocol::session_services::SkillParams,
    ) -> BackendResult<loop_protocol::MutationResult> {
        self.runtime.skill(p).await
    }
    async fn workflow(
        &self,
        p: loop_protocol::session_services::WorkflowParams,
    ) -> BackendResult<loop_protocol::session_services::WorkflowResult> {
        self.runtime.workflow(p).await
    }

    fn admin(&self) -> Option<&dyn loop_wire::RuntimeAdmin> {
        Some(self)
    }
    async fn attach_snapshot_watching(
        &self,
        connection: u64,
        session: &loop_protocol::SessionRef,
        after_seq: u64,
    ) -> loop_protocol::BackendResult<loop_wire::Attachment> {
        loop_wire::RuntimeControl::attach_snapshot_watching(
            self.runtime.as_ref(),
            connection,
            session,
            after_seq,
        )
        .await
    }
    // Session-tree services are forwarded as one additive group.
    async fn tree(
        &self,
        params: loop_protocol::rpc::TreeParams,
    ) -> loop_protocol::BackendResult<loop_protocol::rpc::TreeResult> {
        loop_wire::RuntimeControl::tree(self.runtime.as_ref(), params).await
    }
    async fn view(
        &self,
        params: loop_protocol::rpc::ViewParams,
    ) -> loop_protocol::BackendResult<loop_protocol::rpc::ViewResult> {
        loop_wire::RuntimeControl::view(self.runtime.as_ref(), params).await
    }
    async fn navigate(
        &self,
        params: loop_protocol::rpc::NavigateParams,
    ) -> loop_protocol::BackendResult<loop_protocol::rpc::NavigateResult> {
        loop_wire::RuntimeControl::navigate(self.runtime.as_ref(), params).await
    }
    async fn rename(
        &self,
        params: loop_protocol::rpc::RenameParams,
    ) -> loop_protocol::BackendResult<loop_protocol::rpc::MutationResult> {
        loop_wire::RuntimeControl::rename(self.runtime.as_ref(), params).await
    }
    async fn fork(
        &self,
        params: loop_protocol::rpc::ForkParams,
    ) -> loop_protocol::BackendResult<loop_protocol::rpc::DestinationResult> {
        loop_wire::RuntimeControl::fork(self.runtime.as_ref(), params).await
    }
    async fn clone_session(
        &self,
        params: loop_protocol::rpc::CloneParams,
    ) -> loop_protocol::BackendResult<loop_protocol::rpc::DestinationResult> {
        loop_wire::RuntimeControl::clone_session(self.runtime.as_ref(), params).await
    }
    async fn export(
        &self,
        params: loop_protocol::rpc::SessionParams,
    ) -> loop_protocol::BackendResult<loop_protocol::rpc::ExportResult> {
        loop_wire::RuntimeControl::export(self.runtime.as_ref(), params).await
    }
    async fn import(
        &self,
        params: loop_protocol::rpc::ImportParams,
    ) -> loop_protocol::BackendResult<loop_protocol::rpc::DestinationResult> {
        loop_wire::RuntimeControl::import(self.runtime.as_ref(), params).await
    }
    async fn recipient(
        &self,
        session: &loop_protocol::SessionRef,
        connection: u64,
        recipient: Option<Arc<dyn loop_wire::control::EscalationRecipient>>,
    ) -> loop_protocol::BackendResult<()> {
        loop_wire::RuntimeControl::recipient(self.runtime.as_ref(), session, connection, recipient)
            .await
    }
    async fn register(
        &self,
        session: &loop_protocol::SessionRef,
        connection: u64,
        attached: bool,
        recipient: Option<Arc<dyn loop_wire::control::EscalationRecipient>>,
    ) -> loop_protocol::BackendResult<()> {
        loop_wire::RuntimeControl::register(
            self.runtime.as_ref(),
            session,
            connection,
            attached,
            recipient,
        )
        .await
    }
    async fn disconnected(&self, connection: u64) {
        loop_wire::RuntimeControl::disconnected(self.runtime.as_ref(), connection).await
    }
    async fn list(&self) -> loop_protocol::BackendResult<Vec<loop_protocol::rpc::SessionInfo>> {
        loop_wire::RuntimeControl::list(self.runtime.as_ref()).await
    }
    async fn close(
        &self,
        session: &loop_protocol::SessionRef,
        force: bool,
    ) -> loop_protocol::BackendResult<()> {
        loop_wire::RuntimeControl::close(self.runtime.as_ref(), session, force).await
    }
    async fn attach(
        &self,
        session: &loop_protocol::SessionRef,
        after_seq: u64,
    ) -> loop_protocol::BackendResult<loop_wire::Attachment> {
        loop_wire::RuntimeControl::attach(self.runtime.as_ref(), session, after_seq).await
    }
    async fn attach_watching(
        &self,
        connection: u64,
        session: &loop_protocol::SessionRef,
        after_seq: u64,
    ) -> loop_protocol::BackendResult<loop_wire::Attachment> {
        loop_wire::RuntimeControl::attach_watching(
            self.runtime.as_ref(),
            connection,
            session,
            after_seq,
        )
        .await
    }
    async fn status(&self) -> loop_protocol::BackendResult<loop_protocol::rpc::RuntimeStatus> {
        let mut status = self.runtime.status();
        status.auto_started = self.auto_started;
        status.socket = self.socket.clone();
        status.listen_url = self.listen_url.clone();
        status.max_sessions_from = Some(self.limits.max_sessions.1.into());
        status.max_active_turns_from = Some(self.limits.max_active_turns.1.into());
        Ok(status)
    }
    async fn stop_empty(&self) -> loop_protocol::BackendResult<()> {
        if !self.runtime.shutdown_if_empty().await {
            return Err(loop_protocol::BackendError::Busy);
        }
        self.stop.cancel();
        Ok(())
    }
    async fn stop(&self) -> loop_protocol::BackendResult<()> {
        // Only request it: the accept loop tears down after this answer.
        self.stop.cancel();
        Ok(())
    }
}
fn socket_or_default(socket: Option<PathBuf>) -> anyhow::Result<PathBuf> {
    match socket {
        Some(socket) => Ok(socket),
        None => Ok(crate::config::paths::runtime_socket_path(
            &crate::config::paths::get_agent_dir(),
        )?),
    }
}

/// A control connection to the Runtime on `socket`, or `None` when nothing
/// answers there.
async fn control_client(
    socket: &std::path::Path,
) -> anyhow::Result<Option<Arc<loop_wire::RemoteBackend>>> {
    let Ok(stream) = tokio::net::UnixStream::connect(socket).await else {
        return Ok(None);
    };
    let backend = tokio::time::timeout(
        Duration::from_secs(5),
        loop_wire::RemoteBackend::connect(
            loop_wire::socket::unix(stream),
            loop_protocol::rpc::PeerInfo {
                name: "loop runtime".into(),
                version: env!("CARGO_PKG_VERSION").into(),
            },
            Arc::new(NoInteraction),
        ),
    )
    .await
    .map_err(|_| anyhow::anyhow!("the Runtime at {} did not answer", socket.display()))??;
    Ok(Some(backend))
}
struct NoInteraction;
#[async_trait::async_trait]
impl loop_protocol::HostServices for NoInteraction {
    async fn interact(
        &self,
        _: loop_protocol::InteractionRequest,
    ) -> loop_protocol::InteractionOutcome {
        loop_protocol::InteractionOutcome::Rejected {
            reason: "`loop runtime` answers no Escalations".into(),
        }
    }
}

/// Ask the Runtime on `socket` to stop and wait for its socket to go. Never
/// signals a process: with nothing answering it only says so.
pub async fn stop(socket: Option<PathBuf>) -> anyhow::Result<()> {
    let socket = socket_or_default(socket)?;
    let Some(backend) = control_client(&socket).await? else {
        println!("No Runtime running at {}.", socket.display());
        return Ok(());
    };
    let pid = backend.status().await.ok().map(|s| s.pid);
    match backend.stop().await {
        Ok(()) => {}
        // The Runtime may close the connection before its answer arrives.
        Err(loop_protocol::BackendError::Failed(info))
            if info.category == loop_protocol::ErrorCategory::Network => {}
        Err(error) => return Err(error.into()),
    }
    // No `shutdown` RPC: the Runtime is already closing this connection.
    drop(backend);
    let gone = tokio::time::timeout(STOP_WAIT, async {
        while socket.exists() {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .is_ok();
    if !gone {
        anyhow::bail!(
            "the Runtime was asked to stop but its socket {} is still there after {} s; it may still be closing Sessions",
            socket.display(),
            STOP_WAIT.as_secs()
        );
    }
    match pid {
        Some(pid) => println!("Runtime (pid {pid}) stopped."),
        None => println!("Runtime stopped."),
    }
    Ok(())
}

/// Print the Runtime's status. `Ok(false)` when no Runtime answers.
pub async fn status(socket: Option<PathBuf>, json: bool) -> anyhow::Result<bool> {
    let socket = socket_or_default(socket)?;
    let Some(backend) = control_client(&socket).await? else {
        eprintln!("No Runtime running at {}.", socket.display());
        return Ok(false);
    };
    let status = backend.status().await?;
    let _ = tokio::time::timeout(
        Duration::from_secs(2),
        loop_protocol::AgentBackend::shutdown(backend.as_ref()),
    )
    .await;
    if json {
        println!("{}", serde_json::to_string_pretty(&status)?);
    } else {
        println!(
            "Runtime running (pid {}, generation {})",
            status.pid, status.generation
        );
        println!("socket:  {}", status.socket.display());
        if let Some(url) = &status.listen_url {
            println!("listen:  {url}");
        }
        let from = |source: &Option<String>| {
            source
                .as_deref()
                .map(|s| format!(" ({s})"))
                .unwrap_or_default()
        };
        println!(
            "Sessions: {} loaded of {}{}, {} busy; at most {} turns at once{}",
            status.loaded,
            status.max_sessions,
            from(&status.max_sessions_from),
            status.busy,
            status.max_active_turns,
            from(&status.max_active_turns_from)
        );
        if let Some(lock) = status.tool_lock {
            println!("tool lock: {lock} (fixed until the Runtime stops)");
        }
    }
    Ok(true)
}

/// Outcome of the first poll of the shutdown future.
pub enum Registered<F> {
    /// Handlers are registered; keep awaiting this future.
    Pending(std::pin::Pin<Box<F>>),
    /// Shutdown was already requested; the future must not be polled again.
    Done,
}

/// Poll once so handlers register, keeping a shutdown that is already ready.
pub async fn register<F>(mut future: std::pin::Pin<Box<F>>) -> std::io::Result<Registered<F>>
where
    F: std::future::Future<Output = std::io::Result<()>>,
{
    futures::future::poll_fn(|cx| match future.as_mut().poll(cx) {
        std::task::Poll::Ready(Err(e)) => std::task::Poll::Ready(Err(e)),
        std::task::Poll::Ready(Ok(())) => std::task::Poll::Ready(Ok(true)),
        std::task::Poll::Pending => std::task::Poll::Ready(Ok(false)),
    })
    .await
    .map(|done| {
        if done {
            Registered::Done
        } else {
            Registered::Pending(future)
        }
    })
}

#[cfg(test)]
mod tests {
    /// A private file: 0600 in a 0700 folder, as `token::read` wants.
    fn private_file(dir: &std::path::Path, name: &str, body: &str) -> std::path::PathBuf {
        use std::io::Write;
        use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
        match std::fs::DirBuilder::new().mode(0o700).create(dir) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(e) => panic!("{e}"),
        }
        let file = dir.join(name);
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&file)
            .unwrap();
        writeln!(f, "{body}").unwrap();
        file
    }

    const EDGE: &str = "edge-secret-edge-secret-edge-secret-0123";

    #[test]
    fn edge_options_are_off_unless_both_are_given_safe_and_locked() {
        let loopback: std::net::SocketAddr = "127.0.0.1:0".parse().unwrap();
        let file = std::path::Path::new("/somewhere/edge.token");
        assert!(super::edge_options(None, None, false).unwrap().is_none());
        assert_eq!(
            super::edge_options(Some(loopback), Some(file), true)
                .unwrap()
                .unwrap(),
            (loopback, file.to_path_buf())
        );
        assert!(super::edge_options(Some(loopback), None, true).is_err());
        assert!(super::edge_options(None, Some(file), true).is_err());
        // Without a tool lock an edge user could run commands as this host's
        // user, so the listener is refused.
        let unlocked = super::edge_options(Some(loopback), Some(file), false).unwrap_err();
        assert!(unlocked.to_string().contains("--tool-lock"), "{unlocked}");
        // Only a loopback address: not every address, and not an
        // IPv4-mapped IPv6 address either.
        for public in [
            "0.0.0.0:7801",
            "[::]:7801",
            "[::ffff:127.0.0.1]:7801",
            "10.0.0.1:7801",
        ] {
            let public: std::net::SocketAddr = public.parse().unwrap();
            let refused = super::edge_options(Some(public), Some(file), true).unwrap_err();
            assert!(
                refused.to_string().contains("non-loopback"),
                "{public}: {refused}"
            );
        }
        let v6: std::net::SocketAddr = "[::1]:7801".parse().unwrap();
        assert!(super::edge_options(Some(v6), Some(file), true).is_ok());
    }

    #[test]
    fn a_lock_root_that_holds_a_runtime_secret_is_refused_for_edge_users() {
        let dir = tempfile::tempdir().unwrap();
        let agent = dir.path().join("home/.loop");
        let users = dir.path().join("srv/users");
        std::fs::create_dir_all(&agent).unwrap();
        std::fs::create_dir_all(&users).unwrap();
        let db = agent.join("sessions.db");
        let token = dir.path().join("gateway/edge.token");
        let secrets = [
            ("agent directory", agent.as_path()),
            ("Session database", db.as_path()),
            ("edge token file", token.as_path()),
        ];
        let home = dir.path().join("home");
        assert!(super::edge_reach(&users, &secrets, Some(&home)).is_ok());
        // A secret that does not exist yet, behind a symbolic link into the
        // root, is found through its nearest existing parent.
        let link = dir.path().join("link");
        std::os::unix::fs::symlink(&users, &link).unwrap();
        let hidden = link.join("later/sessions.db");
        let refused = super::edge_reach(
            &users,
            &[("Session database", hidden.as_path())],
            Some(&home),
        )
        .unwrap_err();
        assert!(
            refused.to_string().contains("Session database"),
            "{refused}"
        );
        for (root, what) in [
            (dir.path().join("home"), "agent directory"),
            (agent.clone(), "agent directory"),
            (dir.path().join("gateway"), "edge token file"),
            (dir.path().to_path_buf(), "agent directory"),
            // A root inside the agent directory reaches its secrets too.
            (agent.join("run"), "agent directory"),
            (agent.join("sessions"), "agent directory"),
        ] {
            let refused = super::edge_reach(&root, &secrets, Some(&home)).unwrap_err();
            assert!(refused.to_string().contains(what), "{refused}");
        }
    }

    /// With the edge listener on, a root that holds the home directory, or
    /// that overlaps a path the Seatbelt profile denies, is refused; a
    /// project folder below the home directory is not.
    #[test]
    fn a_lock_root_that_reaches_the_users_secrets_is_refused_for_edge_users() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join("Users/me");
        let agent = dir.path().join("agent");
        let project = home.join("projects/site");
        std::fs::create_dir_all(&project).unwrap();
        std::fs::create_dir_all(home.join(".ssh")).unwrap();
        std::fs::create_dir_all(&agent).unwrap();
        let secrets = [("agent directory", agent.as_path())];
        assert!(super::edge_reach(&project, &secrets, Some(&home)).is_ok());
        assert!(super::edge_reach(&home.join("projects"), &secrets, Some(&home)).is_ok());
        for root in [home.clone(), dir.path().join("Users")] {
            let refused = super::edge_reach(&root, &secrets, Some(&home)).unwrap_err();
            assert!(
                refused.to_string().contains("contains your home directory"),
                "{refused}"
            );
        }
        // The shared list, not a copy: every entry is refused, both ways.
        for path in loop_agent::harness::sandbox::seatbelt::sensitive_home_paths(&home)
            .into_iter()
            .filter(|p| p.starts_with(&home))
        {
            let inside = path.join("sub");
            let refused = super::edge_reach(&inside, &secrets, Some(&home)).unwrap_err();
            assert!(
                refused.to_string().contains("credentials or history"),
                "{refused}"
            );
        }
        let refused = super::edge_reach(&home.join(".ssh"), &secrets, Some(&home)).unwrap_err();
        assert!(refused.to_string().contains(".ssh"), "{refused}");
        let refused =
            super::edge_reach(&home.join(".config/gh"), &secrets, Some(&home)).unwrap_err();
        assert!(refused.to_string().contains(".config/gh"), "{refused}");
        // A symbolic link to the home directory cannot hide it.
        let link = dir.path().join("link");
        std::os::unix::fs::symlink(&home, &link).unwrap();
        assert!(super::edge_reach(&link, &secrets, Some(&home)).is_err());
        // Without a home directory to check against, it fails closed.
        assert!(super::edge_reach(&project, &secrets, None).is_err());
    }

    #[test]
    fn the_edge_token_is_long_private_separate_and_not_the_runtime_token() {
        let dir = tempfile::tempdir().unwrap();
        let run = dir.path().join("run");
        let runtime = private_file(
            &run,
            super::RUNTIME_TOKEN_FILE,
            "runtime-token-runtime-token-0123456789",
        );
        let gateway = dir.path().join("gateway");
        let edge = private_file(&gateway, "edge.token", EDGE);
        assert_eq!(super::edge_token(&edge, &run, Some("other")).unwrap(), EDGE);
        // Equal to the token this start made (with --listen) ...
        let same = super::edge_token(&edge, &run, Some(EDGE)).unwrap_err();
        assert!(same.to_string().contains("must differ"), "{same}");
        // ... and to the one in the token folder (without --listen).
        let twin = private_file(
            &gateway,
            "twin.token",
            "runtime-token-runtime-token-0123456789",
        );
        let stored = super::edge_token(&twin, &run, None).unwrap_err();
        assert!(stored.to_string().contains("must differ"), "{stored}");
        // The Runtime token file itself, by path or by a second name.
        let itself = super::edge_token(&runtime, &run, None).unwrap_err();
        assert!(itself.to_string().contains("token folder"), "{itself}");
        let elsewhere = dir.path().join("elsewhere");
        std::fs::create_dir(&elsewhere).unwrap();
        std::fs::set_permissions(
            &elsewhere,
            std::os::unix::fs::PermissionsExt::from_mode(0o700),
        )
        .unwrap();
        let symlink = elsewhere.join("edge.token");
        std::os::unix::fs::symlink(&runtime, &symlink).unwrap();
        let linked = super::edge_token(&symlink, &run, Some("other")).unwrap_err();
        assert!(linked.to_string().contains("token folder"), "{linked}");
        // Inside the token folder at all.
        let inside = private_file(&run, "edge.token", EDGE);
        assert!(super::edge_token(&inside, &run, Some("other")).is_err());
        // Short.
        let short = private_file(&gateway, "short.token", "edge-secret");
        let refused = super::edge_token(&short, &run, Some("other")).unwrap_err();
        assert!(refused.to_string().contains("32 bytes"), "{refused}");
        // Readable by others, or missing; the text names the edge file, not
        // the Runtime token.
        std::fs::set_permissions(&edge, std::os::unix::fs::PermissionsExt::from_mode(0o644))
            .unwrap();
        let open = format!(
            "{:#}",
            super::edge_token(&edge, &run, Some("other")).unwrap_err()
        );
        assert!(
            open.contains("edge token file") && !open.contains("Runtime token"),
            "{open}"
        );
        assert!(super::edge_token(&gateway.join("missing"), &run, None).is_err());
    }

    #[test]
    fn a_hard_link_to_the_runtime_token_is_refused() {
        // A second name of the Runtime token file in another folder: same
        // device and inode. (`token::read` also refuses nlink != 1.)
        let dir = tempfile::tempdir().unwrap();
        let run = dir.path().join("run");
        let runtime = private_file(
            &run,
            super::RUNTIME_TOKEN_FILE,
            "runtime-token-runtime-token-0123456789",
        );
        let gateway = dir.path().join("gateway");
        std::fs::create_dir(&gateway).unwrap();
        std::fs::set_permissions(
            &gateway,
            std::os::unix::fs::PermissionsExt::from_mode(0o700),
        )
        .unwrap();
        let linked = gateway.join("edge.token");
        std::fs::hard_link(&runtime, &linked).unwrap();
        let refused = super::edge_token(&linked, &run, None).unwrap_err();
        assert!(
            refused.to_string().contains("is the Runtime token file"),
            "{refused}"
        );
    }

    use super::*;

    #[test]
    fn only_auto_started_owners_have_an_idle_deadline() {
        let mut settings = crate::config::RuntimeSettings::default();
        assert_eq!(
            idle_timeout(true, &settings),
            Some(Duration::from_secs(600))
        );
        assert_eq!(idle_timeout(false, &settings), None);
        settings.idle_shutdown_minutes = Some(0);
        assert_eq!(idle_timeout(true, &settings), None);
        settings.idle_shutdown_minutes = Some(2);
        assert_eq!(
            idle_timeout(true, &settings),
            Some(Duration::from_secs(120))
        );
        assert_eq!(idle_timeout(false, &settings), None);
    }

    #[tokio::test]
    async fn a_shutdown_ready_during_registration_is_kept() {
        let ready = Box::pin(std::future::ready(Ok(())));
        assert!(matches!(register(ready).await.unwrap(), Registered::Done));
        let pending = Box::pin(std::future::pending::<std::io::Result<()>>());
        assert!(matches!(
            register(pending).await.unwrap(),
            Registered::Pending(_)
        ));
        let failed = Box::pin(std::future::ready(Err(std::io::Error::other("x"))));
        assert!(register(failed).await.is_err());
    }
}

#[cfg(test)]
mod limit_tests {
    use super::Limits;
    use crate::config::RuntimeSettings;

    fn settings(json: &str) -> RuntimeSettings {
        serde_json::from_str(json).unwrap()
    }

    #[test]
    fn flags_win_over_settings_which_win_over_defaults() {
        let none = RuntimeSettings::default();
        let limits = Limits::resolve(None, None, &none).unwrap();
        assert_eq!(limits.max_sessions, (16, "default"));
        assert_eq!(limits.max_active_turns, (8, "default"));
        let set = settings(r#"{"maxSessions":3,"maxActiveTurns":2}"#);
        let limits = Limits::resolve(None, None, &set).unwrap();
        assert_eq!(
            (limits.max_sessions, limits.max_active_turns),
            ((3, "settings"), (2, "settings"))
        );
        let limits = Limits::resolve(Some(5), None, &set).unwrap();
        assert_eq!(
            (limits.max_sessions, limits.max_active_turns),
            ((5, "flag"), (2, "settings"))
        );
    }

    #[test]
    fn zero_flags_and_bad_settings_are_refused() {
        let none = RuntimeSettings::default();
        let error = Limits::resolve(Some(0), None, &none)
            .unwrap_err()
            .to_string();
        assert!(error.contains("--max-sessions"), "{error}");
        // A bad settings value is refused even when a flag overrides it.
        let bad = settings(r#"{"maxActiveTurns":0}"#);
        let error = Limits::resolve(None, Some(2), &bad)
            .unwrap_err()
            .to_string();
        assert!(error.contains("runtime.maxActiveTurns"), "{error}");
    }
}
