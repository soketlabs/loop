//! Checks of chunked transfer: downloads (`read_chunk`, with
//! `chunked_transfer`) and upload sessions (with `upload`).

use std::future::Future;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use loop_workspace::{
    FileTag, PathNamespace, UploadRequest, Workspace, WorkspaceError, WorkspacePath,
};

use crate::{DriverFixture, OUTSIDE_SECRET, OUTSIDE_SECRET_TEXT};

/// SHA-256 of no bytes.
const SHA_EMPTY: &str = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";
/// SHA-256 of `x`.
const SHA_X: &str = "2d711642b726b04401627ca9fbac32f5c8530fb1903cc4db02258717921a4881";
/// SHA-256 of `abc`.
const SHA_ABC: &str = "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad";

fn wp(rel: &str) -> WorkspacePath {
    WorkspacePath::parse(rel).unwrap_or_else(|e| panic!("bad check path {rel}: {e}"))
}

fn quote(path: &Path) -> String {
    format!("'{}'", path.display().to_string().replace('\'', "'\\''"))
}

fn upload_request(path: &WorkspacePath, size: u64) -> UploadRequest {
    UploadRequest {
        path: path.clone(),
        size,
        sha256: None,
        if_match: None,
        mode: None,
    }
}

/// A small deterministic random source (no dependency).
struct Lcg(u64);

impl Lcg {
    fn next(&mut self) -> u64 {
        self.0 = self
            .0
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        self.0 >> 33
    }
    fn below(&mut self, n: u64) -> u64 {
        if n == 0 {
            0
        } else {
            self.next() % n
        }
    }
}

fn content(size: usize, seed: u64) -> Vec<u8> {
    let mut rng = Lcg(seed);
    (0..size).map(|_| rng.next() as u8).collect()
}

/// Download the whole file in chunks of `piece` with `if_match`.
async fn download(
    id: &str,
    ws: &Arc<dyn Workspace>,
    path: &WorkspacePath,
    piece: usize,
) -> (Vec<u8>, FileTag) {
    let mut out = Vec::new();
    let mut tag: Option<FileTag> = None;
    loop {
        let chunk = ws
            .read_chunk(path, out.len() as u64, piece, tag.as_ref())
            .await
            .unwrap_or_else(|e| panic!("`{id}`: read_chunk {path} at {}: {e}", out.len()));
        assert_eq!(chunk.offset, out.len() as u64, "`{id}`: chunk offset");
        if let Some(t) = &tag {
            assert_eq!(t, &chunk.tag, "`{id}`: the tag stays while the file does");
        }
        tag = Some(chunk.tag.clone());
        out.extend(&chunk.data);
        if chunk.eof {
            assert_eq!(out.len() as u64, chunk.total_size, "`{id}`: total size");
            return (out, chunk.tag);
        }
        assert!(!chunk.data.is_empty(), "`{id}`: empty chunk before eof");
    }
}

/// Upload `data` in one write and commit.
async fn upload_all(
    ws: &Arc<dyn Workspace>,
    request: UploadRequest,
    data: &[u8],
) -> Result<FileTag, WorkspaceError> {
    let upload = ws.begin_upload(request).await?;
    if !data.is_empty() {
        ws.upload_write(&upload, 0, data).await?;
    }
    ws.upload_commit(&upload).await
}

/// Names in the folder of `dir`, sorted.
async fn names(ws: &Arc<dyn Workspace>, dir: &str) -> Vec<String> {
    let mut names: Vec<String> = ws
        .env()
        .list_dir(&ws.resolve(&wp(dir)))
        .await
        .expect("list_dir")
        .into_iter()
        .filter_map(|i| i.path.file_name().map(|n| n.to_string_lossy().into_owned()))
        .collect();
    names.sort();
    names
}

async fn start<F, Fut>(
    factory: &F,
    need_upload: bool,
) -> Option<(String, DriverFixture, Arc<dyn Workspace>)>
where
    F: Fn() -> Fut,
    Fut: Future<Output = DriverFixture>,
{
    let f = factory().await;
    let id = f.driver.id().to_owned();
    let ws = f.start().await;
    let caps = ws.capabilities();
    if !caps.chunked_transfer || (need_upload && !caps.upload) {
        ws.destroy().await.expect("destroy");
        return None;
    }
    Some((id, f, ws))
}

