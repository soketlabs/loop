//! Slash command registry and handlers.

use std::sync::Arc;

/// Built-in slash command descriptor.
#[derive(Debug, Clone)]
pub struct SlashCommand {
    /// Name without leading `/`.
    pub name: &'static str,
    /// Short description.
    pub description: &'static str,
    /// Optional argument hint.
    pub args_hint: Option<&'static str>,
}

/// All built-in slash commands.
pub fn builtin_commands() -> Vec<SlashCommand> {
    vec![
        SlashCommand {
            name: "theme",
            description: "Select or set UI theme",
            args_hint: Some("[name]"),
        },
        SlashCommand {
            name: "sandbox",
            description: "Show or change sandbox mode",
            args_hint: Some("[off|local-shell]"),
        },
        SlashCommand {
            name: "settings",
            description: "Open settings overview",
            args_hint: None,
        },
        SlashCommand {
            name: "model",
            description: "Select model",
            args_hint: Some("[provider/model]"),
        },
        SlashCommand {
            name: "scoped-models",
            description: "Manage models for Ctrl+P cycling",
            args_hint: None,
        },
        SlashCommand {
            name: "export",
            description: "Export session to JSONL/HTML path",
            args_hint: Some("[path]"),
        },
        SlashCommand {
            name: "import",
            description: "Import a JSONL session",
            args_hint: Some("<path>"),
        },
        SlashCommand {
            name: "copy",
            description: "Copy last assistant message",
            args_hint: None,
        },
        SlashCommand {
            name: "name",
            description: "Set session display name",
            args_hint: Some("<name>"),
        },
        SlashCommand {
            name: "session",
            description: "Show session info and token stats",
            args_hint: None,
        },
        SlashCommand {
            name: "changelog",
            description: "Show changelog",
            args_hint: None,
        },
        SlashCommand {
            name: "hotkeys",
            description: "Show keyboard shortcuts",
            args_hint: None,
        },
        SlashCommand {
            name: "fork",
            description: "Edit an earlier user message in a forked session",
            args_hint: None,
        },
        SlashCommand {
            name: "clone",
            description: "Duplicate current session",
            args_hint: None,
        },
        SlashCommand {
            name: "tree",
            description: "Navigate session tree",
            args_hint: None,
        },
        SlashCommand {
            name: "trust",
            description: "Trust or untrust this project",
            args_hint: Some("[yes|no]"),
        },
        SlashCommand {
            name: "login",
            description: "Save API key for a provider",
            args_hint: Some("[provider]"),
        },
        SlashCommand {
            name: "logout",
            description: "Remove stored credentials",
            args_hint: Some("[provider]"),
        },
        SlashCommand {
            name: "new",
            description: "Start a new session",
            args_hint: None,
        },
        SlashCommand {
            name: "review",
            description: "Tool approval policy (files + bash)",
            args_hint: Some("[newSession|always|never]"),
        },
        SlashCommand {
            name: "compact",
            description: "Compact conversation context",
            args_hint: Some("[prompt]"),
        },
        SlashCommand {
            name: "resume",
            description: "Resume a previous session",
            args_hint: None,
        },
        SlashCommand {
            name: "reload",
            description: "Reload config, skills, themes, extensions",
            args_hint: None,
        },
        SlashCommand {
            name: "skills",
            description: "List loaded skills (SKILL.md)",
            args_hint: None,
        },
        SlashCommand {
            name: "quit",
            description: "Quit Loop",
            args_hint: None,
        },
        SlashCommand {
            name: "help",
            description: "List commands",
            args_hint: None,
        },
    ]
}

/// Autocomplete match with description (for the bottom dock).
#[derive(Debug, Clone)]
pub struct AutocompleteEntry {
    /// Display name including leading `/`.
    pub name: String,
    /// Short description.
    pub description: String,
}

/// Autocomplete matches for a partial slash command (names only).
pub fn autocomplete(prefix: &str, extra: &[(String, String)]) -> Vec<String> {
    autocomplete_entries(prefix, extra)
        .into_iter()
        .map(|e| e.name)
        .collect()
}

/// Autocomplete matches with descriptions for the bottom dock.
///
/// `extra` holds dynamic `(name, description)` entries (skills, prompt templates).
pub fn autocomplete_entries(prefix: &str, extra: &[(String, String)]) -> Vec<AutocompleteEntry> {
    let p = prefix.trim_start_matches('/').to_lowercase();
    let mut out: Vec<AutocompleteEntry> = builtin_commands()
        .into_iter()
        .filter(|c| c.name.starts_with(&p))
        .map(|c| AutocompleteEntry {
            name: format!("/{}", c.name),
            description: match c.args_hint {
                Some(h) => format!("{h} — {}", c.description),
                None => c.description.to_string(),
            },
        })
        .collect();
    for (e, desc) in extra {
        let name = e.trim_start_matches('/');
        if name.to_lowercase().starts_with(&p) {
            let label = format!("/{name}");
            if !out.iter().any(|x| x.name == label) {
                out.push(AutocompleteEntry {
                    name: label,
                    description: desc.clone(),
                });
            }
        }
    }
    out.sort_by(|a, b| a.name.cmp(&b.name));
    out
}

/// Result of parsing a slash line.
#[derive(Debug, Clone)]
pub struct ParsedCommand {
    /// Command name.
    pub name: String,
    /// Remainder args.
    pub args: String,
}

/// Parse `/cmd args`.
pub fn parse_command(line: &str) -> Option<ParsedCommand> {
    let line = line.trim();
    if !line.starts_with('/') {
        return None;
    }
    let rest = &line[1..];
    let (name, args) = match rest.split_once(char::is_whitespace) {
        Some((n, a)) => (n.to_string(), a.trim().to_string()),
        None => (rest.to_string(), String::new()),
    };
    if name.is_empty() {
        return None;
    }
    Some(ParsedCommand { name, args })
}

