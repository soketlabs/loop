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
    use loop_wire::server::{serve_with_control, ServeOptions};

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
    let _sessions_lock = lock_session_database(&sessions_db)?;

    let settings = Settings::load_file(&paths::settings_path(&agent_dir)).unwrap_or_default();
    let globals = GlobalResources::load(agent_dir.clone(), &settings.providers, true).await?;
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
    while !shutdown_ready {
        let Some(pending) = signal.as_mut() else {
            break;
        };
        tokio::select! {
            result = pending => {
                if let Err(e) = result {
                    tracing::error!(error = %e, "shutdown signal failed");
                }
                shutdown_ready = true;
            }
            accepted = listener.accept() => match accepted {
                Ok((stream, _)) => {
                    let backend: Arc<dyn AgentBackend> = runtime.clone();
                    let control: Arc<dyn loop_wire::RuntimeControl> = runtime.clone();
                    let served = serve_with_control(
                        backend,
                        control,
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

/// Exclusive flock on `runtime.lock` beside the Session database, held for the
/// Runtime's lifetime. Leases in that database are only reclaimed under it.
#[cfg(unix)]
fn lock_session_database(sessions_db: &std::path::Path) -> anyhow::Result<std::fs::File> {
    use std::os::fd::AsRawFd;
    use std::os::unix::fs::OpenOptionsExt;
    let dir = sessions_db
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(std::path::Path::new("."));
    std::fs::create_dir_all(dir)?;
    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(dir.join("runtime.lock"))?;
    // SAFETY: the fd is valid for this call; flock does not retain it.
    if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
        anyhow::bail!(
            "another Runtime already owns the Session database at {}",
            sessions_db.display()
        );
    }
    Ok(file)
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

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