/// Run every transfer check.
pub async fn transfers_conform<F, Fut>(factory: &F)
where
    F: Fn() -> Fut,
    Fut: Future<Output = DriverFixture>,
{
    transfer_round_trips(factory).await;
    transfer_resumes_and_refuses_gaps(factory).await;
    transfer_checks_size_hash_and_tag(factory).await;
    transfer_refuses_links_and_escapes(factory).await;
    transfer_is_atomic(factory).await;
    transfer_abort_and_expiry_clean_up(factory).await;
    transfer_download_sees_changes(factory).await;
}

/// 0 B, 1 B and 64 MiB round trips. The 64 MiB upload resends and
/// overlaps chunks at random offsets, and tries writes after the committed
/// offset, which are refused and change nothing.
pub async fn transfer_round_trips<F, Fut>(factory: &F)
where
    F: Fn() -> Fut,
    Fut: Future<Output = DriverFixture>,
{
    let Some((id, _f, ws)) = start(factory, true).await else {
        return;
    };
    let empty = wp("kit-up/empty.bin");
    let tag = upload_all(
        &ws,
        UploadRequest {
            sha256: Some(SHA_EMPTY.into()),
            ..upload_request(&empty, 0)
        },
        b"",
    )
    .await
    .unwrap_or_else(|e| panic!("`{id}`: 0 B upload: {e}"));
    let (data, read_tag) = download(&id, &ws, &empty, 16).await;
    assert!(data.is_empty(), "`{id}`: 0 B");
    assert_eq!(tag, read_tag, "`{id}`: 0 B tag");

    let one = wp("kit-up/one.bin");
    upload_all(
        &ws,
        UploadRequest {
            sha256: Some(SHA_X.to_ascii_uppercase()),
            ..upload_request(&one, 1)
        },
        b"x",
    )
    .await
    .unwrap_or_else(|e| panic!("`{id}`: 1 B upload: {e}"));
    assert_eq!(download(&id, &ws, &one, 16).await.0, b"x", "`{id}`: 1 B");

    let size = 64 * 1024 * 1024;
    let big = content(size, 42);
    let path = wp("kit-up/deep/big.bin");
    let upload = ws
        .begin_upload(upload_request(&path, size as u64))
        .await
        .unwrap_or_else(|e| panic!("`{id}`: begin 64 MiB: {e}"));
    let mut rng = Lcg(7);
    let mut committed = 0u64;
    let mut rounds = 0;
    while committed < size as u64 {
        rounds += 1;
        assert!(rounds < 10_000, "`{id}`: the upload does not progress");
        if rng.below(8) == 0 {
            // A chunk after a gap: refused, nothing changes.
            let at = committed + 1 + rng.below(1024);
            let gap = ws.upload_write(&upload, at, b"gap").await;
            assert!(
                matches!(gap, Err(WorkspaceError::UploadOffset { .. })),
                "`{id}`: a write after the committed offset: {gap:?}"
            );
            assert_eq!(
                ws.upload_committed_offset(&upload).await.expect("offset"),
                committed,
                "`{id}`: a refused write changed the offset"
            );
            continue;
        }
        // Resend up to 1 MiB of what is stored, then new bytes.
        let back = rng.below(committed.min(1024 * 1024) + 1);
        let start = committed - back;
        let len = (1 + rng.below(3 * 1024 * 1024)).min(size as u64 - start);
        let end = start + len;
        let now = ws
            .upload_write(&upload, start, &big[start as usize..end as usize])
            .await
            .unwrap_or_else(|e| panic!("`{id}`: write at {start}: {e}"));
        assert_eq!(now, committed.max(end), "`{id}`: committed offset");
        committed = now;
    }
    // A duplicate of the last chunk after everything is in.
    ws.upload_write(&upload, size as u64 - 10, &big[size - 10..])
        .await
        .expect("duplicate of the end");
    let tag = ws
        .upload_commit(&upload)
        .await
        .unwrap_or_else(|e| panic!("`{id}`: commit 64 MiB: {e}"));
    let (back, read_tag) = download(&id, &ws, &path, 4 * 1024 * 1024).await;
    assert_eq!(read_tag, tag, "`{id}`: 64 MiB tag");
    assert!(back == big, "`{id}`: 64 MiB round trip");
    let after = ws.upload_committed_offset(&upload).await;
    assert!(
        matches!(after, Err(WorkspaceError::UnknownUpload(_))),
        "`{id}`: a committed upload is gone: {after:?}"
    );
    ws.destroy().await.expect("destroy");
}

