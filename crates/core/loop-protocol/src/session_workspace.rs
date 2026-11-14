//! The files and the terminal of a Session's workspace
//! (`session/workspace`, `session/files/*`, `session/terminal/*`).
//!
//! Paths are workspace-relative `WorkspacePath` texts: `/`-separated, no
//! leading `/`, never a host path. Bytes travel as Base64 (RFC 4648, with
//! padding) in a JSON string, because the wire has text frames only. See
//! `docs/design/files-and-terminal-protocol.md`.

use serde::{Deserialize, Serialize};

use crate::backend::SessionRef;
use crate::capabilities::ToolLock;
use crate::ids::SessionKey;

/// Most data bytes in one `session/files/read` answer or one
/// `session/files/upload_write` piece (before Base64).
pub const CHUNK_BYTES_MAX: u32 = 1024 * 1024;
/// Entries in one `session/files/list` page when the request names none.
pub const LIST_PAGE_DEFAULT: u32 = 200;
/// Most entries in one `session/files/list` page.
pub const LIST_PAGE_MAX: u32 = 1000;

/// Why a file or terminal call failed, as a stable code
/// ([`crate::BackendError::Workspace`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum WorkspaceErrorCode {
    /// Bad path syntax, or a path that leads out of the Session's folder.
    InvalidPath,
    /// No such file or folder.
    NotFound,
    /// The target exists.
    Exists,
    /// `if_match` did not match: the file changed.
    Changed,
    /// The path is not a regular file.
    NotAFile,
    /// The path is not a folder.
    NotAFolder,
    /// A folder with entries, and the call did not say `recursive`.
    NotEmpty,
    /// More data than declared or allowed.
    TooLarge,
    /// An upload piece after the committed offset; resume from
    /// `session/files/upload_status`.
    UploadOffset,
    /// The SHA-256 of the upload differs.
    Checksum,
    /// No such upload for this caller and Session.
    UnknownUpload,
    /// No such terminal for this caller and Session.
    UnknownTerminal,
    /// `after_seq` is past the newest event.
    SeqAhead,
    /// A count limit (terminals, uploads).
    Limit,
    /// The permission gate refused the action; the message says why.
    Refused,
    /// Another driver error.
    Io,
    /// A code this reader does not know.
    #[serde(other)]
    Other,
}

impl WorkspaceErrorCode {
    /// The code on the wire.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::InvalidPath => "invalid_path",
            Self::NotFound => "not_found",
            Self::Exists => "exists",
            Self::Changed => "changed",
            Self::NotAFile => "not_a_file",
            Self::NotAFolder => "not_a_folder",
            Self::NotEmpty => "not_empty",
            Self::TooLarge => "too_large",
            Self::UploadOffset => "upload_offset",
            Self::Checksum => "checksum",
            Self::UnknownUpload => "unknown_upload",
            Self::UnknownTerminal => "unknown_terminal",
            Self::SeqAhead => "seq_ahead",
            Self::Limit => "limit",
            Self::Refused => "refused",
            Self::Io => "io",
            Self::Other => "other",
        }
    }
}

/// Whether a part of the panel works for this Session now.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct Availability {
    /// The calls are served (the gate still decides each one).
    pub available: bool,
    /// Why not, when not.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

impl Availability {
    /// Served.
    pub fn yes() -> Self {
        Self {
            available: true,
            reason: None,
        }
    }

    /// Not served, because of `reason`.
    pub fn no(reason: impl Into<String>) -> Self {
        Self {
            available: false,
            reason: Some(reason.into()),
        }
    }
}

/// The limits of the file and terminal calls.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct SessionWorkspaceLimits {
    /// Most data bytes in one read answer or one upload piece.
    pub chunk_bytes: u32,
    /// Largest upload, in bytes.
    pub upload_bytes: u64,
    /// Open uploads per caller.
    pub uploads_per_caller: u32,
    /// Terminals per Session.
    pub terminals_per_session: u32,
    /// Terminals per caller, for the whole Runtime.
    pub terminals_per_caller: u32,
    /// A terminal no connection streams is closed after this many seconds.
    pub detached_terminal_secs: u64,
    /// Most terminals of one Session that no connection streams; past it,
    /// the ones detached longest are closed.
    #[serde(default = "default_detached")]
    pub detached_terminals_per_session: u32,
    /// Output kept per terminal for a re-attach, in bytes.
    pub terminal_buffer_bytes: u64,
}

fn default_detached() -> u32 {
    2
}

