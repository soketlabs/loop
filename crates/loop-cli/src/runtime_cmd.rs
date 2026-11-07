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
}

#[cfg(not(unix))]
const UNSUPPORTED: &str =
    "`loop runtime` is not supported on this platform (it needs Unix sockets); Linux and macOS are supported";

/// Run the Runtime until SIGINT, SIGTERM or `loop runtime stop`.
#[cfg(not(unix))]
pub async fn start(_args: StartArgs) -> anyhow::Result<()> {
    anyhow::bail!(UNSUPPORTED)
}

/// Ask the Runtime on `socket` to stop.
#[cfg(not(unix))]
pub async fn stop(_socket: Option<PathBuf>) -> anyhow::Result<()> {
    anyhow::bail!(UNSUPPORTED)
}

/// Print the Runtime's status. `Ok(false)` when no Runtime answers.
#[cfg(not(unix))]
pub async fn status(_socket: Option<PathBuf>, _json: bool) -> anyhow::Result<bool> {
    anyhow::bail!(UNSUPPORTED)
}

/// Run the Runtime until SIGINT, SIGTERM or `loop runtime stop`.
#[cfg(unix)]
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
    use loop_wire::server::{serve_with_control, ServeOptions};

    if let Some(mode) = &args.permission_mode {
        if PermissionMode::parse(mode).is_none() {
            anyhow::bail!(
                "unknown --permission-mode `{mode}`: use read-only, ask, accept-edits or full-access"
            );
        }
    }
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
    let sessions_lock = loop_app_core::runtime_instance::SessionDatabaseLock::acquire(&sessions_db)
        .map_err(|e| anyhow::anyhow!("{e}"))?;

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
    let stop = tokio_util::sync::CancellationToken::new();
    let control: Arc<dyn loop_wire::RuntimeControl> = Arc::new(Control {
        runtime: runtime.clone(),
        socket: std::path::absolute(&socket).unwrap_or_else(|_| socket.clone()),
        stop: stop.clone(),
        limits,
    });

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
#[cfg(unix)]
struct Control {
    runtime: Arc<loop_runtime::Runtime>,
    socket: PathBuf,
    stop: tokio_util::sync::CancellationToken,
    limits: Limits,
}

#[cfg(unix)]
#[async_trait::async_trait]
impl loop_wire::RuntimeControl for Control {
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
    async fn fork(
        &self,
        params: loop_protocol::rpc::ForkParams,
    ) -> loop_protocol::BackendResult<loop_protocol::SessionRef> {
        loop_wire::RuntimeControl::fork(self.runtime.as_ref(), params).await
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
        status.socket = self.socket.clone();
        status.max_sessions_from = Some(self.limits.max_sessions.1.into());
        status.max_active_turns_from = Some(self.limits.max_active_turns.1.into());
        Ok(status)
    }
    async fn stop(&self) -> loop_protocol::BackendResult<()> {
        // Only request it: the accept loop tears down after this answer.
        self.stop.cancel();
        Ok(())
    }
}

#[cfg(unix)]
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
#[cfg(unix)]
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

#[cfg(unix)]
struct NoInteraction;
#[cfg(unix)]
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
#[cfg(unix)]
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
#[cfg(unix)]
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
