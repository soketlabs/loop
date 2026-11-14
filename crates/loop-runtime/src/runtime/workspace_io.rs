//! The files and terminals of a Session's workspace (`session/workspace`,
//! `session/files/*`, `session/terminal/*`).
//!
//! Every entry point checks, in this order: the caller's access to the
//! Session ([`Runtime::check`]), the tool lock for a write or a terminal, an
//! outside caller for a terminal, the Session's sandbox, the path syntax,
//! and then the Session's permission gate as a user action
//! (`ToolGate::check_user_file_action`, `ToolGate::check_user_terminal`),
//! which records the decision.
//!
//! The Session's `host` Workspace serves the calls. With the Session's
//! sandbox on every call is `Unsupported`: the sandbox drivers do neither
//! transfer nor `spawn` yet, and a host terminal or host file access would
//! escape the sandbox.
//!
//! Upload and terminal ids are the Runtime's own random ids. Each belongs to
//! one Session and one caller (tenant and principal); another caller gets
//! the same error as for an id that does not exist. A driver id never goes
//! to a client. See `docs/design/files-and-terminal-protocol.md`.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::time::{Duration, Instant};

use loop_agent::harness::sandbox::{
    ExitState, FileTag, ProcessEventKind, ProcessId, ProcessSignal, PtySize, SpawnRequest,
    UploadId, UploadRequest, Workspace, WorkspaceError, WorkspacePath, WorkspaceProcess,
};
use loop_agent::harness::types::FileErrorCode;
use loop_app_core::tool_gate::{ToolGate, UserFileAction, TERMINAL_COMMAND};
use loop_protocol::rpc::SessionParams;
use loop_protocol::session_workspace::{
    base64, Availability, FileEntry, FileKind, FilePathParams, FilesDeleteParams, FilesListParams,
    FilesListResult, FilesReadParams, FilesReadResult, FilesRenameParams, SessionWorkspaceInfo,
    SessionWorkspaceLimits, TerminalAttachParams, TerminalAttachResult, TerminalCloseResult,
    TerminalEnd, TerminalInfo, TerminalInputParams, TerminalListResult, TerminalOpenParams,
    TerminalOpenResult, TerminalParams, TerminalResizeParams, TerminalSignal, TerminalSignalParams,
    UploadBeginParams, UploadBeginResult, UploadCommitResult, UploadParams, UploadProgress,
    UploadWriteParams, WorkspaceErrorCode as Code, LIST_PAGE_DEFAULT, LIST_PAGE_MAX,
};
use loop_protocol::{BackendError, BackendResult, Caller, SessionKey, SessionRef};
use loop_wire::{TerminalEvent, TerminalFeed};

use super::workspace_fs::{self, FsError, Root};
use super::{LiveSession, Runtime};
use crate::access::{Actor, Need};

/// How often the Runtime checks open terminals and uploads.
const SWEEP: Duration = Duration::from_millis(250);
/// An upload no call touched for this long is forgotten (the `host`
/// driver expires it after 10 minutes too).
const UPLOAD_IDLE: Duration = Duration::from_secs(600);
/// Most entries a recursive delete or a folder rename checks.
const SUBTREE_MAX: usize = 10_000;
/// How long a close waits for the process tree to end after the kill.
const CLOSE_WAIT: Duration = Duration::from_secs(5);

/// The open uploads and terminals of the Runtime, by their ids.
#[derive(Default)]
pub(super) struct WorkspaceIo {
    limits: Mutex<SessionWorkspaceLimits>,
    uploads: Mutex<HashMap<String, Arc<Upload>>>,
    terminals: Mutex<HashMap<String, Arc<Terminal>>>,
    /// The sweep task runs.
    sweeping: AtomicBool,
    /// Slots taken by opens and upload begins still running, so that
    /// calls at the same time cannot pass a count limit together.
    opening: Mutex<HashMap<u64, Reserved>>,
    next_slot: std::sync::atomic::AtomicU64,
    /// Session folders whose stale upload files were removed.
    swept: Mutex<std::collections::HashSet<PathBuf>>,
}

/// What a running open or upload begin holds a slot for.
#[derive(Clone)]
struct Reserved {
    session: SessionKey,
    owner: Option<Caller>,
    kind: Kind,
}

/// A terminal, or an upload of this many bytes.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    Terminal,
    Upload(u64),
}

/// A slot of [`WorkspaceIo::opening`]; dropping it frees the slot.
struct Slot<'a> {
    io: &'a WorkspaceIo,
    id: u64,
}

impl Drop for Slot<'_> {
    fn drop(&mut self) {
        lock(&self.io.opening).remove(&self.id);
    }
}

impl WorkspaceIo {
    /// Count the open terminals (or uploads and their bytes) of `session`
    /// and of `owner`, with the slots of calls still running, and take a
    /// slot when all are below `limits`. One lock: two calls never both
    /// pass.
    fn reserve(
        &self,
        kind: Kind,
        session: &SessionKey,
        owner: &Option<Caller>,
        limits: &SessionWorkspaceLimits,
    ) -> Option<Slot<'_>> {
        let mut opening = lock(&self.opening);
        let (mut here, mut mine, mut bytes) = (0usize, 0usize, 0u64);
        let mut count = |s: &SessionKey, o: &Option<Caller>, size: u64| {
            here += usize::from(s == session);
            mine += usize::from(same_owner(o, owner));
            if s == session {
                bytes = bytes.saturating_add(size);
            }
        };
        match kind {
            Kind::Terminal => {
                for t in lock(&self.terminals).values() {
                    count(&t.session, &t.owner, 0);
                }
            }
            Kind::Upload(_) => {
                for u in lock(&self.uploads).values() {
                    count(&u.session, &u.owner, u.size);
                }
            }
        }
        for r in opening.values() {
            match (r.kind, kind) {
                (Kind::Terminal, Kind::Terminal) => count(&r.session, &r.owner, 0),
                (Kind::Upload(size), Kind::Upload(_)) => count(&r.session, &r.owner, size),
                _ => {}
            }
        }
        let refused = match kind {
            Kind::Terminal => {
                here >= limits.terminals_per_session as usize
                    || mine >= limits.terminals_per_caller as usize
            }
            Kind::Upload(size) => {
                mine >= limits.uploads_per_caller as usize
                    || bytes.saturating_add(size) > limits.upload_bytes_per_session
            }
        };
        if refused {
            return None;
        }
        let id = self.next_slot.fetch_add(1, Ordering::AcqRel);
        opening.insert(
            id,
            Reserved {
                session: session.clone(),
                owner: owner.clone(),
                kind,
            },
        );
        Some(Slot { io: self, id })
    }
}

/// One upload, owned by `owner` in `session`.
struct Upload {
    session: SessionKey,
    owner: Option<Caller>,
    live: Weak<LiveSession>,
    workspace: Arc<dyn Workspace>,
    driver: UploadId,
    path: WorkspacePath,
    size: u64,
    /// Neither `if_match` nor `overwrite`: the commit must not replace a
    /// file that appeared meanwhile.
    no_replace: bool,
    touched: Mutex<Instant>,
}

/// One terminal, owned by `owner` in `session`.
struct Terminal {
    id: String,
    session: SessionKey,
    owner: Option<Caller>,
    live: Weak<LiveSession>,
    workspace: Arc<dyn Workspace>,
    gate: Arc<ToolGate>,
    process: ProcessId,
    /// The Runtime's own handle: input, size and signals. While it lives,
    /// the process lives (it is not detached).
    control: Box<dyn WorkspaceProcess>,
    command: String,
    size: Mutex<PtySize>,
    opened_ms: u64,
    /// Connections that stream it now.
    attached: AtomicUsize,
    /// Since when no connection streams it.
    detached_since: Mutex<Option<Instant>>,
    closed: AtomicBool,
}