impl Default for SessionWorkspaceLimits {
    fn default() -> Self {
        Self {
            chunk_bytes: CHUNK_BYTES_MAX,
            upload_bytes: 4 * 1024 * 1024 * 1024,
            uploads_per_caller: 8,
            terminals_per_session: 4,
            terminals_per_caller: 8,
            detached_terminal_secs: 600,
            detached_terminals_per_session: 2,
            terminal_buffer_bytes: 1024 * 1024,
        }
    }
}

/// `session/workspace` result: what the Session serves now.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct SessionWorkspaceInfo {
    /// List, stat and read.
    pub files: Availability,
    /// Upload, delete, rename and mkdir.
    pub files_write: Availability,
    /// Terminals.
    pub terminal: Availability,
    /// The Session's sandbox is on.
    pub sandboxed: bool,
    /// The Runtime's tool lock, when it has one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_lock: Option<ToolLock>,
    /// The limits.
    pub limits: SessionWorkspaceLimits,
}

/// The kind of a path, without following a final link.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum FileKind {
    /// A regular file.
    File,
    /// A folder.
    Dir,
    /// A symbolic link.
    Symlink,
    /// Anything else (socket, device, FIFO), or a kind this reader does not
    /// know.
    #[serde(other)]
    Other,
}

/// One file or folder.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct FileEntry {
    /// The last part of the path.
    pub name: String,
    /// The workspace path.
    pub path: String,
    /// The kind.
    pub kind: FileKind,
    /// Size in bytes (0 for a folder).
    pub size: u64,
    /// Last change, in Unix milliseconds, when the workspace gives it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mtime_ms: Option<u64>,
    /// The file's version (a regular file in `session/files/stat`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tag: Option<String>,
}

/// `session/files/list` params.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct FilesListParams {
    /// The Session.
    pub session: SessionRef,
    /// The folder; empty for the Session's folder.
    #[serde(default)]
    pub path: String,
    /// The `next_cursor` of the page before.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cursor: Option<String>,
    /// Entries per page ([`LIST_PAGE_DEFAULT`], at most [`LIST_PAGE_MAX`]).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<u32>,
}

/// `session/files/list` result.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct FilesListResult {
    /// The folder.
    pub path: String,
    /// Entries in name order.
    pub entries: Vec<FileEntry>,
    /// Pass as `cursor` for the next page; absent on the last page.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_cursor: Option<String>,
    /// Entries of this page that a rule hides.
    #[serde(default)]
    pub hidden: u64,
}

/// Params naming one path (`session/files/stat`, `session/files/mkdir`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct FilePathParams {
    /// The Session.
    pub session: SessionRef,
    /// The path.
    pub path: String,
}

/// `session/files/delete` params.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct FilesDeleteParams {
    /// The Session.
    pub session: SessionRef,
    /// The path. A link is removed, not its target.
    pub path: String,
    /// Remove a folder with its entries.
    #[serde(default)]
    pub recursive: bool,
}

/// `session/files/rename` params. An existing `to` is never replaced.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct FilesRenameParams {
    /// The Session.
    pub session: SessionRef,
    /// The path now.
    pub from: String,
    /// The new path.
    pub to: String,
}

/// `session/files/read` params.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct FilesReadParams {
    /// The Session.
    pub session: SessionRef,
    /// A regular file.
    pub path: String,
    /// Where to start.
    #[serde(default)]
    pub offset: u64,
    /// Most bytes ([`CHUNK_BYTES_MAX`] when absent or larger).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub length: Option<u32>,
    /// Fail with `changed` unless the file has this tag.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub if_match: Option<String>,
}

/// `session/files/read` result.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct FilesReadResult {
    /// Offset of `data` in the file.
    pub offset: u64,
    /// The bytes, Base64.
    pub data: String,
    /// The file size when it was read.
    pub total_size: u64,
    /// `data` ends at the end of the file.
    pub eof: bool,
    /// The file's version; pass it as `if_match` for the next piece.
    pub tag: String,
}

/// `session/files/upload_begin` params.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct UploadBeginParams {
    /// The Session.
    pub session: SessionRef,
    /// Where the file goes. Missing folders are made.
    pub path: String,
    /// The whole size in bytes.
    pub size: u64,
    /// The SHA-256 of the whole file, hex; checked at commit.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sha256: Option<String>,
    /// Commit only when the destination still has this tag.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub if_match: Option<String>,
    /// Replace an existing file without `if_match`. Without both, a file
    /// that exists is refused with `exists`.
    #[serde(default)]
    pub overwrite: bool,
}

/// `session/files/upload_begin` result.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct UploadBeginResult {
    /// The upload's id.
    pub upload: String,
    /// Most data bytes in one piece.
    pub chunk_max: u32,
}

