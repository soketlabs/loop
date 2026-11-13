//! Shell command reading shared by the bash safety block list and the
//! permission rules.
//!
//! [`parse`] is the one reader. It accepts only a small grammar that it
//! models exactly, and calls everything else **unreadable**:
//!
//! - commands are separated by `|`, `||`, `&&`, `;` and newlines;
//! - a simple command is a program word, then argument words and
//!   redirects;
//! - a program word is plain text: `A-Z a-z 0-9 _ . / + -` only;
//! - an argument word joins unquoted text from `A-Z a-z 0-9 _ . / + - = :
//!   , @ %` and the glob characters `* ? [ ]`, a backslash before one
//!   printable ASCII character, `'single quotes'` (no newline), `"double
//!   quotes"` (no newline, backtick or `!`; a backslash before `$`,
//!   backtick, `"` or `\` gives that character, before any other character
//!   both stay), and `$NAME` or `${NAME}`;
//! - redirects: `<`, `>`, `>>`, `>|`, `&>`, `&>>` with a target word, and
//!   `>&N`, `<&N`, `>&-`, each with an optional one-digit file number;
//! - anything else is unreadable: comments, `(`, `)`, `{`, `}`, backticks,
//!   any other `$` form, `!`, `&` alone, here-documents, control
//!   characters, non-ASCII text outside quotes, a newline inside quotes,
//!   a `~` at the start of a word or after `=` or `:` (elsewhere it is
//!   text, as in `HEAD~1`),
//!   unbalanced quotes, keywords (`if`, `for`, …), programs that run
//!   another program or change how later programs are found (`env`, `sh`,
//!   `eval`, `xargs`, `trap`, `hash`, `read`, …), an assignment before the
//!   program, and interpreters with inline code.

/// One simple command of a readable command line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Simple {
    /// The text of this command as written, trimmed.
    pub raw: String,
    /// The program and its arguments, quotes and escapes removed. `$NAME`
    /// and `${NAME}` stay as written.
    pub words: Vec<String>,
    /// For each word, whether the shell can change it: a variable or an
    /// unquoted glob character.
    pub dynamic: Vec<bool>,
    /// Its redirects.
    pub redirects: Vec<Redirect>,
}

impl Simple {
    /// The program word.
    pub fn program(&self) -> &str {
        self.words.first().map_or("", String::as_str)
    }

    /// Whether it opens a file for writing through a redirect (`>`, `>>`,
    /// `&>`, …). A file number copy such as `2>&1` writes nothing.
    pub fn writes(&self) -> bool {
        self.redirects
            .iter()
            .any(|r| r.output && r.target.is_some())
    }
}

/// One redirect of a simple command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Redirect {
    /// `>`-like (opens for writing) rather than `<`.
    pub output: bool,
    /// The file it opens; `None` for a file number copy (`2>&1`, `>&-`).
    pub target: Option<String>,
}

/// Why a command is unreadable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Unreadable {
    /// What Loop could not read.
    pub reason: &'static str,
    /// The program involved, when the reason is about one.
    pub program: Option<String>,
}

impl Unreadable {
    fn new(reason: &'static str) -> Self {
        Self {
            reason,
            program: None,
        }
    }
}

/// Programs that run the program named in their arguments.
const WRAPPERS: &[&str] = &[
    "env", "command", "builtin", "exec", "eval", "source", ".", "time", "nohup", "nice", "timeout",
    "stdbuf", "xargs", "sudo", "su", "doas", "sh", "bash", "zsh", "dash", "ksh", "fish", "csh",
    "tcsh", "busybox", "parallel", "watch", "script",
];

/// Shell keywords: they change how the following words are read.
const KEYWORDS: &[&str] = &[
    "if", "then", "else", "elif", "fi", "case", "esac", "for", "select", "while", "until", "do",
    "done", "function", "in", "coproc",
];

/// Built-in commands that change how later programs are found or run, or
/// run text later.
const SHELL_STATE: &[&str] = &[
    "alias",
    "unalias",
    "hash",
    "trap",
    "enable",
    "shopt",
    "set",
    "unset",
    "bind",
    "fc",
    "read",
    "mapfile",
    "readarray",
    "getopts",
    "let",
    "complete",
];

/// Built-ins that assign variables: unreadable when they set one of
/// [`RISKY_VARIABLES`] or take an option.
const ASSIGNERS: &[&str] = &["export", "declare", "typeset", "readonly", "local"];

