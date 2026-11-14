//! Chunked download and upload sessions of a host Workspace.
//!
//! # Paths
//!
//! A [`WorkspacePath`] is joined to the Workspace root (canonical). Every
//! directory on the way is opened and checked by descriptor: the kernel's
//! name of the open directory must be inside the root, so a link that
//! leads out is refused, also one swapped in during the call. A link that
//! stays inside the root is followed. The final component of an upload is
//! never followed: a link there is refused. Downloads open the file and
//! check the open file the same way; under the tool lock they use the
//! lock's own rule (`open_confined`: a regular file with one link).
//!
//! # Upload
//!
//! `begin_upload` opens the destination folder and creates a temporary
//! file next to the destination (`.<name>.loop-upload-<id>`, `O_EXCL`,
//! mode 0600). Writes go to it with `pwrite`. `upload_commit` checks the
//! size and the SHA-256, syncs the file, sets the mode (an existing file
//! keeps its mode; a new one gets the request's mode or 0644, without the
//! umask), checks `if_match` against the destination again and renames the
//! temporary file over the destination in the same folder, then syncs the
//! folder. A reader sees the old file or the new one, never a part.
//!
//! Between the last `if_match` check and the rename another writer can
//! still change the destination: POSIX has no compare-and-rename.
//!
//! # Limits
//!
//! [`MAX_UPLOAD_BYTES`] per upload, [`MAX_UPLOADS`] open uploads per
//! Workspace, and an idle time ([`UPLOAD_IDLE`] or the `uploadIdleMs`
//! option) after which an upload expires. Expired uploads and their
//! temporary files are removed at the next transfer call, at `stop`, at
//! `destroy` and when the Workspace is dropped. Temporary files of a
//! Runtime that was killed stay in the folder.

use std::collections::HashMap;
use std::ffi::OsString;
use std::fs::File;
use std::os::fd::{AsFd, OwnedFd};
use std::os::unix::fs::{FileExt, MetadataExt};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use loop_workspace::{
    FileChunk, FileError, FileErrorCode, FileTag, UploadId, UploadRequest, WorkspaceError,
    WorkspacePath,
};
use parking_lot::Mutex;
use rustix::fs::{AtFlags, Mode, OFlags};
use sha2::Digest;

use crate::harness::env::{open_confined, opened_path};

/// Largest upload.
pub(crate) const MAX_UPLOAD_BYTES: u64 = 4 * 1024 * 1024 * 1024;
/// Most open uploads per Workspace.
pub(crate) const MAX_UPLOADS: usize = 16;
/// Default idle time after which an upload expires.
pub(crate) const UPLOAD_IDLE: Duration = Duration::from_secs(10 * 60);
/// Largest chunk `read_chunk` returns.
pub(crate) const MAX_CHUNK: usize = 8 * 1024 * 1024;
/// Mode of a new file without a requested mode.
const DEFAULT_MODE: u32 = 0o644;

fn file_error(code: FileErrorCode, message: impl Into<String>) -> WorkspaceError {
    WorkspaceError::File(FileError::new(code, message))
}

fn io(path: &str, e: std::io::Error) -> WorkspaceError {
    let code = match e.kind() {
        std::io::ErrorKind::NotFound => FileErrorCode::NotFound,
        std::io::ErrorKind::PermissionDenied => FileErrorCode::PermissionDenied,
        std::io::ErrorKind::AlreadyExists => FileErrorCode::AlreadyExists,
        _ => FileErrorCode::Io,
    };
    file_error(code, format!("{path}: {e}"))
}

fn errno(path: &str, e: rustix::io::Errno) -> WorkspaceError {
    io(path, e.into())
}

fn invalid(path: &WorkspacePath, reason: &'static str) -> WorkspaceError {
    WorkspaceError::InvalidPath {
        path: path.as_str().to_owned(),
        reason,
    }
}

/// The tag of a file from its identity and time stamps.
fn tag(dev: u64, ino: u64, size: u64, mtime_ns: i128, ctime_ns: i128) -> FileTag {
    FileTag(format!(
        "v1-{dev:x}-{ino:x}-{size:x}-{mtime_ns:x}-{ctime_ns:x}"
    ))
}

fn tag_of_metadata(meta: &std::fs::Metadata) -> FileTag {
    tag(
        meta.dev(),
        meta.ino(),
        meta.len(),
        i128::from(meta.mtime()) * 1_000_000_000 + i128::from(meta.mtime_nsec()),
        i128::from(meta.ctime()) * 1_000_000_000 + i128::from(meta.ctime_nsec()),
    )
}

