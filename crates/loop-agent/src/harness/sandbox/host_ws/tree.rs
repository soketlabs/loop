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
//! Callers must not signal a session whose leader was reaped: its id may
//! then name another process. The waiter in `process.rs` holds a lock over
//! "signal" and "reap" for this.

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
        let Ok(stat) = std::fs::read_to_string(format!("/proc/{pid}/stat")) else {
            continue;
        };
        // `pid (comm) state ppid pgrp session ...`; comm may hold spaces
        // and parentheses, so read after the last `)`.
        let Some(rest) = stat.rsplit_once(')').map(|(_, r)| r) else {
            continue;
        };
        let fields: Vec<&str> = rest.split_whitespace().collect();
        let (Some(state), Some(session)) = (fields.first(), fields.get(3)) else {
            continue;
        };
        if *state == "Z" || *state == "X" {
            continue;
        }
        if session.parse::<i32>().ok() == Some(sid) {
            out.push(pid);
        }
    }
    out
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

/// Other Unix systems: the process group only.
#[cfg(all(unix, not(any(target_os = "linux", target_os = "macos"))))]
pub(crate) fn session_members(_sid: i32) -> Vec<i32> {
    Vec::new()
}

/// Send `signal` to the group and every live member of session `sid`.
/// The caller guarantees the leader `sid` is not reaped yet.
pub(crate) fn signal_session(sid: i32, signal: i32) {
    // SAFETY: a negative pid names the process group `sid`, which exists
    // while its unreaped leader does.
    unsafe {
        libc::kill(-sid, signal);
    }
    for pid in session_members(sid) {
        // SAFETY: a positive pid listed in this session just now.
        unsafe {
            libc::kill(pid, signal);
        }
    }
}

/// Kill every member of session `sid` until none is left alive (a member
/// may fork while it is listed; the next pass finds the child). Returns
/// the members still alive after the last pass (empty on success). The
/// caller guarantees the leader is not reaped yet.
pub(crate) fn kill_session(sid: i32) -> Vec<i32> {
    let mut left = Vec::new();
    for pass in 0..16 {
        left = session_members(sid);
        if left.is_empty() {
            return left;
        }
        for &pid in &left {
            // SAFETY: a positive pid listed in this session just now.
            unsafe {
                libc::kill(pid, libc::SIGKILL);
            }
        }
        // SAFETY: as in `signal_session`.
        unsafe {
            libc::kill(-sid, libc::SIGKILL);
        }
        // Let the kernel finish the kills before the next listing.
        std::thread::sleep(std::time::Duration::from_millis(1 + pass));
    }
    left
}
