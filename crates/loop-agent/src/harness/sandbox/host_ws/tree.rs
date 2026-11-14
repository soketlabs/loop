//! Signals to the whole tree of a spawned process.
//!
//! Each spawned process starts a new session (`setsid`), so it leads its
//! own session and process group, whose ids are its pid. The tree is every
//! live process in that session. A descendant that calls `setsid` itself
//! starts another session and is out of reach (the same limit as the
//! process-group kill of `Shell::exec`). A descendant that only changes its
//! process group (job control in an interactive shell) stays in the
//! session and is reached.
//!
//! Every kill sends the process-group signal first, so a listing that
//! finds nothing (no `/proc`, `hidepid`, a failed `proc_listallpids`, a
//! platform without a listing) still stops the group. Each listed member
//! is checked again right before its signal: on Linux through a pidfd
//! (the pid cannot be reused while the pidfd is open), elsewhere with a
//! second `getsid`. Pid 1, the Runtime's own pid and the Runtime's own
//! group and session are never signalled.
//!
//! Callers must not signal a session whose leader was reaped: its id may
//! then name another process. The waiter in `process.rs` holds a lock over
//! "signal" and "reap" for this.

/// Whether `sid` may be signalled as a group: not 0, not 1, not the
/// Runtime's own pid, group or session.
fn safe_sid(sid: i32) -> bool {
    // SAFETY: getpid, getpgrp and getsid(0) have no memory arguments.
    let (me, group, session) = unsafe { (libc::getpid(), libc::getpgrp(), libc::getsid(0)) };
    sid > 1 && sid != me && sid != group && sid != session
}

/// Live (not zombie) processes whose session id is `sid`, the leader too.
#[cfg(target_os = "linux")]
pub(crate) fn session_members(sid: i32) -> Vec<i32> {
    let Ok(dir) = std::fs::read_dir("/proc") else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for entry in dir.flatten() {
        let Some(pid) = entry
            .file_name()
            .to_str()
            .and_then(|n| n.parse::<i32>().ok())
        else {
            continue;
        };
        if let Some((state, session)) = stat_fields(pid) {
            if state != b'Z' && state != b'X' && session == sid {
                out.push(pid);
            }
        }
    }
    out
}

/// State and session of `pid` from `/proc/<pid>/stat`, read as bytes (the
/// command name may hold any bytes, spaces and parentheses).
#[cfg(target_os = "linux")]
fn stat_fields(pid: i32) -> Option<(u8, i32)> {
    let stat = std::fs::read(format!("/proc/{pid}/stat")).ok()?;
    parse_stat(&stat)
}

/// `pid (comm) state ppid pgrp session ...`: the fields after the last `)`.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn parse_stat(stat: &[u8]) -> Option<(u8, i32)> {
    let close = stat.iter().rposition(|b| *b == b')')?;
    let mut fields = stat[close + 1..]
        .split(|b| b.is_ascii_whitespace())
        .filter(|f| !f.is_empty());
    let state = *fields.next()?.first()?;
    let session = std::str::from_utf8(fields.nth(2)?).ok()?.parse().ok()?;
    Some((state, session))
}

/// Live (not zombie) processes whose session id is `sid`, the leader too.
#[cfg(target_os = "macos")]
pub(crate) fn session_members(sid: i32) -> Vec<i32> {
    // SAFETY: a null buffer asks for the needed size; the second call
    // writes at most the given buffer length.
    let pids = unsafe {
        let bytes = libc::proc_listallpids(std::ptr::null_mut(), 0);
        if bytes <= 0 {
            return Vec::new();
        }
        let mut pids = vec![0i32; bytes as usize + 256];
        let n = libc::proc_listallpids(
            pids.as_mut_ptr().cast(),
            std::mem::size_of_val(pids.as_slice()) as i32,
        );
        if n <= 0 {
            return Vec::new();
        }
        pids.truncate(n as usize);
        pids
    };
    pids.into_iter()
        .filter(|&pid| pid > 1)
        .filter(|&pid| {
            // SAFETY: getsid has no memory arguments.
            unsafe { libc::getsid(pid) == sid }
        })
        .filter(|&pid| !zombie(pid))
        .collect()
}

#[cfg(target_os = "macos")]
fn zombie(pid: i32) -> bool {
    // SAFETY: this POD buffer has the size PROC_PIDTBSDINFO expects.
    unsafe {
        let mut info = std::mem::zeroed::<libc::proc_bsdinfo>();
        let size = std::mem::size_of_val(&info) as i32;
        let n = libc::proc_pidinfo(
            pid,
            libc::PROC_PIDTBSDINFO,
            0,
            (&mut info as *mut libc::proc_bsdinfo).cast(),
            size,
        );
        // A zombie has no BSD info (the call fails) or status SZOMB (5).
        n != size || info.pbi_status == 5
    }
}

