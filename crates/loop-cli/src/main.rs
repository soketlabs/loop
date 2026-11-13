//! Loop — interactive coding agent CLI by Soket AI.

#[cfg(not(unix))]
compile_error!("Loop supports Linux and macOS only");

use std::path::PathBuf;

use clap::{Parser, Subcommand};
use loop_cli::print_mode::{self, TraceArgs};
use loop_cli::{bootstrap_cli, config, debug_log, BootstrapOpts};
#[cfg(feature = "telemetry")]
use loop_telemetry::TelemetryHandle;
#[cfg(feature = "telemetry")]
use tracing_subscriber::Layer;

/// Loop — interactive coding agent by Soket AI.
#[derive(Debug, Parser)]
#[command(name = "loop", version, about = "Interactive coding agent by Soket AI")]
struct Cli {
    /// Working directory.
    #[arg(long, global = true)]
    cwd: Option<PathBuf>,

    /// Provider id (default: soket).
    #[arg(long, global = true)]
    provider: Option<String>,

    /// Model id (default: qwen3-30b).
    #[arg(long, global = true)]
    model: Option<String>,

    /// Theme name.
    #[arg(long, global = true)]
    theme: Option<String>,

    /// Replace system prompt.
    #[arg(long, global = true)]
    system_prompt: Option<String>,

    /// Append to system prompt.
    #[arg(long, global = true)]
    append_system_prompt: Option<String>,

    /// Do not load AGENTS.md / CLAUDE.md context files.
    #[arg(long, short = 'c', global = true)]
    no_context_files: bool,

    /// Resume a session by id (Loop's own or an external agent's). With no
    /// id, pick one from this folder's sessions.
    #[arg(long, global = true, num_args = 0..=1, default_missing_value = "")]
    resume: Option<String>,

    /// Resume the most recent session in this folder.
    #[arg(long = "continue", global = true, conflicts_with = "resume")]
    continue_last: bool,

    /// Run without the web_search and web_fetch tools.
    #[arg(long, global = true)]
    no_web: bool,

    /// Permission mode: read-only, ask (default), accept-edits, full-access or plan.
    #[arg(long, global = true, value_name = "MODE")]
    permission_mode: Option<String>,

    /// Runtime socket path or authenticated ws:// URL.
    #[arg(long, global = true)]
    connect: Option<String>,

    /// Private WebSocket token file; otherwise use LOOP_RUNTIME_TOKEN.
    #[arg(long, global = true, requires = "connect")]
    token_file: Option<PathBuf>,

    /// Print mode: send one prompt and exit (non-interactive).
    #[arg(long, global = true)]
    print: Option<String>,

    /// With `--print`: `text` (the answer), `json` (one result object) or
    /// `stream-json` (every update as a JSON line, then the result).
    /// Exit codes: 0 completed, 1 failed, 2 usage error, 3 lost contact
    /// with the agent mid-turn, 130 cancelled.
    #[arg(long, global = true, value_enum, default_value_t = print_mode::OutputFormat::Text, requires = "print")]
    output_format: print_mode::OutputFormat,

    /// Write verbose session logs to `target/debug/logs/` (also: `LOOP_DEBUG=1`).
    #[arg(long, global = true)]
    debug: bool,

    /// Start as an MCP server (streamable HTTP) instead of interactive mode.
    #[arg(long)]
    serve_mcp: bool,

    /// Interface for the MCP server (default: 127.0.0.1).
    ///
    /// A non-loopback address requires `--mcp-token`.
    #[arg(long, default_value = "127.0.0.1")]
    mcp_host: std::net::IpAddr,

    /// Port for the MCP server (default: 3100).
    #[arg(long, default_value = "3100")]
    mcp_port: u16,

    /// Bearer token for MCP server authentication. When set, all HTTP requests
    /// to the MCP endpoint must include `Authorization: Bearer <token>`.
    /// Required when `--mcp-host` is not a loopback address.
    #[arg(long)]
    mcp_token: Option<String>,

    #[command(flatten)]
    trace: TraceArgs,

    #[command(subcommand)]
    command: Option<Commands>,
}