/// After a lost connection, `upload_committed_offset` tells where to go on.
pub async fn transfer_resumes_and_refuses_gaps<F, Fut>(factory: &F)
where
    F: Fn() -> Fut,
    Fut: Future<Output = DriverFixture>,
{
    let Some((id, _f, ws)) = start(factory, true).await else {
        return;
    };
    let data = content(300_000, 3);
    let path = wp("kit-resume.bin");
    let upload = ws
        .begin_upload(upload_request(&path, data.len() as u64))
        .await
        .expect("begin");
    ws.upload_write(&upload, 0, &data[..100_000])
        .await
        .expect("first part");
    // "Reconnect": ask where to resume.
    let at = ws.upload_committed_offset(&upload).await.expect("offset") as usize;
    assert_eq!(at, 100_000, "`{id}`: resume offset");
    let early = ws.upload_commit(&upload).await;
    assert!(
        matches!(early, Err(WorkspaceError::UploadOffset { .. })),
        "`{id}`: commit before all bytes: {early:?}"
    );
    ws.upload_write(&upload, at as u64, &data[at..])
        .await
        .expect("rest");
    ws.upload_commit(&upload).await.expect("commit");
    assert!(
        download(&id, &ws, &path, 64 * 1024).await.0 == data,
        "`{id}`"
    );
    ws.destroy().await.expect("destroy");
}