/// Other Unix systems: no listing; the group signal is all.
#[cfg(all(unix, not(any(target_os = "linux", target_os = "macos"))))]
pub(crate) fn session_members(_sid: i32) -> Vec<i32> {
    Vec::new()
}

/// Send `signal` to `pid` only if it is still in session `sid`.
#[cfg(target_os = "linux")]
fn signal_member(pid: i32, sid: i32, signal: i32) {
    if !safe_pid(pid) {
        return;
    }
    // SAFETY: pidfd_open takes a pid and flags and returns a new fd or -1.
    let fd = unsafe { libc::syscall(libc::SYS_pidfd_open, pid, 0) } as i32;
    if fd < 0 {
        // No pidfd (old kernel): check again, then signal by pid.
        signal_member_by_pid(pid, sid, signal);
        return;
    }
    // While the pidfd is open the pid is not reused: this check holds.
    if stat_fields(pid).is_some_and(|(_, session)| session == sid) {
        // SAFETY: `fd` is an open pidfd; no siginfo; no flags.
        unsafe {
            libc::syscall(
                libc::SYS_pidfd_send_signal,
                fd,
                signal,
                std::ptr::null::<libc::siginfo_t>(),
                0,
            );
        }
    }
    // SAFETY: `fd` is ours and closed once.
    unsafe {
        libc::close(fd);
    }
}

/// Send `signal` to `pid` only if it is still in session `sid`.
#[cfg(not(target_os = "linux"))]
fn signal_member(pid: i32, sid: i32, signal: i32) {
    signal_member_by_pid(pid, sid, signal);
}

fn safe_pid(pid: i32) -> bool {
    // SAFETY: getpid has no arguments.
    pid > 1 && pid != unsafe { libc::getpid() }
}

fn signal_member_by_pid(pid: i32, sid: i32, signal: i32) {
    if !safe_pid(pid) {
        return;
    }
    // SAFETY: getsid has no memory arguments; the window between this
    // check and the kill is a few instructions.
    if unsafe { libc::getsid(pid) } == sid {
        // SAFETY: a positive pid, in the session just now.
        unsafe {
            libc::kill(pid, signal);
        }
    }
}

fn signal_group(sid: i32, signal: i32) {
    if !safe_sid(sid) {
        tracing::error!(
            sid,
            "workspace process: refused to signal the Runtime's own group"
        );
        return;
    }
    // SAFETY: a negative pid names the process group `sid`, which exists
    // while its unreaped leader does (the caller's guarantee).
    unsafe {
        libc::kill(-sid, signal);
    }
}

/// Send `signal` to the group and every live member of session `sid`.
/// The caller guarantees the leader `sid` is not reaped yet.
pub(crate) fn signal_session(sid: i32, signal: i32) {
    signal_group(sid, signal);
    if !safe_sid(sid) {
        return;
    }
    for pid in session_members(sid) {
        signal_member(pid, sid, signal);
    }
}

/// Kill every member of session `sid` until none is left alive (a member
/// may fork while it is listed; the next pass finds the child). Returns
/// the members still alive after the last pass (empty on success). The
/// caller guarantees the leader is not reaped yet.
pub(crate) fn kill_session(sid: i32) -> Vec<i32> {
    kill_session_with(sid, session_members)
}

/// [`kill_session`] with the member listing given (tests pass an empty one).
pub(crate) fn kill_session_with(sid: i32, members: impl Fn(i32) -> Vec<i32>) -> Vec<i32> {
    // The group first: it needs no listing.
    signal_group(sid, libc::SIGKILL);
    if !safe_sid(sid) {
        return Vec::new();
    }
    let mut left = Vec::new();
    for pass in 0..16 {
        left = members(sid);
        if left.is_empty() {
            return left;
        }
        for &pid in &left {
            signal_member(pid, sid, libc::SIGKILL);
        }
        signal_group(sid, libc::SIGKILL);
        // Let the kernel finish the kills before the next listing.
        std::thread::sleep(std::time::Duration::from_millis(1 + pass));
    }
    left
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stat_is_parsed_as_bytes() {
        let stat = b"123 (a) b\xff) c) S 1 123 77 0 0";
        assert_eq!(parse_stat(stat), Some((b'S', 77)));
        assert_eq!(parse_stat(b"9 (z) Z 1 9 9"), Some((b'Z', 9)));
        assert_eq!(parse_stat(b"broken"), None);
    }

    #[test]
    fn the_runtime_itself_is_never_a_target() {
        // SAFETY: no memory arguments.
        let (me, group) = unsafe { (libc::getpid(), libc::getpgrp()) };
        for sid in [0, 1, me, group] {
            assert!(!safe_sid(sid), "{sid}");
            assert!(kill_session_with(sid, |_| vec![me]).is_empty());
        }
        assert!(!safe_pid(me) && !safe_pid(1) && !safe_pid(0));
    }
}
