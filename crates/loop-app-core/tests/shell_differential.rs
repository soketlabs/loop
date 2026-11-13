//! Differential test of the shell reader against real shells.
//!
//! A seeded corpus of command strings mixes the reader's grammar with
//! hostile fragments. For each string:
//!
//! - (a) when Loop calls it read-only, it runs under `sh`, `bash` and
//!   `dash` (each when installed) in an empty folder, with a `PATH` of
//!   stand-ins: programs that write make a marker and do nothing else; the
//!   read-only programs log their name and run the real program. No marker
//!   and no new file may appear;
//! - (b) when Loop can read it, the programs the shell starts (from the
//!   stand-ins' log) are programs Loop reports, and with only `;`, `|` and
//!   newlines between commands (no `&&` or `||` that may skip one) they
//!   are exactly the reported ones (without redirects: one that fails
//!   stops its command before it starts);
//! - (c) the bypass strings of the reviews are fixed cases.
//!
//! The run in CI uses 400 strings. `LOOP_SHELL_DIFF_CASES=N` runs more
//! (the ignored test runs 5000).
#![cfg(unix)]

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use loop_agent::harness::tools::shell::parse;
use loop_app_core::permissions::read_only_command;

/// Programs Loop may call read-only; each runs for real through a logging
/// stand-in, when it is installed.
const READERS: &[&str] = &[
    "ls", "cat", "head", "tail", "wc", "grep", "find", "sort", "uniq", "diff", "stat", "du",
    "file", "git", "rg", "tree",
];

/// Programs that write or run others: their stand-ins only log and make a
/// marker.
const WRITERS: &[&str] = &[
    "touch", "rm", "mv", "cp", "tee", "mkdir", "ln", "chmod", "dd", "sh", "bash", "dash", "xargs",
    "env", "python3", "perl", "nohup", "nice", "timeout", "sed", "awk", "curl",
];

/// Arguments from the grammar.
const ARGS: &[&str] = &[
    "-la",
    "-n",
    "-rn",
    "x",
    "a.txt",
    "'a b'",
    "\"x y\"",
    "$HOME",
    "\"${HOME}/x\"",
    "*.rs",
    "-o out",
    "--output=out",
    "-delete",
    "-exec touch M \\;",
    "PWNED",
    "HEAD~1",
    "-- src",
    "<in",
    "2>&1",
    "> out",
    "\"a\\.b\"",
    "x\\ y",
    "-name '*.rs'",
];

/// Fragments the reader must not be fooled by.
const HOSTILE: &[&str] = &[
    "#",
    "'",
    "\"",
    "\n",
    "$[x]",
    "${x:=a}",
    "${s:1}",
    "$(touch M)",
    "`touch M`",
    "\\",
    "<<EOF",
    "f(){ touch M; }",
    "\r",
    "\t",
    "~",
    "{a,b}",
    " & ",
    "!",
    "$'x'",
    ";;",
    "|&",
    "(",
    ")",
    "$x",
    "${x}",
    "X=1 ",
    " touch M ",
    "\n touch M\n",
];

const SEPARATORS: &[&str] = &[" | ", " && ", " || ", "; ", "\n"];

/// The bypass strings of review rounds 1 and 2.
const FIXED: &[&str] = &[
    "echo $'\\''\ntouch PWNED\necho '",
    "ls () ( touch PWNED ); ls",
    "echo #'\ntouch PWNED\n'",
    "echo ${x:=a[\\$(touch PWNED)]}; echo $[x]",
    "echo ${s:1}",
    "export y=ouch; t${y} PWNED",
    "tree -ao out.txt",
    "ls () { touch PWNED; }; ls",
    "tou\\\nch PWNED",
    "cat <(touch PWNED)",
    "echo {touch,PWNED}",
];

/// A small seeded generator, so a run is the same every time.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 = self
            .0
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        self.0 >> 33
    }
    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }
    fn pick<'a>(&mut self, items: &[&'a str]) -> &'a str {
        items[self.below(items.len())]
    }
}

fn corpus(seed: u64, count: usize) -> Vec<String> {
    let mut rng = Rng(seed);
    let mut out: Vec<String> = FIXED.iter().map(|s| s.to_string()).collect();
    while out.len() < count {
        let mut text = String::new();
        for i in 0..1 + rng.below(3) {
            if i > 0 {
                text.push_str(rng.pick(SEPARATORS));
            }
            // Mostly readers, so many strings are read-only.
            if rng.below(4) == 0 {
                text.push_str(rng.pick(WRITERS));
            } else {
                text.push_str(rng.pick(READERS));
            }
            for _ in 0..rng.below(4) {
                text.push(' ');
                text.push_str(rng.pick(ARGS));
            }
        }
        if rng.below(10) < 4 {
            let fragment = rng.pick(HOSTILE);
            let at = rng.below(text.len() + 1);
            let at = (0..=at)
                .rev()
                .find(|&i| text.is_char_boundary(i))
                .unwrap_or(0);
            text.insert_str(at, fragment);
        }
        out.push(text);
    }
    out
}

/// Where a real program is installed.
fn installed(program: &str) -> Option<PathBuf> {
    ["/bin", "/usr/bin", "/usr/local/bin", "/opt/homebrew/bin"]
        .iter()
        .map(|dir| Path::new(dir).join(program))
        .find(|path| path.is_file())
}