/// A wrong size or hash is refused and the destination stays; `if_match`
/// fails with `Changed` when the destination changed, at begin and at
/// commit; the mode of a new file is the requested one, an existing file
/// keeps its own.
pub async fn transfer_checks_size_hash_and_tag<F, Fut>(factory: &F)
where
    F: Fn() -> Fut,
    Fut: Future<Output = DriverFixture>,
{
    let Some((id, _f, ws)) = start(factory, true).await else {
        return;
    };
    let path = wp("kit-checked.txt");
    let native = ws.resolve(&path);
    let env = ws.env();
    env.write_file(&native, b"original").await.expect("seed");

    let upload = ws
        .begin_upload(upload_request(&path, 3))
        .await
        .expect("begin");
    let long = ws.upload_write(&upload, 0, b"abcd").await;
    assert!(
        matches!(long, Err(WorkspaceError::TooLarge { .. })),
        "`{id}`: more than the declared size: {long:?}"
    );
    ws.upload_abort(&upload).await.expect("abort");

    let bad = upload_all(
        &ws,
        UploadRequest {
            sha256: Some(SHA_X.into()),
            ..upload_request(&path, 3)
        },
        b"abc",
    )
    .await;
    assert!(
        matches!(bad, Err(WorkspaceError::ChecksumMismatch(_))),
        "`{id}`: wrong hash: {bad:?}"
    );
    assert_eq!(
        env.read_text_file(&native).await.expect("read"),
        "original",
        "`{id}`: a refused upload changed the file"
    );
    upload_all(
        &ws,
        UploadRequest {
            sha256: Some(SHA_ABC.into()),
            ..upload_request(&path, 3)
        },
        b"abc",
    )
    .await
    .unwrap_or_else(|e| panic!("`{id}`: right hash: {e}"));

    let tag = ws.read_chunk(&path, 0, 1, None).await.expect("tag").tag;
    // Changed between the tag and begin (another size, so any tag sees it).
    env.write_file(&native, b"changed!").await.expect("change");
    let at_begin = ws
        .begin_upload(UploadRequest {
            if_match: Some(tag),
            ..upload_request(&path, 1)
        })
        .await;
    assert!(
        matches!(at_begin, Err(WorkspaceError::Changed { .. })),
        "`{id}`: if_match at begin: {at_begin:?}"
    );
    // Changed between begin and commit.
    let tag = ws.read_chunk(&path, 0, 1, None).await.expect("tag").tag;
    let upload = ws
        .begin_upload(UploadRequest {
            if_match: Some(tag),
            ..upload_request(&path, 1)
        })
        .await
        .expect("begin with if_match");
    ws.upload_write(&upload, 0, b"z").await.expect("write");
    env.write_file(&native, b"changed again")
        .await
        .expect("change");
    let at_commit = ws.upload_commit(&upload).await;
    assert!(
        matches!(at_commit, Err(WorkspaceError::Changed { .. })),
        "`{id}`: if_match at commit: {at_commit:?}"
    );
    assert_eq!(
        env.read_text_file(&native).await.expect("read"),
        "changed again",
        "`{id}`: a conflicting commit changed the file"
    );
    // A tag for a missing file: the file must not exist yet.
    let missing = ws
        .begin_upload(UploadRequest {
            if_match: Some(FileTag("no-such-tag".into())),
            ..upload_request(&wp("kit-not-there.txt"), 1)
        })
        .await;
    assert!(
        matches!(missing, Err(WorkspaceError::Changed { .. })),
        "`{id}`: if_match on a missing file: {missing:?}"
    );

    // Modes, where the env reports them.
    let fresh = wp("kit-mode-new.txt");
    upload_all(
        &ws,
        UploadRequest {
            mode: Some(0o600),
            ..upload_request(&fresh, 1)
        },
        b"m",
    )
    .await
    .expect("upload with mode");
    if let Ok(meta) = env.symlink_metadata(&ws.resolve(&fresh)).await {
        if let Some(mode) = meta.mode {
            assert_eq!(mode, 0o600, "`{id}`: mode of a new file");
        }
    }
    let default = wp("kit-mode-default.txt");
    upload_all(&ws, upload_request(&default, 1), b"d")
        .await
        .expect("upload");
    if let Ok(meta) = env.symlink_metadata(&ws.resolve(&default)).await {
        if let Some(mode) = meta.mode {
            assert_eq!(mode, 0o644, "`{id}`: default mode");
        }
    }
    let kept = env
        .exec(
            &format!("chmod 640 {}", quote(&ws.resolve(&default))),
            Default::default(),
        )
        .await
        .expect("chmod");
    if kept.exit_code == 0 {
        upload_all(
            &ws,
            UploadRequest {
                mode: Some(0o600),
                ..upload_request(&default, 2)
            },
            b"dd",
        )
        .await
        .expect("replace");
        if let Ok(meta) = env.symlink_metadata(&ws.resolve(&default)).await {
            if let Some(mode) = meta.mode {
                assert_eq!(mode, 0o640, "`{id}`: an existing file keeps its mode");
            }
        }
    }
    ws.destroy().await.expect("destroy");
}

