//! A self-contained executable so tests exercise replacement/deletion of a
//! running program, including the Windows helper, rather than just file copies.
use loop_update::{Installation, ReleaseClient, ReleaseVersion};
use std::{
    collections::HashMap,
    fs,
    io::{Read, Write},
    net::TcpListener,
    path::{Path, PathBuf},
    process::Command,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
    time::Duration,
};

fn fixture_version() -> String {
    static VERSION: &str = "LOOP_FIXTURE_VERSION=0.3.4";
    std::hint::black_box(VERSION)
        .strip_prefix("LOOP_FIXTURE_VERSION=")
        .unwrap()
        .into()
}

#[tokio::main]
async fn main() {
    let args: Vec<_> = std::env::args().collect();
    let result = match args.get(1).map(String::as_str) {
        Some("--version") => {
            println!("loop {}", fixture_version());
            return;
        }
        Some("--update") => {
            loop_update::cancellable(async {
                Installation::managed_executable()?;
                let dir = tempfile::Builder::new()
                    .prefix("stage-download-")
                    .tempdir_in(std::env::current_exe()?.parent().unwrap())?;
                let client = ReleaseClient::new(&args[2], &args[2], Duration::from_secs(5))?;
                let release = client.latest().await?;
                let candidate = client.download(&release, dir.path()).await?;
                let installation = Installation::current()?;
                installation
                    .install(&candidate, &release.version.to_string())
                    .await
            })
            .await
        }
        Some("--hold-lock") => {
            loop_update::cancellable(async {
                let installation = Installation::current()?;
                let stage = installation.staging()?;
                fs::write(
                    installation.executable().parent().unwrap().join("ready"),
                    "ready",
                )?;
                std::future::pending::<()>().await;
                drop(stage);
                drop(installation);
                Ok(())
            })
            .await
        }
        Some("--rollback") => {
            async {
                Installation::current()?.rollback().await?;
                Ok(())
            }
            .await
        }
        Some("--uninstall") => Installation::current().and_then(Installation::uninstall),
        _ => {
            run_lifecycle().await;
            return;
        }
    };
    if let Err(error) = result {
        eprintln!("{error:?}");
        std::process::exit(1);
    }
}

#[derive(Clone)]
struct Reply {
    body: Vec<u8>,
    location: Option<String>,
    stalled: bool,
}

struct Server {
    url: String,
    bodies: Arc<Mutex<HashMap<String, Reply>>>,
    requests: Arc<Mutex<Vec<String>>>,
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}
impl Server {
    fn new() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let url = format!("http://{}/", listener.local_addr().unwrap());
        let bodies = Arc::new(Mutex::new(HashMap::<String, Reply>::new()));
        let stop = Arc::new(AtomicBool::new(false));
        let requests = Arc::new(Mutex::new(Vec::new()));
        let thread = {
            let bodies = bodies.clone();
            let stop = stop.clone();
            let requests = requests.clone();
            std::thread::spawn(move || {
                while !stop.load(Ordering::Relaxed) {
                    match listener.accept() {
                        Ok((mut stream, _)) => {
                            stream.set_nonblocking(false).unwrap();
                            stream
                                .set_read_timeout(Some(Duration::from_secs(2)))
                                .unwrap();
                            let mut request = Vec::new();
                            while !request.ends_with(b"\r\n\r\n") && request.len() < 16384 {
                                let mut byte = [0];
                                if stream.read(&mut byte).unwrap_or(0) == 0 {
                                    break;
                                }
                                request.extend_from_slice(&byte);
                            }
                            let request = String::from_utf8_lossy(&request);
                            let path = request.split_whitespace().nth(1).unwrap_or("");
                            requests.lock().unwrap().push(path.into());
                            let reply = bodies.lock().unwrap().get(path).cloned();
                            let status = match &reply {
                                Some(r) if r.location.is_some() => "302 Found",
                                Some(_) => "200 OK",
                                None => "404 Not Found",
                            };
                            let reply = reply.unwrap_or(Reply {
                                body: Vec::new(),
                                location: None,
                                stalled: false,
                            });
                            if reply.stalled {
                                while !stop.load(Ordering::Relaxed) {
                                    std::thread::sleep(Duration::from_millis(10));
                                }
                                continue;
                            }
                            let location = reply
                                .location
                                .map(|s| format!("Location: {s}\r\n"))
                                .unwrap_or_default();
                            let _ = write!(stream, "HTTP/1.1 {status}\r\n{location}Content-Length: {}\r\nConnection: close\r\n\r\n", reply.body.len());
                            if !request.starts_with("HEAD ") {
                                let _ = stream.write_all(&reply.body);
                            }
                        }
                        Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                            std::thread::sleep(Duration::from_millis(5))
                        }
                        Err(e) => panic!("{e}"),
                    }
                }
            })
        };
        Self {
            url,
            bodies,
            requests,
            stop,
            thread: Some(thread),
        }
    }
    fn set(&self, path: &str, body: Vec<u8>) {
        self.bodies.lock().unwrap().insert(
            path.into(),
            Reply {
                body,
                location: None,
                stalled: false,
            },
        );
    }
    fn redirect(&self, path: &str, location: &str) {
        self.bodies.lock().unwrap().insert(
            path.into(),
            Reply {
                body: Vec::new(),
                location: Some(location.into()),
                stalled: false,
            },
        );
    }
    fn stall(&self, path: &str) {
        self.bodies.lock().unwrap().insert(
            path.into(),
            Reply {
                body: Vec::new(),
                location: None,
                stalled: true,
            },
        );
    }
    fn request_count(&self) -> usize {
        self.requests.lock().unwrap().len()
    }
}
impl Drop for Server {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        self.thread.take().unwrap().join().unwrap();
    }
}

