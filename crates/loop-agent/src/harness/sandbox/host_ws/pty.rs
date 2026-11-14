//! A pseudo-terminal pair from `/dev/ptmx`, with only `libc` calls.
//!
//! The master is opened with `O_CLOEXEC` in the open call itself, so a
//! command that another thread starts at the same moment never inherits
//! it. `grantpt`, `unlockpt` and `ptsname` are POSIX on Linux and macOS;
//! `ptsname` uses a static buffer, so a lock guards it.

use std::fs::File;
use std::os::fd::AsRawFd;
use std::os::unix::fs::OpenOptionsExt;

use loop_workspace::PtySize;

static PTSNAME: parking_lot::Mutex<()> = parking_lot::Mutex::new(());

/// The two ends of a new PTY.
pub(crate) struct PtyPair {
    /// The Runtime end.
    pub master: File,
    /// The command's terminal.
    pub slave: File,
}

fn last_error(what: &str) -> std::io::Error {
    let e = std::io::Error::last_os_error();
    std::io::Error::new(e.kind(), format!("{what}: {e}"))
}

/// Open a PTY of `size`.
pub(crate) fn open(size: PtySize) -> std::io::Result<PtyPair> {
    let master = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .custom_flags(libc::O_NOCTTY)
        .open("/dev/ptmx")?;
    let fd = master.as_raw_fd();
    // SAFETY: `fd` is the open master of a new PTY.
    if unsafe { libc::grantpt(fd) } != 0 {
        return Err(last_error("grantpt"));
    }
    // SAFETY: as above.
    if unsafe { libc::unlockpt(fd) } != 0 {
        return Err(last_error("unlockpt"));
    }
    let name = {
        let _guard = PTSNAME.lock();
        // SAFETY: `fd` is a PTY master. The returned pointer is a static
        // buffer, copied before the lock is released.
        let ptr = unsafe { libc::ptsname(fd) };
        if ptr.is_null() {
            return Err(last_error("ptsname"));
        }
        // SAFETY: ptsname returned a NUL-terminated string.
        unsafe { std::ffi::CStr::from_ptr(ptr) }.to_owned()
    };
    let path = std::path::PathBuf::from(
        <std::ffi::OsStr as std::os::unix::ffi::OsStrExt>::from_bytes(name.to_bytes()),
    );
    let slave = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .custom_flags(libc::O_NOCTTY)
        .open(path)?;
    resize(&slave, size)?;
    Ok(PtyPair { master, slave })
}

/// Set the window size of the terminal `fd` belongs to.
pub(crate) fn resize(fd: &impl AsRawFd, size: PtySize) -> std::io::Result<()> {
    let ws = libc::winsize {
        ws_row: size.rows,
        ws_col: size.cols,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    // SAFETY: TIOCSWINSZ reads one `winsize` from the pointer, which lives
    // for the call.
    if unsafe { libc::ioctl(fd.as_raw_fd(), libc::TIOCSWINSZ as _, &ws) } != 0 {
        return Err(last_error("TIOCSWINSZ"));
    }
    Ok(())
}

/// Wait until `file` has data to read, at most `limit`. `select`, because
/// `poll` does not report devices on macOS. A descriptor at or above
/// `FD_SETSIZE` cannot be waited for: it reports ready at once (the read
/// then blocks as before).
pub(crate) fn wait_readable(file: &File, limit: std::time::Duration) -> std::io::Result<bool> {
    let fd = file.as_raw_fd();
    // FD_SETSIZE is `usize` on Linux and `c_int` on macOS.
    #[allow(clippy::unnecessary_cast)]
    let limit_fd = libc::FD_SETSIZE as usize;
    if fd < 0 || fd as usize >= limit_fd {
        return Ok(true);
    }
    let mut timeout = libc::timeval {
        tv_sec: limit.as_secs() as _,
        tv_usec: limit.subsec_micros() as _,
    };
    // SAFETY: `set` is a zeroed fd_set; FD_ZERO and FD_SET write only into
    // it, `fd` is below FD_SETSIZE, and select reads the pointers for the
    // call only.
    let rc = unsafe {
        let mut set: libc::fd_set = std::mem::zeroed();
        libc::FD_ZERO(&mut set);
        libc::FD_SET(fd, &mut set);
        libc::select(
            fd + 1,
            &mut set,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            &mut timeout,
        )
    };
    match rc {
        -1 => {
            let e = std::io::Error::last_os_error();
            if e.kind() == std::io::ErrorKind::Interrupted {
                Ok(false)
            } else {
                Err(e)
            }
        }
        0 => Ok(false),
        _ => Ok(true),
    }
}

/// Make `fd` (the terminal on stdin in the child) the controlling terminal
/// of the calling process, which must lead a new session. For `pre_exec`:
/// async-signal-safe.
pub(crate) fn take_controlling_terminal() -> std::io::Result<()> {
    // SAFETY: TIOCSCTTY takes an integer argument; fd 0 is the slave.
    if unsafe { libc::ioctl(0, libc::TIOCSCTTY as _, 0) } != 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}
