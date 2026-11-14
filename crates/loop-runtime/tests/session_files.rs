//! `session/files/*` and `session/workspace` over a real connection: the
//! happy path, limits, resume, `if_match`, binary data, path escapes, links,
//! protected paths, deny rules, modes, the tool lock and access by another
//! caller. Each decision is recorded.

#[path = "support/workspace.rs"]
mod support;

use loop_protocol::rpc::SessionParams;
use loop_protocol::session_workspace::{
    base64, FileKind, FilePathParams, FilesDeleteParams, FilesListParams, FilesReadParams,
    FilesRenameParams, UploadBeginParams, UploadParams, UploadWriteParams,
};
use loop_protocol::{Caller, Principal, PrincipalKind, SessionRef, Tenant};
use loop_wire::{RemoteBackend, SessionWorkspace};
use serde_json::json;
use sha2::{Digest, Sha256};
use support::{caps, client, code, log_text, new_session, raw, raw_client, runtime, Options};

/// The file and terminal calls of `c` (it has a `list` and a `rename` of
/// its own).
fn sw(c: &RemoteBackend) -> &dyn SessionWorkspace {
    c
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Upload `data` to `path` in pieces of `piece` bytes; the tag.
async fn upload(
    c: &RemoteBackend,
    s: &SessionRef,
    path: &str,
    data: &[u8],
    piece: usize,
) -> Result<String, loop_protocol::BackendError> {
    let begun = c
        .upload_begin(UploadBeginParams {
            session: s.clone(),
            path: path.into(),
            size: data.len() as u64,
            sha256: Some(hex(&Sha256::digest(data))),
            if_match: None,
            overwrite: true,
        })
        .await?;
    let mut offset = 0;
    for chunk in data.chunks(piece.max(1)) {
        c.upload_write(UploadWriteParams {
            session: s.clone(),
            upload: begun.upload.clone(),
            offset,
            data: base64::encode(chunk),
        })
        .await?;
        offset += chunk.len() as u64;
    }
    let done = c
        .upload_commit(UploadParams {
            session: s.clone(),
            upload: begun.upload,
        })
        .await?;
    Ok(done.tag)
}

/// Read the whole file in pieces, with `if_match` after the first.
async fn download(c: &RemoteBackend, s: &SessionRef, path: &str, piece: u32) -> Vec<u8> {
    let mut out = Vec::new();
    let mut tag = None;
    loop {
        let chunk = c
            .read(FilesReadParams {
                session: s.clone(),
                path: path.into(),
                offset: out.len() as u64,
                length: Some(piece),
                if_match: tag.clone(),
            })
            .await
            .unwrap();
        out.extend(base64::decode(&chunk.data).unwrap());
        tag = Some(chunk.tag);
        if chunk.eof {
            return out;
        }
    }
}

fn path(s: &SessionRef, path: &str) -> FilePathParams {
    FilePathParams {
        session: s.clone(),
        path: path.into(),
    }
}

#[tokio::test]
async fn upload_list_stat_download_rename_delete_round_trip_binary_data() {
    let f = runtime(Options::default()).await;
    let c = client(&f.rt, None).await;
    assert!(caps(&c).session_files);
    let s = new_session(&c, &f.ws).await;
    let info = c.info(SessionParams { session: s.clone() }).await.unwrap();
    assert!(
        info.files.available && info.files_write.available,
        "{info:?}"
    );
    assert!(info.terminal.available, "{info:?}");

    let data: Vec<u8> = (0..300_000u32).map(|i| (i * 7 % 256) as u8).collect();
    let tag = upload(&c, &s, "dir/sub/blob.bin", &data, 64 * 1024)
        .await
        .unwrap();
    assert_eq!(std::fs::read(f.ws.join("dir/sub/blob.bin")).unwrap(), data);
    let stat = c.stat(path(&s, "dir/sub/blob.bin")).await.unwrap();
    assert_eq!((stat.kind, stat.size), (FileKind::File, data.len() as u64));
    assert_eq!(stat.tag.as_deref(), Some(tag.as_str()));
    assert_eq!(download(&c, &s, "dir/sub/blob.bin", 70_000).await, data);
    // A new file gets no execute bit.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(f.ws.join("dir/sub/blob.bin"))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o111, 0, "{mode:o}");
    }

    // Listing: name order, kinds, paging with a cursor.
    for name in ["c", "a", "b"] {
        std::fs::write(f.ws.join("dir").join(name), name).unwrap();
    }
    let page = sw(&c)
        .list(FilesListParams {
            session: s.clone(),
            path: "dir".into(),
            cursor: None,
            limit: Some(2),
        })
        .await
        .unwrap();
    let names: Vec<_> = page.entries.iter().map(|e| e.name.as_str()).collect();
    assert_eq!(names, ["a", "b"]);
    assert_eq!(page.entries[0].path, "dir/a");
    let rest = sw(&c)
        .list(FilesListParams {
            session: s.clone(),
            path: "dir".into(),
            cursor: page.next_cursor,
            limit: Some(2),
        })
        .await
        .unwrap();
    let names: Vec<_> = rest
        .entries
        .iter()
        .map(|e| (e.name.as_str(), e.kind))
        .collect();
    assert_eq!(names, [("c", FileKind::File), ("sub", FileKind::Dir)]);
    assert!(rest.next_cursor.is_none());

    // mkdir, rename (never over an existing file), delete.
    c.mkdir(path(&s, "dir/new")).await.unwrap();
    assert_eq!(
        code(&c.mkdir(path(&s, "dir/new")).await.unwrap_err()),
        "exists"
    );
    assert_eq!(
        code(&c.mkdir(path(&s, "nope/deeper")).await.unwrap_err()),
        "not_found"
    );
    let rename = |from: &str, to: &str| FilesRenameParams {
        session: s.clone(),
        from: from.into(),
        to: to.into(),
    };
    sw(&c).rename(rename("dir/a", "dir/new/a2")).await.unwrap();
    assert!(f.ws.join("dir/new/a2").is_file());
    assert_eq!(
        code(&sw(&c).rename(rename("dir/b", "dir/c")).await.unwrap_err()),
        "exists"
    );
    let delete = |p: &str, recursive| FilesDeleteParams {
        session: s.clone(),
        path: p.into(),
        recursive,
    };
    assert_eq!(
        code(&c.delete(delete("dir/new", false)).await.unwrap_err()),
        "not_empty"
    );
    c.delete(delete("dir/new", true)).await.unwrap();
    assert!(!f.ws.join("dir/new").exists());
    assert_eq!(
        code(&c.delete(delete("", true)).await.unwrap_err()),
        "invalid_path"
    );

    // Every action was recorded as the user's.
    let log = log_text(&f.log).await;
    assert!(log.matches("user_files").count() >= 10, "{log}");
    f.rt.shutdown().await;
}