/// Counts one connection's stream of a terminal while it lives.
struct Streaming(Arc<Terminal>);

impl Streaming {
    fn new(terminal: Arc<Terminal>) -> Self {
        terminal.attached.fetch_add(1, Ordering::AcqRel);
        *lock(&terminal.detached_since) = None;
        Self(terminal)
    }
}

impl Drop for Streaming {
    fn drop(&mut self) {
        if self.0.attached.fetch_sub(1, Ordering::AcqRel) == 1 {
            *lock(&self.0.detached_since) = Some(Instant::now());
        }
    }
}

/// A connection's stream of one terminal ([`TerminalFeed`]).
struct Feed {
    handle: Box<dyn WorkspaceProcess>,
    _streaming: Streaming,
}

#[async_trait::async_trait]
impl TerminalFeed for Feed {
    async fn next(&self) -> Option<TerminalEvent> {
        let event = self.handle.next_event().await?;
        Some(match event.kind {
            ProcessEventKind::Output(data) => TerminalEvent::Output {
                seq: event.seq,
                data,
            },
            ProcessEventKind::Dropped { bytes } => TerminalEvent::Dropped {
                seq: event.seq,
                bytes,
            },
            ProcessEventKind::Exited(state) => TerminalEvent::Exited {
                seq: event.seq,
                exit: end_of(state),
            },
        })
    }
}

/// What a call works on: the loaded Session, its `host` Workspace and the
/// Workspace root.
struct Target {
    live: Arc<LiveSession>,
    workspace: Arc<dyn Workspace>,
    root: PathBuf,
    caller: Option<Caller>,
    /// The Runtime has a tool lock: refusals all look the same.
    locked: bool,
}

impl Target {
    fn gate(&self) -> &Arc<ToolGate> {
        &self.live.opened.tool_gate
    }

    fn record_session(&self) -> Option<String> {
        Some(self.live.reference.key.0.clone())
    }

    /// `raw` as a workspace path and its path in the driver's namespace.
    fn path(&self, raw: &str) -> BackendResult<(WorkspacePath, PathBuf)> {
        let path = WorkspacePath::parse(raw).map_err(|error| self.error(error))?;
        let native = self.workspace.resolve(&path);
        Ok((path, native))
    }

    /// [`Self::path`], but never the root.
    fn below_root(&self, raw: &str, what: &str) -> BackendResult<(WorkspacePath, PathBuf)> {
        let (path, native) = self.path(raw)?;
        if path.as_str().is_empty() {
            return Err(BackendError::workspace(
                Code::InvalidPath,
                format!("cannot {what} the workspace root"),
            ));
        }
        Ok((path, native))
    }

    /// The gate's check of a user file action on `native`.
    async fn allow(&self, action: UserFileAction, native: &Path) -> BackendResult<()> {
        self.gate()
            .check_user_file_action(action, native, self.record_session(), self.caller.clone())
            .await
            .map_err(|refused| {
                if self.locked {
                    refused_path()
                } else {
                    BackendError::workspace(Code::Refused, scrub(&refused.0, &self.root))
                }
            })
    }

    /// A driver error as the client sees it, without host paths.
    fn error(&self, error: WorkspaceError) -> BackendError {
        workspace_error(error, &self.root)
    }

    /// Run a descriptor-based file operation on the Session's folder, on
    /// the blocking pool. `raw` names the path in errors.
    async fn fs<T: Send + 'static>(
        &self,
        raw: &str,
        op: impl FnOnce(&Root) -> Result<T, FsError> + Send + 'static,
    ) -> BackendResult<T> {
        let root = self.root.clone();
        let done = tokio::task::spawn_blocking(move || Root::open(&root).and_then(|r| op(&r)))
            .await
            .map_err(|e| {
                BackendError::failed(loop_protocol::ErrorCategory::Internal, e.to_string())
            })?;
        done.map_err(|e| self.fs_error(e, raw))
    }

    /// A file-operation error as the client sees it. Under the tool lock a
    /// path through a link is the one refusal ([`refused_path`]), so a
    /// refused path and a missing one look the same.
    fn fs_error(&self, error: FsError, raw: &str) -> BackendError {
        match error {
            FsError::Link if self.locked => refused_path(),
            FsError::Link => BackendError::workspace(
                Code::InvalidPath,
                format!("`{raw}` passes through a link; the file panel does not follow links"),
            ),
            FsError::NotFound => not_found(raw),
            FsError::Exists => BackendError::workspace(Code::Exists, format!("`{raw}` exists")),
            FsError::NotEmpty => BackendError::workspace(
                Code::NotEmpty,
                format!("`{raw}` is not empty; pass recursive"),
            ),
            e @ (FsError::Changed(_) | FsError::Protected(_) | FsError::TooMany(_)) => {
                BackendError::workspace(Code::Refused, format!("`{raw}`: {e}"))
            }
            FsError::Unsupported(what) => BackendError::Unsupported(what.to_owned()),
            FsError::Io(text) => BackendError::workspace(Code::Io, scrub(&text, &self.root)),
        }
    }

    /// Check every entry inside the folder `path` as a write of its own
    /// (deny rules, `.git`, `.loop`), without records: a recursive delete
    /// or a folder rename takes them with it. Walks by descriptor, links
    /// not entered, at most [`SUBTREE_MAX`] entries. The result is what the
    /// delete may remove.
    async fn subtree_allowed(
        &self,
        path: &WorkspacePath,
        raw: &str,
    ) -> BackendResult<HashMap<String, FileKind>> {
        let walked = {
            let path = path.clone();
            self.fs(raw, move |root| root.walk(&path, SUBTREE_MAX))
                .await?
        };
        for (inner, _) in &walked {
            let refused = workspace_fs::protected_name(inner.rsplit('/').next().unwrap_or(""))
                .then(|| "it is `.git` or `.loop`".to_owned());
            let refused = match refused {
                Some(reason) => Some(reason),
                None => self
                    .gate()
                    .user_file_write_allowed(&self.root.join(inner))
                    .await
                    .err(),
            };
            if let Some(reason) = refused {
                if self.locked {
                    return Err(refused_path());
                }
                return Err(BackendError::workspace(
                    Code::Refused,
                    scrub(
                        &format!("`{raw}` holds `{inner}`, which cannot be changed: {reason}"),
                        &self.root,
                    ),
                ));
            }
        }
        Ok(walked)
    }

    /// Whether the gate's search filter shows `native` (deny rules for
    /// `read` and `ls`; under the tool lock, no link out and one hard
    /// link). It reads the file system, so it runs off the async threads.
    async fn shown(&self, natives: Vec<PathBuf>) -> BackendResult<Vec<bool>> {
        let filter = self.gate().search_filter();
        tokio::task::spawn_blocking(move || {
            natives
                .iter()
                .map(|path| filter("ls", path))
                .collect::<Vec<bool>>()
        })
        .await
        .map_err(|e| BackendError::failed(loop_protocol::ErrorCategory::Internal, e.to_string()))
    }
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// A random id: `prefix-` and 32 hex digits (two UUIDv7 random parts and
/// the time, through SHA-256).
fn new_id(prefix: &str) -> String {
    use sha2::{Digest, Sha256};
    let mut hash = Sha256::new();
    hash.update(loop_ai::new_id());
    hash.update(loop_ai::new_id());
    hash.update(format!("{:?}", Instant::now()));
    let digest = hash.finalize();
    let hex: String = digest[..16].iter().map(|b| format!("{b:02x}")).collect();
    format!("{prefix}-{hex}")
}