/// `session/files/upload_write` params.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct UploadWriteParams {
    /// The Session.
    pub session: SessionRef,
    /// The upload.
    pub upload: String,
    /// Where this piece starts; not after the committed offset.
    pub offset: u64,
    /// The bytes, Base64.
    pub data: String,
}

/// Params naming one upload (`upload_status`, `upload_commit`,
/// `upload_abort`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct UploadParams {
    /// The Session.
    pub session: SessionRef,
    /// The upload.
    pub upload: String,
}

/// `session/files/upload_write` and `upload_status` result.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct UploadProgress {
    /// Bytes stored, from the start.
    pub committed: u64,
    /// The declared size.
    pub size: u64,
}

/// `session/files/upload_commit` result.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct UploadCommitResult {
    /// The file.
    pub path: String,
    /// Its size.
    pub size: u64,
    /// Its new version.
    pub tag: String,
}

/// `session/terminal/open` params.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct TerminalOpenParams {
    /// The Session.
    pub session: SessionRef,
    /// Columns.
    pub cols: u16,
    /// Rows.
    pub rows: u16,
    /// A command for `sh -c`; absent: the user's shell.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,
}

/// `session/terminal/open` result.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct TerminalOpenResult {
    /// The terminal's id.
    pub terminal: String,
    /// The folder it started in.
    pub cwd: String,
}

/// Params naming one terminal (`detach`, `close`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct TerminalParams {
    /// The Session.
    pub session: SessionRef,
    /// The terminal.
    pub terminal: String,
}

/// `session/terminal/attach` params.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct TerminalAttachParams {
    /// The Session.
    pub session: SessionRef,
    /// The terminal.
    pub terminal: String,
    /// The last `seq` the client has; 0 for all that is buffered.
    #[serde(default)]
    pub after_seq: u64,
}

/// `session/terminal/attach` result.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct TerminalAttachResult {
    /// The newest event's `seq` when the attach began.
    pub last_seq: u64,
    /// How it ended, when it has ended.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit: Option<TerminalEnd>,
}

/// `session/terminal/input` params.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct TerminalInputParams {
    /// The Session.
    pub session: SessionRef,
    /// The terminal.
    pub terminal: String,
    /// The bytes to type, Base64.
    pub data: String,
}

/// `session/terminal/resize` params.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct TerminalResizeParams {
    /// The Session.
    pub session: SessionRef,
    /// The terminal.
    pub terminal: String,
    /// Columns.
    pub cols: u16,
    /// Rows.
    pub rows: u16,
}

/// A signal for a terminal's process tree.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum TerminalSignal {
    /// Ctrl-C.
    Interrupt,
    /// A polite stop.
    Terminate,
    /// A forced stop of the whole tree.
    Kill,
    /// The terminal hung up.
    Hangup,
}

/// `session/terminal/signal` params.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct TerminalSignalParams {
    /// The Session.
    pub session: SessionRef,
    /// The terminal.
    pub terminal: String,
    /// The signal.
    pub signal: TerminalSignal,
}

/// How a terminal's process ended.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct TerminalEnd {
    /// `exited`, `signaled`, `timed_out`, `cancelled` or `lost`.
    pub state: String,
    /// The exit code, for `exited`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub code: Option<i32>,
    /// The signal number, for `signaled`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub signal: Option<i32>,
}

impl std::fmt::Display for TerminalEnd {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match (self.code, self.signal) {
            (Some(code), _) => write!(f, "exited with code {code}"),
            (_, Some(signal)) => write!(f, "stopped by signal {signal}"),
            _ => f.write_str(&self.state),
        }
    }
}

/// `session/terminal/close` result.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct TerminalCloseResult {
    /// How it ended, when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit: Option<TerminalEnd>,
}

/// One terminal in `session/terminal/list`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct TerminalInfo {
    /// The id.
    pub terminal: String,
    /// The command.
    pub command: String,
    /// Columns.
    pub cols: u16,
    /// Rows.
    pub rows: u16,
    /// The newest event's `seq`.
    pub last_seq: u64,
    /// Connections that stream it now.
    pub attached: u32,
    /// When it opened, in Unix milliseconds.
    pub opened_ms: u64,
    /// How it ended, when it has ended.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit: Option<TerminalEnd>,
}

/// `session/terminal/list` result: this caller's terminals of the Session.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct TerminalListResult {
    /// The terminals.
    pub terminals: Vec<TerminalInfo>,
}

/// `session/terminal/output` (server to client): one event of a terminal
/// this connection attached. Live only: it is never in the Event Log.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct TerminalOutputParams {
    /// The Session.
    pub session: SessionKey,
    /// The terminal.
    pub terminal: String,
    /// 1 for the first event, one more for each next.
    pub seq: u64,
    /// Output bytes, Base64.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<String>,
    /// This reader lost this many bytes; `seq` is the last lost event.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dropped: Option<u64>,
    /// The process ended. No event follows.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit: Option<TerminalEnd>,
}