#[derive(Debug, Subcommand)]
enum Commands {
    /// Run, stop or check the Runtime: one process that holds many Sessions
    /// and serves them over a local socket.
    #[command(subcommand)]
    Runtime(RuntimeCommand),
    /// List, create, watch or close the Runtime's Sessions.
    #[command(subcommand)]
    Session(SessionCommand),
    /// Show config directory paths.
    Config,
    /// Print version (same as --version).
    Version,
    /// Check for or install a verified GitHub release.
    Update {
        /// Show the available version without changing the installation.
        #[arg(long, conflicts_with = "version")]
        check: bool,
        /// Install a specific release, for example 0.3.5.
        #[arg(long)]
        version: Option<String>,
        /// Reinstall the same version or allow a downgrade.
        #[arg(long, conflicts_with = "check")]
        force: bool,
    },
    /// Restore the previous installed binary.
    Rollback,
    /// Remove the installed program; keep settings, credentials and sessions.
    Uninstall,
    /// Show usage (tokens, models, tools, agents, sessions) in the terminal.
    #[cfg(feature = "usage")]
    Usage,
}

#[derive(Debug, Subcommand)]
enum RuntimeCommand {
    /// Run the Runtime in the foreground until Ctrl+C, SIGTERM or
    /// `loop runtime stop`. Never reads stdin or prompts.
    ///
    /// Folders without recorded trust get global settings only. Use the
    /// global `--permission-mode` flag to choose a mode; `full-access` takes
    /// effect only through that flag, never from settings. The global
    /// `--cwd` sets the folder for Sessions that name none.
    Start {
        #[arg(long, hide = true)]
        auto_started: bool,
        /// Also serve WebSocket clients on a loopback IP address and port.
        #[arg(long)]
        listen: Option<std::net::SocketAddr>,
        /// Socket path (default: `run/runtime.sock` in the agent directory).
        #[arg(long)]
        socket: Option<PathBuf>,
        /// Most Sessions held at once [default: `runtime.maxSessions` in
        /// settings.json, else 16].
        #[arg(long)]
        max_sessions: Option<usize>,
        /// Most turns running at once; further turns queue [default:
        /// `runtime.maxActiveTurns` in settings.json, else 8].
        #[arg(long)]
        max_active_turns: Option<usize>,
        /// Lock the tools of every Session for the life of the Runtime.
        /// `chat`: only reading files in the workspace and the web tools; no
        /// shell, file changes, Subagents, workflows, MCP servers or hooks.
        /// Nothing can widen it [default: the `LOOP_TOOL_LOCK` environment
        /// variable, else no lock].
        #[arg(long, value_name = "LOCK")]
        tool_lock: Option<String>,
        /// With `--tool-lock`: the folder every Session must be in; a
        /// Session folder outside it is refused. Required with a lock
        /// [default: the `LOOP_TOOL_LOCK_ROOT` environment variable].
        #[arg(long, value_name = "DIR")]
        tool_lock_root: Option<PathBuf>,
    },
    /// Ask the running Runtime to stop, through its socket, and wait up to
    /// 10 s for it to go. Never signals a process.
    Stop {
        /// Socket path (default: `run/runtime.sock` in the agent directory).
        #[arg(long)]
        socket: Option<PathBuf>,
    },
    /// Restart as a detached owner without an idle timeout.
    Restart {
        /// Runtime socket path.
        #[arg(long)]
        socket: Option<PathBuf>,
    },
    /// Show whether a Runtime is running and how many Sessions it holds.
    /// Exits 0 when one is running and 1 when not.
    Status {
        /// Socket path (default: `run/runtime.sock` in the agent directory).
        #[arg(long)]
        socket: Option<PathBuf>,
        /// Print the status as JSON.
        #[arg(long)]
        json: bool,
    },
}