fn run(binary: &Path, args: &[&str], pass: bool) -> String {
    let out = Command::new(binary).args(args).output().unwrap();
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(out.status.success(), pass, "{args:?}: {text}");
    text
}

fn fixture(dir: &Path, version: &str) -> PathBuf {
    let path = dir.join(format!("fixture-{version}{}", std::env::consts::EXE_SUFFIX));
    let mut bytes = fs::read(std::env::current_exe().unwrap()).unwrap();
    let old = b"LOOP_FIXTURE_VERSION=0.3.4";
    let replacement = format!("LOOP_FIXTURE_VERSION={version}");
    assert_eq!(old.len(), replacement.len());
    for index in 0..=bytes.len() - old.len() {
        if &bytes[index..index + old.len()] == old {
            bytes[index..index + old.len()].copy_from_slice(replacement.as_bytes());
        }
    }
    fs::write(&path, bytes).unwrap();
    fs::set_permissions(
        &path,
        fs::metadata(std::env::current_exe().unwrap())
            .unwrap()
            .permissions(),
    )
    .unwrap();
    if cfg!(target_os = "macos") {
        assert!(Command::new("codesign")
            .args(["--force", "--sign", "-"])
            .arg(&path)
            .status()
            .unwrap()
            .success());
    }
    path
}

fn archive(binary: &Path) -> (String, Vec<u8>) {
    let target = loop_update::platform_target().unwrap();
    let member = format!("loop-{target}/loop{}", std::env::consts::EXE_SUFFIX);
    #[cfg(windows)]
    {
        let mut zip = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
        zip.start_file(member, zip::write::SimpleFileOptions::default())
            .unwrap();
        zip.write_all(&fs::read(binary).unwrap()).unwrap();
        (
            format!("loop-{target}.zip"),
            zip.finish().unwrap().into_inner(),
        )
    }
    #[cfg(not(windows))]
    {
        let gzip = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
        let mut tar = tar::Builder::new(gzip);
        tar.append_path_with_name(binary, member).unwrap();
        (
            format!("loop-{target}.tar.gz"),
            tar.into_inner().unwrap().finish().unwrap(),
        )
    }
}