/// Whether `a` and `b` are one owner: the same tenant and principal, or
/// both in-process.
fn same_owner(a: &Option<Caller>, b: &Option<Caller>) -> bool {
    match (a, b) {
        (None, None) => true,
        (Some(a), Some(b)) => {
            a.tenant() == b.tenant() && a.principal().same_principal(b.principal())
        }
        _ => false,
    }
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
}

fn end_of(state: ExitState) -> TerminalEnd {
    let (state, code, signal) = match state {
        ExitState::Exited { code } => ("exited", Some(code), None),
        ExitState::Signaled { signal } => ("signaled", None, Some(signal)),
        ExitState::TimedOut => ("timed_out", None, None),
        ExitState::Cancelled => ("cancelled", None, None),
        ExitState::Lost => ("lost", None, None),
    };
    TerminalEnd {
        state: state.to_owned(),
        code,
        signal,
    }
}

/// `text` without the host path `root` (a reply never holds host paths).
fn scrub(text: &str, root: &Path) -> String {
    let root = root.to_string_lossy();
    if root.len() <= 1 {
        return text.to_owned();
    }
    text.replace(&format!("{root}/"), "").replace(&*root, ".")
}

fn workspace_error(error: WorkspaceError, root: &Path) -> BackendError {
    use FileErrorCode as F;
    let code = match &error {
        WorkspaceError::Unsupported { .. } => {
            return BackendError::Unsupported(scrub(&error.to_string(), root))
        }
        WorkspaceError::InvalidPath { .. } => Code::InvalidPath,
        WorkspaceError::Changed { .. } => Code::Changed,
        WorkspaceError::TooLarge { .. } => Code::TooLarge,
        WorkspaceError::UnknownUpload(_) => Code::UnknownUpload,
        WorkspaceError::UploadOffset { .. } => Code::UploadOffset,
        WorkspaceError::ChecksumMismatch(_) => Code::Checksum,
        WorkspaceError::SeqAhead { .. } => Code::SeqAhead,
        WorkspaceError::UnknownProcess(_) => Code::UnknownTerminal,
        WorkspaceError::Busy { .. } => Code::Limit,
        WorkspaceError::File(file) => match file.code {
            F::NotFound => Code::NotFound,
            F::AlreadyExists => Code::Exists,
            F::NotADirectory => Code::NotAFolder,
            F::IsDirectory => Code::NotAFile,
            F::InvalidPath => Code::InvalidPath,
            F::Unsupported => return BackendError::Unsupported(scrub(&file.message, root)),
            _ => Code::Io,
        },
        _ => Code::Io,
    };
    BackendError::workspace(code, scrub(&error.to_string(), root))
}

/// The one refusal under the tool lock for a path that a rule refuses,
/// that leads out of the Session's folder, or that cannot be resolved:
/// the same code and text whether or not something is there.
fn refused_path() -> BackendError {
    BackendError::workspace(
        Code::Refused,
        "the path is refused: a rule denies it, or it leads out of the Session's folder",
    )
}

fn not_found(raw: &str) -> BackendError {
    BackendError::workspace(Code::NotFound, format!("`{raw}` does not exist"))
}

fn unknown_upload() -> BackendError {
    BackendError::workspace(Code::UnknownUpload, "no such upload")
}

fn unknown_terminal() -> BackendError {
    BackendError::workspace(Code::UnknownTerminal, "no such terminal")
}

fn decode(data: &str) -> BackendResult<Vec<u8>> {
    base64::decode(data)
        .map_err(|e| BackendError::failed(loop_protocol::ErrorCategory::Protocol, e.to_string()))
}

/// Why `live` cannot serve files or terminals now: its sandbox is on (or
/// chosen to be on). Fails closed: any sign of a sandbox counts.
async fn sandbox_reason(live: &LiveSession) -> Option<&'static str> {
    let chosen = lock(&live.sandbox_choice).mode != "off" || lock(&live.pending_sandbox).is_some();
    let running = live
        .opened
        .harness
        .sandbox_info()
        .await
        .fields
        .iter()
        .any(|(label, value)| label == "Mode" && value != "off");
    (chosen || running).then_some(
        "the Session's sandbox is on, and the sandbox cannot serve files or a terminal yet",
    )
}

impl Runtime {
    /// Set the limits of the file and terminal calls (for new uploads and
    /// terminals; the detached time applies at once).
    pub fn set_workspace_limits(&self, limits: SessionWorkspaceLimits) {
        *lock(&self.workspace_io.limits) = limits;
    }

    fn workspace_limits(&self) -> SessionWorkspaceLimits {
        *lock(&self.workspace_io.limits)
    }

    /// The Session after the access check, for a call that needs `need`.
    async fn target(&self, session: &SessionRef, need: Need) -> BackendResult<Target> {
        // Read before any spawn: the task-local caller does not cross it.
        let caller = Caller::current();
        self.check(session, need).await?;
        let live = self.loaded(session).await?;
        if let Some(reason) = sandbox_reason(&live).await {
            return Err(BackendError::Unsupported(reason.to_owned()));
        }
        let workspace = Arc::clone(&live.opened.workspace.host_workspace);
        let root = workspace.root();
        Ok(Target {
            live,
            workspace,
            root,
            caller,
            locked: self.factory.globals.tool_lock().is_some(),
        })
    }

    /// Refuse a terminal call of a caller from outside the host (as
    /// `session/shell`) and under the tool lock. The server refuses the
    /// outside caller first; this holds for in-process links too.
    fn terminal_refusal(&self, what: &str) -> BackendResult<()> {
        if Actor::current().is_outside() {
            return Err(BackendError::Forbidden(what.to_owned()));
        }
        self.refuse_if_locked(what)
    }

    /// `session/workspace`.
    pub(super) async fn workspace_info(
        &self,
        params: SessionParams,
    ) -> BackendResult<SessionWorkspaceInfo> {
        self.check(&params.session, Need::Read).await?;
        let live = self.loaded(&params.session).await?;
        let lock = self.factory.globals.tool_lock();
        let sandbox = sandbox_reason(&live).await;
        let caps = live.opened.workspace.host_workspace.capabilities();
        let files = match sandbox {
            Some(reason) => Availability::no(reason),
            None if !caps.chunked_transfer => {
                Availability::no("this workspace cannot read files in pieces")
            }
            None => Availability::yes(),
        };
        let files_write = match (sandbox, lock) {
            (Some(reason), _) => Availability::no(reason),
            (None, Some(lock)) => Availability::no(format!(
                "this Runtime runs with the `{lock}` tool lock, which allows no file change"
            )),
            (None, None) if !caps.upload => Availability::no("this workspace cannot take uploads"),
            (None, None) => Availability::yes(),
        };
        let terminal = if Actor::current().is_outside() {
            Availability::no("a user from outside the host cannot open a terminal")
        } else {
            match (sandbox, lock) {
                (Some(reason), _) => Availability::no(reason),
                (None, Some(lock)) => Availability::no(format!(
                    "this Runtime runs with the `{lock}` tool lock, which allows no terminal"
                )),
                (None, None) if !caps.pty => Availability::no("this workspace has no terminal"),
                (None, None) => match live.opened.tool_gate.user_terminal_allowed().await {
                    Ok(()) => Availability::yes(),
                    Err(reason) => Availability::no(reason),
                },
            }
        };
        Ok(SessionWorkspaceInfo {
            files,
            files_write,
            terminal,
            sandboxed: sandbox.is_some(),
            tool_lock: lock,
            limits: self.workspace_limits(),
        })
    }