/// A folder of stand-ins, and the readers found.
fn stand_ins(dir: &Path) -> Vec<String> {
    use std::os::unix::fs::PermissionsExt;
    let mut readers = Vec::new();
    let write = |name: &str, body: String| {
        let path = dir.join(name);
        std::fs::write(&path, body).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    };
    for program in READERS {
        if let Some(real) = installed(program) {
            write(
                program,
                format!(
                    "#!/bin/sh\necho {program} >> \"$LOOP_DIFF_LOG\"\nexec {} \"$@\"\n",
                    real.display()
                ),
            );
            readers.push(program.to_string());
        }
    }
    for program in WRITERS {
        write(
            program,
            format!(
                "#!/bin/sh\necho {program} >> \"$LOOP_DIFF_LOG\"\n: > \"$LOOP_DIFF_MARK/{program}\"\n"
            ),
        );
    }
    readers
}

/// The shells installed.
fn shells() -> Vec<&'static str> {
    ["/bin/sh", "/bin/bash", "/bin/dash"]
        .into_iter()
        .filter(|s| Path::new(s).exists())
        .collect()
}

/// What one run left: the programs started, markers, and new files in the
/// working folder.
struct Ran {
    started: Vec<String>,
    markers: Vec<String>,
    files: Vec<String>,
}

fn names(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    names
}

fn run(shell: &str, command: &str, bin: &Path) -> Ran {
    let root = tempfile::tempdir().unwrap();
    let (work, home, marks) = (
        root.path().join("work"),
        root.path().join("home"),
        root.path().join("marks"),
    );
    for dir in [&work, &home, &marks] {
        std::fs::create_dir(dir).unwrap();
    }
    let log = root.path().join("log");
    std::fs::write(&log, "").unwrap();
    let mut child = Command::new(shell)
        .arg("-c")
        .arg(command)
        .current_dir(&work)
        .env_clear()
        .env("PATH", bin)
        .env("HOME", &home)
        .env("LOOP_DIFF_LOG", &log)
        .env("LOOP_DIFF_MARK", &marks)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    while child.try_wait().unwrap().is_none() {
        if Instant::now() > deadline {
            let _ = child.kill();
            let _ = child.wait();
            break;
        }
        std::thread::sleep(Duration::from_millis(2));
    }
    let started = std::fs::read_to_string(&log)
        .unwrap()
        .lines()
        .map(str::to_owned)
        .collect();
    Ran {
        started,
        markers: names(&marks),
        files: names(&work),
    }
}

/// Count of each name.
fn counts(names: impl IntoIterator<Item = String>) -> BTreeMap<String, usize> {
    let mut out = BTreeMap::new();
    for name in names {
        *out.entry(name).or_default() += 1;
    }
    out
}

fn differential(count: usize) {
    let bin = tempfile::tempdir().unwrap();
    let readers = stand_ins(bin.path());
    let stubbed = |name: &str| readers.iter().any(|r| r == name) || WRITERS.contains(&name);
    let shells = shells();
    let (mut read_only, mut readable) = (0, 0);
    for command in corpus(20261113, count) {
        let parsed = parse(&command);
        let is_read_only = read_only_command(&command);
        if is_read_only {
            assert!(parsed.is_ok(), "read-only but unreadable: {command:?}");
        }
        let Ok(commands) = parsed else {
            continue;
        };
        readable += 1;
        read_only += usize::from(is_read_only);
        let reported: Vec<String> = commands
            .iter()
            .map(|c| c.program().rsplit('/').next().unwrap_or("").to_owned())
            .filter(|p| stubbed(p))
            .collect();
        // `&&` and `||` can skip a command, and a redirect that fails (a
        // missing `<in`, an empty `>$x`) stops one before it starts.
        let exact = !command.contains("&&")
            && !command.contains("||")
            && commands.iter().all(|c| c.redirects.is_empty());
        for shell in &shells {
            let ran = run(shell, &command, bin.path());
            if is_read_only {
                assert!(
                    ran.markers.is_empty() && ran.files.is_empty(),
                    "{shell}: read-only {command:?} wrote: markers {:?}, files {:?}",
                    ran.markers,
                    ran.files
                );
            }
            let started = counts(ran.started.clone());
            let expected = counts(reported.clone());
            for (program, n) in &started {
                assert!(
                    expected.get(program).is_some_and(|m| n <= m),
                    "{shell}: {command:?} started {program}, reported {reported:?}"
                );
            }
            if exact {
                assert_eq!(started, expected, "{shell}: {command:?}");
            }
        }
    }
    assert!(
        read_only >= count / 10 && readable >= count / 5,
        "too few cases: {read_only} read-only, {readable} readable of {count}"
    );
    eprintln!(
        "shell differential: {count} strings, {readable} readable, {read_only} read-only, shells {shells:?}"
    );
}

#[test]
fn the_reader_agrees_with_real_shells() {
    let count = std::env::var("LOOP_SHELL_DIFF_CASES")
        .ok()
        .and_then(|n| n.parse().ok())
        .unwrap_or(400);
    differential(count);
}

#[test]
#[ignore = "a larger run; minutes"]
fn the_reader_agrees_with_real_shells_at_length() {
    differential(5000);
}

/// The fixed bypass strings are never read-only.
#[test]
fn the_review_bypasses_are_not_read_only() {
    for command in FIXED {
        assert!(!read_only_command(command), "{command:?}");
    }
}