/// Variables that change which program a later command runs, or what the
/// shell runs on its own.
const RISKY_VARIABLES: &[&str] = &[
    "PATH",
    "ENV",
    "BASH_ENV",
    "IFS",
    "CDPATH",
    "PS4",
    "SHELLOPTS",
    "BASHOPTS",
    "GLOBIGNORE",
    "PROMPT_COMMAND",
    "FPATH",
];

/// Read `command` under the grammar of this module: its simple commands,
/// or why it is unreadable.
pub fn parse(command: &str) -> Result<Vec<Simple>, Unreadable> {
    let commands = Lexer::new(command).commands()?;
    for simple in &commands {
        check_program(simple)?;
    }
    Ok(commands)
}

/// Why `command` is unreadable, or `None` when [`parse`] reads it.
pub fn unreadable(command: &str) -> Option<&'static str> {
    parse(command).err().map(|e| e.reason)
}

/// The parts of a command as written, one per simple command. `None` when
/// it is unreadable.
pub fn split_command(command: &str) -> Option<Vec<String>> {
    parse(command)
        .ok()
        .map(|commands| commands.into_iter().map(|c| c.raw).collect())
}

/// The words of one command part, quotes and escapes removed. A readable
/// part gives the words [`parse`] reads; with `redirects`, its redirect
/// operators and targets follow as words. `None` when it writes through a
/// redirect and `redirects` is false.
///
/// For text [`parse`] cannot read (the block list cuts an unreadable
/// command at every separator and checks each piece), this falls back to a
/// loose cut at whitespace outside quotes, so the block list errs on the
/// side of seeing a program.
pub fn split_words(part: &str, redirects: bool) -> Option<Vec<String>> {
    match parse(part) {
        Ok(commands) if commands.len() == 1 => {
            let simple = &commands[0];
            if simple.writes() && !redirects {
                return None;
            }
            let mut words = simple.words.clone();
            if redirects {
                for r in &simple.redirects {
                    words.push(if r.output { ">" } else { "<" }.to_owned());
                    words.extend(r.target.clone());
                }
            }
            Some(words)
        }
        _ => Some(loose_words(part)),
    }
}

/// Whitespace-separated words with quote characters and backslashes
/// removed outside single quotes. Not a reader: only the block list's
/// fallback for unreadable text.
fn loose_words(part: &str) -> Vec<String> {
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
    words
}

/// How to run `command` so that Loop can read it, when it is unreadable.
pub fn unreadable_advice(command: &str) -> Option<String> {
    let why = parse(command).err()?;
    let advice = match why.reason {
        REASON_SUBSTITUTION => {
            "run the inner command on its own first, then use its output as plain text".into()
        }
        REASON_PAREN => "run the commands without the parentheses, one part at a time".into(),
        REASON_BRACE => "write each word out in full, without `{` and `}`".into(),
        REASON_DOLLAR => "use `$NAME` or `${NAME}` only, or write the value out".into(),
        REASON_HEREDOC => "put the text in a file and pass the file".into(),
        REASON_QUOTES => "close every quote, and keep each quoted text on one line".into(),
        REASON_COMMENT => "remove the comment".into(),
        REASON_TILDE => "write the home directory out in full, or use `$HOME`".into(),
        REASON_CHARACTER => "put the text in single quotes".into(),
        REASON_SEPARATOR => "use `;`, `&&`, `||` or `|` between whole commands".into(),
        REASON_REDIRECT => "use `<`, `>`, `>>` or `2>&1` with a plain file name".into(),
        REASON_ASSIGNMENT => {
            "set the variable in its own part first (`export NAME=value; …`), or leave it out"
                .into()
        }
        REASON_PROGRAM_TEXT => "write the program name out in plain text".into(),
        REASON_INTERPRETER => {
            "put the code in a script file and run the file, or run the command directly".into()
        }
        REASON_KEYWORD => "run each command on its own, without the shell keyword".into(),
        REASON_STATE => match &why.program {
            Some(program) => format!("run the commands without `{program}`"),
            None => "run the commands without it".into(),
        },
        _ => match &why.program {
            Some(program) => format!("remove `{program}` and run the inner command directly"),
            None => "write the command in plain form".into(),
        },
    };
    Some(format!(
        "the command has {}; to run it, {advice}",
        why.reason
    ))
}

