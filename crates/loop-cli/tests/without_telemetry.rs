//! A build without the `telemetry` feature has no tracing surface.
#![cfg(not(feature = "telemetry"))]

use clap::Parser;
use loop_cli::commands::{self, CommandEffect};
use loop_cli::print_mode::TraceArgs;

#[test]
fn tracing_command_is_not_offered() {
    assert!(!commands::builtin_commands()
        .iter()
        .any(|c| c.name == "tracing"));
    let cmd = commands::parse_command("/tracing status").unwrap();
    match commands::dispatch(&cmd, &[], &[]) {
        CommandEffect::Status(text) => assert!(text.starts_with("Unknown command: /tracing")),
        other => panic!("expected unknown command, got {other:?}"),
    }
}

#[test]
fn trace_flags_are_not_accepted() {
    #[derive(Parser)]
    struct Cli {
        #[command(flatten)]
        _trace: TraceArgs,
    }
    assert!(Cli::try_parse_from(["loop"]).is_ok());
    assert!(Cli::try_parse_from(["loop", "--trace-tag", "bench"]).is_err());
}