/// A link at the final component is never followed (refused); a folder
/// link that leads out is refused for upload and download, and nothing is
/// written outside; `..` cannot even be named.
pub async fn transfer_refuses_links_and_escapes<F, Fut>(factory: &F)
where
    F: Fn() -> Fut,
    Fut: Future<Output = DriverFixture>,
{
    let Some((id, f, ws)) = start(factory, true).await else {
        return;
    };
    assert!(WorkspacePath::parse("../escape.txt").is_err(), "`{id}`");
    assert!(WorkspacePath::parse("/etc/passwd").is_err(), "`{id}`");
    let env = ws.env();
    let target = ws.resolve(&wp("kit-link-target.txt"));
    env.write_file(&target, b"inside").await.expect("target");
    let link = ws.resolve(&wp("kit-final-link"));
    let made = env
        .exec(
            &format!("ln -s {} {}", quote(&target), quote(&link)),
            Default::default(),
        )
        .await
        .expect("ln");
    if made.exit_code == 0 {
        let refused = upload_all(&ws, upload_request(&wp("kit-final-link"), 1), b"x").await;
        assert!(
            matches!(refused, Err(WorkspaceError::InvalidPath { .. })),
            "`{id}`: upload over a link: {refused:?}"
        );
        assert_eq!(
            env.read_text_file(&target).await.expect("read"),
            "inside",
            "`{id}`: the link was followed"
        );
    }
    let (Some(outside), PathNamespace::Host) = (&f.outside, ws.capabilities().path_namespace)
    else {
        ws.destroy().await.expect("destroy");
        return;
    };
    let out_link = ws.resolve(&wp("kit-out-link"));
    let made = env
        .exec(
            &format!("ln -s {} {}", quote(outside), quote(&out_link)),
            Default::default(),
        )
        .await
        .expect("ln");
    if made.exit_code == 0 {
        let up = upload_all(&ws, upload_request(&wp("kit-out-link/new.txt"), 1), b"x").await;
        assert!(up.is_err(), "`{id}`: upload through a link out");
        let deep = upload_all(
            &ws,
            upload_request(&wp("kit-out-link/made/new.txt"), 1),
            b"x",
        )
        .await;
        assert!(
            deep.is_err(),
            "`{id}`: upload into a new folder through a link out"
        );
        assert!(
            !outside.join("new.txt").exists() && !outside.join("made").exists(),
            "`{id}`: an upload wrote outside the workspace"
        );
        let down = ws
            .read_chunk(&wp(&format!("kit-out-link/{OUTSIDE_SECRET}")), 0, 64, None)
            .await;
        assert!(
            !matches!(&down, Ok(c) if c.data == OUTSIDE_SECRET_TEXT.as_bytes()),
            "`{id}`: a download read the outside secret"
        );
        assert!(down.is_err(), "`{id}`: download through a link out");
    }
    ws.destroy().await.expect("destroy");
}

/// A reader never sees a part of an upload: the file is the old one or the
/// new one.
pub async fn transfer_is_atomic<F, Fut>(factory: &F)
where
    F: Fn() -> Fut,
    Fut: Future<Output = DriverFixture>,
{
    let Some((id, _f, ws)) = start(factory, true).await else {
        return;
    };
    let size = 1024 * 1024;
    let path = wp("kit-atomic.bin");
    let native = ws.resolve(&path);
    ws.env()
        .write_file(&native, &vec![b'A'; size])
        .await
        .expect("seed");
    let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let reader = {
        let env = ws.env();
        let stop = Arc::clone(&stop);
        let id = id.clone();
        tokio::spawn(async move {
            let mut reads = 0;
            while !stop.load(std::sync::atomic::Ordering::Acquire) || reads == 0 {
                let bytes = env.read_binary_file(&native).await.expect("read");
                let all_a = bytes.len() == size && bytes.iter().all(|b| *b == b'A');
                let all_b = bytes.len() == size && bytes.iter().all(|b| *b == b'B');
                assert!(all_a || all_b, "`{id}`: a reader saw a partial file");
                reads += 1;
                tokio::task::yield_now().await;
            }
            reads
        })
    };
    let upload = ws
        .begin_upload(upload_request(&path, size as u64))
        .await
        .expect("begin");
    let piece = vec![b'B'; 64 * 1024];
    for i in 0..(size / piece.len()) {
        ws.upload_write(&upload, (i * piece.len()) as u64, &piece)
            .await
            .expect("write");
        tokio::task::yield_now().await;
    }
    ws.upload_commit(&upload).await.expect("commit");
    tokio::time::sleep(Duration::from_millis(50)).await;
    stop.store(true, std::sync::atomic::Ordering::Release);
    reader.await.expect("reader");
    let now = ws
        .env()
        .read_binary_file(&ws.resolve(&path))
        .await
        .expect("read");
    assert!(now.iter().all(|b| *b == b'B'), "`{id}`: the new file");
    ws.destroy().await.expect("destroy");
}

