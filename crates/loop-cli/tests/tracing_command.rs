//! `/tracing` slash command parsing and dispatch.

use loop_cli::commands::{self, CommandEffect, TracingCommand};

fn dispatch(line: &str) -> CommandEffect {
    let cmd = commands::parse_command(line).unwrap();
    commands::dispatch(&cmd, &[], &[])
}

fn tracing(line: &str) -> TracingCommand {
    match dispatch(line) {
        CommandEffect::Tracing(command) => command,
        other => panic!("`{line}` should dispatch to Tracing, got {other:?}"),
    }
}

#[test]
fn tracing_is_a_builtin_command() {
    assert!(commands::builtin_commands()
        .iter()
        .any(|c| c.name == "tracing"));
}

#[test]
fn bare_and_status_show_status() {
    assert_eq!(tracing("/tracing"), TracingCommand::Status);
    assert_eq!(tracing("/tracing status"), TracingCommand::Status);
}

#[test]
fn enable_and_disable_with_aliases() {
    assert_eq!(tracing("/tracing enable"), TracingCommand::Enable);
    assert_eq!(tracing("/tracing on"), TracingCommand::Enable);
    assert_eq!(tracing("/tracing disable"), TracingCommand::Disable);
    assert_eq!(tracing("/tracing off"), TracingCommand::Disable);
}

#[test]
fn setup_takes_host_and_public_key_only() {
    assert_eq!(
        tracing("/tracing setup https://lf.example pk-lf-1"),
        TracingCommand::Setup {
            host: "https://lf.example".into(),
            public_key: "pk-lf-1".into(),
        }
    );
}

#[test]
fn bad_input_shows_usage() {
    for line in [
        "/tracing nope",
        "/tracing setup",
        "/tracing setup https://lf.example",
        "/tracing setup https://lf.example pk sk-should-not-be-typed-here",
        "/tracing enable now",
    ] {
        match dispatch(line) {
            CommandEffect::Status(usage) => assert!(usage.starts_with("Usage: /tracing"), "{line}"),
            other => panic!("`{line}` should show usage, got {other:?}"),
        }
    }
}