const REASON_SUBSTITUTION: &str = "a command substitution (`$(…)`, `$[…]` or backticks)";
const REASON_PAREN: &str = "a subshell, function or other `(…)`";
const REASON_BRACE: &str = "a group, function body or brace expansion";
const REASON_DOLLAR: &str = "a `$` form other than `$NAME` or `${NAME}`";
const REASON_HEREDOC: &str = "a here-document or here-string";
const REASON_QUOTES: &str = "unbalanced quotes or a quoted newline";
const REASON_COMMENT: &str = "a comment (`#`)";
const REASON_TILDE: &str = "a `~`";
const REASON_CHARACTER: &str = "a character the shell reads specially";
const REASON_SEPARATOR: &str = "a background `&` or an empty command";
const REASON_REDIRECT: &str = "a redirect Loop does not read";
const REASON_ASSIGNMENT: &str = "a variable assignment before the program";
const REASON_PROGRAM_TEXT: &str = "a program name that is not plain text";
const REASON_INTERPRETER: &str = "an interpreter with inline code";
const REASON_KEYWORD: &str = "a shell keyword";
const REASON_WRAPPER: &str = "a program that runs another program";
const REASON_STATE: &str = "a built-in that changes how later commands run";

/// The checks on a simple command's program.
fn check_program(simple: &Simple) -> Result<(), Unreadable> {
    let program = simple.program();
    let named = |reason| Unreadable {
        reason,
        program: Some(program.to_owned()),
    };
    let plain = !program.is_empty()
        && !simple.dynamic[0]
        && simple.raw.starts_with(program)
        && program
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '/' | '+' | '-'));
    if !plain {
        let assignment = program.split_once('=').is_some_and(|(name, _)| {
            !name.is_empty() && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
        });
        return Err(Unreadable::new(if assignment {
            REASON_ASSIGNMENT
        } else {
            REASON_PROGRAM_TEXT
        }));
    }
    let base = program
        .rsplit('/')
        .next()
        .unwrap_or(program)
        .to_ascii_lowercase();
    if KEYWORDS.contains(&base.as_str()) {
        return Err(named(REASON_KEYWORD));
    }
    if WRAPPERS.contains(&base.as_str()) {
        return Err(named(REASON_WRAPPER));
    }
    if SHELL_STATE.contains(&base.as_str()) {
        return Err(named(REASON_STATE));
    }
    let args = &simple.words[1..];
    if ASSIGNERS.contains(&base.as_str()) {
        let risky = args.iter().any(|a| {
            let name = a.split('=').next().unwrap_or(a);
            a.starts_with('-')
                || RISKY_VARIABLES.contains(&name)
                || name.starts_with("LD_")
                || name.starts_with("DYLD_")
        });
        if risky {
            return Err(named(REASON_STATE));
        }
    }
    if base == "printf" && args.iter().any(|a| a.starts_with("-v")) {
        return Err(named(REASON_STATE));
    }
    let inline = |flags: &[&str]| args.iter().any(|w| flags.contains(&w.as_str()));
    let interpreter = (base.starts_with("python") && inline(&["-c"]))
        || (base == "node" && inline(&["-e", "--eval", "-p", "--print"]))
        || (matches!(base.as_str(), "perl" | "ruby") && inline(&["-e", "-E"]));
    if interpreter {
        return Err(named(REASON_INTERPRETER));
    }
    Ok(())
}

/// Unquoted characters an argument word may hold.
fn plain_char(c: char) -> bool {
    c.is_ascii_alphanumeric()
        || matches!(c, '_' | '.' | '/' | '+' | '-' | '=' | ':' | ',' | '@' | '%')
}

/// Unquoted glob characters: kept as text, but they mark the word as one
/// the shell can change.
fn glob_char(c: char) -> bool {
    matches!(c, '*' | '?' | '[' | ']')
}

/// The reader behind [`parse`].
struct Lexer<'a> {
    text: &'a str,
    chars: Vec<(usize, char)>,
    at: usize,
}

/// A separator after a simple command.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Sep {
    Start,
    Pipe,
    And,
    Or,
    Semi,
    Newline,
    End,
}

/// A simple command's words, which of them the shell can change, and its
/// redirects.
type Parts = (Vec<String>, Vec<bool>, Vec<Redirect>);

