//! Detached default-owner startup, using the built CLI with isolated state.
use loop_cli::{
    runtime_launch::{self, Target},
    surface_client::RemoteHost,
};
use loop_protocol::{AgentBackend, SessionSpec};
use std::{path::Path, sync::Arc, time::Duration};

#[tokio::test]
async fn auto_start_and_second_client_reuse_one_owner() {
    tokio::time::timeout(Duration::from_secs(45), async {
        let dir = tempfile::Builder::new()
            .prefix("s36")
            .tempdir_in("/tmp")
            .unwrap();
        let agent = dir.path().join("agent");
        let socket = loop_cli::config::paths::runtime_socket_path(&agent).unwrap();
        let one = runtime_launch::start_and_connect(
            Path::new(env!("CARGO_BIN_EXE_loop")),
            &agent,
            dir.path(),
            &socket,
            Arc::new(RemoteHost::default()),
        )
        .await
        .unwrap();
        let status = one.status().await.unwrap();
        let result = async {
            let discovered = runtime_launch::discover(&agent).await.unwrap();
            assert_eq!(discovered, vec![Target::Unix(socket.clone())]);
            let two =
                runtime_launch::connect(&discovered[0], None, Arc::new(RemoteHost::default()))
                    .await
                    .unwrap();
            assert_eq!(two.status().await.unwrap().pid, status.pid);
            let second_agent = dir.path().join("other-agent");
            let second_socket =
                loop_cli::config::paths::runtime_socket_path(&second_agent).unwrap();
            let other = runtime_launch::start_and_connect(
                Path::new(env!("CARGO_BIN_EXE_loop")),
                &second_agent,
                dir.path(),
                &second_socket,
                Arc::new(RemoteHost::default()),
            )
            .await
            .unwrap();
            let mut inventory = discovered.clone();
            inventory.extend(runtime_launch::discover(&second_agent).await.unwrap());
            let chosen = runtime_launch::select(&inventory, |_| Ok(1))
                .unwrap()
                .unwrap();
            let picked = runtime_launch::connect(&chosen, None, Arc::new(RemoteHost::default()))
                .await
                .unwrap();
            assert_eq!(
                picked.status().await.unwrap().pid,
                other.status().await.unwrap().pid
            );
            assert_ne!(picked.status().await.unwrap().pid, status.pid);
            other.stop_empty().await.unwrap();
            let session = one
                .new_session(SessionSpec {
                    cwd: Some(dir.path().canonicalize().unwrap()),
                    env: Some(std::collections::BTreeMap::from([(
                        "S36_SENTINEL".into(),
                        "S36_SENTINEL_DO_NOT_PERSIST".into(),
                    )])),
                    ..Default::default()
                })
                .await
                .unwrap();
            two.attach(&session, 0).await.unwrap();
            two.detach(&session).await.unwrap();
            assert_eq!(one.status().await.unwrap().loaded, 1);
        }
        .await;
        one.stop().await.unwrap();
        tokio::time::timeout(Duration::from_secs(10), async {
            while socket.exists() {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .unwrap();
        fn assert_secret_absent(dir: &Path) {
            for entry in std::fs::read_dir(dir).unwrap() {
                let path = entry.unwrap().path();
                if path.is_dir() {
                    assert_secret_absent(&path);
                } else if path.is_file() {
                    let bytes = std::fs::read(&path).unwrap();
                    assert!(
                        !bytes
                            .windows(b"S36_SENTINEL_DO_NOT_PERSIST".len())
                            .any(|w| w == b"S36_SENTINEL_DO_NOT_PERSIST"),
                        "secret persisted in {}",
                        path.display()
                    );
                }
            }
        }
        assert_secret_absent(&agent);
        result
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn start_failure_is_returned_for_the_callers_in_process_fallback() {
    tokio::time::timeout(Duration::from_secs(5), async {
        let dir = tempfile::tempdir().unwrap();
        let error = runtime_launch::start_and_connect(
            Path::new("/does/not/exist/loop"),
            dir.path(),
            dir.path(),
            &dir.path().join("runtime.sock"),
            Arc::new(RemoteHost::default()),
        )
        .await;
        assert!(error.is_err());
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn sentinel_environment_never_appears_in_wire_logs_or_frame_debug() {
    tokio::time::timeout(Duration::from_secs(5), async {
        use std::io::Write;
        #[derive(Clone)]
        struct Capture(Arc<std::sync::Mutex<Vec<u8>>>);
        impl Write for Capture {
            fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> { self.0.lock().unwrap().extend_from_slice(bytes); Ok(bytes.len()) }
            fn flush(&mut self) -> std::io::Result<()> { Ok(()) }
        }
        let capture = Capture(Arc::new(std::sync::Mutex::new(vec![])));
        let writer = capture.clone();
        let subscriber = tracing_subscriber::fmt().with_ansi(false).with_max_level(tracing::Level::TRACE).with_writer(move || writer.clone()).finish();
        let _guard = tracing::subscriber::set_default(subscriber);
        let (server, client) = loop_wire::transport::memory();
        let (connection, mut incoming) = loop_wire::connection::Connection::new(server);
        let sentinel = "S36_SENTINEL_DO_NOT_LOG";
        client.outgoing.send(format!("{{bad-json env SECRET={sentinel}" )).unwrap();
        client.outgoing.send(serde_json::json!({"jsonrpc":"2.0","id":1,"method":"session/new", "params":{"env":{"SECRET":sentinel}}}).to_string()).unwrap();
        let frame = incoming.recv().await.unwrap();
        tracing::debug!(?frame, "received frame");
        connection.close();
        let logs = String::from_utf8(capture.0.lock().unwrap().clone()).unwrap();
        assert!(!logs.contains(sentinel), "wire logs exposed the sentinel");
        assert!(!format!("{frame:?}").contains(sentinel));
    }).await.unwrap();
}

#[tokio::test]
async fn four_concurrent_launchers_connect_to_one_owner() {
    tokio::time::timeout(Duration::from_secs(45), async {
        let dir = tempfile::Builder::new()
            .prefix("s36race")
            .tempdir_in("/tmp")
            .unwrap();
        let agent = dir.path().join("agent");
        let socket = loop_cli::config::paths::runtime_socket_path(&agent).unwrap();
        let mut starts = tokio::task::JoinSet::new();
        for _ in 0..4 {
            let (agent, socket, cwd) = (agent.clone(), socket.clone(), dir.path().to_owned());
            starts.spawn(async move {
                runtime_launch::start_and_connect(
                    Path::new(env!("CARGO_BIN_EXE_loop")),
                    &agent,
                    &cwd,
                    &socket,
                    Arc::new(RemoteHost::default()),
                )
                .await
            });
        }
        let mut clients = vec![];
        while let Some(client) = starts.join_next().await {
            clients.push(client.unwrap().unwrap());
        }
        let pid = clients[0].status().await.unwrap().pid;
        for client in &clients {
            assert_eq!(client.status().await.unwrap().pid, pid);
        }
        assert!(clients[0].status().await.unwrap().auto_started);
        clients[0].stop().await.unwrap();
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn a_held_runtime_lock_waits_for_its_owner_without_spawning() {
    tokio::time::timeout(Duration::from_secs(45), async {
        let dir = tempfile::Builder::new()
            .prefix("s36lock")
            .tempdir_in("/tmp")
            .unwrap();
        let agent = dir.path().join("agent");
        let socket = loop_cli::config::paths::runtime_socket_path(&agent).unwrap();
        let lock = loop_app_core::runtime_instance::RuntimeInstanceLock::acquire(&agent).unwrap();
        let waiting = runtime_launch::start_and_connect(
            Path::new("/must/not/be/spawned"),
            &agent,
            dir.path(),
            &socket,
            Arc::new(RemoteHost::default()),
        );
        tokio::pin!(waiting);
        std::future::poll_fn(|cx| {
            assert!(std::future::Future::poll(waiting.as_mut(), cx).is_pending());
            std::task::Poll::Ready(())
        })
        .await;
        drop(lock);
        let owner = runtime_launch::spawn(
            Path::new(env!("CARGO_BIN_EXE_loop")),
            &agent,
            dir.path(),
            &socket,
        )
        .unwrap();
        let client = waiting.await.unwrap();
        assert_eq!(client.status().await.unwrap().pid, owner.id());
        client.stop().await.unwrap();
        std::thread::spawn(move || {
            let mut owner = owner;
            let _ = owner.wait();
        });
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn discovery_ignores_stale_sockets_and_bounds_a_silent_peer() {
    tokio::time::timeout(Duration::from_secs(10), async {
        let dir = tempfile::Builder::new()
            .prefix("s36discovery")
            .tempdir_in("/tmp")
            .unwrap();
        let socket = loop_cli::config::paths::runtime_socket_path(dir.path()).unwrap();
        // A stale socket file: whether the probe is refused, or reaches a
        // listener another thread's fork still holds for a moment and is then
        // closed, there is no Runtime here.
        let listener = tokio::net::UnixListener::bind(&socket).unwrap();
        drop(listener);
        assert!(runtime_launch::discover(dir.path())
            .await
            .unwrap()
            .is_empty());
        std::fs::remove_file(&socket).unwrap();
        let listener = tokio::net::UnixListener::bind(&socket).unwrap();
        let agent = dir.path().to_owned();
        let discovery = tokio::spawn(async move { runtime_launch::discover(&agent).await });
        let (_probe, _) = listener.accept().await.unwrap();
        let (_peer, _) = listener.accept().await.unwrap();
        tokio::time::pause();
        tokio::time::advance(Duration::from_secs(2)).await;
        let error = discovery.await.unwrap().unwrap_err();
        assert!(error.to_string().contains("discovery handshake timed out"));
        tokio::time::resume();
        assert!(
            socket.exists(),
            "discovery must not replace a foreign socket"
        );
    })
    .await
    .unwrap();
}

/// A peer that accepts the probe and the handshake, then closes both without
/// a reply (a Runtime that exited just then) is "no Runtime", not an error.
#[tokio::test]
async fn discovery_treats_a_peer_that_closes_before_replying_as_no_runtime() {
    tokio::time::timeout(Duration::from_secs(10), async {
        let dir = tempfile::Builder::new()
            .prefix("s36discclose")
            .tempdir_in("/tmp")
            .unwrap();
        let socket = loop_cli::config::paths::runtime_socket_path(dir.path()).unwrap();
        let listener = tokio::net::UnixListener::bind(&socket).unwrap();
        let agent = dir.path().to_owned();
        let discovery = tokio::spawn(async move { runtime_launch::discover(&agent).await });
        let (probe, _) = listener.accept().await.unwrap();
        drop(probe);
        let (handshake, _) = listener.accept().await.unwrap();
        drop(handshake);
        assert!(discovery.await.unwrap().unwrap().is_empty());
    })
    .await
    .unwrap();
}

/// A peer that answers the handshake with an error is not "no Runtime": the
/// user must see the error.
#[tokio::test]
async fn discovery_reports_a_peer_that_answers_with_a_protocol_error() {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    tokio::time::timeout(Duration::from_secs(10), async {
        let dir = tempfile::Builder::new()
            .prefix("s36discerror")
            .tempdir_in("/tmp")
            .unwrap();
        let socket = loop_cli::config::paths::runtime_socket_path(dir.path()).unwrap();
        let listener = tokio::net::UnixListener::bind(&socket).unwrap();
        let agent = dir.path().to_owned();
        let discovery = tokio::spawn(async move { runtime_launch::discover(&agent).await });
        let (_probe, _) = listener.accept().await.unwrap();
        let (peer, _) = listener.accept().await.unwrap();
        let mut peer = BufReader::new(peer);
        let mut request = String::new();
        peer.read_line(&mut request).await.unwrap();
        let id = serde_json::from_str::<serde_json::Value>(&request).unwrap()["id"].clone();
        let reply = serde_json::json!({
            "jsonrpc": "2.0",
            "id": id,
            "error": { "code": -32600, "message": "not a Loop Runtime" },
        });
        peer.get_mut()
            .write_all(format!("{reply}\n").as_bytes())
            .await
            .unwrap();
        let error = discovery.await.unwrap().unwrap_err();
        assert!(error.to_string().contains("not a Loop Runtime"), "{error}");
    })
    .await
    .unwrap();
}