    /// `session/files/list`. The gate first; then the folder is read by
    /// descriptor, and only the entries of this page are looked at.
    pub(super) async fn files_list(
        &self,
        params: FilesListParams,
    ) -> BackendResult<FilesListResult> {
        let target = self.target(&params.session, Need::Read).await?;
        let (path, native) = target.path(&params.path)?;
        target.allow(UserFileAction::List, &native).await?;
        self.sweep_temps_once(&target).await;
        let limit = params
            .limit
            .unwrap_or(LIST_PAGE_DEFAULT)
            .clamp(1, LIST_PAGE_MAX) as usize;
        let filter = target.gate().search_filter();
        let root = target.root.clone();
        let (page, next_cursor, hidden) = {
            let path = path.clone();
            let cursor = params.cursor.clone();
            target
                .fs(&params.path, move |fs| {
                    let shown = |rel: &str| filter("ls", &root.join(rel));
                    fs.list(&path, cursor.as_deref(), limit, &shown)
                })
                .await?
        };
        let entries = page
            .into_iter()
            .map(|(name, meta)| FileEntry {
                path: join(path.as_str(), &name),
                name,
                kind: meta.kind,
                size: meta.size,
                mtime_ms: meta.mtime_ms,
                tag: None,
            })
            .collect();
        Ok(FilesListResult {
            path: path.as_str().to_owned(),
            entries,
            next_cursor,
            hidden,
        })
    }

    /// `session/files/stat`.
    pub(super) async fn files_stat(&self, params: FilePathParams) -> BackendResult<FileEntry> {
        let target = self.target(&params.session, Need::Read).await?;
        let (path, native) = target.path(&params.path)?;
        target.allow(UserFileAction::Read, &native).await?;
        // A hidden entry is not there, for stat as for a listing.
        if native != target.root && !target.shown(vec![native.clone()]).await?[0] {
            return Err(if target.locked {
                refused_path()
            } else {
                not_found(&params.path)
            });
        }
        let meta = {
            let path = path.clone();
            target.fs(&params.path, move |fs| fs.stat(&path)).await?
        };
        let tag = if meta.kind == FileKind::File {
            target
                .workspace
                .read_chunk(&path, 0, 0, None)
                .await
                .ok()
                .map(|chunk| chunk.tag.0)
        } else {
            None
        };
        let name = path.as_str().rsplit('/').next().unwrap_or("").to_owned();
        Ok(FileEntry {
            name,
            path: path.as_str().to_owned(),
            kind: meta.kind,
            size: meta.size,
            mtime_ms: meta.mtime_ms,
            tag,
        })
    }

    /// `session/files/read`.
    pub(super) async fn files_read(
        &self,
        params: FilesReadParams,
    ) -> BackendResult<FilesReadResult> {
        let target = self.target(&params.session, Need::Read).await?;
        let (path, native) = target.path(&params.path)?;
        target.allow(UserFileAction::Read, &native).await?;
        let most = self.workspace_limits().chunk_bytes;
        let length = params.length.unwrap_or(most).min(most) as usize;
        let if_match = params.if_match.map(FileTag);
        let chunk = target
            .workspace
            .read_chunk(&path, params.offset, length, if_match.as_ref())
            .await
            .map_err(|e| target.error(e))?;
        Ok(FilesReadResult {
            offset: chunk.offset,
            data: base64::encode(&chunk.data),
            total_size: chunk.total_size,
            eof: chunk.eof,
            tag: chunk.tag.0,
        })
    }

    /// `session/files/upload_begin`.
    pub(super) async fn files_upload_begin(
        &self,
        params: UploadBeginParams,
    ) -> BackendResult<UploadBeginResult> {
        let caller = Caller::current();
        self.check(&params.session, Need::Write).await?;
        self.refuse_if_locked("session/files/upload_begin")?;
        let target = self.target(&params.session, Need::Write).await?;
        self.need_sweep()?;
        let (path, native) = target.below_root(&params.path, "upload to")?;
        let limits = self.workspace_limits();
        if params.size > limits.upload_bytes {
            return Err(BackendError::workspace(
                Code::TooLarge,
                format!(
                    "the upload is {} bytes; the limit is {}",
                    params.size, limits.upload_bytes
                ),
            ));
        }
        // The gate first; only then is anything about the file told.
        target.allow(UserFileAction::Write, &native).await?;
        let no_replace = params.if_match.is_none() && !params.overwrite;
        if no_replace {
            let probe = path.clone();
            if target.fs(&params.path, move |fs| fs.exists(&probe)).await? {
                return Err(BackendError::workspace(
                    Code::Exists,
                    format!(
                        "`{}` exists; pass its tag as if_match, or overwrite",
                        path.as_str()
                    ),
                ));
            }
        }
        let Some(_slot) = self.workspace_io.reserve(
            Kind::Upload(params.size),
            &params.session.key,
            &caller,
            &limits,
        ) else {
            return Err(BackendError::workspace(
                Code::Limit,
                format!(
                    "too many uploads: at most {} open per user and {} bytes open per Session",
                    limits.uploads_per_caller, limits.upload_bytes_per_session
                ),
            ));
        };
        self.sweep_temps_once(&target).await;
        let driver = target
            .workspace
            .begin_upload(UploadRequest {
                path: path.clone(),
                size: params.size,
                sha256: params.sha256,
                if_match: params.if_match.clone().map(FileTag),
                // 0644 for a new file; an existing one loses its execute
                // bits at the commit.
                mode: Some(0o644),
            })
            .await
            .map_err(|e| target.error(e))?;
        let id = new_id("up");
        lock(&self.workspace_io.uploads).insert(
            id.clone(),
            Arc::new(Upload {
                session: params.session.key.clone(),
                owner: caller,
                live: Arc::downgrade(&target.live),
                workspace: Arc::clone(&target.workspace),
                driver,
                path,
                size: params.size,
                no_replace,
                touched: Mutex::new(Instant::now()),
            }),
        );
        Ok(UploadBeginResult {
            upload: id,
            chunk_max: limits.chunk_bytes,
        })
    }

    /// The caller's upload `id` of `session`, after the access check.
    async fn upload(
        &self,
        session: &SessionRef,
        id: &str,
        what: &str,
    ) -> BackendResult<Arc<Upload>> {
        let caller = Caller::current();
        self.check(session, Need::Write).await?;
        self.refuse_if_locked(what)?;
        let upload = lock(&self.workspace_io.uploads)
            .get(id)
            .filter(|u| u.session == session.key && same_owner(&u.owner, &caller))
            .cloned()
            .ok_or_else(unknown_upload)?;
        *lock(&upload.touched) = Instant::now();
        Ok(upload)
    }

    fn forget_upload(&self, id: &str) {
        lock(&self.workspace_io.uploads).remove(id);
    }

    /// `session/files/upload_write`.
    pub(super) async fn files_upload_write(
        &self,
        params: UploadWriteParams,
    ) -> BackendResult<UploadProgress> {
        let upload = self
            .upload(
                &params.session,
                &params.upload,
                "session/files/upload_write",
            )
            .await?;
        let data = decode(&params.data)?;
        let most = self.workspace_limits().chunk_bytes as usize;
        if data.len() > most {
            return Err(BackendError::workspace(
                Code::TooLarge,
                format!("one piece holds at most {most} bytes"),
            ));
        }
        let root = upload.workspace.root();
        let committed = upload
            .workspace
            .upload_write(&upload.driver, params.offset, &data)
            .await
            .map_err(|e| workspace_error(e, &root))?;
        Ok(UploadProgress {
            committed,
            size: upload.size,
        })
    }