impl<'a> Lexer<'a> {
    fn new(text: &'a str) -> Self {
        Self {
            text,
            chars: text.char_indices().collect(),
            at: 0,
        }
    }

    fn peek(&self) -> Option<char> {
        self.chars.get(self.at).map(|&(_, c)| c)
    }

    fn peek_at(&self, n: usize) -> Option<char> {
        self.chars.get(self.at + n).map(|&(_, c)| c)
    }

    fn offset(&self) -> usize {
        self.chars.get(self.at).map_or(self.text.len(), |&(i, _)| i)
    }

    fn skip_blanks(&mut self) {
        while matches!(self.peek(), Some(' ' | '\t')) {
            self.at += 1;
        }
    }

    /// Every simple command. An empty command is allowed only at the end
    /// of a line, after `;` or a newline (`ls;` and blank lines).
    fn commands(&mut self) -> Result<Vec<Simple>, Unreadable> {
        let mut out = Vec::new();
        let mut before = Sep::Start;
        loop {
            let start = self.offset();
            let (words, dynamic, redirects) = self.simple()?;
            let end = self.offset();
            let after = self.separator()?;
            let raw = self.text[start..end].trim().to_owned();
            if words.is_empty() {
                if !redirects.is_empty() {
                    return Err(Unreadable::new(REASON_REDIRECT));
                }
                let open_before = matches!(before, Sep::Pipe | Sep::And | Sep::Or);
                let closes = matches!(after, Sep::Newline | Sep::End);
                if open_before || !closes {
                    return Err(Unreadable::new(REASON_SEPARATOR));
                }
            } else {
                out.push(Simple {
                    raw,
                    words,
                    dynamic,
                    redirects,
                });
            }
            if after == Sep::End {
                break;
            }
            before = after;
        }
        if out.is_empty() {
            return Err(Unreadable::new(REASON_SEPARATOR));
        }
        Ok(out)
    }

    /// The separator at the cursor, consumed.
    fn separator(&mut self) -> Result<Sep, Unreadable> {
        let Some(c) = self.peek() else {
            return Ok(Sep::End);
        };
        let (sep, len) = match (c, self.peek_at(1)) {
            ('|', Some('|')) => (Sep::Or, 2),
            ('|', Some('&')) => return Err(Unreadable::new(REASON_SEPARATOR)),
            ('|', _) => (Sep::Pipe, 1),
            ('&', Some('&')) => (Sep::And, 2),
            ('&', _) => return Err(Unreadable::new(REASON_SEPARATOR)),
            (';', _) => (Sep::Semi, 1),
            ('\n', _) => (Sep::Newline, 1),
            _ => return Err(Unreadable::new(REASON_CHARACTER)),
        };
        self.at += len;
        Ok(sep)
    }

    /// One simple command up to a separator.
    fn simple(&mut self) -> Result<Parts, Unreadable> {
        let (mut words, mut dynamic, mut redirects) = (Vec::new(), Vec::new(), Vec::new());
        loop {
            self.skip_blanks();
            match self.peek() {
                None | Some('|' | ';' | '\n') => break,
                Some('&') if self.peek_at(1) != Some('>') => break,
                _ => {}
            }
            if let Some(redirect) = self.redirect()? {
                redirects.push(redirect);
                continue;
            }
            let (word, changes) = self.word()?;
            // A one-digit file number right before `<` or `>` belongs to
            // the redirect.
            if matches!(self.peek(), Some('<' | '>')) {
                let fd = word.len() == 1 && word.chars().all(|c| c.is_ascii_digit()) && !changes;
                if !fd {
                    words.push(word);
                    dynamic.push(changes);
                }
                match self.redirect()? {
                    Some(redirect) => redirects.push(redirect),
                    None => return Err(Unreadable::new(REASON_REDIRECT)),
                }
                continue;
            }
            words.push(word);
            dynamic.push(changes);
        }
        Ok((words, dynamic, redirects))
    }

