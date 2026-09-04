//! Loop — interactive coding agent CLI by Soket AI.

use std::path::PathBuf;

use clap::{Parser, Subcommand};
use loop_cli::{bootstrap_cli, config, BootstrapOpts};
use tracing_subscriber::EnvFilter;

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

    /// Resume a session by id.
    #[arg(long, global = true)]
    resume: Option<String>,

    /// Print mode: send one prompt and exit (non-interactive).
    #[arg(long, global = true)]
    print: Option<String>,

    /// Start as an MCP server (streamable HTTP) instead of interactive mode.
    #[arg(long)]
    serve_mcp: bool,

    /// Port for the MCP server (default: 3100).
    #[arg(long, default_value = "3100")]
    mcp_port: u16,

    /// Bearer token for MCP server authentication. When set, all HTTP requests
    /// to the MCP endpoint must include `Authorization: Bearer <token>`.
    #[arg(long)]
    mcp_token: Option<String>,

    #[command(subcommand)]
    command: Option<Commands>,
}

#[derive(Debug, Subcommand)]
enum Commands {
    /// Show config directory paths.
    Config,
    /// Print version (same as --version).
    Version,
}

#[tokio::main]
async fn main() {
    if let Err(e) = real_main().await {
        eprintln!("loop: {e:#}");
        std::process::exit(1);
    }
}

async fn real_main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("warn")),
        )
        .with_writer(std::io::stderr)
        .init();

    let cli = Cli::parse();
    let cwd = cli
        .cwd
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")));

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
    let runtime = bootstrap_cli(BootstrapOpts {
        cwd,
        provider: cli.provider,
        model: cli.model,
        theme: cli.theme,
        system_prompt: cli.system_prompt,
        append_system_prompt: cli.append_system_prompt,
        no_context_files: cli.no_context_files,
        interactive,
        session_id: cli.resume,
    })
    .await?;

    if cli.serve_mcp {
        return loop_cli::mcp_serve::run_mcp_server(runtime.inner, cli.mcp_port, cli.mcp_token).await;
    }

    if let Some(prompt) = cli.print {
        let msg = runtime.harness.prompt(prompt).await?;
        if let Some(text) = msg.as_llm().and_then(|m| match m {
            loop_ai::Message::Assistant(a) => Some(
                a.content
                    .iter()
                    .filter_map(|b| match b {
                        loop_ai::AssistantContent::Text(t) => Some(t.text.as_str()),
                        _ => None,
                    })
                    .collect::<Vec<_>>()
                    .join(""),
            ),
            _ => None,
        }) {
            println!("{text}");
        } else {
            println!("{msg:?}");
        }
        return Ok(());
    }

    loop_cli::app::run(runtime).await
}