#[tokio::test]
async fn an_upload_resumes_from_the_committed_offset_and_checks_size_and_sum() {
    let f = runtime(Options::default()).await;
    let c = client(&f.rt, None).await;
    let s = new_session(&c, &f.ws).await;
    let data = b"0123456789".repeat(10);
    let begin = |sha: &str| UploadBeginParams {
        session: s.clone(),
        path: "r.txt".into(),
        size: data.len() as u64,
        sha256: Some(sha.into()),
        if_match: None,
        overwrite: false,
    };
    let up = c
        .upload_begin(begin(&hex(&Sha256::digest(&data))))
        .await
        .unwrap()
        .upload;
    let write = |offset: u64, bytes: &[u8]| UploadWriteParams {
        session: s.clone(),
        upload: up.clone(),
        offset,
        data: base64::encode(bytes),
    };
    c.upload_write(write(0, &data[..40])).await.unwrap();
    // A gap: refused, nothing written; resume from the status.
    let gap = c.upload_write(write(60, &data[60..])).await.unwrap_err();
    assert_eq!(code(&gap), "upload_offset");
    let status = c
        .upload_status(UploadParams {
            session: s.clone(),
            upload: up.clone(),
        })
        .await
        .unwrap();
    assert_eq!((status.committed, status.size), (40, 100));
    // Not complete: the commit refuses and the upload stays.
    let early = c
        .upload_commit(UploadParams {
            session: s.clone(),
            upload: up.clone(),
        })
        .await
        .unwrap_err();
    assert_eq!(code(&early), "upload_offset");
    c.upload_write(write(40, &data[40..])).await.unwrap();
    // More than declared.
    assert_eq!(
        code(&c.upload_write(write(100, b"x")).await.unwrap_err()),
        "too_large"
    );
    c.upload_commit(UploadParams {
        session: s.clone(),
        upload: up.clone(),
    })
    .await
    .unwrap();
    assert_eq!(std::fs::read(f.ws.join("r.txt")).unwrap(), data);
    // The id is gone after the commit.
    assert_eq!(
        code(
            &c.upload_status(UploadParams {
                session: s.clone(),
                upload: up
            })
            .await
            .unwrap_err()
        ),
        "unknown_upload"
    );
    // The file exists now: refused without overwrite or if_match.
    assert_eq!(
        code(&c.upload_begin(begin(&"0".repeat(64))).await.unwrap_err()),
        "exists"
    );
    // A wrong checksum: the commit fails and the file stays as it was.
    let mut wrong = begin(&"0".repeat(64));
    wrong.overwrite = true;
    let up = c.upload_begin(wrong).await.unwrap().upload;
    c.upload_write(UploadWriteParams {
        session: s.clone(),
        upload: up.clone(),
        offset: 0,
        data: base64::encode(&[b'z'; 100]),
    })
    .await
    .unwrap();
    let bad = c
        .upload_commit(UploadParams {
            session: s.clone(),
            upload: up.clone(),
        })
        .await
        .unwrap_err();
    assert_eq!(code(&bad), "checksum");
    assert_eq!(std::fs::read(f.ws.join("r.txt")).unwrap(), data);
    // Abort of another upload removes it; a second abort is unknown.
    let up = c
        .upload_begin(UploadBeginParams {
            path: "a.txt".into(),
            ..begin(&hex(&Sha256::digest(&data)))
        })
        .await
        .unwrap()
        .upload;
    let abort = UploadParams {
        session: s.clone(),
        upload: up,
    };
    c.upload_abort(abort.clone()).await.unwrap();
    assert_eq!(
        code(&c.upload_abort(abort).await.unwrap_err()),
        "unknown_upload"
    );
    assert!(!f.ws.join("a.txt").exists());
    // Not Base64.
    let up = c
        .upload_begin(UploadBeginParams {
            path: "b.txt".into(),
            ..begin(&hex(&Sha256::digest(&data)))
        })
        .await
        .unwrap()
        .upload;
    let refused = c
        .upload_write(UploadWriteParams {
            session: s.clone(),
            upload: up,
            offset: 0,
            data: "not base64!".into(),
        })
        .await
        .unwrap_err();
    assert!(refused.to_string().contains("Base64"), "{refused}");
    f.rt.shutdown().await;
}