/// Shared command side-effect messages for the UI.
#[derive(Debug, Clone)]
pub enum CommandEffect {
    /// Quit the app.
    Quit,
    /// Show a status/system line.
    Status(String),
    /// Replace theme.
    SetTheme(String),
    /// Open model picker (empty = picker, Some = set).
    SelectModel(Option<String>),
    /// Sandbox mode change.
    SetSandbox(String),
    /// Login flow.
    Login(Option<String>),
    /// Logout.
    Logout(Option<String>),
    /// New session.
    NewSession,
    /// File edit review policy.
    SetFileReview(Option<String>),
    /// Compact.
    Compact(Option<String>),
    /// Copy last assistant.
    CopyLast,
    /// Show hotkeys.
    Hotkeys,
    /// Show help.
    Help,
    /// Show session info.
    SessionInfo,
    /// Show settings summary.
    Settings,
    /// Trust decision.
    Trust(Option<String>),
    /// Reload resources.
    Reload,
    /// List loaded skills.
    ListSkills,
    /// Resume picker.
    Resume,
    /// Tree view.
    Tree,
    /// Set session name.
    SetName(String),
    /// Export path.
    Export(Option<String>),
    /// Import path.
    Import(String),
    /// Changelog.
    Changelog,
    /// Scoped models help.
    ScopedModels,
    /// Fork.
    Fork,
    /// Clone.
    CloneSession,
    /// Skill invoke.
    Skill {
        /// Skill name.
        name: String,
        /// Args.
        args: String,
    },
    /// Prompt template.
    Template {
        /// Template name.
        name: String,
        /// Args.
        args: String,
    },
}

/// Dispatch a built-in or dynamic command to an effect.
pub fn dispatch(cmd: &ParsedCommand, skill_names: &[String], template_names: &[String]) -> CommandEffect {
    match cmd.name.as_str() {
        "quit" | "exit" | "q" => CommandEffect::Quit,
        "theme" => {
            if cmd.args.is_empty() {
                CommandEffect::Status("Usage: /theme [name] — use without args to list".into())
            } else {
                CommandEffect::SetTheme(cmd.args.clone())
            }
        }
        "sandbox" => CommandEffect::SetSandbox(cmd.args.clone()),
        "model" => CommandEffect::SelectModel(if cmd.args.is_empty() {
            None
        } else {
            Some(cmd.args.clone())
        }),
        "login" => CommandEffect::Login(if cmd.args.is_empty() {
            None
        } else {
            Some(cmd.args.clone())
        }),
        "logout" => CommandEffect::Logout(if cmd.args.is_empty() {
            None
        } else {
            Some(cmd.args.clone())
        }),
        "new" => CommandEffect::NewSession,
        "review" => CommandEffect::SetFileReview(if cmd.args.is_empty() {
            None
        } else {
            Some(cmd.args.clone())
        }),
        "compact" => CommandEffect::Compact(if cmd.args.is_empty() {
            None
        } else {
            Some(cmd.args.clone())
        }),
        "copy" => CommandEffect::CopyLast,
        "hotkeys" => CommandEffect::Hotkeys,
        "help" => CommandEffect::Help,
        "session" => CommandEffect::SessionInfo,
        "settings" => CommandEffect::Settings,
        "trust" => CommandEffect::Trust(if cmd.args.is_empty() {
            None
        } else {
            Some(cmd.args.clone())
        }),
        "reload" => CommandEffect::Reload,
        "skills" => CommandEffect::ListSkills,
        "resume" => CommandEffect::Resume,
        "tree" => CommandEffect::Tree,
        "name" => CommandEffect::SetName(cmd.args.clone()),
        "export" => CommandEffect::Export(if cmd.args.is_empty() {
            None
        } else {
            Some(cmd.args.clone())
        }),
        "import" => {
            if cmd.args.is_empty() {
                CommandEffect::Status("Usage: /import <path.jsonl>".into())
            } else {
                CommandEffect::Import(cmd.args.clone())
            }
        }
        "changelog" => CommandEffect::Changelog,
        "scoped-models" => CommandEffect::ScopedModels,
        "fork" => CommandEffect::Fork,
        "clone" => CommandEffect::CloneSession,
        other => {
            if let Some(name) = other.strip_prefix("skill:") {
                return CommandEffect::Skill {
                    name: name.to_string(),
                    args: cmd.args.clone(),
                };
            }
            if skill_names.iter().any(|s| s == other) {
                return CommandEffect::Skill {
                    name: other.to_string(),
                    args: cmd.args.clone(),
                };
            }
            if template_names.iter().any(|t| t == other) {
                return CommandEffect::Template {
                    name: other.to_string(),
                    args: cmd.args.clone(),
                };
            }
            CommandEffect::Status(format!("Unknown command: /{other}. Try /help"))
        }
    }
}

/// Format /help text.
pub fn help_text(extra: &[(String, String)]) -> String {
    let mut lines = vec!["Slash commands:".to_string()];
    for c in builtin_commands() {
        let hint = c.args_hint.unwrap_or("");
        lines.push(format!("  /{} {} — {}", c.name, hint, c.description));
    }
    if !extra.is_empty() {
        lines.push("Skill / prompt template commands:".into());
        for (e, desc) in extra {
            if desc.is_empty() {
                lines.push(format!("  /{e}"));
            } else {
                lines.push(format!("  /{e} — {desc}"));
            }
        }
    }
    lines.join("\n")
}

/// Shared Arc alias for command lists in UI.
pub type SharedCommands = Arc<Vec<SlashCommand>>;
