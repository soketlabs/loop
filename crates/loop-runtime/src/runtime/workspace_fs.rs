//! Descriptor-based file operations in a Session's folder, for the file
//! panel (`session/files/list`, `stat`, `delete`, `rename`, `mkdir`, and
//! the checks around uploads).
//!
//! Every path is walked from one descriptor of the Session's folder, one
//! component at a time, with `O_NOFOLLOW | O_DIRECTORY`. No link is ever
//! followed: a path that passes through a link fails with [`FsError::Link`],
//! and a final link is acted on as a link (unlinked, renamed, reported as
//! a link), never through it. A folder swapped for a link during a call
//! therefore cannot lead the call outside: the walk holds the descriptor of
//! the folder it checked, and every operation is relative to it
//! (`openat`, `statat`, `unlinkat`, `mkdirat`, `renameat`).
//!
//! These calls block; the Runtime runs them on the blocking pool.

use std::collections::HashMap;
use std::path::Path;

use loop_agent::harness::sandbox::WorkspacePath;
use loop_protocol::session_workspace::FileKind;

/// Why an operation failed.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub(super) enum FsError {
    /// The path, or a folder on the way, does not exist.
    #[error("it does not exist")]
    NotFound,
    /// The target exists.
    #[error("it exists")]
    Exists,
    /// The path passes through a link or a file where a folder must be.
    #[error("it passes through a link or a file; the file panel does not follow links")]
    Link,
    /// A folder with entries.
    #[error("the folder is not empty")]
    NotEmpty,
    /// An entry changed or appeared after the check; nothing more was done.
    #[error("`{0}` changed after it was checked; nothing more was deleted")]
    Changed(String),
    /// An entry the user may not change (`.git`, `.loop`).
    #[error("`{0}` cannot be changed from the file panel")]
    Protected(String),
    /// More entries than a walk checks.
    #[error("it holds more than {0} entries")]
    TooMany(usize),
    /// This system cannot do it safely.
    #[error("{0} is not supported on this system")]
    Unsupported(&'static str),
    /// Another system error.
    #[error("{0}")]
    Io(String),
}

/// What a path is, without following a final link.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct Meta {
    pub(super) kind: FileKind,
    pub(super) size: u64,
    pub(super) mtime_ms: Option<u64>,
}

/// Whether `name` is a protected name (`.git`, `.loop`, any case).
pub(super) fn protected_name(name: &str) -> bool {
    name.eq_ignore_ascii_case(".git") || name.eq_ignore_ascii_case(".loop")
}

/// Whether `name` is a temporary file of an upload (`.<name>.loop-upload-upload-<id>`).
pub(super) fn upload_temp_name(name: &str) -> bool {
    name.starts_with('.') && name.contains(".loop-upload-upload-")
}

#[cfg(unix)]
mod imp {
    use super::*;
    use rustix::fs::{AtFlags, FileType, Mode, OFlags};
    use rustix::io::Errno;
    use std::os::fd::OwnedFd;

    fn dir_flags() -> OFlags {
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC
    }

    fn io(e: Errno) -> FsError {
        match e {
            Errno::NOENT => FsError::NotFound,
            Errno::EXIST => FsError::Exists,
            Errno::LOOP | Errno::NOTDIR => FsError::Link,
            Errno::NOTEMPTY => FsError::NotEmpty,
            e => FsError::Io(std::io::Error::from(e).to_string()),
        }
    }

    fn kind_of(mode: rustix::fs::RawMode) -> FileKind {
        match FileType::from_raw_mode(mode) {
            FileType::RegularFile => FileKind::File,
            FileType::Directory => FileKind::Dir,
            FileType::Symlink => FileKind::Symlink,
            _ => FileKind::Other,
        }
    }

    fn meta_of(st: &rustix::fs::Stat) -> Meta {
        let kind = kind_of(st.st_mode);
        #[allow(clippy::unnecessary_cast)]
        let mtime = st.st_mtime as i64;
        Meta {
            kind,
            #[allow(clippy::unnecessary_cast)]
            size: if kind == FileKind::Dir {
                0
            } else {
                st.st_size as u64
            },
            mtime_ms: u64::try_from(mtime).ok().map(|s| s.saturating_mul(1000)),
        }
    }

    /// The Session's folder, opened once. Its path is the canonical root,
    /// so no component of it is a link; the final one is not followed.
    pub(in super::super) struct Root(OwnedFd);

    impl Root {
        pub(in super::super) fn open(root: &Path) -> Result<Self, FsError> {
            rustix::fs::open(root, dir_flags(), Mode::empty())
                .map(Self)
                .map_err(io)
        }

