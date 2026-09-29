//! Debug session logging to `target/debug/logs`.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::{fmt, EnvFilter, Layer, Registry};

/// A layer installed next to the log layer, e.g. the trace exporter.
pub type ExportLayer = Box<dyn Layer<Registry> + Send + Sync>;

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

/// Where log lines go.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LogSink {
    /// `--debug`: a session log under `cwd/target/debug/logs` (teed to stderr when not
    /// interactive).
    DebugSession,
    /// Interactive TUI without `--debug`: appended to [`warn_log_path`]. Never the
    /// terminal, where log lines would be drawn over the UI.
    WarnFile,
    /// `--print`, MCP serve and other non-interactive modes.
    Stderr,
}

impl LogSink {
    /// Pick the sink for this run.
    pub fn for_run(debug: bool, interactive: bool) -> Self {
        match (debug, interactive) {
            (true, _) => Self::DebugSession,
            (false, true) => Self::WarnFile,
            (false, false) => Self::Stderr,
        }
    }
}

/// Log file for warnings from interactive sessions: `<agent dir>/logs/loop.log`.
pub fn warn_log_path(agent_dir: &Path) -> PathBuf {
    agent_dir.join("logs").join("loop.log")
}

/// Initialize tracing; see [`LogSink`] for where log lines go.
///
/// Interactive TUI mode never writes to the terminal (it would corrupt the UI).
/// Non-interactive debug mode tees to stderr as well. `export_layer` (the telemetry exporter, when built with
/// the `telemetry` feature) sees observation spans independently of the log filter.
pub fn init_tracing(
    debug: bool,
    cwd: &Path,
    interactive: bool,
    export_layer: Option<ExportLayer>,
) -> anyhow::Result<Option<PathBuf>> {
    let default_filter = if debug {
        "info,loop_agent=debug,loop_ai=debug,loop_cli=debug,loop_app_core=debug,loop_mcp=debug"
    } else {
        "warn"
    };
    let filter =
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(default_filter));

    let (log_layer, path) = match LogSink::for_run(debug, interactive) {
        LogSink::DebugSession => {
            let (writer, path) = debug_log_writer(cwd, interactive)?;
            let layer = fmt::layer()
                .with_writer(Mutex::new(writer))
                .with_ansi(false)
                .with_target(true)
                .with_thread_ids(true)
                .boxed();
            (layer, Some(path))
        }
        LogSink::WarnFile => {
            let path = warn_log_path(&crate::config::paths::get_agent_dir());
            // Best effort: if the file can't be opened, drop log lines rather than
            // print them over the TUI.
            let writer: Box<dyn Write + Send> = match open_append(&path) {
                Ok(file) => Box::new(file),
                Err(_) => Box::new(io::sink()),
            };
            let layer = fmt::layer()
                .with_writer(Mutex::new(writer))
                .with_ansi(false)
                .with_target(true)
                .boxed();
            (layer, None)
        }
        LogSink::Stderr => {
            let layer = fmt::layer()
                .with_writer(io::stderr)
                .with_target(true)
                .boxed();
            (layer, None)
        }
    };

    tracing_subscriber::registry()
        .with(export_layer)
        .with(log_layer.with_filter(filter))
        .init();

    if let Some(path) = &path {
        tracing::info!(
            path = %path.display(),
            interactive,
            "debug logging enabled"
        );
    }
    Ok(path)
}

/// Log file under `cwd/target/debug/logs`, teed to stderr when not interactive.
fn debug_log_writer(cwd: &Path, interactive: bool) -> anyhow::Result<(Box<dyn Write + Send>, PathBuf)> {
    let log_dir = cwd.join("target").join("debug").join("logs");
    fs::create_dir_all(&log_dir)?;
    let ts = chrono::Local::now().format("%Y%m%d-%H%M%S");
    let path = log_dir.join(format!("loop-{ts}.log"));
    let file = open_append(&path)?;

    let writer: Box<dyn Write + Send> = if interactive {
        Box::new(file)
    } else {
        Box::new(TeeWriter {
            file: Mutex::new(file),
        })
    };
    Ok((writer, path))
}

/// Open `path` for appending, creating it and its parent directory.
fn open_append(path: &Path) -> io::Result<File> {
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir)?;
    }
    OpenOptions::new().create(true).append(true).open(path)
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn interactive_runs_never_log_to_the_terminal() {
        assert_eq!(LogSink::for_run(false, true), LogSink::WarnFile);
        assert_eq!(LogSink::for_run(true, true), LogSink::DebugSession);
    }

    #[test]
    fn non_interactive_runs_log_to_stderr_unless_debug() {
        assert_eq!(LogSink::for_run(false, false), LogSink::Stderr);
        assert_eq!(LogSink::for_run(true, false), LogSink::DebugSession);
    }

    #[test]
    fn warn_log_lives_under_the_agent_dir() {
        assert_eq!(
            warn_log_path(Path::new("/home/u/.loop/agent")),
            PathBuf::from("/home/u/.loop/agent/logs/loop.log")
        );
    }

    #[test]
    fn open_append_creates_missing_dirs_and_appends() {
        let dir = tempfile::tempdir().unwrap();
        let path = warn_log_path(dir.path());
        writeln!(open_append(&path).unwrap(), "one").unwrap();
        writeln!(open_append(&path).unwrap(), "two").unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), "one\ntwo\n");
    }
}
