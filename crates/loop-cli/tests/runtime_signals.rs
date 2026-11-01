#![cfg(unix)]

use std::{future::Future, io::Write, task::Poll, time::Duration};
use tokio::io::{AsyncBufReadExt, BufReader};

#[tokio::test]
async fn runtime_shutdown_resolves_on_either_termination_signal() {
    if std::env::var_os("LOOP_TEST_RUNTIME_SIGNAL_CHILD").is_some() {
        let mut signal = Box::pin(loop_cli::runtime_signals::shutdown_signal());
        // Poll once to register both handlers before the parent sends a signal.
        std::future::poll_fn(|cx| {
            assert!(signal.as_mut().poll(cx).is_pending());
            Poll::Ready(())
        })
        .await;
        println!("SIGNALS_READY");
        std::io::stdout().flush().unwrap();
        tokio::time::timeout(Duration::from_secs(10), signal)
            .await
            .unwrap()
            .unwrap();
        return;
    }
    for signal in ["-INT", "-TERM"] {
        let mut child = tokio::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "runtime_shutdown_resolves_on_either_termination_signal",
                "--nocapture",
            ])
            .env("LOOP_TEST_RUNTIME_SIGNAL_CHILD", "1")
            .stdout(std::process::Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        let mut lines = BufReader::new(child.stdout.take().unwrap()).lines();
        tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                let line = lines
                    .next_line()
                    .await
                    .unwrap()
                    .expect("child exited before ready");
                if line == "SIGNALS_READY" {
                    break;
                }
            }
        })
        .await
        .unwrap();
        assert!(std::process::Command::new("kill")
            .args([signal, &child.id().unwrap().to_string()])
            .status()
            .unwrap()
            .success());
        let status = tokio::time::timeout(Duration::from_secs(10), child.wait())
            .await
            .unwrap()
            .unwrap();
        assert!(status.success(), "{signal}: {status}");
    }
}
