//! Private Runtime bearer-token files. Call `create` while holding the Runtime lock.

use std::{
    fs::{File, OpenOptions},
    io::{self, Read, Write},
    os::{
        fd::{AsRawFd, FromRawFd},
        unix::{
            ffi::OsStrExt,
            fs::{DirBuilderExt, MetadataExt, OpenOptionsExt},
        },
    },
    path::{Path, PathBuf},
};

fn denied() -> io::Error {
    io::Error::new(io::ErrorKind::PermissionDenied,
        "a token file requires a user-owned 0700 directory and a regular user-owned 0600 file without links")
}

fn directory(path: &Path) -> io::Result<File> {
    let dir = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(path)?;
    let meta = dir.metadata()?;
    // SAFETY: geteuid has no preconditions.
    if meta.uid() != unsafe { libc::geteuid() } || meta.mode() & 0o777 != 0o700 {
        return Err(denied());
    }
    Ok(dir)
}

fn open(path: &Path, create: bool) -> io::Result<File> {
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let dir = directory(parent)?;
    let name = path.file_name().ok_or_else(denied)?;
    let name = std::ffi::CString::new(name.as_bytes()).map_err(|_| denied())?;
    let flags = libc::O_NOFOLLOW
        | libc::O_NONBLOCK
        | libc::O_CLOEXEC
        | if create {
            libc::O_RDWR | libc::O_CREAT
        } else {
            libc::O_RDONLY
        };
    // SAFETY: dir is open and name is NUL-terminated. The returned fd is owned below.
    let fd = unsafe { libc::openat(dir.as_raw_fd(), name.as_ptr(), flags, 0o600) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: openat returned a new valid fd that this File exclusively owns.
    let file = unsafe { File::from_raw_fd(fd) };
    let meta = file.metadata()?;
    // SAFETY: geteuid has no preconditions.
    if !meta.is_file()
        || meta.uid() != unsafe { libc::geteuid() }
        || meta.mode() & 0o777 != 0o600
        || meta.nlink() != 1
    {
        return Err(denied());
    }
    Ok(file)
}

/// Generate 256 random bits and replace `run/runtime.token` with mode 0600.
/// Refuse unsafe existing files or directories, including symlinks.
pub fn create(agent_dir: &Path) -> io::Result<(String, PathBuf)> {
    let run = agent_dir.join("run");
    match std::fs::DirBuilder::new().mode(0o700).create(&run) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
        Err(e) => return Err(e),
    }
    let path = run.join("runtime.token");
    let mut file = open(&path, true)?;
    let mut bytes = [0u8; 32];
    File::open("/dev/urandom")?.read_exact(&mut bytes)?;
    let token: String = bytes.iter().map(|byte| format!("{byte:02x}")).collect();
    file.set_len(0)?;
    writeln!(file, "{token}")?;
    file.sync_all()?;
    Ok((token, path))
}

/// Read a private token file without following its final directory or file symlink.
/// Invalid, empty, oversized and non-header-safe tokens are refused without logging them.
pub fn read(path: &Path) -> io::Result<String> {
    let file = open(path, false)?;
    let mut token = String::new();
    file.take(4097).read_to_string(&mut token)?;
    if token.len() > 4096 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "token file is too large",
        ));
    }
    let token = token.trim();
    if token.is_empty() || token.len() > 4096 || !token.bytes().all(|b| b.is_ascii_graphic()) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "invalid token file",
        ));
    }
    Ok(token.to_owned())
}