#[derive(Debug, Subcommand)]
enum SessionCommand {
    /// List Sessions: those loaded in the Runtime and those in this folder,
    /// newest first, at most 20.
    List {
        /// List every Session: all folders, closed Subagents included.
        #[arg(long)]
        all: bool,
        /// Print the same Sessions as JSON.
        #[arg(long)]
        json: bool,
        /// Runtime socket path.
        #[arg(long)]
        socket: Option<PathBuf>,
        /// Runtime WebSocket URL (use an SSH tunnel for another host).
        #[arg(long, conflicts_with = "socket")]
        url: Option<String>,
        /// Private token file; otherwise read LOOP_RUNTIME_TOKEN.
        #[arg(long, requires = "url")]
        token_file: Option<PathBuf>,
    },
    /// Create a Session and attach to it. Ctrl+D detaches.
    New {
        /// Working directory for the Session (default: the current folder).
        #[arg(long)]
        cwd: Option<PathBuf>,
        /// Runtime socket path.
        #[arg(long)]
        socket: Option<PathBuf>,
    },
    /// Replay a Session and follow it live. Ctrl+D detaches; the Session
    /// keeps running.
    Attach {
        /// Session id or unique prefix.
        id: String,
        /// Runtime socket path.
        #[arg(long)]
        socket: Option<PathBuf>,
        /// Runtime WebSocket URL (use an SSH tunnel for another host).
        #[arg(long, conflicts_with = "socket")]
        url: Option<String>,
        /// Private token file; otherwise read LOOP_RUNTIME_TOKEN.
        #[arg(long, requires = "url")]
        token_file: Option<PathBuf>,
    },
    /// Unload a Session from the Runtime. Its history is kept, and
    /// `loop session attach` loads it again.
    ///
    /// A busy Session, including a parent whose Subagents are running, is
    /// refused unless `--force`, which cancels the turn (and its Subagents)
    /// first.
    Close {
        /// Session id or unique prefix.
        id: String,
        /// Cancel a running turn, and its Subagents, then close.
        #[arg(long)]
        force: bool,
        /// Runtime socket path.
        #[arg(long)]
        socket: Option<PathBuf>,
    },
}

fn main() {
    // First, before the async runtime and argument parsing: `loop
    // __doc-to-text <format>` is the document reader child the read and
    // write tools start, and in every other process this binary becomes
    // that reader.
    loop_agent::harness::run_document_reader_child();
    async_main();
}

#[tokio::main]
async fn async_main() {
    if let Err(e) = real_main().await {
        eprintln!("loop: {e:#}");
        std::process::exit(1);
    }
}

