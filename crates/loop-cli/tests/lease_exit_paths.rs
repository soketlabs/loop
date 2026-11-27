//! In-process exits release their leases; default TUI exits leave the Runtime
//! owning its Session until it stops. Each test runs the real binary against a fake
//! OpenAI-compatible model with an isolated agent directory.

use loop_agent::harness::session::sqlite::SqliteSessionStore;
use loop_test_support::{FakeHttpServer, FakeResponse, RecordedRequest};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

const WAIT: Duration = Duration::from_secs(30);

/// The screen text once it contains `needle`, or after 5 s. The drain thread
/// can still hold the last bytes when the child has already exited, so a
/// read right after `wait()` can miss the final message.
async fn screen_with(screen: &std::sync::Arc<std::sync::Mutex<Vec<u8>>>, needle: &str) -> String {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        let shown = String::from_utf8_lossy(&screen.lock().unwrap()).into_owned();
        if shown.contains(needle) || tokio::time::Instant::now() >= deadline {
            return shown;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}
const PROVIDER_ENV: &[&str] = &[
    "SOKET_API_KEY",
    "TENSORSTUDIO_API_KEY",
    "LOOP_API_KEY",
    "OPENROUTER_API_KEY",
    "OPENAI_API_KEY",
    "ANTHROPIC_API_KEY",
    "GEMINI_API_KEY",
    "XDG_CONFIG_HOME",
    "XDG_DATA_HOME",
    "XDG_STATE_HOME",
    "XDG_CACHE_HOME",
];

fn sse(chunks: &[Value]) -> String {
    let mut body = String::new();
    for chunk in chunks {
        body += &format!(
            "data: {}\n\n",
            json!({"id":"r","object":"chat.completion.chunk","choices":[chunk]})
        );
    }
    body + "data: [DONE]\n\n"
}
fn text_reply(text: &str) -> FakeResponse {
    FakeResponse::new(
        200,
        "text/event-stream",
        sse(&[
            json!({"index":0,"delta":{"role":"assistant","content":text},"finish_reason":null}),
            json!({"index":0,"delta":{},"finish_reason":"stop"}),
        ]),
    )
}

/// How the fake model answers a chat request.
#[derive(Clone, Copy)]
enum Model {
    Answer,
    Fail,
    /// Never finishes within the test.
    Hang,
}

struct Fixture {
    _dir: tempfile::TempDir,
    agent: PathBuf,
    work: PathBuf,
    home: PathBuf,
    model: FakeHttpServer,
}
impl Fixture {
    fn new(reply: Model) -> Self {
        let dir = tempfile::Builder::new()
            .prefix("lle")
            .tempdir_in("/tmp")
            .unwrap();
        let agent = dir.path().join("agent");
        let work = dir.path().join("work");
        let home = dir.path().join("home");
        for d in [&agent, &work, &home] {
            std::fs::create_dir_all(d).unwrap();
        }
        let model = FakeHttpServer::start(move |r: &RecordedRequest| {
            match (r.method.as_str(), r.path.as_str()) {
                ("GET", "/v1/models") => FakeResponse::json(r#"{"data":[{"id":"fake-model"}]}"#),
                ("POST", "/v1/chat/completions") => match reply {
                    Model::Answer => text_reply("hello"),
                    Model::Fail => FakeResponse::new(
                        400,
                        "application/json",
                        r#"{"error":{"message":"bad request"}}"#,
                    ),
                    Model::Hang => text_reply("late").with_body_delay(Duration::from_secs(120)),
                },
                _ => FakeResponse::new(404, "text/plain", "not found"),
            }
        });
        std::fs::write(
            agent.join("models.json"),
            json!({"providers":[{"id":"fake","baseUrl":format!("{}/v1", model.base_url()),
                "models":["fake-model"]}]})
            .to_string(),
        )
        .unwrap();
        std::fs::write(
            agent.join("settings.json"),
            json!({"defaultProvider":"fake","defaultModel":"fake-model",
                "defaultProjectTrust":"always",
                "providers":[{"id":"fake","name":"Fake",
                    "baseUrl":format!("{}/v1", model.base_url())}]})
            .to_string(),
        )
        .unwrap();
        Self {
            _dir: dir,
            agent,
            work,
            home,
            model,
        }
    }
    fn command(&self, args: &[&str]) -> std::process::Command {
        let mut cmd = std::process::Command::new(env!("CARGO_BIN_EXE_loop"));
        cmd.args(args)
            .env("LOOP_CODING_AGENT_DIR", &self.agent)
            .env("LOOP_CODING_AGENT_SESSION_DIR", self.agent.join("sessions"))
            .env("HOME", &self.home)
            .env("TERM", "xterm-256color")
            .env_remove("LOOP_DEBUG")
            .env_remove("RUST_LOG")
            .current_dir(&self.work);
        for key in PROVIDER_ENV {
            cmd.env_remove(key);
        }
        cmd
    }
    fn db(&self) -> PathBuf {
        self.agent.join("sessions/sessions.db")
    }
    /// `(session, owner, pid)` of every lease row.
    async fn leases(&self) -> Vec<(String, String, u32)> {
        leases(&self.db()).await
    }
    async fn assert_runtime_lease_survives_detach(&self, surface_pid: u32) {
        let leases = self.leases().await;
        assert_eq!(leases.len(), 1, "{leases:?}");
        assert_eq!(leases[0].1, "runtime");
        assert_ne!(leases[0].2, surface_pid);
        let stopped = output(self.command(&["runtime", "stop"])).await;
        assert!(stopped.status.success(), "{stopped:?}");
        assert_eq!(self.leases().await, []);
    }
    /// Wait until the process has written its Session's lease.
    async fn wait_for_lease(&self) {
        let found = tokio::time::timeout(WAIT, async {
            // The process may still be creating the database; never migrate
            // it from here at the same time.
            while !lease_table_has_rows(&self.db()) {
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        })
        .await;
        assert!(found.is_ok(), "the process never took a lease");
    }
    async fn wait_for_model_call(&self) {
        let called = tokio::time::timeout(WAIT, async {
            while !self
                .model
                .requests()
                .iter()
                .any(|r| r.path == "/v1/chat/completions")
            {
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        })
        .await;
        assert!(called.is_ok(), "the model was never called");
    }
}

/// A read-only look that never creates or migrates the database.
fn lease_table_has_rows(db: &Path) -> bool {
    let Ok(conn) =
        rusqlite::Connection::open_with_flags(db, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
    else {
        return false;
    };
    conn.query_row("SELECT COUNT(*) FROM session_leases", [], |row| {
        row.get::<_, i64>(0)
    })
    .is_ok_and(|n| n > 0)
}

async fn leases(db: &Path) -> Vec<(String, String, u32)> {
    if !db.exists() {
        return Vec::new();
    }
    let store = SqliteSessionStore::open(db).unwrap();
    let mut rows = Vec::new();
    for meta in loop_agent::harness::session::SessionStore::list(&store, None)
        .await
        .unwrap()
    {
        if let Some(lease) = store.lease(&meta.id).await.unwrap() {
            rows.push((meta.id, lease.identity.owner, lease.identity.pid));
        }
    }
    rows
}

async fn output(mut cmd: std::process::Command) -> std::process::Output {
    cmd.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let child = tokio::process::Command::from(cmd)
        .kill_on_drop(true)
        .output();
    tokio::time::timeout(WAIT, child)
        .await
        .expect("loop did not exit")
        .unwrap()
}

fn signal(pid: u32, name: &str) {
    assert!(std::process::Command::new("kill")
        .args([name, &pid.to_string()])
        .status()
        .unwrap()
        .success());
}

async fn bounded(test: impl std::future::Future<Output = ()>) {
    tokio::time::timeout(Duration::from_secs(90), test)
        .await
        .expect("test timed out");
}

#[tokio::test]
async fn print_that_succeeds_leaves_no_lease() {
    bounded(async {
        let fx = Fixture::new(Model::Answer);
        let out = output(fx.command(&["--print", "hi"])).await;
        assert!(out.status.success(), "{out:?}");
        assert!(String::from_utf8_lossy(&out.stdout).contains("hello"));
        assert_eq!(fx.leases().await, [], "{out:?}");
        assert!(
            !fx.agent.join("run/runtime.sock").exists(),
            "--print must stay in-process"
        );
        // The shutdown barrier ran before release: the turn is stored.
        use loop_agent::harness::session::SessionStore;
        let store = SqliteSessionStore::open(fx.db()).unwrap();
        let sessions = store.list(None).await.unwrap();
        assert_eq!(sessions.len(), 1);
        let entries = store
            .load(&sessions[0].id)
            .await
            .unwrap()
            .read_entries(None)
            .await
            .unwrap();
        let text = serde_json::to_string(&entries).unwrap();
        assert!(text.contains("hello") && text.contains("hi"), "{text}");
    })
    .await
}

#[tokio::test]

async fn print_that_fails_leaves_no_lease() {
    bounded(async {
        let fx = Fixture::new(Model::Fail);
        let out = output(fx.command(&["--print", "hi"])).await;
        assert_eq!(out.status.code(), Some(1), "{out:?}");
        assert!(!fx.model.requests().is_empty());
        assert_eq!(fx.leases().await, [], "{out:?}");
    })
    .await
}

#[tokio::test]
async fn an_error_after_startup_leaves_no_lease() {
    bounded(async {
        let fx = Fixture::new(Model::Answer);
        // Refused only after the Session was opened.
        let out = output(fx.command(&["--serve-mcp", "--mcp-host", "0.0.0.0"])).await;
        assert_eq!(out.status.code(), Some(1), "{out:?}");
        assert!(String::from_utf8_lossy(&out.stderr).contains("--mcp-token"));
        assert!(fx.db().exists(), "the Session was opened");
        assert_eq!(fx.leases().await, [], "{out:?}");
    })
    .await
}

async fn print_ended_by(name: &str) {
    let fx = Fixture::new(Model::Hang);
    let mut cmd = fx.command(&["--print", "hi"]);
    cmd.stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped());
    let mut child = tokio::process::Command::from(cmd)
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    fx.wait_for_lease().await;
    fx.wait_for_model_call().await;
    signal(child.id().unwrap(), name);
    let status = tokio::time::timeout(WAIT, child.wait())
        .await
        .expect("loop ignored the signal")
        .unwrap();
    assert!(!status.success(), "{name}: {status}");
    assert_eq!(fx.leases().await, [], "{name}: {status}");
}

#[tokio::test]

async fn print_ended_by_sigterm_leaves_no_lease() {
    bounded(print_ended_by("-TERM")).await
}

#[tokio::test]

async fn print_ended_by_sighup_leaves_no_lease() {
    bounded(print_ended_by("-HUP")).await
}

/// A pseudo-terminal for the TUI: the child gets the slave side as its
/// controlling terminal; the test writes keys to and drains the master.
struct Pty {
    master: std::fs::File,
    slave: std::os::fd::OwnedFd,
}
impl Pty {
    fn open() -> Self {
        use std::os::fd::FromRawFd;
        let (mut master, mut slave) = (-1, -1);
        let mut size = libc::winsize {
            ws_row: 40,
            ws_col: 120,
            ws_xpixel: 0,
            ws_ypixel: 0,
        };
        // SAFETY: openpty writes two descriptors it opened; we own them after.
        let rc = unsafe {
            libc::openpty(
                &mut master,
                &mut slave,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                // A raw pointer: Linux takes `*const winsize`, macOS `*mut`.
                &raw mut size,
            )
        };
        assert_eq!(rc, 0, "openpty: {}", std::io::Error::last_os_error());
        // SAFETY: both descriptors are open and owned by nothing else.
        unsafe {
            Self {
                master: std::fs::File::from_raw_fd(master),
                slave: std::os::fd::OwnedFd::from_raw_fd(slave),
            }
        }
    }
    fn attach(&self, cmd: &mut std::process::Command) {
        use std::os::unix::process::CommandExt;
        for (i, stream) in [0, 1, 2].into_iter().enumerate() {
            let fd = self.slave.try_clone().unwrap();
            match i {
                0 => cmd.stdin(Stdio::from(fd)),
                1 => cmd.stdout(Stdio::from(fd)),
                _ => cmd.stderr(Stdio::from(fd)),
            };
            let _ = stream;
        }
        // SAFETY: only async-signal-safe calls between fork and exec.
        unsafe {
            cmd.pre_exec(|| {
                libc::setsid();
                libc::ioctl(0, libc::TIOCSCTTY as _, 0);
                Ok(())
            });
        }
    }
    /// Copy everything the TUI draws into a buffer, so it never blocks, and
    /// answer its cursor position queries like a terminal would.
    fn drain(&self) -> std::sync::Arc<std::sync::Mutex<Vec<u8>>> {
        use std::io::{Read, Write};
        let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let sink = seen.clone();
        let mut master = self.master.try_clone().unwrap();
        let mut reply = self.master.try_clone().unwrap();
        std::thread::spawn(move || {
            let mut buf = [0u8; 4096];
            while let Ok(n) = master.read(&mut buf) {
                if n == 0 {
                    break;
                }
                let chunk = &buf[..n];
                for _ in 0..chunk.windows(4).filter(|w| w == b"\x1b[6n").count() {
                    let _ = reply.write_all(b"\x1b[1;1R");
                }
                sink.lock().unwrap().extend_from_slice(chunk);
            }
        });
        seen
    }
    fn keys(&self, bytes: &[u8]) {
        use std::io::Write;
        (&self.master).write_all(bytes).unwrap();
        (&self.master).flush().unwrap();
    }
}

#[tokio::test]

async fn quitting_the_tui_keeps_the_runtime_lease_until_the_owner_stops() {
    bounded(async {
        let fx = Fixture::new(Model::Answer);
        let pty = Pty::open();
        let mut cmd = fx.command(&[]);
        pty.attach(&mut cmd);
        let mut child = tokio::process::Command::from(cmd)
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        let surface_pid = child.id().unwrap();
        let screen = pty.drain();
        fx.wait_for_lease().await;
        // Let the TUI take the terminal before sending keys.
        tokio::time::sleep(Duration::from_millis(1500)).await;
        let status = tokio::time::timeout(WAIT, async {
            loop {
                pty.keys(b"\x03");
                tokio::time::sleep(Duration::from_millis(150)).await;
                pty.keys(b"\x03");
                if let Ok(Ok(status)) =
                    tokio::time::timeout(Duration::from_secs(2), child.wait()).await
                {
                    return status;
                }
            }
        })
        .await
        .unwrap_or_else(|_| {
            panic!(
                "the TUI did not quit on Ctrl+C twice:\n{}",
                String::from_utf8_lossy(&screen.lock().unwrap())
            )
        });
        let shown = String::from_utf8_lossy(&screen.lock().unwrap()).into_owned();
        assert!(status.success(), "{status}\n{shown}");
        fx.assert_runtime_lease_survives_detach(surface_pid).await;
    })
    .await
}

#[tokio::test]

async fn a_tui_ended_by_sighup_keeps_the_runtime_lease_until_the_owner_stops() {
    bounded(async {
        let fx = Fixture::new(Model::Answer);
        let pty = Pty::open();
        let mut cmd = fx.command(&[]);
        pty.attach(&mut cmd);
        let mut child = tokio::process::Command::from(cmd)
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        let surface_pid = child.id().unwrap();
        let _screen = pty.drain();
        fx.wait_for_lease().await;
        tokio::time::sleep(Duration::from_millis(1500)).await;
        signal(child.id().unwrap(), "-HUP");
        tokio::time::timeout(WAIT, child.wait())
            .await
            .expect("the TUI ignored SIGHUP")
            .unwrap();
        fx.assert_runtime_lease_survives_detach(surface_pid).await;
    })
    .await
}

#[tokio::test]
async fn a_failed_runtime_start_falls_back_and_shows_the_warning_inside_the_tui() {
    bounded(async {
        let fx = Fixture::new(Model::Answer);
        // A directory where the socket belongs keeps the owner from binding.
        let socket = loop_cli::config::paths::runtime_socket_path(&fx.agent).unwrap();
        std::fs::create_dir_all(&socket).unwrap();
        let pty = Pty::open();
        let mut cmd = fx.command(&[]);
        pty.attach(&mut cmd);
        let mut child = tokio::process::Command::from(cmd)
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        let screen = pty.drain();
        fx.wait_for_lease().await;
        let leases = fx.leases().await;
        assert_eq!(leases.len(), 1);
        assert_eq!(leases[0].1, "standalone");
        tokio::time::timeout(WAIT, async {
            loop {
                if String::from_utf8_lossy(&screen.lock().unwrap()).contains("/help for commands") {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("fallback TUI did not become ready");
        pty.keys(b"\x04");
        assert!(tokio::time::timeout(WAIT, child.wait())
            .await
            .unwrap()
            .unwrap()
            .success());
        assert_eq!(fx.leases().await, []);
        assert!(String::from_utf8_lossy(&screen.lock().unwrap())
            .contains("using in-process mode for this run"));
    })
    .await;
}

#[tokio::test]
async fn a_new_terminal_loop_sweeps_rows_of_exited_processes() {
    bounded(async {
        use loop_agent::harness::session::sqlite::{LeaseEnvironment, LeaseIdentity, PidLiveness};
        struct LongAgo;
        impl LeaseEnvironment for LongAgo {
            fn now_ms(&self) -> i64 {
                1
            }
            fn pid_liveness(&self, _: u32) -> PidLiveness {
                PidLiveness::Unknown
            }
        }
        let fx = Fixture::new(Model::Answer);
        std::fs::create_dir_all(fx.db().parent().unwrap()).unwrap();
        let old =
            SqliteSessionStore::with_environment(fx.db(), std::sync::Arc::new(LongAgo)).unwrap();
        let mut exited = std::process::Command::new("true").spawn().unwrap();
        let dead = exited.id();
        exited.wait().unwrap();
        let mut seeded = Vec::new();
        for pid in [dead, std::process::id()] {
            let id = loop_agent::harness::session::SessionStore::create(&old, None, None)
                .await
                .unwrap()
                .metadata()
                .id
                .clone();
            old.acquire_lease(
                &id,
                &LeaseIdentity {
                    owner: "standalone".into(),
                    pid,
                    generation: 1,
                },
            )
            .await
            .unwrap();
            seeded.push(id);
        }
        let out = output(fx.command(&["--print", "hi"])).await;
        assert!(out.status.success(), "{out:?}");
        let left: Vec<_> = fx.leases().await.into_iter().map(|(id, ..)| id).collect();
        assert_eq!(left, [seeded[1].clone()], "only the live owner's row stays");
    })
    .await
}

#[tokio::test]
async fn an_invalid_permission_mode_leaves_no_lease() {
    bounded(async {
        let fx = Fixture::new(Model::Answer);
        let out = output(fx.command(&["--print", "hi", "--permission-mode", "invalid"])).await;
        assert_eq!(out.status.code(), Some(1), "{out:?}");
        assert!(
            String::from_utf8_lossy(&out.stderr).contains("permission"),
            "{out:?}"
        );
        assert_eq!(fx.leases().await, [], "{out:?}");
    })
    .await
}

/// SIGTERM sent while `loop` may still be starting: whenever it lands, no
/// lease of the exited process stays.
#[tokio::test]
async fn a_signal_during_startup_leaves_no_lease() {
    bounded(async {
        for delay_ms in [0u64, 20, 60, 120, 250] {
            let fx = Fixture::new(Model::Hang);
            let mut cmd = fx.command(&["--print", "hi"]);
            cmd.stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null());
            let mut child = tokio::process::Command::from(cmd)
                .kill_on_drop(true)
                .spawn()
                .unwrap();
            // Wait for the database file: the process is opening its Session.
            let started = tokio::time::timeout(WAIT, async {
                while !fx.db().exists() {
                    tokio::time::sleep(Duration::from_millis(2)).await;
                }
            })
            .await;
            assert!(started.is_ok(), "{delay_ms} ms: no database");
            tokio::time::sleep(Duration::from_millis(delay_ms)).await;
            signal(child.id().unwrap(), "-TERM");
            tokio::time::timeout(WAIT, child.wait())
                .await
                .expect("loop ignored SIGTERM")
                .unwrap();
            assert_eq!(
                fx.leases().await,
                [],
                "{delay_ms} ms after the database appeared"
            );
        }
    })
    .await
}

#[tokio::test]
async fn a_stopped_or_incompatible_explicit_owner_is_an_error_never_a_fallback() {
    for (version, loaded) in [(env!("CARGO_PKG_VERSION"), 0), ("0.0.1", 0), ("0.0.1", 1)] {
        bounded(async {
            use loop_wire::connection::{Connection, Incoming};
            let fx = Fixture::new(Model::Answer);
            let socket = fx.agent.join("run/other.sock");
            loop_cli::config::paths::run_dir(&fx.agent).unwrap();
            let listener = loop_wire::socket::unix_listen(&socket).unwrap();
            let peer = tokio::spawn(async move {
                let (stream, _) = listener.accept().await.unwrap();
                let (connection, mut incoming) = Connection::new(loop_wire::socket::unix(stream));
                while let Some(message) = incoming.recv().await {
                    if let Incoming::Request { id, method, .. } = message {
                        if method == "initialize" {
                            connection.respond(id, Ok(json!({"protocol_version":1,"server_info":{"name":"loop","version":version},"instance":"native","capabilities":{"session_environment":true}})));
                        } else if method == "runtime/status" {
                            connection.respond(id, Ok(json!({"pid":1,"socket":"other.sock","generation":1,"loaded":loaded,"busy":0,"max_sessions":16,"max_active_turns":8,"auto_started":false})));
                            break;
                        } else {
                            connection.respond(id, Err((-32603, "Runtime stopped".into())));
                            break;
                        }
                    }
                }
            });
            let pty = Pty::open();
            let mut cmd = fx.command(&["--connect", socket.to_str().unwrap()]);
            pty.attach(&mut cmd);
            let mut child = tokio::process::Command::from(cmd).kill_on_drop(true).spawn().unwrap();
            let screen = pty.drain();
            let status = tokio::time::timeout(WAIT, child.wait()).await.unwrap().unwrap();
            assert!(!status.success(), "an explicit target must not fall back");
            let shown = screen_with(&screen, "loop: ").await;
            assert!(shown.contains("loop: "), "{shown}");
            assert!(!shown.contains("/help for commands"), "no in-process TUI: {shown}");
            assert_eq!(fx.leases().await, []);
            peer.await.unwrap();
            assert_eq!(fx.leases().await, []);
        }).await;
    }
}

#[tokio::test]
async fn explicit_foreign_socket_never_records_trust_in_the_surface_agent_dir() {
    bounded(async {
        let fx = Fixture::new(Model::Answer);
        let other = fx._dir.path().join("other-agent");
        std::fs::create_dir_all(&other).unwrap();
        for file in ["settings.json", "models.json"] {
            std::fs::copy(fx.agent.join(file), other.join(file)).unwrap();
        }
        let socket = other.join("run/runtime.sock");
        let mut command = fx.command(&["runtime", "start"]);
        command
            .env("LOOP_CODING_AGENT_DIR", &other)
            .env("LOOP_CODING_AGENT_SESSION_DIR", other.join("sessions"));
        let mut owner = tokio::process::Command::from(command)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        tokio::time::timeout(WAIT, async {
            while tokio::net::UnixStream::connect(&socket).await.is_err() {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .unwrap();
        let pty = Pty::open();
        let mut command = fx.command(&["--connect", socket.to_str().unwrap()]);
        pty.attach(&mut command);
        let mut surface = tokio::process::Command::from(command)
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        let screen = pty.drain();
        tokio::time::timeout(WAIT, async {
            while !String::from_utf8_lossy(&screen.lock().unwrap()).contains("/help for commands") {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .unwrap();
        assert!(
            !fx.agent.join("trust.json").exists(),
            "foreign connection wrote Surface trust"
        );
        assert!(
            !other.join("trust.json").exists(),
            "Runtime silently trusted the project"
        );
        pty.keys(b"\x04");
        assert!(tokio::time::timeout(WAIT, surface.wait())
            .await
            .unwrap()
            .unwrap()
            .success());
        let stopped =
            output(fx.command(&["runtime", "stop", "--socket", socket.to_str().unwrap()])).await;
        assert!(stopped.status.success(), "{stopped:?}");
        assert!(tokio::time::timeout(WAIT, owner.wait())
            .await
            .unwrap()
            .unwrap()
            .success());
    })
    .await;
}

#[tokio::test]
async fn an_exited_owner_shows_a_recovery_banner_and_the_tui_still_quits_cleanly() {
    bounded(async {
        let fx = Fixture::new(Model::Answer);
        let mut owner_command = fx.command(&["runtime", "start"]);
        owner_command
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        let mut owner = tokio::process::Command::from(owner_command)
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        let socket = fx.agent.join("run/runtime.sock");
        tokio::time::timeout(WAIT, async {
            while tokio::net::UnixStream::connect(&socket).await.is_err() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        let pty = Pty::open();
        let mut command = fx.command(&[]);
        pty.attach(&mut command);
        let mut surface = tokio::process::Command::from(command)
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        let screen = pty.drain();
        fx.wait_for_lease().await;
        tokio::time::timeout(WAIT, async {
            while !String::from_utf8_lossy(&screen.lock().unwrap()).contains("/help for commands") {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .unwrap();
        owner.kill().await.unwrap();
        owner.wait().await.unwrap();
        tokio::time::timeout(Duration::from_secs(5), async {
            while !String::from_utf8_lossy(&screen.lock().unwrap())
                .contains("Runtime connection lost")
            {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("no Runtime recovery banner appeared");
        pty.keys(b"\x04");
        assert!(tokio::time::timeout(WAIT, surface.wait())
            .await
            .unwrap()
            .unwrap()
            .success());
    })
    .await;
}

#[tokio::test]
async fn explicit_and_saved_subscription_agents_stay_in_process_while_signed_out() {
    bounded(async {
        for saved in [false, true] {
            let fx = Fixture::new(Model::Answer);
            if saved {
                let path = fx.agent.join("settings.json");
                let mut settings: Value =
                    serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
                settings["defaultProvider"] = json!("chatgpt");
                settings["defaultModel"] = json!("test-model");
                std::fs::write(path, serde_json::to_vec(&settings).unwrap()).unwrap();
            }
            let args: &[&str] = if saved {
                &[]
            } else {
                &["--provider", "chatgpt", "--model", "test-model"]
            };
            let pty = Pty::open();
            let mut command = fx.command(args);
            command.env("CODEX_HOME", fx.home.join(".codex"));
            pty.attach(&mut command);
            let mut surface = tokio::process::Command::from(command)
                .kill_on_drop(true)
                .spawn()
                .unwrap();
            let screen = pty.drain();
            tokio::select! {
                _ = fx.wait_for_lease() => {},
                status = surface.wait() => panic!("subscription startup exited: {status:?}"),
            }
            tokio::time::timeout(WAIT, async {
                while !String::from_utf8_lossy(&screen.lock().unwrap())
                    .contains("sign in to chatgpt")
                {
                    tokio::time::sleep(Duration::from_millis(20)).await;
                }
            })
            .await
            .unwrap();
            assert!(!fx.agent.join("run/runtime.sock").exists());
            assert_eq!(fx.leases().await[0].1, "standalone");
            signal(surface.id().unwrap(), "-TERM");
            assert!(!tokio::time::timeout(WAIT, surface.wait())
                .await
                .unwrap()
                .unwrap()
                .success());
            assert_eq!(fx.leases().await, []);
        }
    })
    .await;
}

#[tokio::test]
async fn an_explicit_websocket_target_without_a_token_exits_with_one_clear_error() {
    bounded(async {
        let fx = Fixture::new(Model::Answer);
        let pty = Pty::open();
        let mut cmd = fx.command(&["--connect", "ws://127.0.0.1:9"]);
        cmd.env_remove("LOOP_RUNTIME_TOKEN");
        pty.attach(&mut cmd);
        let mut child = tokio::process::Command::from(cmd).kill_on_drop(true).spawn().unwrap();
        let screen = pty.drain();
        let status = tokio::time::timeout(WAIT, child.wait()).await.unwrap().unwrap();
        assert!(!status.success());
        let shown = screen_with(&screen, "missing token").await;
        assert!(
            shown.contains("could not connect to ws://127.0.0.1:9: missing token, pass --token-file or set LOOP_RUNTIME_TOKEN"),
            "{shown}"
        );
        assert!(!shown.contains("in-process"), "{shown}");
        assert_eq!(fx.leases().await, []);
    })
    .await;
}
