//! `/tracing` slash command parsing and dispatch.

use loop_app_core::config::TracingBackend;
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

#[cfg(feature = "dashboard")]
#[test]
fn dashboard_is_a_builtin_command() {
    assert!(commands::builtin_commands()
        .iter()
        .any(|c| c.name == "dashboard"));
    assert!(matches!(dispatch("/dashboard"), CommandEffect::Dashboard));
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
fn local_shows_or_switches_usage_log() {
    assert_eq!(tracing("/tracing local"), TracingCommand::Local(None));
    assert_eq!(
        tracing("/tracing local off"),
        TracingCommand::Local(Some(false))
    );
    assert_eq!(
        tracing("/tracing local on"),
        TracingCommand::Local(Some(true))
    );
}

#[test]
fn anonymous_shows_or_sets_mode() {
    assert_eq!(
        tracing("/tracing anonymous"),
        TracingCommand::Anonymous(None)
    );
    assert_eq!(
        tracing("/tracing anonymous on"),
        TracingCommand::Anonymous(Some(true))
    );
    assert_eq!(
        tracing("/tracing anonymous off"),
        TracingCommand::Anonymous(Some(false))
    );
}

#[test]
fn setup_opens_wizard_with_optional_backend() {
    assert_eq!(
        tracing("/tracing setup"),
        TracingCommand::Setup { backend: None }
    );
    assert_eq!(
        tracing("/tracing setup langfuse"),
        TracingCommand::Setup {
            backend: Some(TracingBackend::Langfuse)
        }
    );
    assert_eq!(
        tracing("/tracing setup otlp"),
        TracingCommand::Setup {
            backend: Some(TracingBackend::Otlp)
        }
    );
    assert_eq!(
        tracing("/tracing setup file"),
        TracingCommand::Setup {
            backend: Some(TracingBackend::File)
        }
    );
}

#[test]
fn bad_input_shows_usage() {
    for line in [
        "/tracing nope",
        "/tracing setup jaeger",
        "/tracing setup https://lf.example pk-lf-1",
        "/tracing enable now",
        "/tracing anonymous maybe",
    ] {
        match dispatch(line) {
            CommandEffect::Status(usage) => assert!(usage.starts_with("Usage: /tracing"), "{line}"),
            other => panic!("`{line}` should show usage, got {other:?}"),
        }
    }
}
