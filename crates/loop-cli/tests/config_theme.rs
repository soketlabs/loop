//! Offline tests for commands, themes, and settings.

use loop_cli::commands;
use loop_cli::config::paths::{self, ENV_AGENT_DIR};
use loop_cli::config::settings::Settings;
use loop_cli::theme::Theme;

#[test]
fn builtin_commands_include_theme_and_sandbox() {
    let names: Vec<_> = commands::builtin_commands()
        .into_iter()
        .map(|c| c.name)
        .collect();
    assert!(names.contains(&"theme"));
    assert!(names.contains(&"sandbox"));
    assert!(names.contains(&"model"));
    assert!(names.contains(&"quit"));
}

#[test]
fn autocomplete_slash() {
    let hits = commands::autocomplete("/th", &[]);
    assert!(hits.iter().any(|h| h == "/theme"));
}

#[test]
fn parse_command_splits_args() {
    let c = commands::parse_command("/model soket/qwen3-30b").unwrap();
    assert_eq!(c.name, "model");
    assert_eq!(c.args, "soket/qwen3-30b");
}

#[test]
fn theme_builtins_load() {
    let dark = Theme::dark();
    assert_eq!(dark.name, "dark");
    let _ = dark.get("accent");
    let light = Theme::light();
    assert_eq!(light.name, "light");
}

#[test]
fn settings_defaults_soket() {
    let s = Settings::default();
    assert_eq!(s.default_provider, "soket");
    assert_eq!(s.default_model, "qwen3-30b");
    assert_eq!(s.theme, "dark");
    assert_eq!(s.file_edit_review, "newSession");
    assert_eq!(s.tool_permissions.get("bash").map(String::as_str), Some("ask"));
    assert_eq!(s.tool_permissions.get("read").map(String::as_str), Some("allow"));
}

#[test]
fn agent_dir_respects_env() {
    let tmp = tempfile::tempdir().unwrap();
    let custom = tmp.path().join("agent");
    std::env::set_var(ENV_AGENT_DIR, &custom);
    assert_eq!(paths::get_agent_dir(), custom);
    std::env::remove_var(ENV_AGENT_DIR);
}

#[test]
fn dispatch_quit() {
    let cmd = commands::parse_command("/quit").unwrap();
    match commands::dispatch(&cmd, &[], &[]) {
        commands::CommandEffect::Quit => {}
        other => panic!("expected Quit, got {other:?}"),
    }
}

#[test]
fn extensions_load_rhai_example() {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("examples/hello.rhai");
    let state = loop_cli::extensions::load_extensions(&[path]);
    assert!(state.commands.iter().any(|c| c.name == "hello"));
}
