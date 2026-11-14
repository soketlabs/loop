//! `session/terminal/*` over a real connection: echo, resize, Ctrl-C, the
//! exit code, an output burst to a slow client (dropped accounting),
//! re-attach by `seq`, close and Session close kill the process tree,
//! limits, the gate, ownership, and no terminal byte in the Event Log.
//!
//! These tests start processes, so they have a binary of their own.

#[path = "support/workspace.rs"]
mod support;

use std::time::Duration;

use loop_protocol::rpc::SessionParams;
use loop_protocol::session_workspace::{
    base64, SessionWorkspaceLimits, TerminalAttachParams, TerminalInputParams, TerminalOpenParams,
    TerminalParams, TerminalResizeParams, TerminalSignal, TerminalSignalParams,
};
use loop_protocol::SessionRef;
use loop_wire::{RemoteBackend, SessionWorkspace, TerminalEvent, TerminalFeed};
use serde_json::json;
use support::{client, code, log_text, new_session, runtime, Fixture, Options};

const WAIT: Duration = Duration::from_secs(20);

/// A plain `sh` that reads the terminal: no rc files, a known prompt.
const SH: &str = "PS1='$ ' exec /bin/sh";

fn sw(c: &RemoteBackend) -> &dyn SessionWorkspace {
    c
}

async fn open(c: &RemoteBackend, s: &SessionRef, command: Option<&str>) -> String {
    sw(c)
        .terminal_open(TerminalOpenParams {
            session: s.clone(),
            cols: 80,
            rows: 24,
            command: command.map(str::to_owned),
        })
        .await
        .unwrap()
        .terminal
}

async fn attach(
    c: &RemoteBackend,
    s: &SessionRef,
    terminal: &str,
    after_seq: u64,
) -> Box<dyn TerminalFeed> {
    sw(c)
        .terminal_attach(TerminalAttachParams {
            session: s.clone(),
            terminal: terminal.into(),
            after_seq,
        })
        .await
        .unwrap()
        .1
}

async fn type_in(c: &RemoteBackend, s: &SessionRef, terminal: &str, text: &str) {
    sw(c)
        .terminal_input(TerminalInputParams {
            session: s.clone(),
            terminal: terminal.into(),
            data: base64::encode(text.as_bytes()),
        })
        .await
        .unwrap();
}

/// What a feed gave: the text, the last `seq`, the exit.
#[derive(Default)]
struct Seen {
    text: String,
    last_seq: u64,
    exit: Option<loop_protocol::session_workspace::TerminalEnd>,
}

/// Read `feed` until `done` holds for the text so far (or the exit).
async fn until(feed: &dyn TerminalFeed, seen: &mut Seen, done: impl Fn(&str) -> bool) {
    tokio::time::timeout(WAIT, async {
        while !done(&seen.text) {
            match feed.next().await {
                Some(TerminalEvent::Output { seq, data }) => {
                    assert_eq!(seq, seen.last_seq + 1, "seq gap");
                    seen.last_seq = seq;
                    seen.text.push_str(&String::from_utf8_lossy(&data));
                }
                Some(TerminalEvent::Dropped { seq, .. }) => seen.last_seq = seq,
                Some(TerminalEvent::Exited { seq, exit }) => {
                    seen.last_seq = seq;
                    seen.exit = Some(exit);
                    return;
                }
                None => return,
            }
        }
    })
    .await
    .unwrap_or_else(|_| panic!("timed out; seen: {:?}", seen.text));
}