    /// `session/files/upload_status`.
    pub(super) async fn files_upload_status(
        &self,
        params: UploadParams,
    ) -> BackendResult<UploadProgress> {
        let upload = self
            .upload(
                &params.session,
                &params.upload,
                "session/files/upload_status",
            )
            .await?;
        let root = upload.workspace.root();
        let committed = upload
            .workspace
            .upload_committed_offset(&upload.driver)
            .await
            .map_err(|e| workspace_error(e, &root))?;
        Ok(UploadProgress {
            committed,
            size: upload.size,
        })
    }

    /// `session/files/upload_commit`. The gate checks the destination again
    /// (a rule or the mode may have changed since the begin).
    pub(super) async fn files_upload_commit(
        &self,
        params: UploadParams,
    ) -> BackendResult<UploadCommitResult> {
        let upload = self
            .upload(
                &params.session,
                &params.upload,
                "session/files/upload_commit",
            )
            .await?;
        let target = self.target(&params.session, Need::Write).await?;
        let native = upload.workspace.resolve(&upload.path);
        if let Err(error) = target.allow(UserFileAction::Write, &native).await {
            let _ = upload.workspace.upload_abort(&upload.driver).await;
            self.forget_upload(&params.upload);
            return Err(error);
        }
        let root = upload.workspace.root();
        let raw = upload.path.as_str().to_owned();
        if upload.no_replace {
            // A file that appeared since the begin is not replaced. (The
            // driver has no "must not exist" commit; a file made between
            // this check and the rename is the remaining race.)
            let probe = upload.path.clone();
            if target.fs(&raw, move |fs| fs.exists(&probe)).await? {
                let _ = upload.workspace.upload_abort(&upload.driver).await;
                self.forget_upload(&params.upload);
                return Err(BackendError::workspace(
                    Code::Exists,
                    format!("`{raw}` appeared after the upload began; it was not replaced"),
                ));
            }
        }
        match upload.workspace.upload_commit(&upload.driver).await {
            Ok(tag) => {
                self.forget_upload(&params.upload);
                // An upload never leaves an execute bit, also over a file
                // that had one (the driver keeps an existing file's mode).
                let path = upload.path.clone();
                target.fs(&raw, move |fs| fs.clear_execute(&path)).await?;
                let tag = match upload.workspace.read_chunk(&upload.path, 0, 0, None).await {
                    Ok(chunk) => chunk.tag,
                    Err(_) => tag,
                };
                Ok(UploadCommitResult {
                    path: upload.path.as_str().to_owned(),
                    size: upload.size,
                    tag: tag.0,
                })
            }
            // Not complete yet: the upload stays, to resume.
            Err(error @ WorkspaceError::UploadOffset { .. }) => Err(workspace_error(error, &root)),
            Err(error) => {
                self.forget_upload(&params.upload);
                Err(workspace_error(error, &root))
            }
        }
    }

    /// `session/files/upload_abort`.
    pub(super) async fn files_upload_abort(&self, params: UploadParams) -> BackendResult<()> {
        let upload = self
            .upload(
                &params.session,
                &params.upload,
                "session/files/upload_abort",
            )
            .await?;
        self.forget_upload(&params.upload);
        let root = upload.workspace.root();
        upload
            .workspace
            .upload_abort(&upload.driver)
            .await
            .map_err(|e| workspace_error(e, &root))
    }

    /// `session/files/delete`. The gate first; then by descriptor: a link
    /// is unlinked, never followed. A folder with entries needs
    /// `recursive`; its entries are checked first (a check pass), then
    /// deleted by descriptor; an entry that appeared or changed between
    /// the passes stops the delete (what was deleted stays deleted).
    pub(super) async fn files_delete(&self, params: FilesDeleteParams) -> BackendResult<()> {
        self.check(&params.session, Need::Write).await?;
        self.refuse_if_locked("session/files/delete")?;
        let target = self.target(&params.session, Need::Write).await?;
        let (path, native) = target.below_root(&params.path, "delete")?;
        target.allow(UserFileAction::Delete, &native).await?;
        let meta = {
            let path = path.clone();
            target.fs(&params.path, move |fs| fs.stat(&path)).await?
        };
        let checked = if meta.kind == FileKind::Dir && params.recursive {
            Some(target.subtree_allowed(&path, &params.path).await?)
        } else {
            None
        };
        target
            .fs(&params.path, move |fs| fs.delete(&path, checked.as_ref()))
            .await
    }

    /// `session/files/rename`: by descriptor, never replacing `to`
    /// (`RENAME_NOREPLACE` on Linux, `RENAME_EXCL` on macOS), never
    /// following a link on the way. A folder's entries are checked as
    /// writes first.
    pub(super) async fn files_rename(&self, params: FilesRenameParams) -> BackendResult<()> {
        self.check(&params.session, Need::Write).await?;
        self.refuse_if_locked("session/files/rename")?;
        let target = self.target(&params.session, Need::Write).await?;
        let (from, from_native) = target.below_root(&params.from, "rename")?;
        let (to, to_native) = target.below_root(&params.to, "rename to")?;
        target.allow(UserFileAction::Write, &from_native).await?;
        target.allow(UserFileAction::Write, &to_native).await?;
        let meta = {
            let from = from.clone();
            target.fs(&params.from, move |fs| fs.stat(&from)).await?
        };
        if meta.kind == FileKind::Dir {
            target.subtree_allowed(&from, &params.from).await?;
        }
        let raw = format!("{} -> {}", params.from, params.to);
        target
            .fs(&raw, move |fs| fs.rename(&from, &to))
            .await
            .map_err(|error| match error {
                BackendError::Workspace {
                    code: Code::Exists, ..
                } => BackendError::workspace(Code::Exists, format!("`{}` exists", params.to)),
                other => other,
            })
    }

    /// `session/files/mkdir`: one new folder (0755) in a folder that
    /// exists, by descriptor.
    pub(super) async fn files_mkdir(&self, params: FilePathParams) -> BackendResult<()> {
        self.check(&params.session, Need::Write).await?;
        self.refuse_if_locked("session/files/mkdir")?;
        let target = self.target(&params.session, Need::Write).await?;
        let (path, native) = target.below_root(&params.path, "make")?;
        target.allow(UserFileAction::Write, &native).await?;
        target.fs(&params.path, move |fs| fs.mkdir(&path)).await
    }

    /// `session/terminal/open`.
    pub(super) async fn term_open(
        &self,
        params: TerminalOpenParams,
    ) -> BackendResult<TerminalOpenResult> {
        let caller = Caller::current();
        self.check(&params.session, Need::Write).await?;
        self.terminal_refusal("session/terminal/open")?;
        let target = self.target(&params.session, Need::Write).await?;
        if !target.workspace.capabilities().pty {
            return Err(BackendError::Unsupported(
                "a terminal in this workspace".into(),
            ));
        }
        if params.cols == 0 || params.rows == 0 {
            return Err(BackendError::failed(
                loop_protocol::ErrorCategory::Protocol,
                "cols and rows must be at least 1",
            ));
        }
        self.need_sweep()?;
        let limits = self.workspace_limits();
        let Some(_slot) =
            self.workspace_io
                .reserve(Kind::Terminal, &params.session.key, &caller, &limits)
        else {
            return Err(BackendError::workspace(
                Code::Limit,
                format!(
                    "too many terminals: at most {} per Session and {} per user; close one",
                    limits.terminals_per_session, limits.terminals_per_caller
                ),
            ));
        };
        let id = new_id("term");
        target
            .gate()
            .check_user_terminal(&id, target.record_session(), caller.clone())
            .await
            .map_err(|refused| {
                BackendError::workspace(Code::Refused, scrub(&refused.0, &target.root))
            })?;
        let cwd = WorkspacePath::from_native(&target.root, &target.live.opened.harness.cwd())
            .unwrap_or_else(|_| WorkspacePath::root());
        let command = params
            .command
            .clone()
            .unwrap_or_else(|| TERMINAL_COMMAND.to_owned());
        let size = PtySize {
            rows: params.rows,
            cols: params.cols,
        };
        let request = self.spawn_request(&target, &params.session.key, &command, &cwd, size);
        let control = target
            .workspace
            .spawn(request)
            .await
            .map_err(|e| target.error(e))?;
        let terminal = Arc::new(Terminal {
            id: id.clone(),
            session: params.session.key.clone(),
            owner: caller,
            live: Arc::downgrade(&target.live),
            workspace: Arc::clone(&target.workspace),
            gate: Arc::clone(target.gate()),
            process: control.id().clone(),
            control,
            command,
            size: Mutex::new(size),
            opened_ms: now_ms(),
            attached: AtomicUsize::new(0),
            detached_since: Mutex::new(Some(Instant::now())),
            closed: AtomicBool::new(false),
        });
        lock(&self.workspace_io.terminals).insert(id.clone(), terminal);
        Ok(TerminalOpenResult {
            terminal: id,
            cwd: cwd.as_str().to_owned(),
        })
    }

