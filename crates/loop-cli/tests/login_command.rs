//! `/login` and `/logout` parsing.

use loop_cli::commands::{self, CommandEffect};

fn dispatch(line: &str) -> CommandEffect {
    commands::dispatch(&commands::parse_command(line).unwrap(), &[], &[])
}

#[test]
fn login_takes_an_optional_provider() {
    assert!(matches!(dispatch("/login"), CommandEffect::Login(None)));
    assert!(matches!(
        dispatch("/login openrouter"),
        CommandEffect::Login(Some(p)) if p == "openrouter"
    ));
}

#[test]
fn logout_takes_an_optional_provider() {
    assert!(matches!(dispatch("/logout"), CommandEffect::Logout(None)));
    assert!(matches!(
        dispatch("/logout openai"),
        CommandEffect::Logout(Some(p)) if p == "openai"
    ));
}

#[test]
fn login_help_lists_the_providers() {
    let login = commands::builtin_commands()
        .into_iter()
        .find(|c| c.name == "login")
        .unwrap();
    assert_eq!(login.args_hint, Some("[soket|openrouter|openai|custom]"));
}