#[tokio::test]
async fn if_match_finds_a_file_that_changed() {
    let f = runtime(Options::default()).await;
    let c = client(&f.rt, None).await;
    let s = new_session(&c, &f.ws).await;
    std::fs::write(f.ws.join("x.txt"), b"first version, long enough").unwrap();
    let first = c
        .read(FilesReadParams {
            session: s.clone(),
            path: "x.txt".into(),
            offset: 0,
            length: Some(5),
            if_match: None,
        })
        .await
        .unwrap();
    assert!(!first.eof);
    std::fs::write(f.ws.join("x.txt"), b"a second version").unwrap();
    let next = c
        .read(FilesReadParams {
            session: s.clone(),
            path: "x.txt".into(),
            offset: 5,
            length: Some(5),
            if_match: Some(first.tag.clone()),
        })
        .await
        .unwrap_err();
    assert_eq!(code(&next), "changed");
    // An upload with an old tag fails at the begin (the driver checks it
    // there too) or at the commit.
    let begun = c
        .upload_begin(UploadBeginParams {
            session: s.clone(),
            path: "x.txt".into(),
            size: 1,
            sha256: None,
            if_match: Some(first.tag.clone()),
            overwrite: false,
        })
        .await;
    assert_eq!(code(&begun.unwrap_err()), "changed");
    // A tag that matched at the begin and changed before the commit.
    let now = c.stat(path(&s, "x.txt")).await.unwrap().tag.unwrap();
    let up = c
        .upload_begin(UploadBeginParams {
            session: s.clone(),
            path: "x.txt".into(),
            size: 1,
            sha256: None,
            if_match: Some(now),
            overwrite: false,
        })
        .await
        .unwrap()
        .upload;
    std::fs::write(f.ws.join("x.txt"), b"a third version!").unwrap();
    c.upload_write(UploadWriteParams {
        session: s.clone(),
        upload: up.clone(),
        offset: 0,
        data: base64::encode(b"!"),
    })
    .await
    .unwrap();
    let commit = c
        .upload_commit(UploadParams {
            session: s.clone(),
            upload: up,
        })
        .await
        .unwrap_err();
    assert_eq!(code(&commit), "changed");
    assert_eq!(
        std::fs::read(f.ws.join("x.txt")).unwrap(),
        b"a third version!"
    );
    f.rt.shutdown().await;
}