/// `upload_abort` and expiry remove the upload and its temporary file;
/// abort twice is not an error; stop removes open uploads.
pub async fn transfer_abort_and_expiry_clean_up<F, Fut>(factory: &F)
where
    F: Fn() -> Fut,
    Fut: Future<Output = DriverFixture>,
{
    let Some((id, f, ws)) = start(factory, true).await else {
        return;
    };
    ws.env()
        .create_dir(&ws.resolve(&wp("kit-clean")))
        .await
        .expect("dir");
    let before = names(&ws, "kit-clean").await;
    let path = wp("kit-clean/f.bin");

    let upload = ws
        .begin_upload(upload_request(&path, 10))
        .await
        .expect("begin");
    ws.upload_write(&upload, 0, b"12345").await.expect("write");
    ws.upload_abort(&upload).await.expect("abort");
    ws.upload_abort(&upload).await.expect("abort twice");
    let gone = ws.upload_write(&upload, 5, b"67890").await;
    assert!(
        matches!(gone, Err(WorkspaceError::UnknownUpload(_))),
        "`{id}`: write after abort: {gone:?}"
    );
    assert_eq!(
        names(&ws, "kit-clean").await,
        before,
        "`{id}`: abort left files"
    );

    let upload = ws
        .begin_upload(upload_request(&path, 10))
        .await
        .expect("begin");
    ws.upload_write(&upload, 0, b"12345").await.expect("write");
    ws.stop().await.expect("stop");
    ws.start().await.expect("start again");
    assert_eq!(
        names(&ws, "kit-clean").await,
        before,
        "`{id}`: stop left files"
    );
    assert!(
        matches!(
            ws.upload_committed_offset(&upload).await,
            Err(WorkspaceError::UnknownUpload(_))
        ),
        "`{id}`: an upload outlived stop"
    );

    if let Some(idle) = f.upload_idle {
        let upload = ws
            .begin_upload(upload_request(&path, 10))
            .await
            .expect("begin");
        ws.upload_write(&upload, 0, b"12345").await.expect("write");
        tokio::time::sleep(idle * 2 + Duration::from_millis(100)).await;
        let expired = ws.upload_committed_offset(&upload).await;
        assert!(
            matches!(expired, Err(WorkspaceError::UnknownUpload(_))),
            "`{id}`: an idle upload did not expire: {expired:?}"
        );
        assert_eq!(
            names(&ws, "kit-clean").await,
            before,
            "`{id}`: expiry left files"
        );
    }
    ws.destroy().await.expect("destroy");
}

/// A download fails with `Changed` when the file is replaced between
/// chunks; without `if_match` the next chunk has the new tag.
pub async fn transfer_download_sees_changes<F, Fut>(factory: &F)
where
    F: Fn() -> Fut,
    Fut: Future<Output = DriverFixture>,
{
    let Some((id, _f, ws)) = start(factory, false).await else {
        return;
    };
    let path = wp("kit-dl.bin");
    let native = ws.resolve(&path);
    let env = ws.env();
    env.write_file(&native, &content(100_000, 9))
        .await
        .expect("seed");
    let first = ws.read_chunk(&path, 0, 1000, None).await.expect("first");
    assert_eq!(first.data.len(), 1000, "`{id}`");
    // Replace by rename: same size, new file.
    let next = ws.resolve(&wp("kit-dl.next"));
    env.write_file(&next, &content(100_000, 10))
        .await
        .expect("next");
    let moved = env
        .exec(
            &format!("mv {} {}", quote(&next), quote(&native)),
            Default::default(),
        )
        .await
        .expect("mv");
    assert_eq!(moved.exit_code, 0, "`{id}`: mv");
    let second = ws.read_chunk(&path, 1000, 1000, Some(&first.tag)).await;
    assert!(
        matches!(second, Err(WorkspaceError::Changed { .. })),
        "`{id}`: a replaced file must fail the download: {second:?}"
    );
    let fresh = ws.read_chunk(&path, 1000, 1000, None).await.expect("fresh");
    assert_ne!(fresh.tag, first.tag, "`{id}`: new file, new tag");
    let past = ws
        .read_chunk(&path, 200_000, 10, None)
        .await
        .expect("past the end");
    assert!(past.data.is_empty() && past.eof, "`{id}`: past the end");
    let missing = ws.read_chunk(&wp("kit-no-such.bin"), 0, 10, None).await;
    assert!(missing.is_err(), "`{id}`: download of a missing file");
    ws.destroy().await.expect("destroy");
}