async fn real_main() -> anyhow::Result<()> {
    let mut cli = Cli::parse();
    // Distribution commands do not load agent settings, providers or sessions.
    if let Some(command) = &cli.command {
        match command {
            Commands::Update {
                check,
                version,
                force,
            } => {
                // Establish ownership before network traffic. The download has
                // no installation lock; replacement rechecks ownership later.
                loop_update::Installation::managed_executable()?;
                loop_update::cancellable(async {
                let client =
                    loop_update::ReleaseClient::github(std::time::Duration::from_secs(300))?;
                let release = match version {
                    Some(value) => loop_update::ReleaseVersion::parse(value)?,
                    None => { eprintln!("Checking GitHub for the latest release..."); client.latest().await? },
                };
                let current = env!("CARGO_PKG_VERSION");
                println!("Installed: {current}\nAvailable: {}", release.version);
                if *check {
                    if loop_update::is_newer(current, &release)? {
                        println!("Run `loop update` to install the new release.");
                    }
                    return Ok(());
                }
                match loop_update::plan(current, &release, *force)? {
                    loop_update::UpdatePlan::UpToDate => {
                        println!("Loop is up to date.");
                        return Ok(());
                    },
                    loop_update::UpdatePlan::DowngradeBlocked => return Err(loop_update::Error::Invalid("Use --force for an explicit downgrade, or `loop rollback` to restore the previous binary".into())),
                    loop_update::UpdatePlan::Install => (),
                }
                let staging = tempfile::Builder::new().prefix("loop-update-").tempdir()?;
                eprintln!("Downloading Loop {}...", release.version);
                let mut last = std::time::Instant::now() - std::time::Duration::from_secs(1);
                let binary = client.download_with_progress(&release, staging.path(), |bytes, total| {
                    if last.elapsed() >= std::time::Duration::from_secs(1) || total == Some(bytes) {
                        eprintln!("Downloaded {} KiB{}", bytes / 1024, total.map(|n| format!(" / {} KiB", n / 1024)).unwrap_or_default());
                        last = std::time::Instant::now();
                    }
                }).await?;
                eprintln!("Verifying and replacing the installed binary...");
                let installation = loop_update::Installation::current()?;
                installation
                    .install(&binary, &release.version.to_string())
                    .await?;
                println!("Installed Loop {}. Run `loop` to start it, or `loop rollback` to restore the previous binary.", release.version);
                Ok(())
                }).await?;
                return Ok(());
            }
            Commands::Rollback => {
                let version = loop_update::cancellable(async {
                    loop_update::Installation::current()?.rollback().await
                })
                .await?;
                println!("Restored Loop {version}. Run `loop` to start it.");
                return Ok(());
            }
            Commands::Uninstall => {
                loop_update::Installation::current()?.uninstall()?;
                println!("Loop program files removed. User settings, credentials, skills and sessions kept.");
                return Ok(());
            }
            #[cfg(feature = "usage")]
            Commands::Usage => {
                use loop_app_core::theme::{self, Theme};
                let agent_dir = config::paths::get_agent_dir();
                let settings =
                    config::Settings::load_file(&config::paths::settings_path(&agent_dir))
                        .unwrap_or_default();
                let theme =
                    Theme::load(&settings.theme, &theme::theme_search_dirs(&agent_dir, None))
                        .unwrap_or_else(|_| Theme::dark());
                let options = loop_cli::app::dashboard::options(
                    &theme,
                    config::paths::usage_dir(&agent_dir),
                    None,
                );
                tokio::task::block_in_place(|| loop_dashboard::run(options))?;
                return Ok(());
            }
            // Before bootstrap: no stdin, no trust prompt, no TUI.
            Commands::Runtime(command) => {
                use loop_cli::runtime_cmd;
                return match command {
                    RuntimeCommand::Start {
                        auto_started,
                        listen,
                        socket,
                        max_sessions,
                        max_active_turns,
                        tool_lock,
                        tool_lock_root,
                    } => {
                        let cwd = match &cli.cwd {
                            Some(cwd) => std::path::absolute(cwd)?,
                            None => std::env::current_dir()?,
                        };
                        runtime_cmd::start(runtime_cmd::StartArgs {
                            auto_started: *auto_started,
                            listen: *listen,
                            socket: socket.clone(),
                            max_sessions: *max_sessions,
                            max_active_turns: *max_active_turns,
                            permission_mode: cli.permission_mode.clone(),
                            cwd,
                            tool_lock: tool_lock.clone(),
                            tool_lock_root: tool_lock_root.clone(),
                        })
                        .await
                    }
                    RuntimeCommand::Stop { socket } => runtime_cmd::stop(socket.clone()).await,
                    RuntimeCommand::Restart { socket } => {
                        let cwd = cli.cwd.clone().unwrap_or(std::env::current_dir()?);
                        loop_cli::runtime_launch::restart(socket.clone(), &cwd).await
                    }
                    RuntimeCommand::Status { socket, json } => {
                        if !runtime_cmd::status(socket.clone(), *json).await? {
                            std::process::exit(1);
                        }
                        Ok(())
                    }
                };
            }
            Commands::Session(command) => {
                use loop_cli::session_cmd::{run, SessionArgs};
                let args = match command {
                    SessionCommand::List {
                        all,
                        json,
                        socket,
                        url,
                        token_file,
                    } => SessionArgs {
                        url: url.clone(),
                        token_file: token_file.clone(),
                        cwd: cli.cwd.clone(),
                        socket: socket.clone(),
                        all: *all,
                        json: *json,
                        ..Default::default()
                    },
                    SessionCommand::New { cwd, socket } => SessionArgs {
                        new: true,
                        cwd: cwd.clone().or(cli.cwd.clone()),
                        socket: socket.clone(),
                        ..Default::default()
                    },
                    SessionCommand::Attach {
                        id,
                        socket,
                        url,
                        token_file,
                    } => SessionArgs {
                        url: url.clone(),
                        token_file: token_file.clone(),
                        id: Some(id.clone()),
                        socket: socket.clone(),
                        ..Default::default()
                    },
                    SessionCommand::Close { id, force, socket } => SessionArgs {
                        id: Some(id.clone()),
                        close: true,
                        force: *force,
                        socket: socket.clone(),
                        ..Default::default()
                    },
                };
                return run(args).await;
            }
            _ => (),
        }
    }
    let cwd = cli
        .cwd
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")));

    let debug = debug_log::debug_enabled(cli.debug);
    let interactive = cli.print.is_none() && !cli.serve_mcp && cli.command.is_none();
    // Exports nothing until Langfuse credentials are attached below.
    #[cfg(feature = "telemetry")]
    let telemetry = TelemetryHandle::new(env!("CARGO_PKG_VERSION"), true);
    #[cfg(feature = "telemetry")]
    let export_layer = Some(telemetry.layer().boxed());
    #[cfg(not(feature = "telemetry"))]
    let export_layer = None;
    let debug_log_path = debug_log::init_tracing(debug, &cwd, interactive, export_layer)?;

    if let Some(Commands::Config) = cli.command {
        let agent = config::paths::get_agent_dir();
        println!("agent dir:     {}", agent.display());
        println!(
            "settings:      {}",
            config::paths::settings_path(&agent).display()
        );
        println!(
            "auth:          {}",
            config::paths::auth_path(&agent).display()
        );
        println!(
            "models store:  {}",
            config::paths::models_store_path(&agent).display()
        );
        println!(
            "sessions db:   {}",
            config::paths::sessions_db_path(&agent).display()
        );
        return Ok(());
    }
    if let Some(Commands::Version) = cli.command {
        println!("{}", env!("CARGO_PKG_VERSION"));
        return Ok(());
    }

    let interactive = cli.print.is_none() && !cli.serve_mcp;
    // `--resume` with no id and `--continue` choose a session in the TUI.
    let startup_sessions = match cli.resume.as_deref() {
        Some("") => Some(loop_cli::StartupSessions::Pick),
        _ if cli.continue_last => Some(loop_cli::StartupSessions::Latest),
        _ => None,
    };
    if startup_sessions.is_some() && !interactive {
        anyhow::bail!(
            "--resume without an id and --continue need the terminal UI; pass --resume <session-id>"
        );
    }
    if startup_sessions.is_some() {
        cli.resume = None;
    }
    let agent_dir = config::paths::get_agent_dir();
    // `--resume` may name a session Loop opened with a subscription, and
    // `--provider` may name a subscription (such as `chatgpt`).
    let agent_resume = cli
        .resume
        .as_deref()
        .and_then(|id| loop_app_core::agents::find_recorded_session(&agent_dir, id));
    let subscription = cli
        .provider
        .clone()
        .filter(|p| loop_app_core::agents::subscription_ids(&agent_dir).contains(p));
    let pending_agent = match (&agent_resume, &subscription) {
        (Some(record), _) => Some(loop_cli::PendingAgent::Resume(record.clone())),
        (None, Some(id)) => Some(loop_cli::PendingAgent::New {
            id: id.clone(),
            model: cli.model.clone(),
        }),
        (None, None) => None,
    };
    let (provider, model) = if subscription.is_some() {
        (None, None)
    } else {
        (cli.provider, cli.model)
    };
    let explicit_model =
        provider.is_some() || model.is_some() || cli.resume.is_some() || startup_sessions.is_some();
    if cli.serve_mcp && pending_agent.is_some() {
        anyhow::bail!("--serve-mcp serves Loop's own tools; it cannot run with an external agent");
    }
    // Refuse a bad mode before any Session is opened and leased.
    if let Some(mode) = &cli.permission_mode {
        if loop_app_core::permissions::PermissionMode::parse(mode).is_none() {
            anyhow::bail!(
                "unknown --permission-mode `{mode}`: use read-only, ask, accept-edits, full-access or plan"
            );
        }
    }
    // Before startup opens a Session: a signal from here on releases it.
    let shutdown_slot = loop_cli::runtime_signals::ShutdownSlot::default();
    if let Err(error) = loop_cli::runtime_signals::release_on_signals(
        shutdown_slot.clone(),
        !cli.serve_mcp,
        interactive,
    ) {
        tracing::warn!(%error, "could not register signal handlers");
    }
    // Subscription admission stays in-process, including a saved default.
    let saved_subscription_default = (!explicit_model || startup_sessions.is_some())
        && config::Settings::load_file(&config::paths::settings_path(&agent_dir))
            .ok()
            .and_then(|s| s.selected_model().map(|(p, _)| p.to_owned()))
            .is_some_and(|p| loop_app_core::agents::subscription_ids(&agent_dir).contains(&p));
    let mut fallback_notice: Option<String> = None;
    if interactive && pending_agent.is_none() && !saved_subscription_default {
        let opts = BootstrapOpts {
            cwd: std::fs::canonicalize(&cwd)?,
            provider: provider.clone(),
            model: model.clone(),
            theme: cli.theme.clone(),
            system_prompt: cli.system_prompt.clone(),
            append_system_prompt: cli.append_system_prompt.clone(),
            no_context_files: cli.no_context_files,
            no_web: cli.no_web,
            permission_mode: cli.permission_mode.clone(),
            interactive: true,
            session_id: cli.resume.clone(),
            agent: None,
        };
        let connected = async {
            // Trust must be recorded on the host before the daemon loads any
            // project settings or hooks for a newly opened local workspace.
            if cli.connect.is_none() {
                let mut trust =
                    config::trust::TrustStore::load(config::paths::trust_path(&agent_dir))?;
                let settings =
                    config::Settings::load_file(&config::paths::settings_path(&agent_dir))?;
                loop_app_core::runtime::resolve_trust(&mut trust, &opts.cwd, &settings, true)?;
            }
            let host = std::sync::Arc::new(loop_cli::surface_client::RemoteHost::default());
            let remote = loop_cli::runtime_launch::launch(
                cli.connect.as_deref(),
                cli.token_file.as_deref(),
                &agent_dir,
                &opts.cwd,
                host.clone(),
            )
            .await?;
            let resume = match startup_sessions {
                Some(loop_cli::StartupSessions::Pick) => Some(""),
                _ => cli.resume.as_deref(),
            };
            loop_cli::runtime_launch::open(remote, host, &opts, resume, cli.continue_last).await
        }
        .await;
        match connected {
            Ok(mut client) => {
                client.debug = debug;
                client.debug_log_path = debug_log_path;
                let result = loop_cli::app::run(client).await;
                #[cfg(feature = "telemetry")]
                print_mode::shutdown_telemetry(&telemetry).await;
                return result;
            }
            // An explicit target is what the user asked for; running somewhere
            // else would silently use a different provider and machine.
            Err(error) if cli.connect.is_some() => {
                eprintln!("loop: {error}");
                std::process::exit(1);
            }
            Err(error) => {
                let line = format!(
                    "{error}; using in-process mode for this run (Runtime log: {})",
                    agent_dir.join("logs/runtime.log").display()
                );
                // A terminal UI takes the screen next, so it shows the line
                // as its first notice; scripts and pipes get it on stderr.
                if std::io::IsTerminal::is_terminal(&std::io::stderr())
                    && std::io::IsTerminal::is_terminal(&std::io::stdout())
                {
                    fallback_notice = Some(line);
                } else {
                    eprintln!("loop: {line}");
                }
            }
        }
    }
    let mut runtime = bootstrap_cli(BootstrapOpts {
        cwd,
        provider,
        model,
        theme: cli.theme,
        system_prompt: cli.system_prompt,
        append_system_prompt: cli.append_system_prompt,
        no_context_files: cli.no_context_files,
        no_web: cli.no_web,
        permission_mode: cli.permission_mode,
        interactive,
        session_id: if agent_resume.is_some() {
            None
        } else {
            cli.resume
        },
        agent: pending_agent.as_ref().map(|pending| match pending {
            loop_cli::PendingAgent::New { id, .. } => id.clone(),
            loop_cli::PendingAgent::Resume(record) => record.agent.clone(),
        }),
    })
    .await?;
    // With no model asked for, a saved subscription model starts as before.
    let saved_subscription = (pending_agent.is_none() && !explicit_model)
        .then(|| {
            runtime
                .settings
                .selected_model()
                .filter(|(p, _)| runtime.is_subscription(p))
                .map(|(p, m)| loop_cli::PendingAgent::New {
                    id: p.to_owned(),
                    model: Some(m.to_owned()),
                })
        })
        .flatten();
    runtime.pending_agent = pending_agent.or(saved_subscription);
    runtime.startup_sessions = startup_sessions;
    runtime.startup_notices.extend(fallback_notice);
    runtime.debug = debug;
    runtime.debug_log_path = debug_log_path;
    #[cfg(feature = "telemetry")]
    if let Err(err) = runtime.attach_telemetry(telemetry.clone()) {
        tracing::warn!(error = %err, "Langfuse tracing disabled");
    }

    // Kept before the Runtime is moved: every exit path below, and signals,
    // end the Session work and release its leases through it.
    let shutdown = runtime.session_shutdown();
    let _ = shutdown_slot.set(shutdown.clone());
    let release = || async {
        if let Err(error) = shutdown.run().await {
            eprintln!("loop: {error:#}");
        }
    };
    let result = if cli.serve_mcp {
        loop_cli::mcp_serve::run_mcp_server(
            runtime.inner,
            cli.mcp_host,
            cli.mcp_port,
            cli.mcp_token,
        )
        .await
    } else if let Some(prompt) = cli.print {
        let result =
            print_mode::run_print(&mut runtime, prompt, cli.output_format, &cli.trace).await;
        runtime.agents.shutdown().await;
        release().await;
        match result {
            Ok(0) => Ok(()),
            Ok(code) => {
                #[cfg(feature = "telemetry")]
                print_mode::shutdown_telemetry(&telemetry).await;
                std::process::exit(code);
            }
            Err(err) => Err(err),
        }
    } else {
        loop_cli::app::run(runtime.into()).await
    };
    // Idempotent: the TUI and `--print` already released.
    release().await;
    #[cfg(feature = "telemetry")]
    print_mode::shutdown_telemetry(&telemetry).await;
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(args: &[&str]) -> Cli {
        Cli::try_parse_from(args).unwrap_or_else(|e| panic!("{args:?}: {e}"))
    }

    #[test]
    fn the_command_tree_is_valid() {
        use clap::CommandFactory;
        Cli::command().debug_assert();
    }

    #[test]
    fn serve_and_attach_are_gone() {
        for args in [
            &["loop", "serve"][..],
            &["loop", "attach"],
            &["loop", "attach", "abc"],
            &["loop", "attach", "--new"],
        ] {
            assert!(Cli::try_parse_from(args).is_err(), "{args:?} still parses");
        }
    }

    #[test]
    fn interactive_connection_and_continue_flags_parse() {
        let cli = parse(&["loop", "-c", "--connect", "/tmp/runtime.sock"]);
        assert!(!cli.continue_last);
        assert!(cli.no_context_files);
        assert_eq!(cli.connect.as_deref(), Some("/tmp/runtime.sock"));
        assert!(parse(&["loop", "-c", "--resume", "id"]).no_context_files);
        assert!(parse(&["loop", "--continue"]).continue_last);

        let cli = parse(&[
            "loop",
            "--resume",
            "session",
            "--connect",
            "ws://127.0.0.1:9",
            "--token-file",
            "/tmp/token",
        ]);
        assert_eq!(cli.resume.as_deref(), Some("session"));
        assert_eq!(cli.token_file, Some("/tmp/token".into()));
        assert!(matches!(
            parse(&["loop", "runtime", "restart"]).command,
            Some(Commands::Runtime(RuntimeCommand::Restart { .. }))
        ));
    }

    #[test]
    fn runtime_subcommands_parse() {
        let cli = parse(&["loop", "runtime", "start"]);
        let Some(Commands::Runtime(RuntimeCommand::Start {
            socket,
            max_sessions,
            max_active_turns,
            ..
        })) = cli.command
        else {
            panic!("not runtime start")
        };
        assert_eq!((socket, max_sessions, max_active_turns), (None, None, None));
        let cli = parse(&[
            "loop",
            "runtime",
            "start",
            "--socket",
            "/tmp/x.sock",
            "--max-sessions",
            "3",
            "--max-active-turns",
            "2",
            "--permission-mode",
            "full-access",
            "--cwd",
            "/work",
        ]);
        assert_eq!(cli.permission_mode.as_deref(), Some("full-access"));
        assert_eq!(cli.cwd, Some(PathBuf::from("/work")));
        let Some(Commands::Runtime(RuntimeCommand::Start {
            socket,
            max_sessions,
            max_active_turns,
            ..
        })) = cli.command
        else {
            panic!("not runtime start")
        };
        assert_eq!(socket, Some(PathBuf::from("/tmp/x.sock")));
        assert_eq!((max_sessions, max_active_turns), (Some(3), Some(2)));
        let cli = parse(&[
            "loop",
            "runtime",
            "start",
            "--tool-lock",
            "chat",
            "--tool-lock-root",
            "/srv/users",
        ]);
        let Some(Commands::Runtime(RuntimeCommand::Start {
            tool_lock,
            tool_lock_root,
            ..
        })) = cli.command
        else {
            panic!("not runtime start")
        };
        assert_eq!(tool_lock.as_deref(), Some("chat"));
        assert_eq!(tool_lock_root, Some(PathBuf::from("/srv/users")));
        let cli = parse(&["loop", "runtime", "stop", "--socket", "/tmp/x.sock"]);
        assert!(matches!(
            cli.command,
            Some(Commands::Runtime(RuntimeCommand::Stop { socket: Some(_) }))
        ));
        let cli = parse(&["loop", "runtime", "status", "--json"]);
        assert!(matches!(
            cli.command,
            Some(Commands::Runtime(RuntimeCommand::Status {
                json: true,
                socket: None
            }))
        ));
        assert!(Cli::try_parse_from(["loop", "runtime"]).is_err());
    }

    #[test]
    fn session_subcommands_parse() {
        let cli = parse(&["loop", "session", "list", "--all", "--json"]);
        assert!(matches!(
            cli.command,
            Some(Commands::Session(SessionCommand::List {
                all: true,
                json: true,
                socket: None,
                ..
            }))
        ));
        let cli = parse(&[
            "loop",
            "session",
            "new",
            "--cwd",
            "/tmp",
            "--socket",
            "/tmp/r.sock",
        ]);
        let Some(Commands::Session(SessionCommand::New { cwd, socket })) = cli.command else {
            panic!("not session new")
        };
        assert_eq!(cwd, Some(PathBuf::from("/tmp")));
        assert_eq!(socket, Some(PathBuf::from("/tmp/r.sock")));
        let cli = parse(&["loop", "session", "attach", "abc"]);
        let Some(Commands::Session(SessionCommand::Attach { id, .. })) = cli.command else {
            panic!("not session attach")
        };
        assert_eq!(id, "abc");
        assert!(Cli::try_parse_from(["loop", "session", "attach"]).is_err());
        let cli = parse(&["loop", "session", "close", "abc", "--force"]);
        let Some(Commands::Session(SessionCommand::Close { id, force, .. })) = cli.command else {
            panic!("not session close")
        };
        assert_eq!((id.as_str(), force), ("abc", true));
        let cli = parse(&["loop", "session", "close", "abc"]);
        assert!(matches!(
            cli.command,
            Some(Commands::Session(SessionCommand::Close {
                force: false,
                ..
            }))
        ));
        assert!(Cli::try_parse_from(["loop", "session", "close"]).is_err());
        assert!(Cli::try_parse_from(["loop", "session", "new", "abc"]).is_err());
        assert!(Cli::try_parse_from(["loop", "session"]).is_err());
    }

    #[test]
    fn runtime_start_help_describes_the_command() {
        use clap::CommandFactory;
        let mut cmd = Cli::command();
        let help = cmd
            .find_subcommand_mut("runtime")
            .unwrap()
            .find_subcommand_mut("start")
            .unwrap()
            .render_long_help()
            .to_string();
        assert!(help.contains("--socket") && help.contains("--max-sessions"));
        assert!(help.contains("full-access"));
    }
}
