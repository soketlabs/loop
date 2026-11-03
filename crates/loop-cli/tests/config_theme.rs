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
fn parse_command_preserves_slash_in_skill_and_template_names() {
    let skill_cmd = commands::parse_command("/skill:ops/review").unwrap();
    assert_eq!(skill_cmd.name, "skill:ops/review");
    assert_eq!(skill_cmd.args, "");

    let tmpl_cmd = commands::parse_command("/ops/template arg1 arg2").unwrap();
    assert_eq!(tmpl_cmd.name, "ops/template");
    assert_eq!(tmpl_cmd.args, "arg1 arg2");
}

#[test]
fn dispatch_slash_in_skill_and_template_names() {
    let skill_names = vec!["ops/review".to_string()];
    let template_names = vec!["ops/template".to_string()];

    // Explicit skill: prefix with slash
    let cmd = commands::parse_command("/skill:ops/review").unwrap();
    assert!(commands::is_command(&cmd, &skill_names, &template_names));
    match commands::dispatch(&cmd, &skill_names, &template_names) {
        commands::CommandEffect::Skill { name, args } => {
            assert_eq!(name, "ops/review");
            assert_eq!(args, "");
        }
        other => panic!("expected Skill effect, got {other:?}"),
    }

    // Bare skill name containing slash
    let cmd = commands::parse_command("/ops/review arg").unwrap();
    assert!(commands::is_command(&cmd, &skill_names, &template_names));
    match commands::dispatch(&cmd, &skill_names, &template_names) {
        commands::CommandEffect::Skill { name, args } => {
            assert_eq!(name, "ops/review");
            assert_eq!(args, "arg");
        }
        other => panic!("expected Skill effect, got {other:?}"),
    }

    // Template name containing slash
    let cmd = commands::parse_command("/ops/template arg").unwrap();
    assert!(commands::is_command(&cmd, &skill_names, &template_names));
    match commands::dispatch(&cmd, &skill_names, &template_names) {
        commands::CommandEffect::Template { name, args } => {
            assert_eq!(name, "ops/template");
            assert_eq!(args, "arg");
        }
        other => panic!("expected Template effect, got {other:?}"),
    }
}

#[test]
fn is_command_differentiates_paths_from_commands() {
    let skill_names = vec!["ops/review".to_string()];
    let template_names = vec!["ops/template".to_string()];

    // Paths starting with slash are parsed by parse_command, but not recognized as commands
    let path_cmd = commands::parse_command("/home/user/app.rs explain").unwrap();
    assert!(!commands::is_command(
        &path_cmd,
        &skill_names,
        &template_names
    ));

    // Relative path does not start with slash
    assert!(commands::parse_command("./src/main.rs").is_none());

    // Standard builtin commands and unknown single-token commands are treated as commands
    let model_cmd = commands::parse_command("/model soket/qwen3-30b").unwrap();
    assert!(commands::is_command(
        &model_cmd,
        &skill_names,
        &template_names
    ));

    let unknown_cmd = commands::parse_command("/unknown_cmd").unwrap();
    assert!(commands::is_command(
        &unknown_cmd,
        &skill_names,
        &template_names
    ));

    // Autocomplete on file paths returns empty
    assert!(commands::autocomplete("/home/user", &[]).is_empty());
}

#[test]
fn autocomplete_slash_in_dynamic_commands() {
    let extra = vec![
        ("skill:ops/review".to_string(), "review code".to_string()),
        ("ops/template".to_string(), "template".to_string()),
    ];
    let hits = commands::autocomplete("/skill:ops/", &extra);
    assert!(hits.contains(&"/skill:ops/review".to_string()));

    let hits = commands::autocomplete("/ops/", &extra);
    assert!(hits.contains(&"/ops/template".to_string()));
}

#[test]
fn theme_builtins_load() {
    let dark = Theme::dark();
    assert_eq!(dark.name, "dark");
    let _ = dark.get("accent");
    let light = Theme::light();
    assert_eq!(light.name, "light");
    assert!(Theme::load("Light", &[]).is_ok());
}

#[test]
fn parse_local_sandbox_flags_defaults_and_options() {
    use loop_agent::harness::{KrunIsolation, LocalSandboxRuntime};

    let (iso, rt) = commands::parse_local_sandbox_flags(&[]).unwrap();
    assert_eq!(iso, KrunIsolation::Full);
    assert_eq!(rt, LocalSandboxRuntime::Runc);

    let (iso, rt) = commands::parse_local_sandbox_flags(&["--partial", "--krun"]).unwrap();
    assert_eq!(iso, KrunIsolation::Partial);
    assert_eq!(rt, LocalSandboxRuntime::Krun);

    let (iso, rt) = commands::parse_local_sandbox_flags(&["full", "runsc"]).unwrap();
    assert_eq!(iso, KrunIsolation::Full);
    assert_eq!(rt, LocalSandboxRuntime::Runsc);

    assert!(commands::parse_local_sandbox_flags(&["--crun", "--krun"]).is_err());
    assert!(commands::parse_local_sandbox_flags(&["--full", "--partial"]).is_err());
}

#[test]
fn settings_defaults_soket() {
    let s = Settings::default();
    // No model is chosen for the user; `/model` sets one.
    assert_eq!(s.selected_model(), None);
    assert_eq!(s.theme, "dark");
    assert_eq!(s.file_edit_review, "newSession");
    assert_eq!(
        s.tool_permissions.get("bash").map(String::as_str),
        Some("ask")
    );
    assert_eq!(
        s.tool_permissions.get("read").map(String::as_str),
        Some("allow")
    );
    assert_eq!(s.sandbox.mode, "off");
    assert_eq!(s.sandbox.isolation, "full");
    assert_eq!(s.sandbox.runtime, "runc");
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