#[allow(clippy::unnecessary_cast)]
fn tag_of_stat(st: &rustix::fs::Stat) -> FileTag {
    tag(
        st.st_dev as u64,
        st.st_ino as u64,
        st.st_size as u64,
        i128::from(st.st_mtime as i64) * 1_000_000_000 + i128::from(st.st_mtime_nsec as i64),
        i128::from(st.st_ctime as i64) * 1_000_000_000 + i128::from(st.st_ctime_nsec as i64),
    )
}

/// Whether the open directory `dir` is inside `root`, by the kernel's name.
fn inside(dir: &File, root: &Path) -> bool {
    opened_path(dir).is_some_and(|real| real.starts_with(root))
}

/// Open the directory `rel` under `root`, checking each directory by
/// descriptor. With `create`, missing directories are made (0755), each
/// only inside a directory already checked.
fn open_dir(root: &Path, rel: &WorkspacePath, create: bool) -> Result<File, WorkspaceError> {
    let flags = OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC;
    let mut dir = File::from(
        rustix::fs::open(root, flags, Mode::empty()).map_err(|e| errno(rel.as_str(), e))?,
    );
    for segment in rel.segments() {
        if !inside(&dir, root) {
            return Err(invalid(rel, "leads outside the workspace root"));
        }
        let next = match rustix::fs::openat(&dir, segment, flags, Mode::empty()) {
            Err(rustix::io::Errno::NOENT) if create => {
                match rustix::fs::mkdirat(&dir, segment, Mode::from_raw_mode(0o755)) {
                    Ok(()) | Err(rustix::io::Errno::EXIST) => {}
                    Err(e) => return Err(errno(rel.as_str(), e)),
                }
                rustix::fs::openat(&dir, segment, flags, Mode::empty())
            }
            other => other,
        }
        .map_err(|e| match e {
            rustix::io::Errno::NOTDIR => invalid(rel, "has a component that is not a folder"),
            e => errno(rel.as_str(), e),
        })?;
        dir = File::from(next);
    }
    if !inside(&dir, root) {
        return Err(invalid(rel, "leads outside the workspace root"));
    }
    Ok(dir)
}

/// What is at the final component, without following a link.
enum Target {
    Missing,
    File(FileTag, u32),
}

fn target(dir: &File, name: &OsString, path: &WorkspacePath) -> Result<Target, WorkspaceError> {
    match rustix::fs::statat(dir, name.as_os_str(), AtFlags::SYMLINK_NOFOLLOW) {
        Err(rustix::io::Errno::NOENT) => Ok(Target::Missing),
        Err(e) => Err(errno(path.as_str(), e)),
        Ok(st) => match rustix::fs::FileType::from_raw_mode(st.st_mode) {
            rustix::fs::FileType::RegularFile => {
                #[allow(clippy::unnecessary_cast)]
                let mode = (st.st_mode as u32) & 0o7777;
                Ok(Target::File(tag_of_stat(&st), mode))
            }
            rustix::fs::FileType::Symlink => Err(invalid(path, "is a symbolic link")),
            rustix::fs::FileType::Directory => Err(invalid(path, "is a folder")),
            _ => Err(invalid(path, "is not a regular file")),
        },
    }
}

fn check_if_match(
    path: &WorkspacePath,
    current: &Target,
    want: Option<&FileTag>,
) -> Result<(), WorkspaceError> {
    match (want, current) {
        (None, _) => Ok(()),
        (Some(want), Target::File(tag, _)) if tag == want => Ok(()),
        (Some(_), _) => Err(WorkspaceError::Changed {
            path: path.as_str().to_owned(),
        }),
    }
}

struct Upload {
    path: WorkspacePath,
    dir: File,
    name: OsString,
    temp: OsString,
    file: File,
    size: u64,
    sha256: Option<String>,
    if_match: Option<FileTag>,
    mode: Option<u32>,
    committed: u64,
    touched: Instant,
}

impl Upload {
    fn remove_temp(&self) {
        let _ = rustix::fs::unlinkat(&self.dir, self.temp.as_os_str(), AtFlags::empty());
    }
}

/// The transfer state of one Workspace.
pub(crate) struct Transfers {
    root: PathBuf,
    confined: bool,
    idle: Duration,
    uploads: Mutex<HashMap<String, Arc<Mutex<Upload>>>>,
}

impl Transfers {
    pub(crate) fn new(root: PathBuf, confined: bool, idle: Duration) -> Self {
        Self {
            root,
            confined,
            idle,
            uploads: Mutex::new(HashMap::new()),
        }
    }

