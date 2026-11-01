//! Signal future for the Runtime command's graceful shutdown path.

/// Wait for SIGINT or SIGTERM. Registration failures are returned to the caller.
/// Platforms without Unix signals report Unsupported.
pub async fn shutdown_signal() -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{signal, SignalKind};
        let mut interrupt = signal(SignalKind::interrupt())?;
        let mut terminate = signal(SignalKind::terminate())?;
        tokio::select! {
            _ = interrupt.recv() => {},
            _ = terminate.recv() => {},
        }
        Ok(())
    }
    #[cfg(not(unix))]
    {
        Err(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "Runtime shutdown signals are unsupported on this platform",
        ))
    }
}
