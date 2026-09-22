//! Debug session logging to `target/debug/logs`.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use tracing_subscriber::EnvFilter;

/// Whether debug mode is enabled via `--debug` or `LOOP_DEBUG=1`.
pub fn debug_enabled(cli_flag: bool) -> bool {
    if cli_flag {
        return true;
    }
    matches!(
        std::env::var("LOOP_DEBUG").as_deref(),
        Ok("1") | Ok("true") | Ok("TRUE") | Ok("yes") | Ok("YES")
    )
}

/// Initialize tracing. In debug mode, writes session logs under `cwd/target/debug/logs`.
///
/// Interactive TUI mode writes **file only** (stderr would corrupt the UI). Non-interactive
/// mode tees to stderr as well.
pub fn init_tracing(debug: bool, cwd: &Path, interactive: bool) -> anyhow::Result<Option<PathBuf>> {
    let default_filter = if debug {
        "info,loop_agent=debug,loop_ai=debug,loop_cli=debug,loop_app_core=debug,loop_mcp=debug"
    } else {
        "warn"
    };
    let filter =
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(default_filter));

    if !debug {
        tracing_subscriber::fmt()
            .with_env_filter(filter)
            .with_writer(io::stderr)
            .with_target(true)
            .init();
        return Ok(None);
    }

    let log_dir = cwd.join("target").join("debug").join("logs");
    fs::create_dir_all(&log_dir)?;
    let ts = chrono::Local::now().format("%Y%m%d-%H%M%S");
    let path = log_dir.join(format!("loop-{ts}.log"));
    let file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)?;

    let writer: Box<dyn Write + Send> = if interactive {
        Box::new(file)
    } else {
        Box::new(TeeWriter {
            file: Mutex::new(file),
        })
    };

    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(Mutex::new(writer))
        .with_ansi(false)
        .with_target(true)
        .with_thread_ids(true)
        .init();

    tracing::info!(
        path = %path.display(),
        interactive,
        "debug logging enabled"
    );
    Ok(Some(path))
}

/// Append a raw session note to an existing debug log file (best-effort).
pub fn append_note(path: &Path, note: &str) {
    let Ok(mut f) = OpenOptions::new().create(true).append(true).open(path) else {
        return;
    };
    let ts = chrono::Local::now().format("%Y-%m-%d %H:%M:%S%.3f");
    let _ = writeln!(f, "{ts}  {note}");
}

struct TeeWriter {
    file: Mutex<File>,
}

impl Write for TeeWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let _ = io::stderr().write_all(buf);
        self.file
            .lock()
            .map_err(|_| io::Error::other("debug log lock poisoned"))?
            .write(buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        let _ = io::stderr().flush();
        self.file
            .lock()
            .map_err(|_| io::Error::other("debug log lock poisoned"))?
            .flush()
    }
}