    /// Remove uploads idle for longer than the idle time, and their files.
    fn expire(&self) {
        let now = Instant::now();
        let mut uploads = self.uploads.lock();
        uploads.retain(|_, upload| {
            // An upload in use (locked) is not idle.
            let Some(u) = upload.try_lock() else {
                return true;
            };
            let keep = now.duration_since(u.touched) < self.idle;
            if !keep {
                u.remove_temp();
            }
            keep
        });
    }

    /// Remove every upload and its temporary file.
    pub(crate) fn clear(&self) {
        let all: Vec<_> = self.uploads.lock().drain().map(|(_, u)| u).collect();
        for upload in all {
            upload.lock().remove_temp();
        }
    }

    fn get(&self, id: &UploadId) -> Result<Arc<Mutex<Upload>>, WorkspaceError> {
        self.expire();
        self.uploads
            .lock()
            .get(&id.0)
            .cloned()
            .ok_or_else(|| WorkspaceError::UnknownUpload(id.0.clone()))
    }

    pub(crate) fn begin(
        &self,
        workspace_id: &str,
        request: UploadRequest,
    ) -> Result<UploadId, WorkspaceError> {
        self.expire();
        if request.size > MAX_UPLOAD_BYTES {
            return Err(WorkspaceError::TooLarge {
                size: request.size,
                limit: MAX_UPLOAD_BYTES,
            });
        }
        if request.mode.is_some_and(|m| m & !0o7777 != 0) {
            return Err(invalid(
                &request.path,
                "has a mode with more than permission bits",
            ));
        }
        if request
            .sha256
            .as_ref()
            .is_some_and(|h| h.len() != 64 || !h.bytes().all(|b| b.is_ascii_hexdigit()))
        {
            return Err(invalid(
                &request.path,
                "has a SHA-256 that is not 64 hex digits",
            ));
        }
        if self.uploads.lock().len() >= MAX_UPLOADS {
            return Err(WorkspaceError::Busy {
                id: workspace_id.to_owned(),
                operation: "begin_upload",
            });
        }
        let path = request.path.clone();
        let (Some(parent), Some(name)) = (path.parent(), path.file_name()) else {
            return Err(invalid(&path, "names the workspace root"));
        };
        let name = OsString::from(name);
        let dir = open_dir(&self.root, &parent, true)?;
        let current = target(&dir, &name, &path)?;
        check_if_match(&path, &current, request.if_match.as_ref())?;

        let id = format!("upload-{}", loop_ai::new_id());
        let mut temp = OsString::from(".");
        temp.push(&name);
        temp.push(format!(".loop-upload-{id}"));
        let fd: OwnedFd = rustix::fs::openat(
            &dir,
            temp.as_os_str(),
            OFlags::RDWR | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::from_raw_mode(0o600),
        )
        .map_err(|e| errno(path.as_str(), e))?;
        let upload = Upload {
            path,
            dir,
            name,
            temp,
            file: File::from(fd),
            size: request.size,
            sha256: request.sha256.map(|h| h.to_ascii_lowercase()),
            if_match: request.if_match,
            mode: request.mode,
            committed: 0,
            touched: Instant::now(),
        };
        self.uploads
            .lock()
            .insert(id.clone(), Arc::new(Mutex::new(upload)));
        Ok(UploadId(id))
    }

    pub(crate) fn write(
        &self,
        id: &UploadId,
        offset: u64,
        data: &[u8],
    ) -> Result<u64, WorkspaceError> {
        let upload = self.get(id)?;
        let mut u = upload.lock();
        u.touched = Instant::now();
        if offset > u.committed {
            return Err(WorkspaceError::UploadOffset {
                upload: id.0.clone(),
                offset,
                committed: u.committed,
            });
        }
        let end = offset + data.len() as u64;
        if end > u.size {
            return Err(WorkspaceError::TooLarge {
                size: end,
                limit: u.size,
            });
        }
        u.file
            .write_all_at(data, offset)
            .map_err(|e| io(u.path.as_str(), e))?;
        u.committed = u.committed.max(end);
        u.touched = Instant::now();
        Ok(u.committed)
    }

    pub(crate) fn committed(&self, id: &UploadId) -> Result<u64, WorkspaceError> {
        let upload = self.get(id)?;
        let mut u = upload.lock();
        u.touched = Instant::now();
        Ok(u.committed)
    }

    pub(crate) fn abort(&self, id: &UploadId) {
        if let Some(upload) = self.uploads.lock().remove(&id.0) {
            upload.lock().remove_temp();
        }
        self.expire();
    }