    /// A redirect at the cursor, consumed; `None` when there is none.
    fn redirect(&mut self) -> Result<Option<Redirect>, Unreadable> {
        let c = self.peek();
        let (output, len) = match (c, self.peek_at(1)) {
            (Some('&'), Some('>')) => (true, if self.peek_at(2) == Some('>') { 3 } else { 2 }),
            (Some('>'), Some('>' | '|')) => (true, 2),
            (Some('>' | '<'), Some('&')) => {
                self.at += 2;
                // A file number copy or close: `>&2`, `<&0`, `>&-`.
                let mut digits = 0;
                while matches!(self.peek(), Some('0'..='9')) {
                    self.at += 1;
                    digits += 1;
                }
                let closed = digits == 0 && self.peek() == Some('-');
                if closed {
                    self.at += 1;
                }
                let ends = matches!(self.peek(), None | Some(' ' | '\t' | '\n' | ';' | '|'))
                    || (self.peek() == Some('&') && self.peek_at(1) == Some('&'));
                if (digits == 1 || closed) && ends {
                    return Ok(Some(Redirect {
                        output: c == Some('>'),
                        target: None,
                    }));
                }
                return Err(Unreadable::new(REASON_REDIRECT));
            }
            (Some('<'), Some('<')) => return Err(Unreadable::new(REASON_HEREDOC)),
            (Some('<'), Some('>' | '(')) | (Some('>'), Some('(')) => {
                return Err(Unreadable::new(REASON_REDIRECT))
            }
            (Some('>'), _) => (true, 1),
            (Some('<'), _) => (false, 1),
            _ => return Ok(None),
        };
        self.at += len;
        self.skip_blanks();
        if matches!(self.peek(), None | Some('|' | '&' | ';' | '\n' | '<' | '>')) {
            return Err(Unreadable::new(REASON_REDIRECT));
        }
        let (target, _) = self.word()?;
        Ok(Some(Redirect {
            output,
            target: Some(target),
        }))
    }

    /// One word at the cursor: its text and whether the shell can change
    /// it. It stops at a blank, a separator or a redirect.
    fn word(&mut self) -> Result<(String, bool), Unreadable> {
        let start = self.at;
        let mut text = String::new();
        let mut changes = false;
        while let Some(c) = self.peek() {
            match c {
                ' ' | '\t' | '\n' | '|' | ';' | '&' | '<' | '>' => break,
                '\'' => {
                    self.at += 1;
                    loop {
                        match self.peek() {
                            None | Some('\n' | '\r') => return Err(Unreadable::new(REASON_QUOTES)),
                            Some('\'') => break,
                            Some(c) => text.push(c),
                        }
                        self.at += 1;
                    }
                }
                '"' => {
                    self.at += 1;
                    self.double_quoted(&mut text, &mut changes)?;
                    continue;
                }
                '\\' => {
                    match self.peek_at(1) {
                        Some(c) if c.is_ascii_graphic() || c == ' ' => text.push(c),
                        _ => return Err(Unreadable::new(REASON_CHARACTER)),
                    }
                    self.at += 1;
                }
                '$' => {
                    self.at += 1;
                    text.push_str(&self.parameter()?);
                    changes = true;
                    continue;
                }
                '#' => return Err(Unreadable::new(REASON_COMMENT)),
                '(' | ')' => return Err(Unreadable::new(REASON_PAREN)),
                '{' | '}' => return Err(Unreadable::new(REASON_BRACE)),
                '`' => return Err(Unreadable::new(REASON_SUBSTITUTION)),
                // A tilde expands at the start of a word and after `=` or
                // `:`; elsewhere (`HEAD~1`) it is text.
                '~' => {
                    let previous = (self.at > start).then(|| self.chars[self.at - 1].1);
                    if matches!(previous, None | Some('=' | ':')) {
                        return Err(Unreadable::new(REASON_TILDE));
                    }
                    text.push('~');
                }
                c if plain_char(c) => text.push(c),
                c if glob_char(c) => {
                    text.push(c);
                    changes = true;
                }
                _ => return Err(Unreadable::new(REASON_CHARACTER)),
            }
            self.at += 1;
        }
        Ok((text, changes))
    }

