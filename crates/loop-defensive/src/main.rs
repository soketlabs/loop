//! `loop-defensive`: the `/patch` and `/audit` workflows for Loop.
//!
//!   loop-defensive serve patch|audit    MCP server (Loop starts it)
//!   loop-defensive install [--settings] put /patch and /audit into Loop
//!   loop-defensive patch <target>       headless run via `loop --print`
//!   loop-defensive audit <github-url>   headless audit
//!   loop-defensive report <run-dir>     print a finished report.md

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use clap::{Parser, Subcommand, ValueEnum};
use loop_defensive::{headless, mcp, task, Session, Workflow};

#[derive(Parser)]
#[command(name = "loop-defensive", version, about = "Find, fix and grade vulnerabilities in a podman sandbox, for Loop")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Clone, Copy, ValueEnum)]
enum Kind {
    Patch,
    Audit,
}

impl From<Kind> for Workflow {
    fn from(k: Kind) -> Self {
        match k {
            Kind::Patch => Workflow::Patch,
            Kind::Audit => Workflow::Audit,
        }
    }
}

#[derive(Subcommand)]
enum Command {
    /// Serve a workflow's tools over MCP on stdin/stdout (Loop starts this).
    Serve {
        /// Which workflow.
        workflow: Kind,
    },
    /// Install the /patch and /audit prompt templates into Loop; with
    /// --settings also register the MCP servers in Loop's settings.json
    /// (backups are kept).
    Install {
        /// Also update settings.json.
        #[arg(long)]
        settings: bool,
    },
    /// Find, reproduce and fix a vulnerability, then grade the fix (headless).
    #[command(alias = "run")]
    Patch {
        /// CVE id, GitHub URL (optionally .../tree/<ref>) or folder.
        target: String,
        /// Model id (default: Loop's default model).
        #[arg(long)]
        model: Option<String>,
        /// Provider id (default: Loop's default provider).
        #[arg(long)]
        provider: Option<String>,
        /// Give up after this many seconds.
        #[arg(long, default_value_t = 7200)]
        timeout: u64,
    },
    /// Review a GitHub repository for vulnerabilities (headless).
    Audit {
        /// GitHub repository URL.
        url: String,
        /// Model id.
        #[arg(long)]
        model: Option<String>,
        /// Provider id.
        #[arg(long)]
        provider: Option<String>,
        /// Give up after this many seconds.
        #[arg(long, default_value_t = 7200)]
        timeout: u64,
    },
    /// Print a finished run's report.md (a run directory or its `latest` link).
    Report {
        /// Run directory.
        dir: PathBuf,
    },
}

fn main() {
    let cli = Cli::parse();
    let code = match cli.command {
        Command::Serve { workflow } => {
            let runtime = tokio::runtime::Builder::new_multi_thread().enable_all().build().expect("tokio runtime");
            let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
            let session = Arc::new(Session::new(task::runs_root(), cwd));
            match runtime.block_on(mcp::serve(workflow.into(), session)) {
                Ok(()) => 0,
                Err(e) => {
                    eprintln!("loop-defensive: {e}");
                    1
                }
            }
        }
        Command::Install { settings } => {
            let exe = std::env::current_exe().unwrap_or_else(|_| PathBuf::from("loop-defensive"));
            let dir = headless::loop_agent_dir();
            match headless::install(&dir, &exe, settings) {
                Ok(done) => {
                    for p in &done.prompts {
                        println!("wrote {}", p.display());
                    }
                    for b in &done.backups {
                        println!("kept the previous file as {}", b.display());
                    }
                    if done.settings_written {
                        println!("updated {}", dir.join("settings.json").display());
                    } else {
                        println!(
                            "\nTo register the servers, merge into {} (or rerun with --settings):\n{}",
                            dir.join("settings.json").display(),
                            serde_json::to_string_pretty(&done.settings_entries).unwrap_or_default()
                        );
                    }
                    0
                }
                Err(e) => {
                    eprintln!("loop-defensive: {e}");
                    1
                }
            }
        }
        Command::Patch { target, model, provider, timeout } => headless_run(Workflow::Patch, &target, model, provider, timeout),
        Command::Audit { url, model, provider, timeout } => headless_run(Workflow::Audit, &url, model, provider, timeout),
        Command::Report { dir } => match std::fs::read_to_string(dir.join("report.md")) {
            Ok(text) => {
                print!("{text}");
                0
            }
            Err(e) => {
                eprintln!("loop-defensive: {}: {e}", dir.join("report.md").display());
                1
            }
        },
    };
    std::process::exit(code);
}

fn headless_run(workflow: Workflow, target: &str, model: Option<String>, provider: Option<String>, timeout: u64) -> i32 {
    eprintln!("loop-defensive: reports under {}", task::runs_root().display());
    match headless::run(workflow, target, model.as_deref(), provider.as_deref(), Duration::from_secs(timeout)) {
        Ok(code) => code,
        Err(e) => {
            eprintln!("loop-defensive: {e}");
            1
        }
    }
}