        /// The folder `segments` below the root, each opened by descriptor
        /// without following a link.
        fn dir<'a>(&self, segments: impl Iterator<Item = &'a str>) -> Result<OwnedFd, FsError> {
            let mut dir = rustix::io::dup(&self.0).map_err(io)?;
            for segment in segments {
                dir = rustix::fs::openat(&dir, segment, dir_flags(), Mode::empty()).map_err(io)?;
            }
            Ok(dir)
        }

        /// The parent folder of `path` (not the root) and its last name.
        fn parent<'p>(&self, path: &'p WorkspacePath) -> Result<(OwnedFd, &'p str), FsError> {
            let segments: Vec<&str> = path.segments().collect();
            let (name, parents) = segments.split_last().ok_or(FsError::NotFound)?;
            Ok((self.dir(parents.iter().copied())?, name))
        }

        /// What `path` is, without following a final link.
        pub(in super::super) fn stat(&self, path: &WorkspacePath) -> Result<Meta, FsError> {
            if path.as_str().is_empty() {
                return rustix::fs::fstat(&self.0)
                    .map(|st| meta_of(&st))
                    .map_err(io);
            }
            let (dir, name) = self.parent(path)?;
            rustix::fs::statat(&dir, name, AtFlags::SYMLINK_NOFOLLOW)
                .map(|st| meta_of(&st))
                .map_err(io)
        }

        /// Whether `path` exists (a link counts as existing).
        pub(in super::super) fn exists(&self, path: &WorkspacePath) -> Result<bool, FsError> {
            match self.stat(path) {
                Ok(_) => Ok(true),
                Err(FsError::NotFound) => Ok(false),
                Err(e) => Err(e),
            }
        }

        /// The folder `path`: its entries after `after` in name order, at
        /// most `limit` shown ones, with what each is. `shown` decides on
        /// the workspace path of each entry; hidden ones are counted. The
        /// second value is the name to continue after, when more are left.
        #[allow(clippy::type_complexity)]
        pub(in super::super) fn list(
            &self,
            path: &WorkspacePath,
            after: Option<&str>,
            limit: usize,
            shown: &dyn Fn(&str) -> bool,
        ) -> Result<(Vec<(String, Meta)>, Option<String>, u64), FsError> {
            let dir = self.dir(path.segments())?;
            let mut names = Vec::new();
            for entry in rustix::fs::Dir::read_from(&dir).map_err(io)? {
                let entry = entry.map_err(io)?;
                let name = entry.file_name().to_string_lossy().into_owned();
                if name != "." && name != ".." {
                    names.push(name);
                }
            }
            names.sort();
            let mut out = Vec::new();
            let mut hidden = 0;
            let mut next = None;
            for name in names
                .into_iter()
                .filter(|n| after.is_none_or(|after| n.as_str() > after))
            {
                let rel = if path.as_str().is_empty() {
                    name.clone()
                } else {
                    format!("{}/{name}", path.as_str())
                };
                if !shown(&rel) {
                    hidden += 1;
                    continue;
                }
                if out.len() == limit {
                    next = out.last().map(|(n, _): &(String, Meta)| n.clone());
                    break;
                }
                // Gone between the listing and the stat: not there.
                match rustix::fs::statat(&dir, name.as_str(), AtFlags::SYMLINK_NOFOLLOW) {
                    Ok(st) => out.push((name, meta_of(&st))),
                    Err(Errno::NOENT) => {}
                    Err(e) => return Err(io(e)),
                }
            }
            Ok((out, next, hidden))
        }

        /// Make the folder `path` in a folder that exists (0755).
        pub(in super::super) fn mkdir(&self, path: &WorkspacePath) -> Result<(), FsError> {
            let (dir, name) = self.parent(path)?;
            rustix::fs::mkdirat(&dir, name, Mode::from_raw_mode(0o755)).map_err(io)
        }

        /// Every entry below the folder `path`, by workspace path, with its
        /// kind; links are listed, never entered. At most `cap` entries.
        pub(in super::super) fn walk(
            &self,
            path: &WorkspacePath,
            cap: usize,
        ) -> Result<HashMap<String, FileKind>, FsError> {
            let mut found = HashMap::new();
            let top = self.dir(path.segments())?;
            let mut todo = vec![(top, path.as_str().to_owned())];
            while let Some((dir, rel)) = todo.pop() {
                for entry in rustix::fs::Dir::read_from(&dir).map_err(io)? {
                    let entry = entry.map_err(io)?;
                    let name = entry.file_name().to_string_lossy().into_owned();
                    if name == "." || name == ".." {
                        continue;
                    }
                    let st =
                        match rustix::fs::statat(&dir, name.as_str(), AtFlags::SYMLINK_NOFOLLOW) {
                            Ok(st) => st,
                            Err(Errno::NOENT) => continue,
                            Err(e) => return Err(io(e)),
                        };
                    let kind = kind_of(st.st_mode);
                    let child = format!("{rel}/{name}");
                    found.insert(child.clone(), kind);
                    if found.len() > cap {
                        return Err(FsError::TooMany(cap));
                    }
                    if kind == FileKind::Dir {
                        let sub =
                            rustix::fs::openat(&dir, name.as_str(), dir_flags(), Mode::empty())
                                .map_err(io)?;
                        todo.push((sub, child));
                    }
                }
            }
            Ok(found)
        }

        /// Delete `path` (a link is unlinked, not followed). A folder needs
        /// `checked`: every entry below it as [`Self::walk`] found it and
        /// the gate allowed it. An entry that is not in `checked` with the
        /// same kind (it appeared or changed after the check) stops the
        /// delete with [`FsError::Changed`]; what was deleted before stays
        /// deleted. A protected name stops it too.
        pub(in super::super) fn delete(
            &self,
            path: &WorkspacePath,
            checked: Option<&HashMap<String, FileKind>>,
        ) -> Result<(), FsError> {
            let (dir, name) = self.parent(path)?;
            let st = rustix::fs::statat(&dir, name, AtFlags::SYMLINK_NOFOLLOW).map_err(io)?;
            if kind_of(st.st_mode) != FileKind::Dir {
                return rustix::fs::unlinkat(&dir, name, AtFlags::empty()).map_err(io);
            }
            let Some(checked) = checked else {
                return rustix::fs::unlinkat(&dir, name, AtFlags::REMOVEDIR).map_err(io);
            };
            remove_tree(&dir, name, path.as_str(), checked)
        }

        /// Rename `from` to `to` without replacing `to` (`RENAME_NOREPLACE`,
        /// `RENAME_EXCL`). Both parents are opened by descriptor.
        pub(in super::super) fn rename(
            &self,
            from: &WorkspacePath,
            to: &WorkspacePath,
        ) -> Result<(), FsError> {
            let (from_dir, from_name) = self.parent(from)?;
            let (to_dir, to_name) = self.parent(to)?;
            rename_noreplace(&from_dir, from_name, &to_dir, to_name)
        }

        /// Clear the execute bits of the regular file `path` (an upload
        /// over a file keeps that file's mode in the driver).
        pub(in super::super) fn clear_execute(&self, path: &WorkspacePath) -> Result<(), FsError> {
            let (dir, name) = self.parent(path)?;
            let file = rustix::fs::openat(
                &dir,
                name,
                OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC | OFlags::NONBLOCK,
                Mode::empty(),
            )
            .map_err(io)?;
            let st = rustix::fs::fstat(&file).map_err(io)?;
            if FileType::from_raw_mode(st.st_mode) != FileType::RegularFile {
                return Err(FsError::Link);
            }
            #[allow(clippy::unnecessary_cast)]
            let mode = (st.st_mode as u32) & 0o7777;
            if mode & 0o111 == 0 {
                return Ok(());
            }
            // `RawMode` is u16 on macOS and u32 on Linux; the bits fit both.
            #[allow(clippy::unnecessary_cast)]
            let cleared = (mode & 0o666) as rustix::fs::RawMode;
            rustix::fs::fchmod(&file, Mode::from_raw_mode(cleared)).map_err(io)
        }

        /// Remove upload temporary files older than `age` in the folder and
        /// below it (at most `cap` entries looked at). Returns how many.
        pub(in super::super) fn sweep_upload_temps(
            &self,
            age: std::time::Duration,
            cap: usize,
        ) -> usize {
            let Ok(top) = self.dir(std::iter::empty()) else {
                return 0;
            };
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |d| d.as_secs() as i64);
            let mut todo = vec![top];
            let (mut seen, mut removed) = (0, 0);
            while let Some(dir) = todo.pop() {
                let Ok(entries) = rustix::fs::Dir::read_from(&dir) else {
                    continue;
                };
                for entry in entries.flatten() {
                    seen += 1;
                    if seen > cap {
                        return removed;
                    }
                    let name = entry.file_name().to_string_lossy().into_owned();
                    if name == "." || name == ".." {
                        continue;
                    }
                    let Ok(st) = rustix::fs::statat(&dir, name.as_str(), AtFlags::SYMLINK_NOFOLLOW)
                    else {
                        continue;
                    };
                    match FileType::from_raw_mode(st.st_mode) {
                        FileType::Directory => {
                            if let Ok(sub) =
                                rustix::fs::openat(&dir, name.as_str(), dir_flags(), Mode::empty())
                            {
                                todo.push(sub);
                            }
                        }
                        FileType::RegularFile if upload_temp_name(&name) => {
                            #[allow(clippy::unnecessary_cast)]
                            let mtime = st.st_mtime as i64;
                            if now - mtime >= age.as_secs() as i64
                                && rustix::fs::unlinkat(&dir, name.as_str(), AtFlags::empty())
                                    .is_ok()
                            {
                                removed += 1;
                            }
                        }
                        _ => {}
                    }
                }
            }
            removed
        }
    }

    fn remove_tree(
        dir: &OwnedFd,
        name: &str,
        rel: &str,
        checked: &HashMap<String, FileKind>,
    ) -> Result<(), FsError> {
        if protected_name(name) {
            return Err(FsError::Protected(rel.to_owned()));
        }
        let st = rustix::fs::statat(dir, name, AtFlags::SYMLINK_NOFOLLOW).map_err(io)?;
        let kind = kind_of(st.st_mode);
        if kind != FileKind::Dir {
            return rustix::fs::unlinkat(dir, name, AtFlags::empty()).map_err(io);
        }
        let sub = rustix::fs::openat(dir, name, dir_flags(), Mode::empty()).map_err(io)?;
        let mut names = Vec::new();
        for entry in rustix::fs::Dir::read_from(&sub).map_err(io)? {
            let entry = entry.map_err(io)?;
            let child = entry.file_name().to_string_lossy().into_owned();
            if child != "." && child != ".." {
                names.push(child);
            }
        }
        for child in names {
            let child_rel = format!("{rel}/{child}");
            let st = match rustix::fs::statat(&sub, child.as_str(), AtFlags::SYMLINK_NOFOLLOW) {
                Ok(st) => st,
                Err(Errno::NOENT) => continue,
                Err(e) => return Err(io(e)),
            };
            if checked.get(&child_rel) != Some(&kind_of(st.st_mode)) {
                return Err(FsError::Changed(child_rel));
            }
            remove_tree(&sub, &child, &child_rel, checked)?;
        }
        rustix::fs::unlinkat(dir, name, AtFlags::REMOVEDIR).map_err(io)
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    fn rename_noreplace(
        from_dir: &OwnedFd,
        from: &str,
        to_dir: &OwnedFd,
        to: &str,
    ) -> Result<(), FsError> {
        use rustix::fs::RenameFlags;
        rustix::fs::renameat_with(from_dir, from, to_dir, to, RenameFlags::NOREPLACE).map_err(|e| {
            match e {
                Errno::INVAL | Errno::NOSYS => FsError::Unsupported("a rename that never replaces"),
                e => io(e),
            }
        })
    }

    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    fn rename_noreplace(_: &OwnedFd, _: &str, _: &OwnedFd, _: &str) -> Result<(), FsError> {
        Err(FsError::Unsupported("a rename that never replaces"))
    }
}

