//! Decision records: one [`PolicyDecisionRecord`] for every call the gate
//! decides, on every path (S4 §12).
//!
//! The gate builds the record and hands it to a [`DecisionSink`], which it
//! awaits before it returns its decision. A record that cannot be written
//! refuses the call: the gate never acts on a decision it could not record.
//!
//! - In the Runtime, the sink appends an `EventKind::PolicyDecision` to the
//!   Event Log (`loop_runtime::decisions`).
//! - Embedded paths (`--print`, the TUI's fallback, in-process ACP, `loop
//!   mcp serve`) have no Event Log. [`JsonlDecisionSink`] appends JSON lines
//!   to `audit/decisions.jsonl` in the agent directory (0600).

use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use loop_protocol::{Caller, Principal, ToolKind};
pub use loop_store::{
    DecidedBy, DecisionAnswer, DecisionOutcome, DecisionSource, DecisionSubject,
    PolicyDecisionRecord,
};
use serde_json::Value;
use sha2::{Digest, Sha256};

use super::rules::split_command;

/// The folder below the agent directory that holds embedded decision
/// records.
pub const AUDIT_DIR: &str = "audit";
/// The file in [`AUDIT_DIR`] that holds embedded decision records.
pub const DECISIONS_FILE: &str = "decisions.jsonl";

