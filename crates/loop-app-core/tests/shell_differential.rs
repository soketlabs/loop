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

use loop_agent::harness::tools::shell::{parse, parse_with, Simple, Unreadable, WRAPPERS};
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
    "~",
    "~/x",
    // Option injection: `$X` holds `-exec touch M ;`, `$Y` holds `-o`, and
    // `*` matches the files `-o` and `zz` in the working folder.
    "$X",
    "$Y",
    "*",
    // Options that run a program.
    "-exec touch M \\;",
    "--pre=touch",
    "--compress-program=touch",
    "-c alias.x='!touch M'",
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

/// Whole commands: an export then a use, and wrapper forms.
const WHOLE: &[&str] = &[
    "export GIT_EXTERNAL_DIFF=touch; git diff",
    "export FOO=1; ls",
    "export FOO=1",
    "export 'BASH_FUNC_ls%%=() { touch M; }'; ls",
    "find . -exec touch M \\;",
    "git -c alias.x='!touch M' x",
    "rg --pre=touch x a.txt",
    "env touch M",
    "env FOO=1 touch M",
    "xargs touch M",
    "nohup touch M",
    "find . $X",
    "sort *",
];

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
    out.extend(WHOLE.iter().map(|s| s.to_string()));
    while out.len() < count {
        if rng.below(20) == 0 {
            let mut text = rng.pick(WHOLE).to_owned();
            text.push_str(rng.pick(SEPARATORS));
            text.push_str(rng.pick(READERS));
            out.push(text);
            continue;
        }
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
        let body = match *program {
            // Wrappers run what they are given, as the real ones do, so a
            // program they start shows in the log.
            "env" => "#!/bin/sh\necho env >> \"$LOOP_DIFF_LOG\"\nwhile case \"$1\" in *=*) true;; *) false;; esac; do shift; done\n[ $# -gt 0 ] && exec \"$@\"\n".to_owned(),
            "xargs" | "nohup" => format!(
                "#!/bin/sh\necho {program} >> \"$LOOP_DIFF_LOG\"\n[ $# -gt 0 ] && exec \"$@\"\n"
            ),
            _ => format!(
                "#!/bin/sh\necho {program} >> \"$LOOP_DIFF_LOG\"\n: > \"$LOOP_DIFF_MARK/{program}\"\n"
            ),
        };
        write(program, body);
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
    /// The working folder changed (a file added, removed or rewritten).
    changed: bool,
}

/// The working folder's files and their contents.
fn snapshot(dir: &Path) -> Vec<(String, Vec<u8>)> {
    names(dir)
        .into_iter()
        .map(|name| {
            let content = std::fs::read(dir.join(&name)).unwrap_or_default();
            (name, content)
        })
        .collect()
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
    // Files a glob can turn into options.
    std::fs::write(work.join("-o"), "").unwrap();
    std::fs::write(work.join("zz"), "z\n").unwrap();
    let before = snapshot(&work);
    let mut child = Command::new(shell)
        .arg("-c")
        .arg(command)
        .current_dir(&work)
        .env_clear()
        .env("PATH", bin)
        .env("HOME", &home)
        .env("LOOP_DIFF_LOG", &log)
        .env("LOOP_DIFF_MARK", &marks)
        .env("X", "-exec touch M ;")
        .env("Y", "-o")
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
        changed: snapshot(&work) != before,
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

/// Run `corpus` with `classify` as the reader; every disagreement with a
/// real shell, as text. Returns the failures and the counts of readable
/// and read-only strings.
fn check(
    corpus: &[String],
    classify: &dyn Fn(&str) -> Result<Vec<Simple>, Unreadable>,
) -> (Vec<String>, usize, usize) {
    let bin = tempfile::tempdir().unwrap();
    let readers = stand_ins(bin.path());
    let stubbed = |name: &str| readers.iter().any(|r| r == name) || WRITERS.contains(&name);
    let shells = shells();
    let (mut failures, mut read_only, mut readable) = (Vec::new(), 0, 0);
    for command in corpus {
        let parsed = classify(command);
        let is_read_only = read_only_command(command);
        if is_read_only && parsed.is_err() {
            failures.push(format!("read-only but unreadable: {command:?}"));
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
        // Every readable string, read-only or not.
        for shell in &shells {
            let ran = run(shell, command, bin.path());
            if is_read_only && (!ran.markers.is_empty() || ran.changed) {
                failures.push(format!(
                    "{shell}: read-only {command:?} wrote: markers {:?}, folder changed {}",
                    ran.markers, ran.changed
                ));
            }
            let started = counts(ran.started.clone());
            let expected = counts(reported.clone());
            for (program, n) in &started {
                if !expected.get(program).is_some_and(|m| n <= m) {
                    failures.push(format!(
                        "{shell}: {command:?} started {program}, reported {reported:?}"
                    ));
                }
            }
            if exact && started != expected {
                failures.push(format!(
                    "{shell}: {command:?} started {started:?}, reported {expected:?}"
                ));
            }
        }
    }
    (failures, readable, read_only)
}

fn differential(count: usize) {
    let corpus = corpus(20261113, count);
    let (failures, readable, read_only) = check(&corpus, &parse);
    assert!(failures.is_empty(), "{failures:#?}");
    assert!(
        read_only >= count / 10 && readable >= count / 5,
        "too few cases: {read_only} read-only, {readable} readable of {count}"
    );
    eprintln!(
        "shell differential: {count} strings, {readable} readable, {read_only} read-only, shells {:?}",
        shells()
    );
}

/// The checker catches a weaker reader: with one wrapper left off the
/// list, the programs it starts are not reported, and the check fails.
#[test]
fn the_check_fails_when_a_wrapper_is_missing() {
    let corpus: Vec<String> = ["env touch M", "xargs touch M", "nohup touch M"]
        .iter()
        .map(|s| s.to_string())
        .collect();
    let (failures, _, _) = check(&corpus, &parse);
    assert!(failures.is_empty(), "{failures:#?}");
    for missing in ["env", "xargs", "nohup"] {
        let fewer: Vec<&str> = WRAPPERS.iter().copied().filter(|w| *w != missing).collect();
        let weak = move |command: &str| parse_with(command, &fewer);
        let (failures, _, _) = check(&corpus, &weak);
        assert!(
            failures.iter().any(|f| f.contains(missing)),
            "without `{missing}` the check passed: {failures:#?}"
        );
    }
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
