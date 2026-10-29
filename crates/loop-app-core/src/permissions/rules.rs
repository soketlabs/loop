//! `toolPermissions` rules: pattern parsing and matching, and bash command
//! splitting.

use std::collections::BTreeMap;
use std::path::Path;

use serde_json::Value;

use super::Call;
use crate::hooks_load::glob_match;

/// Per-tool default from `~/.loop/agent/settings.json`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolPermission {
    /// Prompt when approval policy says so.
    Ask,
    /// Never prompt.
    Allow,
    /// Always block.
    Deny,
}

impl ToolPermission {
    /// Parse `ask` / `allow` / `deny`.
    pub fn parse(s: &str) -> Self {
        match s.trim().to_ascii_lowercase().as_str() {
            "allow" | "auto" | "yes" => Self::Allow,
            "deny" | "block" | "no" => Self::Deny,
            _ => Self::Ask,
        }
    }

    /// Settings label.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Ask => "ask",
            Self::Allow => "allow",
            Self::Deny => "deny",
        }
    }

    /// `allow` < `ask` < `deny`.
    pub fn strictness(self) -> u8 {
        match self {
            Self::Allow => 0,
            Self::Ask => 1,
            Self::Deny => 2,
        }
    }
}

/// What `toolPermissions` says about one call.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rule {
    /// The permission.
    pub permission: ToolPermission,
    /// Whether a pattern rule (`bash(git *)`) decided, not the plain tool rule.
    pub specific: bool,
}

impl Rule {
    /// A plain tool rule.
    pub fn plain(permission: ToolPermission) -> Self {
        Self {
            permission,
            specific: false,
        }
    }
}

/// Split `tool(pattern)` keys; plain keys have no pattern.
fn split_key(key: &str) -> (&str, Option<&str>) {
    match key.split_once('(') {
        Some((tool, rest)) if rest.ends_with(')') => (tool.trim(), Some(&rest[..rest.len() - 1])),
        _ => (key, None),
    }
}

/// The call argument patterns match: the command, path, URL or query.
fn subject(tool: &str, args: &Value) -> Option<String> {
    let key = match tool {
        "bash" => "command",
        "read" | "write" | "edit" => "path",
        "web_fetch" => "url",
        "web_search" => "query",
        _ => return Some(args.to_string()),
    };
    args.get(key).and_then(Value::as_str).map(str::to_owned)
}

/// The parts of a shell command, split at `;`, `&&`, `||`, `|`, `&` and
/// newlines outside quotes. `None` when it substitutes commands (`$(…)`,
/// backticks, `<(…)`) or has unbalanced quotes.
pub fn split_command(command: &str) -> Option<Vec<String>> {
    let mut parts = Vec::new();
    let mut current = String::new();
    let (mut single, mut double) = (false, false);
    let mut chars = command.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\'' if !double => single = !single,
            '"' if !single => double = !double,
            '`' if !single => return None,
            '$' | '<' | '>' if !single && chars.peek() == Some(&'(') => return None,
            ';' | '\n' | '&' | '|' if !single && !double => {
                if matches!(chars.peek(), Some('&' | '|')) {
                    chars.next();
                }
                parts.push(std::mem::take(&mut current));
                continue;
            }
            _ => {}
        }
        current.push(c);
    }
    if single || double {
        return None;
    }
    parts.push(current);
    let parts: Vec<String> = parts
        .into_iter()
        .map(|p| p.trim().to_owned())
        .filter(|p| !p.is_empty())
        .collect();
    (!parts.is_empty()).then_some(parts)
}

/// Commands `read-only` runs: they print what they read and change nothing
/// (with the options [`read_only_part`] refuses).
const READ_ONLY_COMMANDS: &[&str] = &[
    "ls", "pwd", "cat", "head", "tail", "wc", "grep", "rg", "find", "tree", "file", "stat", "du",
    "which", "echo", "sort", "uniq", "diff",
];

/// `git` subcommands `read-only` runs.
const READ_ONLY_GIT: &[&str] = &[
    "status",
    "log",
    "diff",
    "show",
    "branch",
    "blame",
    "ls-files",
    "rev-parse",
];