#[tokio::test]
async fn paths_never_leave_the_sessions_folder() {
    let f = runtime(Options::default()).await;
    let c = client(&f.rt, None).await;
    let s = new_session(&c, &f.ws).await;
    let outside = f.dir.path().join("outside");
    std::fs::create_dir_all(&outside).unwrap();
    std::fs::write(outside.join("secret"), "SECRET-OUTSIDE").unwrap();
    #[cfg(unix)]
    std::os::unix::fs::symlink(&outside, f.ws.join("out")).unwrap();
    for bad in [
        "../outside/secret",
        "/etc/passwd",
        "a/../../x",
        "a\\b",
        "C:/x",
    ] {
        let read = c
            .read(FilesReadParams {
                session: s.clone(),
                path: bad.into(),
                offset: 0,
                length: None,
                if_match: None,
            })
            .await
            .unwrap_err();
        assert_eq!(code(&read), "invalid_path", "{bad}");
        let list = sw(&c)
            .list(FilesListParams {
                session: s.clone(),
                path: bad.into(),
                cursor: None,
                limit: None,
            })
            .await
            .unwrap_err();
        assert_eq!(code(&list), "invalid_path", "{bad}");
    }
    #[cfg(unix)]
    {
        // Through a link out: refused for every call that would use it.
        let read = c
            .read(FilesReadParams {
                session: s.clone(),
                path: "out/secret".into(),
                offset: 0,
                length: None,
                if_match: None,
            })
            .await
            .unwrap_err();
        assert_eq!(code(&read), "invalid_path");
        for (what, error) in [
            (
                "list",
                sw(&c)
                    .list(FilesListParams {
                        session: s.clone(),
                        path: "out".into(),
                        cursor: None,
                        limit: None,
                    })
                    .await
                    .unwrap_err(),
            ),
            ("stat", c.stat(path(&s, "out/secret")).await.unwrap_err()),
            ("mkdir", c.mkdir(path(&s, "out/new")).await.unwrap_err()),
            (
                "delete",
                c.delete(FilesDeleteParams {
                    session: s.clone(),
                    path: "out/secret".into(),
                    recursive: false,
                })
                .await
                .unwrap_err(),
            ),
            (
                "upload",
                upload(&c, &s, "out/planted", b"x", 1).await.unwrap_err(),
            ),
        ] {
            assert_eq!(code(&error), "invalid_path", "{what}: {error}");
        }
        // Existence through the link is not told: refused as a path out,
        // not `exists`.
        let begun = c
            .upload_begin(UploadBeginParams {
                session: s.clone(),
                path: "out/secret".into(),
                size: 1,
                sha256: None,
                if_match: None,
                overwrite: false,
            })
            .await
            .unwrap_err();
        assert_eq!(code(&begun), "invalid_path", "{begun}");
        // The link itself: stat shows a link; delete removes the link only.
        assert_eq!(
            c.stat(path(&s, "out")).await.unwrap().kind,
            FileKind::Symlink
        );
        c.delete(FilesDeleteParams {
            session: s.clone(),
            path: "out".into(),
            recursive: true,
        })
        .await
        .unwrap();
        assert!(outside.join("secret").exists());
        assert!(!outside.join("planted").exists());
    }
    // No reply named a host path.
    let host = f.dir.path().to_string_lossy().into_owned();
    let error = c.stat(path(&s, "missing")).await.unwrap_err();
    assert!(!error.to_string().contains(&host), "{error}");
    f.rt.shutdown().await;
}

