//! Shell command splitting shared by the bash safety block list and the
//! permission rules: parts at `;`, `&&`, `||`, `|`, `&` and newlines, and
//! words with quotes and escapes removed, as the shell reads them.
//!
//! The splitter models only plain commands. A command with a construct it
//! cannot model is **unreadable** ([`unreadable`]): the shell could read
//! the text after that construct differently from the splitter, so no
//! check that reads the command as text can trust it.

/// Why `command` is unreadable, or `None` when the splitter models it
/// (S4 §7, the first slice).
///
/// Syntax the splitter cannot model (then [`split_command`] is `None`):
/// - command substitution: `$(…)`, backticks;
/// - process substitution `<(…)`, `>(…)`, and any other unquoted `(` or `)`:
///   subshells, function definitions, `case` patterns, arithmetic `$((…))`;
/// - an unquoted `{` or `}` (a group, a function body, brace expansion),
///   except a `${…}` parameter;
/// - ANSI-C and locale quoting, `$'…'` and `$"…"`;
/// - here-documents and here-strings (`<<`, `<<<`);
/// - a line continuation (a backslash before a newline);
/// - unbalanced quotes.
///
/// Programs that run another program, so that the program the shell runs
/// is not the first word (the command parses, but patterns cannot see the
/// program): a `NAME=value` assignment before it, a variable or a glob as
/// the program, `env`, `command`, `builtin`, `exec`, `eval`, `source`, `.`,
/// `alias`, `time`, `nohup`, `nice`, `timeout`, `stdbuf`, `xargs`, `sudo`,
/// `su`, `doas`, `!`, a shell (`sh`, `bash`, `zsh`, `dash`, `ksh`, `fish`,
/// also after a pipe), and an interpreter with inline code (`python* -c`,
/// `node -e`, `perl -e`, `ruby -e`).
pub fn unreadable(command: &str) -> Option<&'static str> {
    if let Some(why) = unmodelled_syntax(command) {
        return Some(why);
    }
    let parts = split_command(command)?;
    parts.iter().find_map(|part| runs_another_program(part))
}

/// The first construct of `command` that the splitter cannot model.
fn unmodelled_syntax(command: &str) -> Option<&'static str> {
    let (mut single, mut double) = (false, false);
    let mut chars = command.chars().peekable();
    while let Some(c) = chars.next() {
        if single {
            if c == '\'' {
                single = false;
            }
            continue;
        }
        match c {
            '\\' => {
                if chars.next() == Some('\n') {
                    return Some("a line continuation");
                }
            }
            '\'' if !double => single = true,
            '"' => double = !double,
            '`' => return Some("a command substitution (backticks)"),
            '$' => match chars.peek() {
                Some('(') => return Some("a command substitution `$(…)`"),
                Some('\'') if !double => return Some("ANSI-C quoting `$'…'`"),
                Some('"') if !double => return Some("locale quoting `$\"…\"`"),
                Some('{') => {
                    // A `${…}` parameter: skip to its end.
                    chars.next();
                    let mut closed = false;
                    for c in chars.by_ref() {
                        if c == '}' {
                            closed = true;
                            break;
                        }
                    }
                    if !closed {
                        return Some("an unclosed `${`");
                    }
                }
                _ => {}
            },
            '(' | ')' if !double => return Some("a subshell, function or other `(…)`"),
            '{' | '}' if !double => return Some("a group, function body or brace expansion"),
            '<' if !double && chars.peek() == Some(&'<') => {
                return Some("a here-document or here-string")
            }
            _ => {}
        }
    }
    (single || double).then_some("unbalanced quotes")
}

/// Programs that run the program named in their arguments.
const WRAPPERS: &[&str] = &[
    "env", "command", "builtin", "exec", "eval", "source", ".", "alias", "time", "nohup", "nice",
    "timeout", "stdbuf", "xargs", "sudo", "su", "doas", "!", "sh", "bash", "zsh", "dash", "ksh",
    "fish",
];

/// Why the program the shell runs for `part` is not its first word.
fn runs_another_program(part: &str) -> Option<&'static str> {
    let raw = part.split_whitespace().next()?;
    if raw.trim_start_matches('"').starts_with('$') {
        return Some("a variable as the program");
    }
    let words = split_words(part, true)?;
    let first = words.first()?;
    let assignment = first.split_once('=').is_some_and(|(name, _)| {
        !name.is_empty() && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
    });
    if assignment {
        return Some("a variable assignment before the program");
    }
    if raw.contains(['*', '?', '[']) {
        return Some("a glob as the program");
    }
    let program = first
        .rsplit('/')
        .next()
        .unwrap_or(first)
        .to_ascii_lowercase();
    if WRAPPERS.contains(&program.as_str()) {
        return Some("a program that runs another program");
    }
    let inline = |flags: &[&str]| words[1..].iter().any(|w| flags.contains(&w.as_str()));
    let interpreter = (program.starts_with("python") && inline(&["-c"]))
        || (program == "node" && inline(&["-e", "--eval", "-p", "--print"]))
        || (matches!(program.as_str(), "perl" | "ruby") && inline(&["-e", "-E"]));
    interpreter.then_some("an interpreter with inline code")
}

/// The parts of a shell command, split at `;`, `&&`, `||`, `|`, `&` and
/// newlines outside quotes. A redirect such as `2>&1`, `&>` or `>|` is part
/// of its command. `None` when the command is unreadable as syntax
/// ([`unreadable`]: substitutions, `(`, `{`, `$'…'`, here-documents, line
/// continuations, unbalanced quotes).
pub fn split_command(command: &str) -> Option<Vec<String>> {
    if unmodelled_syntax(command).is_some() {
        return None;
    }
    let mut parts = Vec::new();
    let mut current = String::new();
    let (mut single, mut double) = (false, false);
    let mut chars = command.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            // An escaped character never opens a quote or ends a part, as
            // in the shell (and in `words`).
            '\\' if !single => {
                current.push(c);
                current.extend(chars.next());
                continue;
            }
            '\'' if !double => single = !single,
            '"' if !single => double = !double,
            '`' if !single => return None,
            '$' | '<' | '>' if !single && chars.peek() == Some(&'(') => return None,
            // A redirect: `>&`, `<&`, `>|` and `&>` do not end the part.
            '&' | '|' if !single && !double && current.ends_with(['>', '<']) => {}
            '&' if !single && !double && chars.peek() == Some(&'>') => {}
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

/// The words of one command part, with quotes and escapes removed. `None`
/// when it redirects output (an unquoted `>`), unless `redirects` is true:
/// then `>` is kept as a character.
pub fn split_words(part: &str, redirects: bool) -> Option<Vec<String>> {
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
            '>' if !single && !double && !redirects => return None,
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
