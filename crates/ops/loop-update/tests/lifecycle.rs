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
            async {
                let installation = Installation::at(std::env::current_exe()?)?;
                let dir = installation.staging()?;
                let client = ReleaseClient::new(&args[2], &args[2], Duration::from_secs(5))?;
                let release = client.latest().await?;
                let candidate = client.download(&release, dir.path()).await?;
                installation
                    .install(&candidate, &release.version.to_string())
                    .await
            }
            .await
        }
        Some("--rollback") => {
            async {
                Installation::at(std::env::current_exe()?)?
                    .rollback()
                    .await?;
                Ok(())
            }
            .await
        }
        Some("--uninstall") => {
            Installation::at(std::env::current_exe().unwrap()).and_then(Installation::uninstall)
        }
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

struct Server {
    url: String,
    bodies: Arc<Mutex<HashMap<String, Vec<u8>>>>,
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}
impl Server {
    fn new() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let url = format!("http://{}/", listener.local_addr().unwrap());
        let bodies = Arc::new(Mutex::new(HashMap::<String, Vec<u8>>::new()));
        let stop = Arc::new(AtomicBool::new(false));
        let thread = {
            let bodies = bodies.clone();
            let stop = stop.clone();
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
                            let body = bodies.lock().unwrap().get(path).cloned();
                            let status = if body.is_some() {
                                "200 OK"
                            } else {
                                "404 Not Found"
                            };
                            let body = body.unwrap_or_default();
                            let _ = write!(stream, "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", body.len());
                            if let Err(error) = stream.write_all(&body) {
                                eprintln!("Fixture response {path}: {error}");
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
            stop,
            thread: Some(thread),
        }
    }
    fn set(&self, path: &str, body: Vec<u8>) {
        self.bodies.lock().unwrap().insert(path.into(), body);
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
    if cfg!(windows) {
        let mut zip = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
        zip.start_file(member, zip::write::SimpleFileOptions::default())
            .unwrap();
        zip.write_all(&fs::read(binary).unwrap()).unwrap();
        (
            format!("loop-{target}.zip"),
            zip.finish().unwrap().into_inner(),
        )
    } else {
        let gzip = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
        let mut tar = tar::Builder::new(gzip);
        tar.append_path_with_name(binary, member).unwrap();
        (
            format!("loop-{target}.tar.gz"),
            tar.into_inner().unwrap().finish().unwrap(),
        )
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
    let mut installer = if cfg!(windows) {
        let mut c = Command::new("pwsh");
        c.args(["-NoProfile", "-File"])
            .arg(repo.join("install.ps1"))
            .arg("-InstallDir")
            .arg(&bin)
            .arg("-Binary")
            .arg(&v1)
            .args(["-Checksum", &hash, "-NoModifyPath"]);
        c
    } else {
        let mut c = Command::new("bash");
        c.arg(repo.join("install.sh"))
            .arg("--binary")
            .arg(&v1)
            .args(["--checksum", &hash, "--no-modify-path"])
            .env("LOOP_INSTALL_DIR", &bin);
        c
    };
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
    let mut bad_installer = if cfg!(windows) {
        let mut c = Command::new("pwsh");
        c.args(["-NoProfile", "-File"])
            .arg(repo.join("install.ps1"))
            .arg("-InstallDir")
            .arg(&bin)
            .arg("-Binary")
            .arg(&v2)
            .args(["-Checksum", &bad_hash, "-NoModifyPath"]);
        c
    } else {
        let mut c = Command::new("bash");
        c.arg(repo.join("install.sh"))
            .arg("--binary")
            .arg(&v2)
            .args(["--checksum", &bad_hash, "--no-modify-path"])
            .env("LOOP_INSTALL_DIR", &bin);
        c
    };
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

    let server = Server::new();
    server.set(
        "/latest",
        br#"{"tag_name":"v0.3.5","draft":false,"prerelease":false}"#.to_vec(),
    );
    let client = ReleaseClient::new(&server.url, &server.url, Duration::from_secs(5)).unwrap();
    assert!(loop_update::is_newer("0.3.4", &client.latest().await.unwrap()).unwrap());
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

    let installation = Installation::at(exe.clone()).unwrap();
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
