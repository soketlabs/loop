//! `loop serve`: the Runtime command. Never reads stdin, never prompts, never starts a TUI.

use std::path::PathBuf;

/// Options of `loop serve`.
#[derive(Debug, Clone)]
pub struct ServeArgs {
    /// Socket path; default is the agent directory's Runtime socket.
    pub socket: Option<PathBuf>,
    /// Most Sessions the Runtime holds at once.
    pub max_sessions: usize,
    /// Most turns running at once; the rest queue.
    pub max_active_turns: usize,
    /// `--permission-mode`; `full-access` only takes effect through this flag.
    pub permission_mode: Option<String>,
    /// Folder for Sessions that name none.
    pub cwd: PathBuf,
}

/// Run the Runtime until SIGINT or SIGTERM.
#[cfg(not(unix))]
pub async fn run(_args: ServeArgs) -> anyhow::Result<()> {
    anyhow::bail!("`loop serve` is not supported on this platform (it needs Unix sockets)")
}

/// Run the Runtime until SIGINT or SIGTERM.
#[cfg(unix)]
pub async fn run(args: ServeArgs) -> anyhow::Result<()> {
    use anyhow::Context;
    use loop_agent::harness::session::{
        sqlite::{LeaseIdentity, SqliteSessionStore},
        SessionStore,
    };
    use loop_app_core::{
        config::{paths, Settings},
        permissions::PermissionMode,
        runtime::{BootstrapOpts, GlobalResources},
        runtime_instance::RuntimeInstanceLock,
    };
    use loop_protocol::{rpc::PeerInfo, AgentBackend};
    use loop_runtime::{log::SqliteEventLog, Runtime, RuntimeLimits};
    use loop_wire::server::{serve_with_options, ServeOptions};

    if let Some(mode) = &args.permission_mode {
        if PermissionMode::parse(mode).is_none() {
            anyhow::bail!(
                "unknown --permission-mode `{mode}`: use read-only, ask, accept-edits or full-access"
            );
        }
    }
    if args.max_sessions == 0 || args.max_active_turns == 0 {
        anyhow::bail!("--max-sessions and --max-active-turns must be at least 1");
    }
    let agent_dir = paths::get_agent_dir();
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
    let mut signal = Box::pin(crate::runtime_signals::shutdown_signal());
    futures::future::poll_fn(|cx| match std::future::Future::poll(signal.as_mut(), cx) {
        std::task::Poll::Ready(Err(e)) => std::task::Poll::Ready(Err(e)),
        _ => std::task::Poll::Ready(Ok(())),
    })
    .await
    .context("could not register shutdown signal handlers")?;

    let settings = Settings::load_file(&paths::settings_path(&agent_dir)).unwrap_or_default();
    let globals = GlobalResources::load(agent_dir.clone(), &settings.providers, true).await?;
    let sessions_db = globals.sessions_db.clone();
    let log = Arc::new(
        SqliteEventLog::open(&agent_dir)
            .await
            .context("could not open the Event Log")?,
    );
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
        RuntimeLimits {
            max_sessions: args.max_sessions,
            max_active_turns: args.max_active_turns,
        },
    )
    .await
    .map_err(|e| anyhow::anyhow!("could not start the Runtime: {e}"))?;
    let runtime = Arc::new(runtime);

    // Holding runtime.lock proves the previous Runtime is dead: free its leases.
    let store = SqliteSessionStore::open(&sessions_db)
        .map_err(|e| anyhow::anyhow!("sqlite session store: {e}"))?;
    let ours = LeaseIdentity {
        owner: "runtime".into(),
        pid: std::process::id(),
        generation: i64::try_from(runtime.generation())?,
    };
    let mut reclaimed = 0usize;
    for meta in store
        .list(None)
        .await
        .map_err(|e| anyhow::anyhow!("listing Sessions: {e}"))?
    {
        let Ok(Some(lease)) = store.lease(&meta.id).await else {
            continue;
        };
        if lease.identity.owner != "runtime" || lease.identity == ours {
            continue;
        }
        match store.takeover_lease(&meta.id, &ours, true).await {
            Ok(()) => {
                reclaimed += 1;
                if let Err(e) = store.release_lease(&meta.id, &ours).await {
                    tracing::warn!(session = %meta.id, error = %e, "could not release reclaimed lease");
                }
            }
            Err(e) => {
                tracing::warn!(session = %meta.id, error = %e, "could not reclaim lease")
            }
        }
    }
    drop(store);

    let socket = match &args.socket {
        Some(path) => path.clone(),
        None => paths::runtime_socket_path(&agent_dir)?,
    };
    // `unix_listen` probes, and replaces only a socket proven stale.
    let listener = loop_wire::socket::unix_listen(&socket)
        .with_context(|| format!("could not listen on {}", socket.display()))?;
    tracing::info!(
        socket = %socket.display(),
        generation = runtime.generation(),
        reclaimed,
        "Runtime listening"
    );
    eprintln!("loop serve: listening on {}", socket.display());

    let info = PeerInfo {
        name: "loop".into(),
        version: env!("CARGO_PKG_VERSION").into(),
    };
    let mut connections = tokio::task::JoinSet::new();
    loop {
        tokio::select! {
            result = &mut signal => {
                if let Err(e) = result {
                    tracing::error!(error = %e, "shutdown signal failed");
                }
                break;
            }
            accepted = listener.accept() => match accepted {
                Ok((stream, _)) => {
                    let backend: Arc<dyn AgentBackend> = runtime.clone();
                    let served = serve_with_options(
                        backend,
                        loop_wire::socket::unix(stream),
                        info.clone(),
                        ServeOptions { owns_backend: false },
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
            Some(_) = connections.join_next(), if !connections.is_empty() => {}
        }
    }
    tracing::info!("Runtime shutting down");
    drop(listener);
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

#[cfg(unix)]
use std::sync::Arc;