/// Whether every part of `command` only reads: a command from
/// [`READ_ONLY_COMMANDS`] or a read-only `git` subcommand, with no output
/// redirect, command substitution or variable assignment.
pub fn read_only_command(command: &str) -> bool {
    split_command(command).is_some_and(|parts| {
        parts
            .iter()
            .all(|part| words(part).is_some_and(|w| read_only_part(&w)))
    })
}

/// The words of one command part, with quotes removed. `None` when it
/// redirects output (an unquoted `>`).
fn words(part: &str) -> Option<Vec<String>> {
    let mut words = Vec::new();
    let mut current = String::new();
    let (mut single, mut double, mut started) = (false, false, false);
    let mut chars = part.chars();
    while let Some(c) = chars.next() {
        match c {
            '\'' if !double => {
                single = !single;
                started = true;
            }
            '"' if !single => {
                double = !double;
                started = true;
            }
            '\\' if !single => current.extend(chars.next()),
            '>' if !single && !double => return None,
            c if c.is_whitespace() && !single && !double => {
                if started || !current.is_empty() {
                    words.push(std::mem::take(&mut current));
                }
                started = false;
            }
            c => current.push(c),
        }
    }
    if started || !current.is_empty() {
        words.push(current);
    }
    Some(words)
}

fn read_only_part(words: &[String]) -> bool {
    let Some((program, args)) = words.split_first() else {
        return false;
    };
    let has = |options: &[&str]| {
        args.iter().any(|a| {
            options
                .iter()
                .any(|o| a == o || (o.starts_with("--") && a.starts_with(&format!("{o}="))))
        })
    };
    let positional = || args.iter().filter(|a| !a.starts_with('-')).count();
    match program.as_str() {
        "git" => {
            let Some((sub, rest)) = args.split_first() else {
                return false;
            };
            if !READ_ONLY_GIT.contains(&sub.as_str()) || has(&["--output"]) {
                return false;
            }
            // `git branch NAME` creates a branch; only listing is a read.
            sub != "branch"
                || rest.iter().all(|a| {
                    a.starts_with('-')
                        && !matches!(
                            a.as_str(),
                            "-d" | "-D"
                                | "-m"
                                | "-M"
                                | "-c"
                                | "-C"
                                | "-f"
                                | "-u"
                                | "--delete"
                                | "--move"
                                | "--copy"
                                | "--force"
                                | "--edit-description"
                                | "--unset-upstream"
                        )
                        && !a.starts_with("--set-upstream-to")
                })
        }
        "find" => !has(&[
            "-exec", "-execdir", "-ok", "-okdir", "-delete", "-fprint", "-fprint0", "-fprintf",
            "-fls",
        ]),
        // `-o FILE` writes FILE, also inside a cluster such as `-uo`.
        // `--compress-program` runs a program when sort spills to disk.
        "sort" => !args.iter().any(|a| {
            a.starts_with("--output")
                || a.starts_with("--compress-program")
                || (a.starts_with('-') && !a.starts_with("--") && a.contains('o'))
        }),
        // `file -C` compiles a magic file.
        "file" => !has(&["-C", "--compile"]),
        "rg" => !has(&["--pre", "--hostname-bin"]),
        "tree" => !has(&["-o"]),
        // `uniq IN OUT` writes OUT.
        "uniq" => positional() <= 1,
        program => READ_ONLY_COMMANDS.contains(&program),
    }
}

/// Whether a path pattern matches `path` as written, with `~` expanded to
/// `home`, or as an absolute path under `workspace`.
fn path_matches(pattern: &str, path: &str, workspace: &Path, home: Option<&Path>) -> bool {
    let expand = |p: &str| match (p.strip_prefix("~/"), home) {
        (Some(rest), Some(home)) => home.join(rest).to_string_lossy().into_owned(),
        _ => p.to_owned(),
    };
    let pattern = expand(pattern);
    let path = expand(path);
    let absolute = workspace.join(&path).to_string_lossy().into_owned();
    glob_match(&pattern, &path) || glob_match(&pattern, &absolute)
}