fn alive(pid: &str) -> bool {
    std::process::Command::new("kill")
        .args(["-0", pid])
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

async fn gone(pid: &str) {
    tokio::time::timeout(WAIT, async {
        while alive(pid) {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("process {pid} still runs"));
}

/// The pid after `marker=` in `text`.
fn pid_after(text: &str, marker: &str) -> String {
    let at = text.find(marker).unwrap() + marker.len();
    text[at..]
        .chars()
        .take_while(char::is_ascii_digit)
        .collect()
}

async fn finish(f: Fixture) {
    f.rt.shutdown().await;
    let leaked = loop_agent::harness::sandbox::leaked_host_processes();
    assert!(leaked.is_empty(), "left behind: {leaked:?}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_terminal_echoes_resizes_takes_ctrl_c_and_reports_the_exit_code() {
    let f = runtime(Options::default()).await;
    let c = client(&f.rt, None).await;
    assert!(support::caps(&c).session_terminal);
    let s = new_session(&c, &f.ws).await;
    let t = open(&c, &s, Some(SH)).await;
    assert!(t.starts_with("term-") && t.len() == 37, "{t}");
    let feed = attach(&c, &s, &t, 0).await;
    let mut seen = Seen::default();

    type_in(&c, &s, &t, "echo hi-$((40+2))\n").await;
    until(&*feed, &mut seen, |text| text.contains("hi-42")).await;

    type_in(&c, &s, &t, "stty size\n").await;
    until(&*feed, &mut seen, |text| text.contains("24 80")).await;
    sw(&c)
        .terminal_resize(TerminalResizeParams {
            session: s.clone(),
            terminal: t.clone(),
            cols: 100,
            rows: 30,
        })
        .await
        .unwrap();
    type_in(&c, &s, &t, "stty size\n").await;
    until(&*feed, &mut seen, |text| text.contains("30 100")).await;

    // Ctrl-C stops the foreground command; the shell stays.
    type_in(&c, &s, &t, "sleep 30; echo slept\n").await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    sw(&c)
        .terminal_signal(TerminalSignalParams {
            session: s.clone(),
            terminal: t.clone(),
            signal: TerminalSignal::Interrupt,
        })
        .await
        .unwrap();
    type_in(&c, &s, &t, "echo after-$((1+1))\n").await;
    let started = std::time::Instant::now();
    until(&*feed, &mut seen, |text| text.contains("after-2")).await;
    assert!(started.elapsed() < Duration::from_secs(10));
    // The typed line holds "slept"; its output would start a line.
    assert!(!seen.text.contains("\nslept"), "{}", seen.text);

    type_in(&c, &s, &t, "exit 3\n").await;
    until(&*feed, &mut seen, |_| false).await;
    let exit = seen.exit.take().expect("an exit event");
    assert_eq!((exit.state.as_str(), exit.code), ("exited", Some(3)));
    let listed = sw(&c)
        .terminal_list(SessionParams { session: s.clone() })
        .await
        .unwrap();
    assert_eq!(listed.terminals.len(), 1);
    assert_eq!(
        listed.terminals[0].exit.as_ref().and_then(|e| e.code),
        Some(3)
    );
    let closed = sw(&c)
        .terminal_close(TerminalParams {
            session: s.clone(),
            terminal: t.clone(),
        })
        .await
        .unwrap();
    assert_eq!(closed.exit.and_then(|e| e.code), Some(3));
    finish(f).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_new_connection_re_attaches_by_seq() {
    let f = runtime(Options::default()).await;
    let first = client(&f.rt, None).await;
    let s = new_session(&first, &f.ws).await;
    let t = open(&first, &s, Some(SH)).await;
    let feed = attach(&first, &s, &t, 0).await;
    let mut seen = Seen::default();
    type_in(&first, &s, &t, "echo one-$((0+1))\n").await;
    until(&*feed, &mut seen, |text| text.contains("one-1")).await;
    let cursor = seen.last_seq;
    // The client goes away; the terminal runs on, detached.
    drop(feed);
    first.disconnect();
    tokio::time::sleep(Duration::from_millis(200)).await;
    let second = client(&f.rt, None).await;
    let listed = sw(&second)
        .terminal_list(SessionParams { session: s.clone() })
        .await
        .unwrap();
    assert_eq!(listed.terminals[0].attached, 0);
    type_in(&second, &s, &t, "echo two-$((1+1))\n").await;
    let feed = attach(&second, &s, &t, cursor).await;
    let mut after = Seen {
        last_seq: cursor,
        ..Seen::default()
    };
    until(&*feed, &mut after, |text| text.contains("two-2")).await;
    assert!(!after.text.contains("one-1"), "{}", after.text);
    // From 0: the whole buffer again.
    let feed = attach(&second, &s, &t, 0).await;
    let mut all = Seen::default();
    until(&*feed, &mut all, |text| text.contains("two-2")).await;
    assert!(all.text.contains("one-1"), "{}", all.text);
    // A cursor past the newest event.
    let ahead = sw(&second)
        .terminal_attach(TerminalAttachParams {
            session: s.clone(),
            terminal: t.clone(),
            after_seq: 1_000_000,
        })
        .await
        .err()
        .unwrap();
    assert_eq!(code(&ahead), "seq_ahead");
    finish(f).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn close_and_session_close_kill_the_process_tree_with_a_record() {
    let f = runtime(Options::default()).await;
    let c = client(&f.rt, None).await;
    let s = new_session(&c, &f.ws).await;
    let tree = "sleep 300 & echo CHILD=$!; sleep 300";
    // Closed by the user.
    let t = open(&c, &s, Some(tree)).await;
    let feed = attach(&c, &s, &t, 0).await;
    let mut seen = Seen::default();
    until(&*feed, &mut seen, |text| {
        text.contains("CHILD=") && text.ends_with('\n')
    })
    .await;
    let child = pid_after(&seen.text, "CHILD=");
    assert!(alive(&child));
    sw(&c)
        .terminal_close(TerminalParams {
            session: s.clone(),
            terminal: t.clone(),
        })
        .await
        .unwrap();
    gone(&child).await;
    let unknown = sw(&c)
        .terminal_input(TerminalInputParams {
            session: s.clone(),
            terminal: t.clone(),
            data: base64::encode(b"x"),
        })
        .await
        .unwrap_err();
    assert_eq!(code(&unknown), "unknown_terminal");
    // Closed with its Session.
    let t2 = open(&c, &s, Some(tree)).await;
    let feed = attach(&c, &s, &t2, 0).await;
    let mut seen = Seen::default();
    until(&*feed, &mut seen, |text| {
        text.contains("CHILD=") && text.ends_with('\n')
    })
    .await;
    let child = pid_after(&seen.text, "CHILD=");
    loop_wire::RuntimeControl::close(&*f.rt, &s, true)
        .await
        .unwrap();
    gone(&child).await;
    until(&*feed, &mut seen, |_| false).await;
    assert!(seen.exit.is_some(), "the stream ends with the exit");
    // Records: an open and an end for each, with the terminal id. The end
    // is recorded once the tree is gone.
    let log = tokio::time::timeout(WAIT, async {
        loop {
            let log = log_text(&f.log).await;
            if log.contains("the Session closed or was unloaded") {
                return log;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .unwrap();
    for id in [&t, &t2] {
        assert_eq!(log.matches(id.as_str()).count(), 2, "{id}: {log}");
    }
    assert!(log.contains("closed by the user"), "{log}");
    assert!(log.contains("the Session closed or was unloaded"), "{log}");
    finish(f).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn limits_count_terminals_per_session_and_a_detached_one_ends() {
    let f = runtime(Options::default()).await;
    f.rt.set_workspace_limits(SessionWorkspaceLimits {
        terminals_per_session: 1,
        detached_terminal_secs: 1,
        ..SessionWorkspaceLimits::default()
    });
    let c = client(&f.rt, None).await;
    let s = new_session(&c, &f.ws).await;
    let t = open(&c, &s, Some(SH)).await;
    let second = sw(&c)
        .terminal_open(TerminalOpenParams {
            session: s.clone(),
            cols: 80,
            rows: 24,
            command: None,
        })
        .await
        .unwrap_err();
    assert_eq!(code(&second), "limit");
    // Nobody streams it: it ends after the detached time.
    tokio::time::timeout(WAIT, async {
        loop {
            let listed = sw(&c)
                .terminal_list(SessionParams { session: s.clone() })
                .await
                .unwrap();
            if listed.terminals.is_empty() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    })
    .await
    .unwrap();
    // The end is recorded once the process tree is gone.
    tokio::time::timeout(WAIT, async {
        while !log_text(&f.log).await.contains("no client streamed it") {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .unwrap();
    let _ = t;
    finish(f).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_gate_refuses_a_terminal_it_cannot_check() {
    let f = runtime(Options {
        settings: json!({"toolPermissions": {"bash(rm *)": "deny"}}),
        ..Options::default()
    })
    .await;
    let c = client(&f.rt, None).await;
    let s = new_session(&c, &f.ws).await;
    let info = sw(&c)
        .info(SessionParams { session: s.clone() })
        .await
        .unwrap();
    assert!(!info.terminal.available, "{info:?}");
    let refused = sw(&c)
        .terminal_open(TerminalOpenParams {
            session: s.clone(),
            cols: 80,
            rows: 24,
            command: None,
        })
        .await
        .unwrap_err();
    assert_eq!(code(&refused), "refused");
    assert!(refused.to_string().contains("cannot check"), "{refused}");
    let log = log_text(&f.log).await;
    assert!(
        log.contains("user_terminal") && log.contains("unreadable"),
        "{log}"
    );
    finish(f).await;

    let f = runtime(Options {
        mode: "read-only",
        ..Options::default()
    })
    .await;
    let c = client(&f.rt, None).await;
    let s = new_session(&c, &f.ws).await;
    let refused = sw(&c)
        .terminal_open(TerminalOpenParams {
            session: s.clone(),
            cols: 80,
            rows: 24,
            command: None,
        })
        .await
        .unwrap_err();
    assert_eq!(code(&refused), "refused");
    finish(f).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn another_owner_cannot_use_a_terminal_id() {
    let f = runtime(Options::default()).await;
    let c = client(&f.rt, None).await;
    let s = new_session(&c, &f.ws).await;
    let t = open(&c, &s, Some(SH)).await;
    // An in-process call has every right on the Session, but it is not the
    // terminal's owner (the host's user on the connection).
    let rt = f.rt.clone();
    let refused = SessionWorkspace::terminal_input(
        &*rt,
        TerminalInputParams {
            session: s.clone(),
            terminal: t.clone(),
            data: base64::encode(b"echo pwned\n"),
        },
    )
    .await
    .unwrap_err();
    assert_eq!(code(&refused), "unknown_terminal");
    let attach = SessionWorkspace::terminal_attach(
        &*rt,
        TerminalAttachParams {
            session: s.clone(),
            terminal: t.clone(),
            after_seq: 0,
        },
    )
    .await
    .err()
    .unwrap();
    assert_eq!(code(&attach), "unknown_terminal");
    let listed = SessionWorkspace::terminal_list(&*rt, SessionParams { session: s.clone() })
        .await
        .unwrap();
    assert!(listed.terminals.is_empty());
    // A guessed id on the owner's own Session.
    let guess = sw(&c)
        .terminal_close(TerminalParams {
            session: s.clone(),
            terminal: "term-00000000000000000000000000000000".into(),
        })
        .await
        .unwrap_err();
    assert_eq!(code(&guess), "unknown_terminal");
    finish(f).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn terminal_bytes_never_reach_the_event_log() {
    let f = runtime(Options::default()).await;
    let c = client(&f.rt, None).await;
    let s = new_session(&c, &f.ws).await;
    let t = open(&c, &s, Some(SH)).await;
    let feed = attach(&c, &s, &t, 0).await;
    let mut seen = Seen::default();
    // The marker is made by the shell: the typed text does not hold it.
    type_in(&c, &s, &t, "printf 'MARK%s%s\\n' 7731 9902\n").await;
    until(&*feed, &mut seen, |text| text.contains("MARK77319902")).await;
    type_in(&c, &s, &t, "exit\n").await;
    until(&*feed, &mut seen, |_| false).await;
    sw(&c)
        .terminal_close(TerminalParams {
            session: s.clone(),
            terminal: t.clone(),
        })
        .await
        .unwrap();
    f.rt.shutdown().await;
    let log = log_text(&f.log).await;
    assert!(!log.contains("MARK77319902"), "terminal output in the log");
    assert!(!log.contains("7731"), "typed text in the log");
    assert!(log.contains("user_terminal"), "{log}");
}

/// A raw client on a Unix socket that stops reading: the server stops
/// taking the terminal's events, the ring drops the oldest, and the client
/// later gets a `dropped` count. Delivered plus dropped bytes are all the
/// output, and the sequence numbers have no gap. Another connection is
/// served meanwhile.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_slow_client_gets_a_dropped_count_and_others_are_not_touched() {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    let f = runtime(Options::default()).await;
    f.rt.set_workspace_limits(SessionWorkspaceLimits {
        terminal_buffer_bytes: 64 * 1024,
        ..SessionWorkspaceLimits::default()
    });
    let (server, raw) = tokio::net::UnixStream::pair().unwrap();
    support::serve_raw(&f.rt, loop_wire::socket::unix(server));
    let (read, mut write) = raw.into_split();
    let mut lines = BufReader::new(read).lines();
    let mut id = 0;
    let mut send = |method: &str, params: serde_json::Value| {
        id += 1;
        format!(
            "{}\n",
            json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params})
        )
    };
    // Read lines until the answer to request `want`.
    async fn answer(
        lines: &mut tokio::io::Lines<BufReader<tokio::net::unix::OwnedReadHalf>>,
        want: u64,
    ) -> serde_json::Value {
        loop {
            let line = lines.next_line().await.unwrap().unwrap();
            let value: serde_json::Value = serde_json::from_str(&line).unwrap();
            if value["id"] == want {
                return value["result"].clone();
            }
        }
    }
    use loop_protocol::rpc::methods;
    write
        .write_all(
            send(
                methods::INITIALIZE,
                json!({"protocol_version": 1, "client_info": {"name": "raw", "version": "0"}}),
            )
            .as_bytes(),
        )
        .await
        .unwrap();
    answer(&mut lines, 1).await;
    write
        .write_all(send(methods::SESSION_NEW, json!({"cwd": f.ws})).as_bytes())
        .await
        .unwrap();
    let session = answer(&mut lines, 2).await;
    const TOTAL: usize = 3_000_000;
    let command = format!("sleep 1; head -c {TOTAL} /dev/zero | tr '\\0' x; sleep 600");
    write
        .write_all(
            send(
                methods::SESSION_TERMINAL_OPEN,
                json!({"session": session, "cols": 80, "rows": 24, "command": command}),
            )
            .as_bytes(),
        )
        .await
        .unwrap();
    let terminal = answer(&mut lines, 3).await["terminal"]
        .as_str()
        .unwrap()
        .to_owned();
    write
        .write_all(
            send(
                methods::SESSION_TERMINAL_ATTACH,
                json!({"session": session, "terminal": terminal, "after_seq": 0}),
            )
            .as_bytes(),
        )
        .await
        .unwrap();
    answer(&mut lines, 4).await;
    // Stop reading while the burst runs. Another connection is served.
    let other = client(&f.rt, None).await;
    let started = std::time::Instant::now();
    tokio::time::sleep(Duration::from_secs(4)).await;
    let mine = new_session(&other, &f.ws).await;
    sw(&other)
        .info(SessionParams { session: mine })
        .await
        .unwrap();
    assert!(started.elapsed() < Duration::from_secs(10));
    // Now read everything the stream gives.
    let (mut data, mut dropped, mut last) = (0usize, 0u64, 0u64);
    let mut saw_drop = false;
    tokio::time::timeout(Duration::from_secs(30), async {
        while data + (dropped as usize) < TOTAL {
            let line = lines.next_line().await.unwrap().unwrap();
            let value: serde_json::Value = serde_json::from_str(&line).unwrap();
            if value["method"] != methods::SESSION_TERMINAL_OUTPUT {
                continue;
            }
            let params = &value["params"];
            let seq = params["seq"].as_u64().unwrap();
            if let Some(bytes) = params["dropped"].as_u64() {
                assert!(seq > last, "dropped seq {seq} after {last}");
                dropped += bytes;
                saw_drop = true;
            } else {
                assert_eq!(seq, last + 1, "a gap without a dropped event");
                if let Some(text) = params["data"].as_str() {
                    data += base64::decode(text).unwrap().len();
                }
            }
            last = seq;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("got {data} + {dropped} of {TOTAL}"));
    assert!(
        saw_drop,
        "a slow client must lose output, not the connection"
    );
    assert_eq!(data + dropped as usize, TOTAL);
    // The connection still works.
    write
        .write_all(
            send(
                methods::SESSION_TERMINAL_CLOSE,
                json!({"session": session, "terminal": terminal}),
            )
            .as_bytes(),
        )
        .await
        .unwrap();
    answer(&mut lines, 5).await;
    finish(f).await;
}