/// Why a decision record could not be written.
#[derive(Debug, thiserror::Error)]
pub enum DecisionError {
    /// The store refused or failed the write.
    #[error("could not record the Policy decision: {0}")]
    Write(String),
    /// The record could not be encoded.
    #[error("could not encode the Policy decision: {0}")]
    Encode(#[from] serde_json::Error),
    /// The file could not be written.
    #[error("could not write the Policy decision record: {0}")]
    Io(#[from] std::io::Error),
}

/// Where a decision is made, as the Session-bound sink knows it.
#[derive(Debug, Clone)]
pub struct DecisionContext {
    /// The Session.
    pub session: Option<String>,
    /// The running turn.
    pub turn: Option<String>,
    /// Who acts: the caller that started the turn, or the sink's default
    /// (the Runtime's principal in the Runtime, the OS user embedded).
    pub caller: Caller,
    /// Who answered the Escalation the gate asked about, when known.
    pub answered_by: Option<Caller>,
}

/// Where the gate writes its decision records. One per Session.
#[async_trait]
pub trait DecisionSink: Send + Sync {
    /// The Session, turn and actor of a decision made now. `escalation`
    /// names the Escalation the approver raised for it, if any, so the sink
    /// can say who answered it.
    fn context(&self, escalation: Option<&str>) -> DecisionContext;

    /// Write `record`. The gate awaits this before the call runs; an error
    /// refuses the call.
    async fn record(&self, record: &PolicyDecisionRecord) -> Result<(), DecisionError>;
}

/// The OS user that runs this process: `{ person, local, uid:<euid> }`
/// (ADR-0009), or [`Principal::local_os_user`] where there is no uid. The
/// same value as `loop_wire::server::local_principal`.
pub fn local_user() -> Principal {
    #[cfg(unix)]
    {
        // SAFETY: geteuid has no arguments or preconditions.
        Principal::local_uid(unsafe { libc::geteuid() })
    }
    #[cfg(not(unix))]
    {
        Principal::local_os_user()
    }
}

/// The SHA-256 of `args` as JSON with object keys sorted, in hex: the same
/// arguments give the same hash whatever their key order.
pub fn args_sha256(args: &Value) -> String {
    let digest = Sha256::digest(canonical(args).to_string().as_bytes());
    digest.iter().map(|b| format!("{b:02x}")).collect()
}

/// `value` with every object's keys in sorted order.
fn canonical(value: &Value) -> Value {
    match value {
        Value::Object(map) => {
            let mut keys: Vec<&String> = map.keys().collect();
            keys.sort();
            Value::Object(
                keys.into_iter()
                    .map(|key| (key.clone(), canonical(&map[key])))
                    .collect(),
            )
        }
        Value::Array(items) => Value::Array(items.iter().map(canonical).collect()),
        other => other.clone(),
    }
}

/// A short, stable summary of a call (S4 §12) that keeps no secrets: the
/// program of each part of a command (not its arguments), the path a file
/// tool names, and the host of a URL (not its path or query). Queries,
/// file contents and MCP arguments are kept only in the hash.
pub fn subject_of(kind: ToolKind, args: &Value) -> DecisionSubject {
    let text = |key: &str| args.get(key).and_then(Value::as_str);
    let mut subject = DecisionSubject::default();
    if let Some(command) = text("command") {
        subject.programs = programs(command);
    }
    if matches!(
        kind,
        ToolKind::Read | ToolKind::Edit | ToolKind::Delete | ToolKind::Move | ToolKind::Search
    ) {
        subject.path = ["path", "file_path", "filePath", "abs_path"]
            .into_iter()
            .find_map(text)
            .map(str::to_owned);
    }
    if let Some(url) = text("url") {
        subject.host = host_of(url);
    }
    subject.capped()
}

/// The program of each part of `command`: its first word that is not a
/// `NAME=value` assignment, with the path left off. An unreadable command
/// gives its first word only.
fn programs(command: &str) -> Vec<String> {
    let parts = split_command(command).unwrap_or_else(|| vec![command.to_owned()]);
    parts
        .iter()
        .filter_map(|part| {
            part.split_whitespace()
                .find(|word| !is_assignment(word))
                .map(|word| {
                    let word = word.trim_matches(|c| c == '\'' || c == '"');
                    word.rsplit('/').next().unwrap_or(word).to_owned()
                })
        })
        .filter(|word| !word.is_empty())
        .collect()
}

fn is_assignment(word: &str) -> bool {
    word.split_once('=').is_some_and(|(name, _)| {
        !name.is_empty() && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
    })
}

/// `scheme://host[:port]` of `url`, without user info, path or query.
fn host_of(url: &str) -> Option<String> {
    let (scheme, rest) = url.split_once("://")?;
    let authority = rest.split(['/', '?', '#']).next()?;
    let host = authority.rsplit('@').next()?;
    (!host.is_empty()).then(|| format!("{scheme}://{host}"))
}

/// The longest `reason` a record keeps, in bytes.
pub const MAX_REASON_BYTES: usize = 512;
/// The longest `tool` or `rule` a record keeps, in bytes.
pub const MAX_NAME_BYTES: usize = 128;

/// `text` made safe to keep in a record: every URL loses its user info,
/// query and fragment (which can hold credentials and tokens), and the
/// result is cut to `cap` bytes.
pub fn scrub(text: &str, cap: usize) -> String {
    let mut out = String::with_capacity(text.len().min(cap));
    let mut rest = text;
    while let Some(at) = rest.find("://") {
        out.push_str(&rest[..at + 3]);
        let after = &rest[at + 3..];
        let end = after
            .find(|c: char| c.is_whitespace() || matches!(c, '"' | '\'' | '`' | '<' | '>'))
            .unwrap_or(after.len());
        let url = &after[..end];
        let url = url.split(['?', '#']).next().unwrap_or_default();
        let (authority, path) = match url.find('/') {
            Some(i) => url.split_at(i),
            None => (url, ""),
        };
        let host = authority.rsplit('@').next().unwrap_or_default();
        out.push_str(host);
        out.push_str(path);
        rest = &after[end..];
    }
    out.push_str(rest);
    if out.len() > cap {
        let mut end = cap;
        while !out.is_char_boundary(end) {
            end -= 1;
        }
        out.truncate(end);
    }
    out
}

/// A new record id, `pd-…`.
pub fn record_id() -> String {
    format!("pd-{}", loop_ai::new_id())
}

/// Now, in Unix milliseconds.
pub fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX))
}

/// The largest `decisions.jsonl` before it is rotated, in bytes.
pub const MAX_AUDIT_BYTES: u64 = 8 * 1024 * 1024;
/// How many rotated files are kept beside `decisions.jsonl`
/// (`decisions.jsonl.1` is the newest).
pub const KEPT_AUDIT_FILES: usize = 4;