/// The `toolPermissions` rule for one call; tools with no rule are allowed.
pub fn rule_for(rules: &BTreeMap<String, ToolPermission>, call: &Call) -> Rule {
    let tool = call.tool.as_str();
    let subject = subject(tool, &call.args);
    let mut specific: Option<ToolPermission> = None;
    let mut bash_allows = Vec::new();
    for (key, permission) in rules {
        let (tool_pattern, Some(pattern)) = split_key(key) else {
            continue;
        };
        if !glob_match(tool_pattern, tool) {
            continue;
        }
        let Some(subject) = subject.as_deref() else {
            continue;
        };
        let hit = if tool == "bash" {
            if *permission == ToolPermission::Allow {
                bash_allows.push(pattern);
                false
            } else {
                match split_command(subject) {
                    Some(parts) => parts.iter().any(|part| glob_match(pattern, part)),
                    None => {
                        glob_match(pattern, subject) || subject.contains(pattern.trim_matches('*'))
                    }
                }
            }
        } else if matches!(tool, "read" | "write" | "edit") {
            path_matches(pattern, subject, &call.workspace, call.home.as_deref())
        } else {
            glob_match(pattern, subject)
        };
        if hit && specific.is_none_or(|s| permission.strictness() > s.strictness()) {
            specific = Some(*permission);
        }
    }
    if specific.is_none() && !bash_allows.is_empty() {
        let all_allowed = subject
            .as_deref()
            .and_then(split_command)
            .is_some_and(|parts| {
                parts
                    .iter()
                    .all(|part| bash_allows.iter().any(|pattern| glob_match(pattern, part)))
            });
        if all_allowed {
            specific = Some(ToolPermission::Allow);
        }
    }
    if let Some(permission) = specific {
        return Rule {
            permission,
            specific: true,
        };
    }
    if let Some(permission) = rules.get(tool) {
        return Rule::plain(*permission);
    }
    // Plain glob keys such as `mcp__github__*`; the strictest match wins.
    let plain = rules
        .iter()
        .filter(|(key, _)| split_key(key).1.is_none() && key.contains('*') && glob_match(key, tool))
        .map(|(_, p)| *p)
        .max_by_key(|p| p.strictness());
    Rule::plain(plain.unwrap_or(ToolPermission::Allow))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    pub(crate) fn rules(entries: &[(&str, &str)]) -> BTreeMap<String, ToolPermission> {
        crate::tool_approval::permissions_from_settings(
            &entries
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
        )
    }

    fn call(tool: &str, args: Value) -> Call {
        Call {
            home: Some("/home/me".into()),
            ..Call::new(tool, args, "/ws")
        }
    }

    fn bash_rule(rules: &BTreeMap<String, ToolPermission>, cmd: &str) -> Rule {
        rule_for(rules, &call("bash", json!({ "command": cmd })))
    }

    #[test]
    fn read_only_commands() {
        let allowed = [
            "ls -la",
            "ls | grep x",
            "cat a.txt",
            "grep -rn 'a > b' src",
            "rg --files | wc -l",
            "find . -name '*.rs'",
            "git status && git log --oneline -5",
            "git diff HEAD~1 -- src",
            "git branch -a",
            "head -20 README.md; tail -n 5 log.txt",
            "sort -u names.txt",
            "uniq names.txt",
        ];
        for command in allowed {
            assert!(read_only_command(command), "{command}");
        }
        let refused = [
            "cat a > b",
            "echo hi >> notes",
            "find . -delete",
            "find . -name x -exec rm {} ;",
            "git log && rm x",
            "git push",
            "git branch new-branch",
            "git branch -D old",
            "git -c core.pager=evil log",
            "git log --output=out.txt",
            "sort -o out.txt in.txt",
            "sort -uo out.txt in.txt",
            "file -C -m magic",
            "rg --pre ./script x",
            "rg --hostname-bin=./script x",
            "sort -S 1 --compress-program=./script big.txt",
            "tree -o out.txt",
            "uniq in.txt out.txt",
            "ls $(pwd)",
            "cat `which ls`",
            "FOO=1 ls",
            "/bin/ls",
            "cargo test",
            "tee out.txt",
            "",
        ];
        for command in refused {
            assert!(!read_only_command(command), "{command}");
        }
    }

    #[test]
    fn splitting_commands() {
        assert_eq!(
            split_command("git status && rm -rf x; ls | wc -l").unwrap(),
            ["git status", "rm -rf x", "ls", "wc -l"]
        );
        assert_eq!(
            split_command("echo 'a && b' \"c;d\"").unwrap(),
            ["echo 'a && b' \"c;d\""]
        );
        assert!(split_command("echo $(whoami)").is_none());
        assert!(split_command("echo `whoami`").is_none());
        assert!(split_command("echo 'unclosed").is_none());
        assert_eq!(
            split_command("echo '$(literal)'").unwrap(),
            ["echo '$(literal)'"]
        );
    }

    #[test]
    fn bash_allow_patterns_need_every_part() {
        let r = rules(&[
            ("bash", "ask"),
            ("bash(git status*)", "allow"),
            ("bash(ls*)", "allow"),
        ]);
        let allow = Rule {
            permission: ToolPermission::Allow,
            specific: true,
        };
        assert_eq!(bash_rule(&r, "git status --short"), allow);
        assert_eq!(bash_rule(&r, "git status && ls -la"), allow);
        assert_eq!(
            bash_rule(&r, "git status && rm -rf x"),
            Rule::plain(ToolPermission::Ask)
        );
        assert_eq!(
            bash_rule(&r, "git status $(rm -rf x)"),
            Rule::plain(ToolPermission::Ask)
        );
    }

    #[test]
    fn deny_patterns_match_any_part_and_beat_allow() {
        let r = rules(&[("bash(git *)", "allow"), ("bash(rm -rf *)", "deny")]);
        assert_eq!(
            bash_rule(&r, "git status; rm -rf build").permission,
            ToolPermission::Deny
        );
        assert_eq!(
            bash_rule(&r, "echo $(rm -rf build)").permission,
            ToolPermission::Deny,
            "an unparseable command is still checked against deny patterns"
        );
    }

    #[test]
    fn path_url_and_glob_tool_rules() {
        let r = rules(&[
            ("write(~/secrets/*)", "deny"),
            ("write(src/generated/*)", "ask"),
            ("web_fetch(https://docs.rs/*)", "allow"),
            ("mcp__github__*", "ask"),
        ]);
        let permission = |tool: &str, args: Value| rule_for(&r, &call(tool, args)).permission;
        // Built the way `~/` expands, so the separators match on Windows too.
        let secret = Path::new("/home/me").join("secrets/key");
        assert_eq!(
            permission("write", json!({ "path": secret.to_string_lossy() })),
            ToolPermission::Deny
        );
        assert_eq!(
            permission("write", json!({"path": "~/secrets/key"})),
            ToolPermission::Deny
        );
        let generated = rule_for(&r, &call("write", json!({"path": "src/generated/a.rs"})));
        assert_eq!(generated.permission, ToolPermission::Ask);
        assert!(generated.specific);
        #[cfg(unix)]
        assert_eq!(
            permission("write", json!({"path": "/ws/src/generated/a.rs"})),
            ToolPermission::Ask,
            "an absolute path in the workspace matches a relative pattern"
        );
        assert_eq!(
            permission("web_fetch", json!({"url": "https://docs.rs/serde"})),
            ToolPermission::Allow
        );
        assert_eq!(
            permission("web_fetch", json!({"url": "https://evil.example"})),
            ToolPermission::Allow,
            "the default web_fetch rule"
        );
        assert_eq!(
            permission("mcp__github__create_issue", json!({})),
            ToolPermission::Ask
        );
        assert_eq!(
            permission("mcp__slack__post", json!({})),
            ToolPermission::Allow
        );
    }

    #[test]
    fn strictness_orders_permissions() {
        use ToolPermission::*;
        assert!(Allow.strictness() < Ask.strictness() && Ask.strictness() < Deny.strictness());
    }
}