#[tokio::test]
async fn protected_paths_and_deny_rules_hold_for_the_file_panel() {
    let f = runtime(Options {
        settings: json!({"toolPermissions": {"read(*secret*)": "deny", "write(*locked*)": "deny"}}),
        ..Options::default()
    })
    .await;
    std::fs::create_dir_all(f.ws.join(".git")).unwrap();
    std::fs::write(f.ws.join(".git/config"), "[core]").unwrap();
    std::fs::write(f.ws.join("secret.txt"), "s").unwrap();
    std::fs::write(f.ws.join("plain.txt"), "p").unwrap();
    let c = client(&f.rt, None).await;
    let s = new_session(&c, &f.ws).await;
    for (p, data) in [
        (".git/config", b"x"),
        (".loop/settings.json", b"x"),
        ("locked.txt", b"x"),
    ] {
        let error = upload(&c, &s, p, data, 1).await.unwrap_err();
        assert_eq!(code(&error), "refused", "{p}: {error}");
    }
    assert_eq!(
        code(&c.mkdir(path(&s, ".loop")).await.unwrap_err()),
        "refused"
    );
    assert_eq!(
        code(
            &c.delete(FilesDeleteParams {
                session: s.clone(),
                path: ".git".into(),
                recursive: true
            })
            .await
            .unwrap_err()
        ),
        "refused"
    );
    assert!(f.ws.join(".git/config").exists());
    // A folder change takes nothing with it that the user could not change.
    std::fs::create_dir_all(f.ws.join("proj/.git")).unwrap();
    std::fs::write(f.ws.join("proj/.git/HEAD"), "ref").unwrap();
    std::fs::create_dir_all(f.ws.join("box/inner")).unwrap();
    std::fs::write(f.ws.join("box/inner/locked.txt"), "l").unwrap();
    for (what, error) in [
        (
            "delete proj",
            c.delete(FilesDeleteParams {
                session: s.clone(),
                path: "proj".into(),
                recursive: true,
            })
            .await
            .unwrap_err(),
        ),
        (
            "rename proj",
            sw(&c)
                .rename(FilesRenameParams {
                    session: s.clone(),
                    from: "proj".into(),
                    to: "proj2".into(),
                })
                .await
                .unwrap_err(),
        ),
        (
            "delete box",
            c.delete(FilesDeleteParams {
                session: s.clone(),
                path: "box".into(),
                recursive: true,
            })
            .await
            .unwrap_err(),
        ),
    ] {
        assert_eq!(code(&error), "refused", "{what}: {error}");
    }
    assert!(f.ws.join("proj/.git/HEAD").exists() && f.ws.join("box/inner/locked.txt").exists());
    // Reading `.git` is a read; a read deny rule hides and refuses.
    download(&c, &s, ".git/config", 1024).await;
    let read = c
        .read(FilesReadParams {
            session: s.clone(),
            path: "secret.txt".into(),
            offset: 0,
            length: None,
            if_match: None,
        })
        .await
        .unwrap_err();
    assert_eq!(code(&read), "refused");
    assert!(read.to_string().contains("denied"), "{read}");
    let listed = sw(&c)
        .list(FilesListParams {
            session: s.clone(),
            path: String::new(),
            cursor: None,
            limit: None,
        })
        .await
        .unwrap();
    let names: Vec<_> = listed.entries.iter().map(|e| e.name.as_str()).collect();
    assert!(!names.contains(&"secret.txt"), "{names:?}");
    assert!(names.contains(&"plain.txt"), "{names:?}");
    assert_eq!(listed.hidden, 1);
    let log = log_text(&f.log).await;
    assert!(log.contains(r#""by": String("protected")"#), "{log}");
    f.rt.shutdown().await;
}

#[tokio::test]
async fn read_only_and_plan_modes_allow_reads_only() {
    for mode in ["read-only", "plan"] {
        let f = runtime(Options {
            mode,
            ..Options::default()
        })
        .await;
        std::fs::write(f.ws.join("a.txt"), "a").unwrap();
        let c = client(&f.rt, None).await;
        let s = new_session(&c, &f.ws).await;
        download(&c, &s, "a.txt", 10).await;
        let error = upload(&c, &s, "b.txt", b"b", 1).await.unwrap_err();
        assert_eq!(code(&error), "refused", "{mode}: {error}");
        assert_eq!(
            code(&c.mkdir(path(&s, "d")).await.unwrap_err()),
            "refused",
            "{mode}"
        );
        let info = c.info(SessionParams { session: s.clone() }).await.unwrap();
        assert!(!info.terminal.available, "{mode}: {info:?}");
        f.rt.shutdown().await;
    }
}

#[tokio::test]
async fn ask_mode_takes_the_clicking_user_as_the_approver() {
    let f = runtime(Options {
        mode: "ask",
        ..Options::default()
    })
    .await;
    let c = client(&f.rt, None).await;
    let s = new_session(&c, &f.ws).await;
    upload(&c, &s, "ok.txt", b"fine", 2).await.unwrap();
    let log = log_text(&f.log).await;
    assert!(log.contains(r#""by": String("user")"#), "{log}");
    f.rt.shutdown().await;
}

fn edge(id: &str) -> Caller {
    let principal = Principal::new(PrincipalKind::Person, "https://idp.example", id).unwrap();
    Caller::asserted(principal, Tenant::new("acme").unwrap()).unwrap()
}

#[tokio::test]
async fn the_tool_lock_serves_reads_and_refuses_every_change() {
    let f = runtime(Options {
        lock: true,
        ..Options::default()
    })
    .await;
    std::fs::write(f.ws.join("a.txt"), "a").unwrap();
    std::fs::write(f.dir.path().join("elsewhere"), "x").unwrap();
    #[cfg(unix)]
    std::fs::hard_link(f.dir.path().join("elsewhere"), f.ws.join("hard")).unwrap();
    for caller in [None, Some(edge("alice"))] {
        let outside = caller.is_some();
        let c = client(&f.rt, caller.clone()).await;
        let r = raw_client(&f.rt, caller).await;
        let caps = caps(&c);
        assert!(caps.session_files && !caps.session_files_write && !caps.session_terminal);
        let folder = f.ws.join(if outside { "alice" } else { "host" });
        std::fs::create_dir_all(&folder).unwrap();
        std::fs::write(folder.join("a.txt"), "a").unwrap();
        let s = new_session(&c, &folder).await;
        download(&c, &s, "a.txt", 10).await;
        let info = c.info(SessionParams { session: s.clone() }).await.unwrap();
        assert!(
            info.files.available && !info.files_write.available,
            "{info:?}"
        );
        assert!(!info.terminal.available, "{info:?}");
        // The client refuses what the server did not offer; the server
        // refuses the raw call too.
        let refused = raw(
            &r,
            loop_protocol::rpc::methods::SESSION_FILES_UPLOAD_BEGIN,
            json!({"session": s, "path": "b.txt", "size": 1}),
        )
        .await
        .unwrap_err();
        assert_eq!(code(&refused), "tool_locked", "{refused}");
        for method in [
            loop_protocol::rpc::methods::SESSION_FILES_DELETE,
            loop_protocol::rpc::methods::SESSION_FILES_MKDIR,
        ] {
            let refused = raw(&r, method, json!({"session": s, "path": "a.txt"}))
                .await
                .unwrap_err();
            assert_eq!(code(&refused), "tool_locked", "{method}: {refused}");
        }
        let refused = raw(
            &r,
            loop_protocol::rpc::methods::SESSION_TERMINAL_OPEN,
            json!({"session": s, "cols": 80, "rows": 24}),
        )
        .await
        .unwrap_err();
        let expected = if outside { "forbidden" } else { "tool_locked" };
        assert_eq!(code(&refused), expected, "{refused}");
        assert!(folder.join("a.txt").exists());
    }
    // A file with two hard links is hidden and refused under the lock.
    #[cfg(unix)]
    {
        let c = client(&f.rt, None).await;
        let root_session = {
            let folder = f.ws.join("links");
            std::fs::create_dir_all(&folder).unwrap();
            std::fs::hard_link(f.dir.path().join("elsewhere"), folder.join("hard")).unwrap();
            std::fs::write(folder.join("plain"), "p").unwrap();
            new_session(&c, &folder).await
        };
        let listed = sw(&c)
            .list(FilesListParams {
                session: root_session.clone(),
                path: String::new(),
                cursor: None,
                limit: None,
            })
            .await
            .unwrap();
        let names: Vec<_> = listed.entries.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, ["plain"]);
        assert_eq!(listed.hidden, 1);
        let read = c
            .read(FilesReadParams {
                session: root_session.clone(),
                path: "hard".into(),
                offset: 0,
                length: None,
                if_match: None,
            })
            .await
            .unwrap_err();
        assert!(
            matches!(code(&read).as_str(), "refused" | "invalid_path"),
            "{read}"
        );
        // Refused by the gate (the lock's one-link rule) before the filter.
        let stat = c.stat(path(&root_session, "hard")).await.unwrap_err();
        assert!(
            matches!(code(&stat).as_str(), "refused" | "not_found"),
            "{stat}"
        );
    }
    f.rt.shutdown().await;
}

#[tokio::test]
async fn another_caller_gets_session_not_found_and_cannot_use_an_upload_id() {
    let f = runtime(Options::default()).await;
    let host = client(&f.rt, None).await;
    let s = new_session(&host, &f.ws).await;
    let up = host
        .upload_begin(UploadBeginParams {
            session: s.clone(),
            path: "u.txt".into(),
            size: 3,
            sha256: None,
            if_match: None,
            overwrite: false,
        })
        .await
        .unwrap()
        .upload;
    assert!(up.starts_with("up-") && up.len() == 35, "{up}");
    // In process: an outside caller of another tenant and principal. The
    // wire refuses an outside caller without a lock, so the Runtime is
    // called directly with that caller in scope.
    let rt = f.rt.clone();
    let session = s.clone();
    let upload = up.clone();
    let refused = edge("mallory")
        .scope(async move {
            let read = SessionWorkspace::read(
                &*rt,
                FilesReadParams {
                    session: session.clone(),
                    path: "u.txt".into(),
                    offset: 0,
                    length: None,
                    if_match: None,
                },
            )
            .await
            .unwrap_err();
            let write = SessionWorkspace::upload_write(
                &*rt,
                UploadWriteParams {
                    session: session.clone(),
                    upload,
                    offset: 0,
                    data: base64::encode(b"pwn"),
                },
            )
            .await
            .unwrap_err();
            (read, write)
        })
        .await;
    assert_eq!(code(&refused.0), "session_not_found");
    assert_eq!(code(&refused.1), "session_not_found");
    // The owner still has it; nothing was written by the other caller.
    let status = host
        .upload_status(UploadParams {
            session: s.clone(),
            upload: up.clone(),
        })
        .await
        .unwrap();
    assert_eq!(status.committed, 0);
    // An in-process call may use the Session, but it is not the owner.
    let internal = SessionWorkspace::upload_status(
        &*f.rt,
        UploadParams {
            session: s.clone(),
            upload: up.clone(),
        },
    )
    .await
    .unwrap_err();
    assert_eq!(code(&internal), "unknown_upload");
    // A guessed id on the owner's own Session is unknown.
    let guess = host
        .upload_status(UploadParams {
            session: s.clone(),
            upload: "up-00000000000000000000000000000000".into(),
        })
        .await
        .unwrap_err();
    assert_eq!(code(&guess), "unknown_upload");
    f.rt.shutdown().await;
}

#[tokio::test]
async fn closing_the_session_aborts_its_uploads() {
    let f = runtime(Options::default()).await;
    let c = client(&f.rt, None).await;
    let s = new_session(&c, &f.ws).await;
    let up = c
        .upload_begin(UploadBeginParams {
            session: s.clone(),
            path: "u.txt".into(),
            size: 3,
            sha256: None,
            if_match: None,
            overwrite: false,
        })
        .await
        .unwrap()
        .upload;
    c.upload_write(UploadWriteParams {
        session: s.clone(),
        upload: up.clone(),
        offset: 0,
        data: base64::encode(b"ab"),
    })
    .await
    .unwrap();
    loop_wire::RuntimeControl::close(&*f.rt, &s, true)
        .await
        .unwrap();
    // The temporary file goes with the upload.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        let left: Vec<_> = std::fs::read_dir(&f.ws)
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.contains("loop-upload"))
            .collect();
        if left.is_empty() {
            break;
        }
        assert!(std::time::Instant::now() < deadline, "left: {left:?}");
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    f.rt.shutdown().await;
}

/// H1, M1: a refused path and a missing one look the same. Under the tool
/// lock, through a link out, every file method answers `l/exists/…` and
/// `l/missing/…` identically; with a deny rule, `vault/real` and
/// `vault/nope` are identical too.
#[cfg(unix)]
#[tokio::test]
async fn a_refused_path_never_tells_whether_something_is_there() {
    let f = runtime(Options {
        lock: true,
        settings: json!({"toolPermissions": {"read(*vault*)": "deny", "ls(*vault*)": "deny"}}),
        ..Options::default()
    })
    .await;
    let outside = f.dir.path().join("outside");
    std::fs::create_dir_all(outside.join("exists")).unwrap();
    std::fs::write(outside.join("exists/x"), "x").unwrap();
    let folder = f.ws.join("mine");
    std::fs::create_dir_all(folder.join("vault/real")).unwrap();
    std::fs::write(folder.join("vault/real/f"), "f").unwrap();
    std::os::unix::fs::symlink(&outside, folder.join("l")).unwrap();
    let c = client(&f.rt, None).await;
    let s = new_session(&c, &folder).await;
    let all = |p: &str| {
        let s = s.clone();
        let c = c.clone();
        let p = p.to_owned();
        async move {
            let mut out = Vec::new();
            out.push(format!("{:?}", c.stat(path(&s, &p)).await.err()));
            out.push(format!(
                "{:?}",
                sw(&c)
                    .list(FilesListParams {
                        session: s.clone(),
                        path: p.clone(),
                        cursor: None,
                        limit: None
                    })
                    .await
                    .err()
            ));
            out.push(format!(
                "{:?}",
                c.read(FilesReadParams {
                    session: s.clone(),
                    path: p.clone(),
                    offset: 0,
                    length: None,
                    if_match: None
                })
                .await
                .err()
            ));
            out
        }
    };
    for (there, missing) in [
        ("l/exists/x", "l/missing/x"),
        ("l/exists", "l/missing"),
        ("vault/real", "vault/nope"),
        ("vault/real/f", "vault/nope/f"),
    ] {
        let a = all(there).await;
        let b = all(missing).await;
        assert_eq!(a, b, "{there} vs {missing}");
        assert!(a.iter().all(|e| e.contains("Refused")), "{there}: {a:?}");
    }
    f.rt.shutdown().await;
}

/// M2: a folder swapped for a link to a folder outside, again and again
/// during delete, rename and mkdir: nothing outside is ever touched.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_folder_swapped_for_a_link_never_leads_a_change_outside() {
    let f = runtime(Options::default()).await;
    let outside = f.dir.path().join("outside");
    std::fs::create_dir_all(outside.join("b")).unwrap();
    std::fs::write(outside.join("b/keep.txt"), "keep").unwrap();
    let real = f.ws.join("realA");
    std::fs::create_dir_all(real.join("b")).unwrap();
    std::os::unix::fs::symlink(&outside, f.ws.join("linkA")).unwrap();
    let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let swapper = {
        let (ws, stop) = (f.ws.clone(), stop.clone());
        std::thread::spawn(move || {
            while !stop.load(std::sync::atomic::Ordering::Relaxed) {
                for name in ["realA", "linkA"] {
                    if std::fs::rename(ws.join(name), ws.join("a")).is_ok() {
                        std::thread::yield_now();
                        let _ = std::fs::rename(ws.join("a"), ws.join(name));
                    }
                }
            }
        })
    };
    let c = client(&f.rt, None).await;
    let s = new_session(&c, &f.ws).await;
    for _ in 0..300 {
        let _ = std::fs::create_dir_all(real.join("b"));
        let _ = c
            .delete(FilesDeleteParams {
                session: s.clone(),
                path: "a/b".into(),
                recursive: true,
            })
            .await;
        let _ = sw(&c)
            .rename(FilesRenameParams {
                session: s.clone(),
                from: "a/b".into(),
                to: "moved".into(),
            })
            .await;
        let _ = std::fs::remove_dir_all(f.ws.join("moved"));
        let _ = c.mkdir(path(&s, "a/new")).await;
        let _ = std::fs::remove_dir(real.join("new"));
    }
    stop.store(true, std::sync::atomic::Ordering::Relaxed);
    swapper.join().unwrap();
    assert_eq!(std::fs::read(outside.join("b/keep.txt")).unwrap(), b"keep");
    assert!(!outside.join("new").exists(), "mkdir reached outside");
    let names: Vec<_> = std::fs::read_dir(&outside)
        .unwrap()
        .map(|e| e.unwrap().file_name())
        .collect();
    assert_eq!(names, ["b"], "outside changed: {names:?}");
    f.rt.shutdown().await;
}

/// L4, L5: an upload never leaves an execute bit, also over a file that had
/// one; without overwrite, a file that appears before the commit is not
/// replaced.
#[cfg(unix)]
#[tokio::test]
async fn uploads_clear_execute_bits_and_never_replace_a_late_file() {
    use std::os::unix::fs::PermissionsExt;
    let f = runtime(Options::default()).await;
    let c = client(&f.rt, None).await;
    let s = new_session(&c, &f.ws).await;
    std::fs::write(f.ws.join("run.sh"), "old").unwrap();
    std::fs::set_permissions(f.ws.join("run.sh"), std::fs::Permissions::from_mode(0o755)).unwrap();
    upload(&c, &s, "run.sh", b"#!/bin/sh\n", 4).await.unwrap();
    let mode = std::fs::metadata(f.ws.join("run.sh"))
        .unwrap()
        .permissions()
        .mode();
    assert_eq!(mode & 0o111, 0, "{mode:o}");
    let up = c
        .upload_begin(UploadBeginParams {
            session: s.clone(),
            path: "late.txt".into(),
            size: 1,
            sha256: None,
            if_match: None,
            overwrite: false,
        })
        .await
        .unwrap()
        .upload;
    c.upload_write(UploadWriteParams {
        session: s.clone(),
        upload: up.clone(),
        offset: 0,
        data: base64::encode(b"u"),
    })
    .await
    .unwrap();
    std::fs::write(f.ws.join("late.txt"), "someone else").unwrap();
    let commit = c
        .upload_commit(UploadParams {
            session: s.clone(),
            upload: up,
        })
        .await
        .unwrap_err();
    assert_eq!(code(&commit), "exists");
    assert_eq!(
        std::fs::read(f.ws.join("late.txt")).unwrap(),
        b"someone else"
    );
    f.rt.shutdown().await;
}

/// L6: upload files a killed Runtime left behind go at the first file call
/// in the folder (only stale ones with the exact name pattern); the open
/// upload bytes of a Session have a limit.
#[tokio::test]
async fn stale_upload_files_are_swept_and_session_upload_bytes_are_limited() {
    let f = runtime(Options::default()).await;
    std::fs::create_dir_all(f.ws.join("sub")).unwrap();
    let stale = f.ws.join("sub/.a.txt.loop-upload-upload-0001");
    let fresh = f.ws.join("sub/.b.txt.loop-upload-upload-0002");
    let other = f.ws.join("sub/.c.txt.loop-uploadX");
    for file in [&stale, &fresh, &other] {
        std::fs::write(file, "t").unwrap();
    }
    let old = std::time::SystemTime::now() - std::time::Duration::from_secs(3600);
    for file in [&stale, &other] {
        std::fs::File::options()
            .write(true)
            .open(file)
            .unwrap()
            .set_modified(old)
            .unwrap();
    }
    f.rt.set_workspace_limits(loop_protocol::session_workspace::SessionWorkspaceLimits {
        upload_bytes_per_session: 100,
        ..Default::default()
    });
    let c = client(&f.rt, None).await;
    let s = new_session(&c, &f.ws).await;
    sw(&c)
        .list(FilesListParams {
            session: s.clone(),
            path: String::new(),
            cursor: None,
            limit: None,
        })
        .await
        .unwrap();
    assert!(!stale.exists() && fresh.exists() && other.exists());
    let begin = |p: &str, size| UploadBeginParams {
        session: s.clone(),
        path: p.into(),
        size,
        sha256: None,
        if_match: None,
        overwrite: false,
    };
    c.upload_begin(begin("one", 60)).await.unwrap();
    let over = c.upload_begin(begin("two", 60)).await.unwrap_err();
    assert_eq!(code(&over), "limit");
    f.rt.shutdown().await;
}