/// The embedded sink: JSON lines in `audit/decisions.jsonl` below the agent
/// directory, 0600 in a 0700 folder. It is not the Event Log: replay and
/// Surfaces do not see it (S4 §12, a stated limit).
///
/// - It never follows a link: a symlinked `audit/` folder or file is
///   refused, and both must belong to this user. The modes are set again
///   at every open.
/// - The file is rotated at [`MAX_AUDIT_BYTES`]; [`KEPT_AUDIT_FILES`]
///   older files are kept.
/// - Durability: every record is written to the file before the gate
///   returns, so a crash of the process keeps it. A record of a call with
///   an effect is also synced to disk (`fsync`) before the call runs; an
///   allowed read of files is synced with the next such record, so a power
///   loss can lose the last reads.
pub struct JsonlDecisionSink {
    path: PathBuf,
    max_bytes: u64,
    kept: usize,
    /// The open file, shared with the blocking writer; one writer at a
    /// time, so lines never mix.
    file: Arc<std::sync::Mutex<Option<std::fs::File>>>,
}

impl JsonlDecisionSink {
    /// A sink that writes below `agent_dir`.
    pub fn new(agent_dir: &Path) -> Self {
        Self::with_limits(agent_dir, MAX_AUDIT_BYTES, KEPT_AUDIT_FILES)
    }

    /// [`Self::new`] with another rotation size and count.
    pub fn with_limits(agent_dir: &Path, max_bytes: u64, kept: usize) -> Self {
        Self {
            path: agent_dir.join(AUDIT_DIR).join(DECISIONS_FILE),
            max_bytes,
            kept,
            file: Arc::new(std::sync::Mutex::new(None)),
        }
    }

    /// The file it writes.
    pub fn path(&self) -> &Path {
        &self.path
    }
}

/// Whether a record may wait for the next sync: an allowed read of files,
/// which has no effect to protect.
fn may_defer_sync(record: &PolicyDecisionRecord) -> bool {
    record.outcome == DecisionOutcome::Allow && record.capability == "read_files"
}

#[async_trait]
impl DecisionSink for JsonlDecisionSink {
    fn context(&self, escalation: Option<&str>) -> DecisionContext {
        // Embedded, the person at this host is the only one who can act or
        // answer.
        let caller = Caller::host(local_user());
        DecisionContext {
            session: None,
            turn: None,
            answered_by: escalation.map(|_| caller.clone()),
            caller,
        }
    }

    async fn record(&self, record: &PolicyDecisionRecord) -> Result<(), DecisionError> {
        let mut line = serde_json::to_vec(record)?;
        line.push(b'\n');
        let sync = !may_defer_sync(record);
        let (path, file, max, kept) = (
            self.path.clone(),
            Arc::clone(&self.file),
            self.max_bytes,
            self.kept,
        );
        tokio::task::spawn_blocking(move || {
            let mut file = file
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            append_private(&path, &mut file, &line, sync, max, kept)
        })
        .await
        .map_err(|e| DecisionError::Write(e.to_string()))??;
        Ok(())
    }
}

/// Append `line` to the file at `path`, opening it when `open` is empty or
/// after a rotation. `sync` syncs it to disk.
fn append_private(
    path: &Path,
    open: &mut Option<std::fs::File>,
    line: &[u8],
    sync: bool,
    max: u64,
    kept: usize,
) -> std::io::Result<()> {
    use std::io::Write;
    let full = match open {
        Some(file) => file.metadata()?.len() + line.len() as u64 > max,
        None => false,
    };
    if full {
        if let Some(file) = open.take() {
            file.sync_data()?;
        }
        rotate(path, kept)?;
    }
    let file = match open {
        Some(file) => file,
        None => open.insert(open_private(path)?),
    };
    file.write_all(line)?;
    if sync {
        file.sync_data()?;
    }
    Ok(())
}

/// Move `path` to `path.1`, `path.1` to `path.2` and so on; the oldest of
/// `kept` is removed.
fn rotate(path: &Path, kept: usize) -> std::io::Result<()> {
    let numbered = |n: usize| {
        let mut name = path.as_os_str().to_owned();
        name.push(format!(".{n}"));
        PathBuf::from(name)
    };
    if kept == 0 {
        return std::fs::remove_file(path);
    }
    let _ = std::fs::remove_file(numbered(kept));
    for n in (1..kept).rev() {
        if numbered(n).exists() {
            std::fs::rename(numbered(n), numbered(n + 1))?;
        }
    }
    std::fs::rename(path, numbered(1))
}