    /// The terminal's process: a PTY in the Session's cwd, with the
    /// Session's tool environment (the caller environment, or this
    /// process's, without the names the tool env policy removes).
    fn spawn_request(
        &self,
        target: &Target,
        key: &SessionKey,
        command: &str,
        cwd: &WorkspacePath,
        size: PtySize,
    ) -> SpawnRequest {
        let policy = loop_app_core::config::tool_env::tool_env_policy(
            &self.factory.globals.agent_dir,
            &target.live.opened.settings,
        );
        let caller_env = self
            .launch_specs
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(key)
            .and_then(|spec| spec.env.clone());
        let (mut env, inherit_env, remove_env) = match caller_env {
            Some(values) => (
                values
                    .into_iter()
                    .filter(|(name, _)| !policy.removes(name))
                    .collect::<HashMap<_, _>>(),
                false,
                Vec::new(),
            ),
            None => (
                HashMap::new(),
                true,
                std::env::vars_os()
                    .filter_map(|(name, _)| name.into_string().ok())
                    .filter(|name| policy.removes(name))
                    .collect(),
            ),
        };
        env.insert("TERM".into(), "xterm-256color".into());
        SpawnRequest {
            command: command.to_owned(),
            cwd: Some(cwd.clone()),
            env,
            inherit_env,
            remove_env,
            pty: Some(size),
            timeout_ms: None,
            cancel: None,
            detach: false,
            output_buffer_bytes: Some(self.workspace_limits().terminal_buffer_bytes as usize),
        }
    }

    /// The caller's terminal `id` of `session`, after the access check.
    async fn terminal(
        &self,
        session: &SessionRef,
        id: &str,
        what: &str,
    ) -> BackendResult<Arc<Terminal>> {
        let caller = Caller::current();
        self.check(session, Need::Write).await?;
        self.terminal_refusal(what)?;
        lock(&self.workspace_io.terminals)
            .get(id)
            .filter(|t| t.session == session.key && same_owner(&t.owner, &caller))
            .cloned()
            .ok_or_else(unknown_terminal)
    }

    /// The newest `seq` and the exit of `terminal`, from the driver.
    async fn terminal_state(terminal: &Terminal) -> (u64, Option<ExitState>) {
        match terminal.workspace.list_processes().await {
            Ok(list) => list
                .into_iter()
                .find(|p| p.id == terminal.process)
                .map_or((0, Some(ExitState::Lost)), |p| (p.last_seq, p.exit)),
            Err(_) => (0, None),
        }
    }

    /// `session/terminal/attach`.
    pub(super) async fn term_attach(
        &self,
        params: TerminalAttachParams,
    ) -> BackendResult<(TerminalAttachResult, Box<dyn TerminalFeed>)> {
        let terminal = self
            .terminal(&params.session, &params.terminal, "session/terminal/attach")
            .await?;
        let root = terminal.workspace.root();
        let handle = terminal
            .workspace
            .attach_process(&terminal.process, params.after_seq)
            .await
            .map_err(|e| workspace_error(e, &root))?;
        let (last_seq, exit) = Self::terminal_state(&terminal).await;
        let feed = Feed {
            handle,
            _streaming: Streaming::new(Arc::clone(&terminal)),
        };
        Ok((
            TerminalAttachResult {
                last_seq,
                exit: exit.map(end_of),
            },
            Box::new(feed),
        ))
    }

    /// `session/terminal/detach`: only checks; the server stops the stream.
    pub(super) async fn term_detach(&self, params: TerminalParams) -> BackendResult<()> {
        self.terminal(&params.session, &params.terminal, "session/terminal/detach")
            .await
            .map(|_| ())
    }

    /// `session/terminal/input`.
    pub(super) async fn term_input(&self, params: TerminalInputParams) -> BackendResult<()> {
        let terminal = self
            .terminal(&params.session, &params.terminal, "session/terminal/input")
            .await?;
        let data = decode(&params.data)?;
        let most = self.workspace_limits().chunk_bytes as usize;
        if data.len() > most {
            return Err(BackendError::workspace(
                Code::TooLarge,
                format!("one input holds at most {most} bytes"),
            ));
        }
        let root = terminal.workspace.root();
        terminal
            .control
            .write_stdin(&data)
            .await
            .map_err(|e| workspace_error(e, &root))
    }

    /// `session/terminal/resize`.
    pub(super) async fn term_resize(&self, params: TerminalResizeParams) -> BackendResult<()> {
        let terminal = self
            .terminal(&params.session, &params.terminal, "session/terminal/resize")
            .await?;
        if params.cols == 0 || params.rows == 0 {
            return Err(BackendError::failed(
                loop_protocol::ErrorCategory::Protocol,
                "cols and rows must be at least 1",
            ));
        }
        let size = PtySize {
            rows: params.rows,
            cols: params.cols,
        };
        let root = terminal.workspace.root();
        terminal
            .control
            .resize(size)
            .await
            .map_err(|e| workspace_error(e, &root))?;
        *lock(&terminal.size) = size;
        Ok(())
    }

    /// `session/terminal/signal`.
    pub(super) async fn term_signal(&self, params: TerminalSignalParams) -> BackendResult<()> {
        let terminal = self
            .terminal(&params.session, &params.terminal, "session/terminal/signal")
            .await?;
        let signal = match params.signal {
            TerminalSignal::Interrupt => ProcessSignal::Interrupt,
            TerminalSignal::Terminate => ProcessSignal::Terminate,
            TerminalSignal::Kill => ProcessSignal::Kill,
            TerminalSignal::Hangup => ProcessSignal::Hangup,
        };
        let root = terminal.workspace.root();
        terminal
            .control
            .signal(signal)
            .await
            .map_err(|e| workspace_error(e, &root))
    }

    /// `session/terminal/close`.
    pub(super) async fn term_close(
        &self,
        params: TerminalParams,
    ) -> BackendResult<TerminalCloseResult> {
        let terminal = self
            .terminal(&params.session, &params.terminal, "session/terminal/close")
            .await?;
        let exit = self.close_terminal(terminal, "closed by the user").await;
        Ok(TerminalCloseResult { exit })
    }