    pub(crate) fn commit(&self, id: &UploadId) -> Result<FileTag, WorkspaceError> {
        let upload = self.get(id)?;
        let mut u = upload.lock();
        u.touched = Instant::now();
        if u.committed != u.size {
            return Err(WorkspaceError::UploadOffset {
                upload: id.0.clone(),
                offset: u.size,
                committed: u.committed,
            });
        }
        // From here on the upload ends, whatever the result.
        self.uploads.lock().remove(&id.0);
        let result = self.finish(&u, id);
        if result.is_err() {
            u.remove_temp();
        }
        result
    }

    fn finish(&self, u: &Upload, id: &UploadId) -> Result<FileTag, WorkspaceError> {
        let path = u.path.as_str();
        // The temporary file may have been made longer by nobody but us;
        // still cut it to the declared size.
        u.file.set_len(u.size).map_err(|e| io(path, e))?;
        if let Some(want) = &u.sha256 {
            let mut hasher = sha2::Sha256::new();
            let mut buf = vec![0u8; 1024 * 1024];
            let mut at = 0u64;
            while at < u.size {
                let n = u.file.read_at(&mut buf, at).map_err(|e| io(path, e))?;
                if n == 0 {
                    break;
                }
                hasher.update(&buf[..n]);
                at += n as u64;
            }
            let got: String = hasher
                .finalize()
                .iter()
                .map(|b| format!("{b:02x}"))
                .collect();
            if &got != want {
                return Err(WorkspaceError::ChecksumMismatch(id.0.clone()));
            }
        }
        u.file.sync_all().map_err(|e| io(path, e))?;
        let current = target(&u.dir, &u.name, &u.path)?;
        check_if_match(&u.path, &current, u.if_match.as_ref())?;
        let mode = match current {
            Target::File(_, mode) => mode,
            Target::Missing => u.mode.unwrap_or(DEFAULT_MODE),
        };
        rustix::fs::fchmod(u.file.as_fd(), Mode::from_raw_mode(mode as _))
            .map_err(|e| errno(path, e))?;
        rustix::fs::renameat(&u.dir, u.temp.as_os_str(), &u.dir, u.name.as_os_str())
            .map_err(|e| errno(path, e))?;
        // The rename is durable once the folder is synced.
        rustix::fs::fsync(&u.dir).map_err(|e| errno(path, e))?;
        let meta = u.file.metadata().map_err(|e| io(path, e))?;
        Ok(tag_of_metadata(&meta))
    }

    pub(crate) fn read_chunk(
        &self,
        path: &WorkspacePath,
        offset: u64,
        max_len: usize,
        if_match: Option<&FileTag>,
    ) -> Result<FileChunk, WorkspaceError> {
        let native = path.to_native(&self.root);
        let (file, meta) = if self.confined {
            open_confined(&native, &self.root).map_err(WorkspaceError::File)?
        } else {
            let (Some(parent), Some(name)) = (path.parent(), path.file_name()) else {
                return Err(invalid(path, "names the workspace root"));
            };
            let dir = open_dir(&self.root, &parent, false)?;
            let fd = rustix::fs::openat(
                &dir,
                name,
                OFlags::RDONLY | OFlags::NONBLOCK | OFlags::CLOEXEC,
                Mode::empty(),
            )
            .map_err(|e| errno(path.as_str(), e))?;
            let file = File::from(fd);
            let meta = file.metadata().map_err(|e| io(path.as_str(), e))?;
            if !meta.is_file() {
                return Err(invalid(path, "is not a regular file"));
            }
            match opened_path(&file) {
                Some(real) if real.starts_with(&self.root) => {}
                _ => return Err(invalid(path, "leads outside the workspace root")),
            }
            (file, meta)
        };
        let before = tag_of_metadata(&meta);
        let changed = || WorkspaceError::Changed {
            path: path.as_str().to_owned(),
        };
        if if_match.is_some_and(|want| *want != before) {
            return Err(changed());
        }
        let size = meta.len();
        let want = max_len.min(MAX_CHUNK) as u64;
        let len = size.saturating_sub(offset).min(want) as usize;
        let mut data = vec![0u8; len];
        let mut got = 0;
        while got < len {
            let n = file
                .read_at(&mut data[got..], offset + got as u64)
                .map_err(|e| io(path.as_str(), e))?;
            if n == 0 {
                break;
            }
            got += n;
        }
        if got < len {
            // The file got shorter while it was read.
            return Err(changed());
        }
        let after = tag_of_metadata(&file.metadata().map_err(|e| io(path.as_str(), e))?);
        if after != before {
            return Err(changed());
        }
        Ok(FileChunk {
            offset,
            eof: offset + len as u64 >= size,
            data,
            total_size: size,
            tag: before,
        })
    }
}

impl Drop for Transfers {
    fn drop(&mut self) {
        self.clear();
    }
}