/// Open `path` to append, as a private file in a private folder, never
/// through a link.
fn open_private(path: &Path) -> std::io::Result<std::fs::File> {
    let refused = |what: String| std::io::Error::new(std::io::ErrorKind::PermissionDenied, what);
    let dir = path
        .parent()
        .ok_or_else(|| refused("the audit file has no folder".into()))?;
    match std::fs::symlink_metadata(dir) {
        Ok(meta) if meta.file_type().is_symlink() || !meta.is_dir() => {
            return Err(refused(format!(
                "{} is a link or not a folder; Loop writes decision records only to a real folder",
                dir.display()
            )));
        }
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            let mut builder = std::fs::DirBuilder::new();
            #[cfg(unix)]
            std::os::unix::fs::DirBuilderExt::mode(&mut builder, 0o700);
            builder.create(dir)?;
        }
        Err(error) => return Err(error),
    }
    let mut options = std::fs::OpenOptions::new();
    options.create(true).append(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600).custom_flags(libc::O_NOFOLLOW);
    }
    let file = options.open(path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        // SAFETY: geteuid has no arguments or preconditions.
        let me = unsafe { libc::geteuid() };
        let dir_meta = std::fs::symlink_metadata(dir)?;
        let meta = file.metadata()?;
        if dir_meta.uid() != me || meta.uid() != me || !meta.is_file() || meta.nlink() != 1 {
            return Err(refused(format!(
                "{} or its folder is not a private file of this user",
                path.display()
            )));
        }
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
        file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
    }
    Ok(file)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn a_subject_keeps_no_arguments_paths_of_files_and_url_hosts_only() {
        let s = subject_of(
            ToolKind::Execute,
            &json!({"command": "TOKEN=abc /usr/bin/curl -H 'Authorization: x' https://a && jq .k"}),
        );
        assert_eq!(s.programs, ["curl", "jq"]);
        let text = serde_json::to_string(&s).unwrap();
        assert!(
            !text.contains("abc") && !text.contains("Authorization"),
            "{text}"
        );
        let s = subject_of(
            ToolKind::Edit,
            &json!({"path": "src/a.rs", "content": "secret"}),
        );
        assert_eq!(s.path.as_deref(), Some("src/a.rs"));
        assert!(!serde_json::to_string(&s).unwrap().contains("secret"));
        let s = subject_of(
            ToolKind::Fetch,
            &json!({"url": "https://user:pw@docs.rs/x?token=t"}),
        );
        assert_eq!(s.host.as_deref(), Some("https://docs.rs"));
        let s = subject_of(ToolKind::Fetch, &json!({"query": "my secret"}));
        assert!(s.is_empty());
    }

    #[test]
    fn scrub_keeps_no_url_credentials_or_queries() {
        let text = scrub(
            "User rejected fetch of https://user:S3CRET@h.example/x?token=T0KEN#f, see ftp://a@b",
            MAX_REASON_BYTES,
        );
        assert_eq!(
            text,
            "User rejected fetch of https://h.example/x see ftp://b"
        );
        assert_eq!(scrub(&"x".repeat(1000), 10).len(), 10);
    }

    #[test]
    fn the_args_hash_is_stable() {
        let a = args_sha256(&json!({"command": "ls", "cwd": "."}));
        let b = args_sha256(&json!({"cwd": ".", "command": "ls"}));
        assert_eq!(a, b);
        assert_eq!(a.len(), 64);
    }

    fn sample() -> PolicyDecisionRecord {
        let caller = Caller::host(local_user());
        PolicyDecisionRecord {
            id: record_id(),
            ts_ms: now_ms(),
            tenant: caller.tenant().clone(),
            principal: caller.principal().clone(),
            via: None,
            session: None,
            turn: None,
            call_id: None,
            source: DecisionSource::Native,
            tool: "bash".into(),
            kind: ToolKind::Execute,
            capability: "shell".into(),
            subject: DecisionSubject::default(),
            args_sha256: None,
            outcome: DecisionOutcome::Allow,
            answer: None,
            by: DecidedBy::Preset,
            layer: None,
            rule: None,
            mode: None,
            escalation: None,
            answered_by: None,
            reason: None,
        }
    }

    /// A symlinked `audit/` folder or file is refused, so a record cannot
    /// be sent elsewhere; a folder's loose mode is set back.
    #[cfg(unix)]
    #[tokio::test]
    async fn the_embedded_sink_never_follows_a_link() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let elsewhere = dir.path().join("elsewhere");
        std::fs::create_dir(&elsewhere).unwrap();
        let agent = dir.path().join("agent");
        std::fs::create_dir(&agent).unwrap();
        std::os::unix::fs::symlink(&elsewhere, agent.join(AUDIT_DIR)).unwrap();
        let sink = JsonlDecisionSink::new(&agent);
        assert!(sink.record(&sample()).await.is_err());
        assert!(std::fs::read_dir(&elsewhere).unwrap().next().is_none());

        std::fs::remove_file(agent.join(AUDIT_DIR)).unwrap();
        std::fs::create_dir(agent.join(AUDIT_DIR)).unwrap();
        std::fs::set_permissions(
            agent.join(AUDIT_DIR),
            std::fs::Permissions::from_mode(0o755),
        )
        .unwrap();
        std::os::unix::fs::symlink(elsewhere.join("x"), sink.path()).unwrap();
        assert!(JsonlDecisionSink::new(&agent)
            .record(&sample())
            .await
            .is_err());
        assert!(!elsewhere.join("x").exists());
        std::fs::remove_file(sink.path()).unwrap();
        JsonlDecisionSink::new(&agent)
            .record(&sample())
            .await
            .unwrap();
        let mode = std::fs::metadata(agent.join(AUDIT_DIR))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o700);
    }

    /// The file is rotated at its size limit, keeping a fixed number of
    /// older files.
    #[tokio::test]
    async fn the_embedded_sink_rotates_its_file() {
        let dir = tempfile::tempdir().unwrap();
        let sink = JsonlDecisionSink::with_limits(dir.path(), 2000, 2);
        for _ in 0..40 {
            sink.record(&sample()).await.unwrap();
        }
        let audit = dir.path().join(AUDIT_DIR);
        let mut names: Vec<_> = std::fs::read_dir(&audit)
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect();
        names.sort();
        assert_eq!(
            names,
            ["decisions.jsonl", "decisions.jsonl.1", "decisions.jsonl.2"]
        );
        for name in names {
            let len = std::fs::metadata(audit.join(name)).unwrap().len();
            assert!(len <= 2000, "{len}");
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn the_embedded_sink_appends_private_json_lines() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let sink = JsonlDecisionSink::new(dir.path());
        let context = sink.context(None);
        assert!(context.caller.is_host_user());
        let record = PolicyDecisionRecord {
            id: record_id(),
            ts_ms: now_ms(),
            tenant: context.caller.tenant().clone(),
            principal: context.caller.principal().clone(),
            via: None,
            session: None,
            turn: None,
            call_id: None,
            source: DecisionSource::Native,
            tool: "bash".into(),
            kind: ToolKind::Execute,
            capability: "shell".into(),
            subject: DecisionSubject::default(),
            args_sha256: None,
            outcome: DecisionOutcome::Allow,
            answer: None,
            by: DecidedBy::Preset,
            layer: None,
            rule: None,
            mode: Some("full-access".into()),
            escalation: None,
            answered_by: None,
            reason: None,
        };
        sink.record(&record).await.unwrap();
        sink.record(&record).await.unwrap();
        let text = std::fs::read_to_string(sink.path()).unwrap();
        let lines: Vec<PolicyDecisionRecord> = text
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();
        assert_eq!(lines, [record.clone(), record]);
        let mode = std::fs::metadata(sink.path()).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
        let dir_mode = std::fs::metadata(sink.path().parent().unwrap())
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(dir_mode & 0o777, 0o700);
    }
}