    /// `session/terminal/list`: this caller's terminals of the Session.
    pub(super) async fn term_list(
        &self,
        params: SessionParams,
    ) -> BackendResult<TerminalListResult> {
        let caller = Caller::current();
        self.check(&params.session, Need::Write).await?;
        self.terminal_refusal("session/terminal/list")?;
        let mine: Vec<Arc<Terminal>> = lock(&self.workspace_io.terminals)
            .values()
            .filter(|t| t.session == params.session.key && same_owner(&t.owner, &caller))
            .cloned()
            .collect();
        let mut terminals = Vec::new();
        for terminal in mine {
            let (last_seq, exit) = Self::terminal_state(&terminal).await;
            let size = *lock(&terminal.size);
            terminals.push(TerminalInfo {
                terminal: terminal.id.clone(),
                command: terminal.command.clone(),
                cols: size.cols,
                rows: size.rows,
                last_seq,
                attached: terminal.attached.load(Ordering::Acquire) as u32,
                opened_ms: terminal.opened_ms,
                exit: exit.map(end_of),
            });
        }
        terminals.sort_by_key(|t| t.opened_ms);
        Ok(TerminalListResult { terminals })
    }

    /// Close `terminal`: kill its process tree, wait for the exit (bounded)
    /// and record its end with `reason`. Once only.
    async fn close_terminal(&self, terminal: Arc<Terminal>, reason: &str) -> Option<TerminalEnd> {
        if terminal.closed.swap(true, Ordering::AcqRel) {
            return None;
        }
        lock(&self.workspace_io.terminals).remove(&terminal.id);
        let (_, mut exit) = Self::terminal_state(&terminal).await;
        if exit.is_none() {
            let _ = terminal.control.signal(ProcessSignal::Kill).await;
            let deadline = Instant::now() + CLOSE_WAIT;
            while exit.is_none() && Instant::now() < deadline {
                tokio::time::sleep(Duration::from_millis(20)).await;
                exit = Self::terminal_state(&terminal).await.1;
            }
        }
        let end = exit.map(end_of);
        let why = match &end {
            Some(end) => format!("{reason}: {end}"),
            None => format!("{reason}: the exit is not known"),
        };
        if let Err(error) = terminal
            .gate
            .record_user_terminal_end(
                &terminal.id,
                &why,
                Some(terminal.session.0.clone()),
                terminal.owner.clone(),
            )
            .await
        {
            eprintln!("Runtime: the end of a terminal was not recorded: {error}");
        }
        end
    }

    /// Close every terminal and abort every upload: the Runtime stops.
    pub(super) async fn close_workspace_io(&self, reason: &str) {
        let terminals: Vec<Arc<Terminal>> = lock(&self.workspace_io.terminals)
            .values()
            .cloned()
            .collect();
        futures::future::join_all(
            terminals
                .into_iter()
                .map(|terminal| self.close_terminal(terminal, reason)),
        )
        .await;
        let uploads: Vec<Arc<Upload>> = lock(&self.workspace_io.uploads)
            .drain()
            .map(|(_, upload)| upload)
            .collect();
        for upload in uploads {
            let _ = upload.workspace.upload_abort(&upload.driver).await;
        }
    }

    /// Remove the upload temporary files that a killed Runtime left in the
    /// Session's folder (older than the idle expiry), once per folder.
    async fn sweep_temps_once(&self, target: &Target) {
        if !lock(&self.workspace_io.swept).insert(target.root.clone()) {
            return;
        }
        let _ = target
            .fs("", |fs| Ok(fs.sweep_upload_temps(UPLOAD_IDLE, SUBTREE_MAX)))
            .await;
    }

    /// Start the sweep task once. Only a shared Runtime can run it; without
    /// it no terminal or upload would end with its Session, so none opens.
    fn need_sweep(&self) -> BackendResult<()> {
        if self.workspace_io.sweeping.swap(true, Ordering::AcqRel) {
            return Ok(());
        }
        let Some(me) = self.me.get().cloned() else {
            self.workspace_io.sweeping.store(false, Ordering::Release);
            return Err(BackendError::Unsupported(
                "terminals and uploads need a shared Runtime".into(),
            ));
        };
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(SWEEP).await;
                let Some(runtime) = me.upgrade() else {
                    return;
                };
                runtime.sweep().await;
            }
        });
        Ok(())
    }

    /// Whether `live` is still the loaded instance of its Session.
    fn current(&self, key: &SessionKey, live: &Weak<LiveSession>) -> Option<Arc<LiveSession>> {
        let live = live.upgrade()?;
        let loaded = self.sessions.lock().unwrap().get(key).cloned()?;
        (Arc::ptr_eq(&loaded, &live) && !live.closing.load(Ordering::Acquire)).then_some(live)
    }

    /// Close the terminals that may not stay open, and forget the uploads
    /// of Sessions that are gone or idle.
    async fn sweep(&self) {
        let terminals: Vec<Arc<Terminal>> = lock(&self.workspace_io.terminals)
            .values()
            .cloned()
            .collect();
        let detached_limit = Duration::from_secs(self.workspace_limits().detached_terminal_secs);
        for terminal in terminals {
            let reason = match self.current(&terminal.session, &terminal.live) {
                None => Some("the Session closed or was unloaded".to_owned()),
                Some(live) => match sandbox_reason(&live).await {
                    Some(_) => Some("the Session's sandbox was turned on".to_owned()),
                    None => match terminal.gate.user_terminal_allowed().await {
                        Err(reason) => Some(format!("the Session no longer allows it: {reason}")),
                        Ok(()) => lock(&terminal.detached_since)
                            .filter(|since| since.elapsed() >= detached_limit)
                            .map(|_| {
                                format!(
                                    "no client streamed it for {} seconds",
                                    detached_limit.as_secs()
                                )
                            }),
                    },
                },
            };
            if let Some(reason) = reason {
                self.close_terminal(terminal, &reason).await;
            }
        }
        // Too many detached terminals in one Session: the ones detached
        // longest close.
        let most = self.workspace_limits().detached_terminals_per_session as usize;
        let mut detached: HashMap<SessionKey, Vec<(Instant, Arc<Terminal>)>> = HashMap::new();
        for terminal in lock(&self.workspace_io.terminals).values() {
            if let Some(since) = *lock(&terminal.detached_since) {
                detached
                    .entry(terminal.session.clone())
                    .or_default()
                    .push((since, Arc::clone(terminal)));
            }
        }
        for (_, mut list) in detached {
            if list.len() <= most {
                continue;
            }
            list.sort_by_key(|(since, _)| *since);
            let extra = list.len() - most;
            for (_, terminal) in list.into_iter().take(extra) {
                let reason = format!(
                    "more than {most} terminals of the Session had no client streaming them"
                );
                self.close_terminal(terminal, &reason).await;
            }
        }
        let gone: Vec<(String, Arc<Upload>)> = lock(&self.workspace_io.uploads)
            .iter()
            .filter(|(_, upload)| {
                self.current(&upload.session, &upload.live).is_none()
                    || lock(&upload.touched).elapsed() >= UPLOAD_IDLE
            })
            .map(|(id, upload)| (id.clone(), Arc::clone(upload)))
            .collect();
        for (id, upload) in gone {
            self.forget_upload(&id);
            let _ = upload.workspace.upload_abort(&upload.driver).await;
        }
    }
}

fn join(folder: &str, name: &str) -> String {
    if folder.is_empty() {
        name.to_owned()
    } else {
        format!("{folder}/{name}")
    }
}