/// Base64 (RFC 4648 §4, standard alphabet, with padding).
pub mod base64 {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

    /// Text that is not Base64.
    #[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
    #[error("the data is not valid Base64 (RFC 4648, with padding)")]
    pub struct InvalidBase64;

    /// Encode `bytes`.
    pub fn encode(bytes: &[u8]) -> String {
        let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
        for chunk in bytes.chunks(3) {
            let b = [
                chunk[0],
                chunk.get(1).copied().unwrap_or(0),
                chunk.get(2).copied().unwrap_or(0),
            ];
            let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
            let sextet = |shift: u32| char::from(ALPHABET[((n >> shift) & 63) as usize]);
            out.push(sextet(18));
            out.push(sextet(12));
            out.push(if chunk.len() > 1 { sextet(6) } else { '=' });
            out.push(if chunk.len() > 2 { sextet(0) } else { '=' });
        }
        out
    }

    fn value(byte: u8) -> Option<u32> {
        Some(u32::from(match byte {
            b'A'..=b'Z' => byte - b'A',
            b'a'..=b'z' => byte - b'a' + 26,
            b'0'..=b'9' => byte - b'0' + 52,
            b'+' => 62,
            b'/' => 63,
            _ => return None,
        }))
    }

    /// Decode `text`. Padding is required; whitespace is not allowed.
    pub fn decode(text: &str) -> Result<Vec<u8>, InvalidBase64> {
        let bytes = text.as_bytes();
        if !bytes.len().is_multiple_of(4) {
            return Err(InvalidBase64);
        }
        let mut out = Vec::with_capacity(bytes.len() / 4 * 3);
        let quads = bytes.len() / 4;
        for (index, quad) in bytes.chunks(4).enumerate() {
            let last = index + 1 == quads;
            let pad = quad.iter().rev().take_while(|b| **b == b'=').count();
            if pad > 2 || (pad > 0 && !last) {
                return Err(InvalidBase64);
            }
            let mut n = 0u32;
            for (i, byte) in quad.iter().enumerate() {
                let v = if i >= 4 - pad {
                    0
                } else {
                    value(*byte).ok_or(InvalidBase64)?
                };
                n = (n << 6) | v;
            }
            let decoded = [(n >> 16) as u8, (n >> 8) as u8, n as u8];
            // Bits after the data must be zero (canonical form).
            let keep = 3 - pad;
            if decoded[keep..].iter().any(|b| *b != 0) {
                return Err(InvalidBase64);
            }
            out.extend_from_slice(&decoded[..keep]);
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::base64::{decode, encode, InvalidBase64};
    use super::*;

    #[test]
    fn base64_matches_rfc4648_vectors() {
        let vectors = [
            ("", ""),
            ("f", "Zg=="),
            ("fo", "Zm8="),
            ("foo", "Zm9v"),
            ("foob", "Zm9vYg=="),
            ("fooba", "Zm9vYmE="),
            ("foobar", "Zm9vYmFy"),
        ];
        for (plain, coded) in vectors {
            assert_eq!(encode(plain.as_bytes()), coded);
            assert_eq!(decode(coded).unwrap(), plain.as_bytes());
        }
        let all: Vec<u8> = (0..=255u8).collect();
        assert_eq!(decode(&encode(&all)).unwrap(), all);
        assert_eq!(encode(&[0xfb, 0xff]), "+/8=");
    }

    #[test]
    fn base64_refuses_what_is_not_canonical() {
        for bad in [
            "Zg", "Zg=", "Z===", "Zg==Zg==", "Zh==", "Zm9 ", "Zm9v\n", "Zm=v",
        ] {
            assert_eq!(decode(bad), Err(InvalidBase64), "{bad}");
        }
    }

    #[test]
    fn unknown_codes_and_kinds_read_as_other() {
        let code: WorkspaceErrorCode = serde_json::from_value(serde_json::json!("new")).unwrap();
        assert_eq!(code, WorkspaceErrorCode::Other);
        let kind: FileKind = serde_json::from_value(serde_json::json!("door")).unwrap();
        assert_eq!(kind, FileKind::Other);
        assert_eq!(
            serde_json::to_value(WorkspaceErrorCode::UploadOffset).unwrap(),
            "upload_offset"
        );
        for code in [WorkspaceErrorCode::SeqAhead, WorkspaceErrorCode::NotAFile] {
            assert_eq!(serde_json::to_value(code).unwrap(), code.as_str());
        }
    }
}