fn installer_command(repo: &Path, bin: &Path, binary: &Path, hash: &str) -> Command {
    if cfg!(windows) {
        let shell = std::env::var("LOOP_TEST_POWERSHELL").unwrap_or_else(|_| "pwsh".into());
        let mut command = Command::new(shell);
        command
            .args(["-NoProfile", "-File"])
            .arg(repo.join("install.ps1"))
            .arg("-InstallDir")
            .arg(bin)
            .arg("-Binary")
            .arg(binary)
            .args(["-Checksum", hash, "-NoModifyPath"]);
        command
    } else {
        let mut command = Command::new("bash");
        command
            .arg(repo.join("install.sh"))
            .arg("--binary")
            .arg(binary)
            .args(["--checksum", hash, "--no-modify-path"])
            .env("LOOP_INSTALL_DIR", bin);
        command
    }
}

async fn run_lifecycle() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("user with spaces/.loop");
    let bin = root.join("bin");
    fs::create_dir_all(&bin).unwrap();
    let sentinel_paths = [
        "agent/auth.json",
        "agent/settings.json",
        "agent/sessions/session.jsonl",
        "skills/example/SKILL.md",
    ];
    for name in sentinel_paths {
        let path = root.join(name);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, "keep me").unwrap();
    }
    fs::write(bin.join("unrelated-file"), "also keep").unwrap();
    let v1 = fixture(tmp.path(), "0.3.4");
    let v2 = fixture(tmp.path(), "0.3.5");
    let exe = bin.join(format!("loop{}", std::env::consts::EXE_SUFFIX));
    let repo = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../..");
    let hash = loop_update::sha256_file(&v1).unwrap();
    let mut installer = installer_command(&repo, &bin, &v1, &hash);
    let output = installer.output().unwrap();
    assert!(
        output.status.success(),
        "Installer failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(loop_update::sha256_file(&exe).unwrap(), hash);
    assert!(run(&exe, &["--version"], true).contains("0.3.4"));

    // Both installers must preserve the current binary on bad checksums.
    let bad_hash = "0".repeat(64);
    let mut bad_installer = installer_command(&repo, &bin, &v2, &bad_hash);
    assert!(!bad_installer.output().unwrap().status.success());
    assert_eq!(loop_update::sha256_file(&exe).unwrap(), hash);

    #[cfg(unix)]
    {
        use std::os::unix::fs::symlink;
        let tools = tmp.path().join("tools-without-sha256");
        fs::create_dir(&tools).unwrap();
        for name in [
            "uname", "sysctl", "grep", "ls", "ldd", "mkdir", "rmdir", "rm", "awk", "tr", "head",
        ] {
            if let Some(source) = ["/usr/bin", "/bin", "/usr/sbin", "/sbin"]
                .iter()
                .map(|dir| Path::new(dir).join(name))
                .find(|p| p.is_file())
            {
                symlink(source, tools.join(name)).unwrap();
            }
        }
        let output = Command::new("/bin/bash")
            .arg(repo.join("install.sh"))
            .arg("--binary")
            .arg(&v2)
            .args(["--checksum", &hash, "--no-modify-path"])
            .env("LOOP_INSTALL_DIR", &bin)
            .env("PATH", &tools)
            .env("LOOP_ALLOW_UNVERIFIED", "1")
            .output()
            .unwrap();
        assert!(!output.status.success());
        assert!(
            String::from_utf8_lossy(&output.stderr).contains("No SHA-256 tool"),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(loop_update::sha256_file(&exe).unwrap(), hash);
    }

    // Repair a truncated installed binary even when a stale rollback record exists.
    fs::write(&exe, "garbage executable").unwrap();
    fs::write(bin.join(".loop-update/rollback.json"), "stale record").unwrap();
    let output = installer_command(&repo, &bin, &v1, &hash).output().unwrap();
    assert!(
        output.status.success(),
        "repair failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(loop_update::sha256_file(&exe).unwrap(), hash);
    assert!(!bin.join(".loop-update/rollback.json").exists());
    assert!(!bin.join(".loop-update/operation.lock").exists());
    assert!(run(&exe, &["--version"], true).contains("0.3.4"));

    // Installer records remain compatible with the Rust rollback operation.
    let output = installer_command(&repo, &bin, &v2, &loop_update::sha256_file(&v2).unwrap())
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "upgrade failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    run(&exe, &["--rollback"], true);
    assert!(run(&exe, &["--version"], true).contains("0.3.4"));

    #[cfg(unix)]
    {
        for signal in ["INT", "HUP", "TERM"] {
            let ready = bin.join("ready");
            let mut child = Command::new(&exe)
                .arg("--hold-lock")
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .spawn()
                .unwrap();
            for _ in 0..200 {
                if ready.exists() {
                    break;
                }
                assert!(
                    child.try_wait().unwrap().is_none(),
                    "child exited before acquiring lock"
                );
                std::thread::sleep(Duration::from_millis(25));
            }
            assert!(ready.exists());
            std::thread::sleep(Duration::from_millis(100));
            assert!(Command::new("kill")
                .args([format!("-{signal}"), child.id().to_string()])
                .status()
                .unwrap()
                .success());
            for _ in 0..200 {
                if child.try_wait().unwrap().is_some() {
                    break;
                }
                std::thread::sleep(Duration::from_millis(25));
            }
            assert!(
                child.try_wait().unwrap().is_some(),
                "signal cleanup timed out"
            );
            fs::remove_file(ready).unwrap();
            assert!(
                !bin.join(".loop-update/operation.lock").exists(),
                "signal {signal} left a lock"
            );
            assert!(!fs::read_dir(bin.join(".loop-update"))
                .unwrap()
                .flatten()
                .any(|e| e.file_name().to_string_lossy().starts_with("stage-")));
            assert_eq!(loop_update::sha256_file(&exe).unwrap(), hash);
        }
    }

    let server = Server::new();
    server.set(
        "/latest",
        br#"{"tag_name":"v0.3.5","draft":false,"prerelease":false}"#.to_vec(),
    );
    let client = ReleaseClient::new(&server.url, &server.url, Duration::from_secs(5)).unwrap();
    assert!(loop_update::is_newer("0.3.4", &client.latest().await.unwrap()).unwrap());
    // HEAD redirect lookup needs no API quota, and startup reuses a one-day cache.
    server.redirect(
        "/releases/latest",
        &format!("{}releases/tag/v0.3.5", server.url),
    );
    server.set("/releases/tag/v0.3.5", Vec::new());
    let redirect = client
        .clone()
        .with_latest_redirect(&format!("{}releases/latest", server.url))
        .unwrap();
    let cache = bin.join(".loop-update/update-check.json");
    let before = server.request_count();
    assert_eq!(
        redirect
            .latest_cached(&cache)
            .await
            .unwrap()
            .version
            .to_string(),
        "0.3.5"
    );
    let checked = server.request_count();
    assert_eq!(checked - before, 2);
    assert_eq!(
        redirect
            .latest_cached(&cache)
            .await
            .unwrap()
            .version
            .to_string(),
        "0.3.5"
    );
    assert_eq!(
        server.request_count(),
        checked,
        "fresh cache must avoid network requests"
    );
    fs::write(&cache, r#"{"checked_at":0,"tag":"v0.3.4"}"#).unwrap();
    assert_eq!(
        redirect
            .latest_cached(&cache)
            .await
            .unwrap()
            .version
            .to_string(),
        "0.3.5"
    );
    assert_eq!(
        server.request_count(),
        checked + 2,
        "stale cache must refresh"
    );
    fs::write(&cache, "invalid cache").unwrap();
    assert_eq!(
        redirect
            .latest_cached(&cache)
            .await
            .unwrap()
            .version
            .to_string(),
        "0.3.5"
    );

    let (asset, bytes) = archive(&v2);
    let asset_path = format!("/v0.3.5/{asset}");
    let hash_path = format!("{asset_path}.sha256");
    server.set(&asset_path, bytes.clone());
    // No checksum: fail before extraction/replacement.
    run(&exe, &["--update", &server.url], false);
    assert_eq!(loop_update::sha256_file(&exe).unwrap(), hash);
    server.set(
        &hash_path,
        format!("{}  {asset}", "0".repeat(64)).into_bytes(),
    );
    run(&exe, &["--update", &server.url], false);
    assert_eq!(loop_update::sha256_file(&exe).unwrap(), hash);
    let archive_file = tmp.path().join("archive");
    fs::write(&archive_file, &bytes).unwrap();
    let archive_hash = loop_update::sha256_file(&archive_file).unwrap();
    server.set(&hash_path, format!("{archive_hash}  {asset}").into_bytes());
    #[cfg(unix)]
    {
        // Cancel an in-flight download. No install lock may be held, and the
        // temporary download directory must be removed without replacing loop.
        let slow = Server::new();
        slow.set(
            "/latest",
            br#"{"tag_name":"v0.3.5","draft":false,"prerelease":false}"#.to_vec(),
        );
        slow.set(&hash_path, format!("{archive_hash}  {asset}").into_bytes());
        slow.stall(&asset_path);
        let mut child = Command::new(&exe)
            .args(["--update", &slow.url])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .unwrap();
        for _ in 0..200 {
            if slow.request_count() >= 3 {
                break;
            }
            assert!(child.try_wait().unwrap().is_none());
            std::thread::sleep(Duration::from_millis(25));
        }
        assert_eq!(slow.request_count(), 3);
        assert!(
            !bin.join(".loop-update/operation.lock").exists(),
            "download must not hold installation lock"
        );
        assert!(Command::new("kill")
            .args(["-INT".into(), child.id().to_string()])
            .status()
            .unwrap()
            .success());
        for _ in 0..200 {
            if child.try_wait().unwrap().is_some() {
                break;
            }
            std::thread::sleep(Duration::from_millis(25));
        }
        assert!(child.try_wait().unwrap().is_some());
        assert!(!fs::read_dir(&bin).unwrap().flatten().any(|e| e
            .file_name()
            .to_string_lossy()
            .starts_with("stage-download-")));
        assert_eq!(loop_update::sha256_file(&exe).unwrap(), hash);
    }
    run(&exe, &["--update", &server.url], true);
    assert!(run(&exe, &["--version"], true).contains("0.3.5"));
    assert_eq!(
        loop_update::sha256_file(&exe).unwrap(),
        loop_update::sha256_file(&v2).unwrap()
    );

    // A corrupted backup must never replace the current binary.
    let previous = bin
        .join(".loop-update")
        .join(format!("previous-{hash}{}", std::env::consts::EXE_SUFFIX));
    fs::write(&previous, "damaged").unwrap();
    run(&exe, &["--rollback"], false);
    assert!(run(&exe, &["--version"], true).contains("0.3.5"));
    fs::copy(&v1, &previous).unwrap();
    run(&exe, &["--rollback"], true);
    assert!(run(&exe, &["--version"], true).contains("0.3.4"));

    fs::write(bin.join(".loop-update/retired-fixture.exe"), "retired").unwrap();
    let installation = Installation::at(exe.clone()).unwrap();
    assert!(!bin.join(".loop-update/retired-fixture.exe").exists());
    assert!(
        Installation::at(exe.clone()).is_err(),
        "operations must be serialized"
    );
    let bad = tmp.path().join("bad");
    fs::write(&bad, "not an executable").unwrap();
    assert!(installation.install(&bad, "0.3.6").await.is_err());
    assert_eq!(loop_update::sha256_file(&exe).unwrap(), hash);
    assert!(
        installation.install(&v2, "0.3.6").await.is_err(),
        "wrong version must be rejected"
    );
    drop(installation);
    run(&exe, &["--uninstall"], true);
    // Windows defers deletion until the child and its deletion helper exit.
    for _ in 0..100 {
        if !exe.exists() {
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    assert!(!exe.exists());
    assert!(!bin.join(".loop-update").exists());
    for name in sentinel_paths {
        assert_eq!(fs::read_to_string(root.join(name)).unwrap(), "keep me");
    }
    assert_eq!(
        fs::read_to_string(bin.join("unrelated-file")).unwrap(),
        "also keep"
    );
    assert!(ReleaseVersion::parse("v0.3.5").is_ok());
    println!("Lifecycle passed: install, checked update, self-replacement, rollback, uninstall, user-data preservation");
}