#[async_trait::async_trait]
impl loop_wire::SessionWorkspace for Runtime {
    async fn info(&self, params: SessionParams) -> BackendResult<SessionWorkspaceInfo> {
        self.workspace_info(params).await
    }
    async fn list(&self, params: FilesListParams) -> BackendResult<FilesListResult> {
        self.files_list(params).await
    }
    async fn stat(&self, params: FilePathParams) -> BackendResult<FileEntry> {
        self.files_stat(params).await
    }
    async fn read(&self, params: FilesReadParams) -> BackendResult<FilesReadResult> {
        self.files_read(params).await
    }
    async fn upload_begin(&self, params: UploadBeginParams) -> BackendResult<UploadBeginResult> {
        self.files_upload_begin(params).await
    }
    async fn upload_write(&self, params: UploadWriteParams) -> BackendResult<UploadProgress> {
        self.files_upload_write(params).await
    }
    async fn upload_status(&self, params: UploadParams) -> BackendResult<UploadProgress> {
        self.files_upload_status(params).await
    }
    async fn upload_commit(&self, params: UploadParams) -> BackendResult<UploadCommitResult> {
        self.files_upload_commit(params).await
    }
    async fn upload_abort(&self, params: UploadParams) -> BackendResult<()> {
        self.files_upload_abort(params).await
    }
    async fn delete(&self, params: FilesDeleteParams) -> BackendResult<()> {
        self.files_delete(params).await
    }
    async fn rename(&self, params: FilesRenameParams) -> BackendResult<()> {
        self.files_rename(params).await
    }
    async fn mkdir(&self, params: FilePathParams) -> BackendResult<()> {
        self.files_mkdir(params).await
    }
    async fn terminal_open(&self, params: TerminalOpenParams) -> BackendResult<TerminalOpenResult> {
        self.term_open(params).await
    }
    async fn terminal_attach(
        &self,
        params: TerminalAttachParams,
    ) -> BackendResult<(TerminalAttachResult, Box<dyn TerminalFeed>)> {
        self.term_attach(params).await
    }
    async fn terminal_detach(&self, params: TerminalParams) -> BackendResult<()> {
        self.term_detach(params).await
    }
    async fn terminal_input(&self, params: TerminalInputParams) -> BackendResult<()> {
        self.term_input(params).await
    }
    async fn terminal_resize(&self, params: TerminalResizeParams) -> BackendResult<()> {
        self.term_resize(params).await
    }
    async fn terminal_signal(&self, params: TerminalSignalParams) -> BackendResult<()> {
        self.term_signal(params).await
    }
    async fn terminal_close(&self, params: TerminalParams) -> BackendResult<TerminalCloseResult> {
        self.term_close(params).await
    }
    async fn terminal_list(&self, params: SessionParams) -> BackendResult<TerminalListResult> {
        self.term_list(params).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn runtime(dir: &Path) -> Arc<Runtime> {
        use loop_app_core::runtime::{BootstrapOpts, GlobalResources};
        let models = Arc::new(loop_ai::Models::create(Default::default()));
        models.set_provider(loop_ai::providers::faux_provider_fn(Arc::new(|_| {
            Box::pin(std::future::ready(
                loop_ai::providers::faux::FauxResponse::Text("ok".into()),
            ))
        })));
        let globals = GlobalResources::with_models(dir.join("agent"), models).unwrap();
        let ws = dir.join("ws");
        std::fs::create_dir_all(&ws).unwrap();
        let opts = BootstrapOpts {
            cwd: ws.canonicalize().unwrap(),
            provider: Some("faux".into()),
            model: Some("faux-model".into()),
            theme: None,
            system_prompt: None,
            append_system_prompt: None,
            no_context_files: true,
            interactive: false,
            session_id: None,
            no_web: true,
            permission_mode: Some("full-access".into()),
            agent: None,
        };
        let store = globals.open_runtime_session_store().unwrap();
        Runtime::new(
            globals,
            opts,
            Arc::new(crate::log::MemoryEventLog::new()),
            store,
            super::super::RuntimeLimits::default(),
        )
        .await
        .unwrap()
        .into_shared()
    }

    fn turn_sandbox_on(rt: &Runtime, session: &SessionRef) {
        let live = rt.route(session).unwrap();
        *lock(&live.sandbox_choice) = loop_protocol::admin::SandboxChoice {
            mode: "local".into(),
            isolation: None,
            runtime: None,
        };
    }

    /// With the Session's sandbox on, no file call and no terminal is
    /// served: the sandbox drivers do neither yet, and the host would
    /// escape the sandbox. An open host terminal closes, with a record.
    #[tokio::test]
    async fn a_sandboxed_session_serves_no_files_and_no_terminal() {
        use loop_protocol::AgentBackend;
        let dir = tempfile::tempdir().unwrap();
        let rt = runtime(dir.path()).await;
        let session = rt.new_session(Default::default()).await.unwrap();
        let open = || TerminalOpenParams {
            session: session.clone(),
            cols: 80,
            rows: 24,
            command: Some("sleep 300".into()),
        };
        // Off: a terminal opens.
        let terminal = rt.term_open(open()).await.unwrap().terminal;
        turn_sandbox_on(&rt, &session);
        let info = rt
            .workspace_info(SessionParams {
                session: session.clone(),
            })
            .await
            .unwrap();
        assert!(info.sandboxed);
        assert!(!info.files.available && !info.files_write.available && !info.terminal.available);
        let list = rt
            .files_list(FilesListParams {
                session: session.clone(),
                path: String::new(),
                cursor: None,
                limit: None,
            })
            .await
            .unwrap_err();
        assert!(matches!(list, BackendError::Unsupported(_)), "{list:?}");
        let read = rt
            .files_read(FilesReadParams {
                session: session.clone(),
                path: "x".into(),
                offset: 0,
                length: None,
                if_match: None,
            })
            .await
            .unwrap_err();
        assert!(matches!(read, BackendError::Unsupported(_)), "{read:?}");
        let again = rt.term_open(open()).await.unwrap_err();
        assert!(matches!(again, BackendError::Unsupported(_)), "{again:?}");
        // The open one closes within a sweep or two.
        tokio::time::timeout(Duration::from_secs(10), async {
            while lock(&rt.workspace_io.terminals).contains_key(&terminal) {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .unwrap();
        rt.shutdown().await;
        let left = loop_agent::harness::sandbox::leaked_host_processes();
        assert!(left.is_empty(), "{left:?}");
    }

    #[test]
    fn ids_are_random_and_prefixed() {
        let a = new_id("term");
        let b = new_id("term");
        assert_ne!(a, b);
        assert!(a.starts_with("term-") && a.len() == 5 + 32, "{a}");
    }

    #[test]
    fn host_paths_are_scrubbed_from_messages() {
        let root = Path::new("/srv/users/alice");
        assert_eq!(
            scrub("cannot open /srv/users/alice/a/b: denied", root),
            "cannot open a/b: denied"
        );
        assert_eq!(scrub("in /srv/users/alice", root), "in .");
    }

    #[test]
    fn owners_compare_tenant_and_principal() {
        use loop_protocol::{Principal, PrincipalKind, Tenant};
        let edge = |id: &str, tenant: &str| {
            Some(
                Caller::asserted(
                    Principal::new(PrincipalKind::Person, "https://idp", id).unwrap(),
                    Tenant::new(tenant).unwrap(),
                )
                .unwrap(),
            )
        };
        assert!(same_owner(&edge("a", "t"), &edge("a", "t")));
        assert!(!same_owner(&edge("a", "t"), &edge("b", "t")));
        assert!(!same_owner(&edge("a", "t"), &edge("a", "u")));
        assert!(!same_owner(&edge("a", "t"), &None));
        assert!(same_owner(&None, &None));
        let host = Some(Caller::host(Principal::local_uid(501)));
        let token = Some(Caller::local_token(Principal::local_uid(501)));
        assert!(same_owner(&host, &token));
    }
}