#[cfg(not(unix))]
mod imp {
    use super::*;

    /// No descriptor-based file operations here.
    pub(in super::super) struct Root;

    impl Root {
        pub(in super::super) fn open(_: &Path) -> Result<Self, FsError> {
            Err(FsError::Unsupported("the file panel"))
        }
        pub(in super::super) fn stat(&self, _: &WorkspacePath) -> Result<Meta, FsError> {
            Err(FsError::Unsupported("the file panel"))
        }
        pub(in super::super) fn exists(&self, _: &WorkspacePath) -> Result<bool, FsError> {
            Err(FsError::Unsupported("the file panel"))
        }
        #[allow(clippy::type_complexity)]
        pub(in super::super) fn list(
            &self,
            _: &WorkspacePath,
            _: Option<&str>,
            _: usize,
            _: &dyn Fn(&str) -> bool,
        ) -> Result<(Vec<(String, Meta)>, Option<String>, u64), FsError> {
            Err(FsError::Unsupported("the file panel"))
        }
        pub(in super::super) fn mkdir(&self, _: &WorkspacePath) -> Result<(), FsError> {
            Err(FsError::Unsupported("the file panel"))
        }
        pub(in super::super) fn walk(
            &self,
            _: &WorkspacePath,
            _: usize,
        ) -> Result<HashMap<String, FileKind>, FsError> {
            Err(FsError::Unsupported("the file panel"))
        }
        pub(in super::super) fn delete(
            &self,
            _: &WorkspacePath,
            _: Option<&HashMap<String, FileKind>>,
        ) -> Result<(), FsError> {
            Err(FsError::Unsupported("the file panel"))
        }
        pub(in super::super) fn rename(
            &self,
            _: &WorkspacePath,
            _: &WorkspacePath,
        ) -> Result<(), FsError> {
            Err(FsError::Unsupported("the file panel"))
        }
        pub(in super::super) fn clear_execute(&self, _: &WorkspacePath) -> Result<(), FsError> {
            Err(FsError::Unsupported("the file panel"))
        }
        pub(in super::super) fn sweep_upload_temps(
            &self,
            _: std::time::Duration,
            _: usize,
        ) -> usize {
            0
        }
    }
}

pub(super) use imp::Root;