    /// The rest of a double-quoted string, after its `"`.
    fn double_quoted(&mut self, text: &mut String, changes: &mut bool) -> Result<(), Unreadable> {
        loop {
            match self.peek() {
                None | Some('\n' | '\r') => return Err(Unreadable::new(REASON_QUOTES)),
                Some('"') => {
                    self.at += 1;
                    return Ok(());
                }
                Some('`') => return Err(Unreadable::new(REASON_SUBSTITUTION)),
                Some('!') => return Err(Unreadable::new(REASON_CHARACTER)),
                Some('\\') => {
                    match self.peek_at(1) {
                        Some(c @ ('$' | '`' | '"' | '\\')) => text.push(c),
                        Some(c) if !c.is_control() => {
                            text.push('\\');
                            text.push(c);
                        }
                        _ => return Err(Unreadable::new(REASON_QUOTES)),
                    }
                    self.at += 2;
                }
                Some('$') => {
                    self.at += 1;
                    text.push_str(&self.parameter()?);
                    *changes = true;
                }
                Some(c) if c.is_control() && c != '\t' => {
                    return Err(Unreadable::new(REASON_CHARACTER))
                }
                Some(c) => {
                    text.push(c);
                    self.at += 1;
                }
            }
        }
    }

    /// `NAME` or `{NAME}` after a `$`, returned as written with the `$`.
    fn parameter(&mut self) -> Result<String, Unreadable> {
        let name_start = |c: char| c.is_ascii_alphabetic() || c == '_';
        let name_char = |c: char| c.is_ascii_alphanumeric() || c == '_';
        let braced = self.peek() == Some('{');
        if braced {
            self.at += 1;
        }
        match self.peek() {
            Some(c) if name_start(c) => {}
            Some('(' | '[') => return Err(Unreadable::new(REASON_SUBSTITUTION)),
            _ => return Err(Unreadable::new(REASON_DOLLAR)),
        }
        let mut name = String::new();
        while let Some(c) = self.peek().filter(|&c| name_char(c)) {
            name.push(c);
            self.at += 1;
        }
        if braced {
            if self.peek() != Some('}') {
                return Err(Unreadable::new(REASON_DOLLAR));
            }
            self.at += 1;
            return Ok(format!("${{{name}}}"));
        }
        Ok(format!("${name}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn programs(command: &str) -> Vec<String> {
        parse(command)
            .unwrap()
            .iter()
            .map(|s| s.program().to_owned())
            .collect()
    }

    #[test]
    fn reads_the_grammar() {
        assert_eq!(
            programs("ls -la | grep x && wc -l; cat a\nhead b"),
            ["ls", "grep", "wc", "cat", "head"]
        );
        let simple = &parse("grep -rn \"x y\" 'a b' c\\ d $HOME \"${HOME}/x\"").unwrap()[0];
        assert_eq!(
            simple.words,
            ["grep", "-rn", "x y", "a b", "c d", "$HOME", "${HOME}/x"]
        );
        assert_eq!(
            simple.dynamic,
            [false, false, false, false, false, true, true]
        );
        let simple = &parse("cargo test 2>&1 >out <in").unwrap()[0];
        assert_eq!(simple.words, ["cargo", "test"]);
        assert!(simple.writes());
        assert!(!parse("cargo test 2>&1").unwrap()[0].writes());
        assert_eq!(parse("ls;\n").unwrap().len(), 1);
        assert_eq!(parse("grep \"a\\.b\" f").unwrap()[0].words[1], "a\\.b");
    }

    #[test]
    fn everything_else_is_unreadable() {
        for command in [
            "echo #'\ntouch x\n'",
            "echo ${x:=a[\\$(touch x)]}; echo $[x]",
            "echo ${s:1}",
            "export y=ouch; t${y} x",
            "echo $'a'",
            "echo $\"a\"",
            "echo $$",
            "echo $?",
            "echo $1",
            "echo 'a\nb'",
            "echo \"a\nb\"",
            "echo \"`x`\"",
            "echo \"$(x)\"",
            "ls &",
            "ls |& cat",
            "ls ;; ls",
            "ls | ",
            "(ls)",
            "{ ls; }",
            "ls ~",
            "echo a\r",
            "cat <<EOF",
            "cat <> f",
            "echo a\\",
            "for f in a; do touch $f; done",
            "f() { touch x; }",
            "X=1 ls",
            "'ls' -la",
            "\\ls",
            "env ls",
            "hash -p /bin/touch ls; ls x",
            "read PATH < f; ls",
            "export PATH=.; ls",
            "printf -v PATH x; ls",
            "trap 'touch x' EXIT",
            "echo é",
            "> out",
            "[ -f x ]",
        ] {
            assert!(parse(command).is_err(), "readable: {command:?}");
        }
        assert!(parse("export FOO=1; cargo test").is_ok());
    }
}
