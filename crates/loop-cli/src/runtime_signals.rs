//! Signal handling: the Runtime command's graceful shutdown path, and the
//! terminal `loop`'s release of its Session leases on SIGTERM and SIGHUP.

/// Wait for SIGINT or SIGTERM. Registration failures are returned to the caller.
pub async fn shutdown_signal() -> std::io::Result<()> {
    use tokio::signal::unix::{signal, SignalKind};
    let mut interrupt = signal(SignalKind::interrupt())?;
    let mut terminate = signal(SignalKind::terminate())?;
    tokio::select! {
        _ = interrupt.recv() => {},
        _ = terminate.recv() => {},
    }
    Ok(())
}

/// Where `main` puts the Surface's shutdown handle once startup has made it.
pub type ShutdownSlot =
    std::sync::Arc<std::sync::OnceLock<loop_app_core::runtime::SessionShutdown>>;

/// On SIGTERM or SIGHUP (and SIGINT when `interrupt`), end the Session work,
/// release its leases and exit with 128 + the signal number. Install it
/// before startup opens a Session: until `slot` holds the shutdown handle,
/// every standalone writer of the process is closed instead. With
/// `restore_terminal`, leave the TUI's raw mode first.
pub fn release_on_signals(
    slot: ShutdownSlot,
    interrupt: bool,
    restore_terminal: bool,
) -> std::io::Result<()> {
    use tokio::signal::unix::{signal, SignalKind};
    let mut terminate = signal(SignalKind::terminate())?;
    let mut hangup = signal(SignalKind::hangup())?;
    let mut interrupted = if interrupt {
        Some(signal(SignalKind::interrupt())?)
    } else {
        None
    };
    tokio::spawn(async move {
        let code = tokio::select! {
            _ = terminate.recv() => 143,
            _ = hangup.recv() => 129,
            _ = async {
                match interrupted.as_mut() {
                    Some(signal) => signal.recv().await,
                    None => std::future::pending().await,
                }
            } => 130,
        };
        if restore_terminal {
            restore();
        }
        let released = match slot.get() {
            Some(shutdown) => shutdown.run().await,
            // Still starting: no turn has run, so close every writer.
            None => loop_agent::harness::session::sqlite::close_standalone_writers()
                .await
                .map_err(|e| anyhow::anyhow!("could not release Session leases: {e}")),
        };
        if let Err(error) = released {
            eprintln!("loop: {error:#}");
        }
        std::process::exit(code);
    });
    Ok(())
}

/// Undo what the TUI set up, best effort: the terminal may already be gone.
fn restore() {
    use crossterm::cursor::Show;
    use crossterm::event::{DisableBracketedPaste, PopKeyboardEnhancementFlags};
    use crossterm::execute;
    let _ = crossterm::terminal::disable_raw_mode();
    let _ = execute!(
        std::io::stdout(),
        DisableBracketedPaste,
        PopKeyboardEnhancementFlags,
        Show
    );
}
