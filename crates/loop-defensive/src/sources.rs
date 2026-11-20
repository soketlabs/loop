//! Where the code comes from: CVE records, git clones and local folders.
//! Host side; nothing here runs repository code. It downloads public records,
//! clones, and reads git objects.

use std::fs;
use std::io::Cursor;
use std::path::{Path, PathBuf};
use std::sync::LazyLock;
use std::time::Duration;

use regex::Regex;
use serde_json::{json, Value};

use crate::patchlib;
use crate::profiles;
use crate::sandbox::{run, Output};

/// A stage could not continue; carries the lifecycle status to record.
#[derive(Debug, Clone, thiserror::Error)]
#[error("{reason}")]
pub struct StopStage {
    /// `UNSUPPORTED`, `INCONCLUSIVE`, ...
    pub lifecycle: String,
    /// Why.
    pub reason: String,
}

impl StopStage {
    /// A stop with `lifecycle` and `reason`.
    pub fn new(lifecycle: &str, reason: impl Into<String>) -> Self {
        Self {
            lifecycle: lifecycle.into(),
            reason: reason.into(),
        }
    }
}

/// `~/.cache/defensive`: clones and downloaded records.
pub fn cache_dir() -> PathBuf {
    dirs::home_dir().unwrap_or_else(|| PathBuf::from("/tmp")).join(".cache").join("defensive")
}

static CVE_ID: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^CVE-\d{4}-\d{4,}$").unwrap());
static GH_COMMIT: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)https?://github\.com/([\w.-]+)/([\w.-]+)/commit/([0-9a-f]{7,40})").unwrap()
});
static GH_REPO: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)^(?:https?://)?(?:www\.)?github\.com[/:]([\w.-]+)/([\w.-]+?)(?:\.git)?/?$").unwrap()
});
static GH_TREE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)^(?:https?://)?(?:www\.)?github\.com/([\w.-]+)/([\w.-]+?)(?:\.git)?(?:/tree/(\S+?))?/?$").unwrap()
});

/// Whether `s` is a CVE id.
pub fn is_cve(s: &str) -> bool {
    CVE_ID.is_match(&s.trim().to_uppercase())
}

fn http_json(url: &str, cache_name: &str) -> Result<Option<Value>, String> {
    let p = cache_dir().join("http").join(cache_name);
    if let Ok(text) = fs::read_to_string(&p) {
        if let Ok(v) = serde_json::from_str(&text) {
            return Ok(Some(v));
        }
    }
    let url = url.to_owned();
    let fetch = async move {
        let client = reqwest::Client::builder()
            .user_agent("defensive-cve/0.1")
            .timeout(Duration::from_secs(30))
            .build()
            .map_err(|e| e.to_string())?;
        let resp = client.get(&url).send().await.map_err(|e| e.to_string())?;
        if resp.status() == reqwest::StatusCode::NOT_FOUND {
            return Ok(None);
        }
        let resp = resp.error_for_status().map_err(|e| e.to_string())?;
        resp.json::<Value>().await.map(Some).map_err(|e| e.to_string())
    };
    // Callers run on blocking threads (`spawn_blocking`), where `block_on` is allowed.
    let data = match tokio::runtime::Handle::try_current() {
        Ok(handle) => handle.block_on(fetch),
        Err(_) => tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|e| e.to_string())?
            .block_on(fetch),
    }?;
    if let Some(v) = &data {
        if let Some(parent) = p.parent() {
            let _ = fs::create_dir_all(parent);
        }
        let _ = fs::write(&p, v.to_string());
    }
    Ok(data)
}

/// `git` with hooks off and file protocol disallowed (we only read).
pub fn git(args: &[&str], cwd: Option<&Path>) -> Output {
    let mut argv: Vec<String> = ["git", "-c", "core.hooksPath=/dev/null", "-c", "protocol.file.allow=never"]
        .iter()
        .map(|s| (*s).to_owned())
        .collect();
    argv.extend(args.iter().map(|s| (*s).to_owned()));
    run(&argv, cwd, None, Duration::from_secs(900))
}

fn git_ok(args: &[&str], cwd: Option<&Path>) -> Result<String, String> {
    let r = git(args, cwd);
    if r.ok() {
        Ok(r.stdout)
    } else {
        let head: Vec<&str> = args.iter().take(3).copied().collect();
        Err(format!("git {}: {}", head.join(" "), r.stderr.trim().chars().take(300).collect::<String>()))
    }
}

fn git_bytes(args: &[&str], cwd: &Path) -> Result<Vec<u8>, String> {
    let out = std::process::Command::new("git")
        .args(["-c", "core.hooksPath=/dev/null"])
        .args(args)
        .current_dir(cwd)
        .output()
        .map_err(|e| e.to_string())?;
    if out.status.success() {
        Ok(out.stdout)
    } else {
        Err(String::from_utf8_lossy(&out.stderr).trim().to_owned())
    }
}

/// The public facts about a CVE.
#[derive(Debug, Clone, PartialEq)]
pub struct CveInfo {
    /// Upper-case id.
    pub cve: String,
    /// English description.
    pub description: String,
    /// CWE ids.
    pub cwes: Vec<String>,
    /// Publication date.
    pub published: Option<String>,
    /// `(owner, repo, sha)` of GitHub fix commits from official references.
    pub commits: Vec<(String, String, String)>,
    /// `cve.org`, `osv.dev`.
    pub sources: Vec<String>,
}

/// Stage 0 for a CVE: its record on cve.org and osv.dev.
pub fn resolve_cve(cve: &str) -> Result<CveInfo, StopStage> {
    let cve = cve.trim().to_uppercase();
    if !CVE_ID.is_match(&cve) {
        return Err(StopStage::new("UNSUPPORTED", format!("not a CVE id: '{cve}'")));
    }
    let net = |e: String| StopStage::new("UNSUPPORTED", format!("could not download the CVE record: {e}"));
    let rec = http_json(&format!("https://cveawg.mitre.org/api/cve/{cve}"), &format!("cve_{cve}.json")).map_err(net)?;
    let osv = http_json(&format!("https://api.osv.dev/v1/vulns/{cve}"), &format!("osv_{cve}.json")).map_err(net)?;
    parse_cve(&cve, rec.as_ref(), osv.as_ref())
}

/// [`resolve_cve`] from already downloaded records.
pub fn parse_cve(cve: &str, rec: Option<&Value>, osv: Option<&Value>) -> Result<CveInfo, StopStage> {
    if rec.is_none() && osv.is_none() {
        return Err(StopStage::new("UNSUPPORTED", "no CVE record and no OSV record"));
    }
    let (mut desc, mut cwes, mut published, mut urls) = (String::new(), Vec::<String>::new(), None, Vec::<String>::new());
    let s = |v: &Value| v.as_str().unwrap_or("").to_owned();
    if let Some(rec) = rec {
        if rec.pointer("/cveMetadata/state").and_then(Value::as_str) == Some("REJECTED") {
            return Err(StopStage::new("UNSUPPORTED", "CVE is REJECTED"));
        }
        published = rec.pointer("/cveMetadata/datePublished").and_then(Value::as_str).map(str::to_owned);
        let cna = rec.pointer("/containers/cna").cloned().unwrap_or(json!({}));
        desc = cna["descriptions"]
            .as_array()
            .into_iter()
            .flatten()
            .find(|d| d["lang"].as_str().unwrap_or("").starts_with("en"))
            .map(|d| s(&d["value"]))
            .unwrap_or_default();
        for pt in cna["problemTypes"].as_array().into_iter().flatten() {
            for d in pt["descriptions"].as_array().into_iter().flatten() {
                if let Some(id) = d["cweId"].as_str().filter(|x| !x.is_empty()) {
                    cwes.push(id.to_owned());
                }
            }
        }
        urls.extend(cna["references"].as_array().into_iter().flatten().map(|r| s(&r["url"])));
    }
    if let Some(osv) = osv {
        if desc.is_empty() {
            desc = osv["details"].as_str().filter(|d| !d.is_empty()).map(str::to_owned).unwrap_or_else(|| s(&osv["summary"]));
        }
        urls.extend(osv["references"].as_array().into_iter().flatten().map(|r| s(&r["url"])));
        for c in osv.pointer("/database_specific/cwe_ids").and_then(Value::as_array).into_iter().flatten() {
            let c = s(c);
            if !cwes.contains(&c) {
                cwes.push(c);
            }
        }
    }
    let mut commits: Vec<(String, String, String)> = Vec::new();
    for u in &urls {
        if let Some(m) = GH_COMMIT.captures(u) {
            let name = m[2].strip_suffix(".git").unwrap_or(&m[2]).to_owned();
            let c = (m[1].to_owned(), name, m[3].to_lowercase());
            if !commits.contains(&c) {
                commits.push(c);
            }
        }
    }
    cwes.sort();
    cwes.dedup();
    let mut sources = Vec::new();
    if rec.is_some() {
        sources.push("cve.org".to_owned());
    }
    if osv.is_some() {
        sources.push("osv.dev".to_owned());
    }
    Ok(CveInfo {
        cve: cve.to_owned(),
        description: desc.trim().to_owned(),
        cwes,
        published,
        commits,
        sources,
    })
}

/// A CVE task: the vulnerable source, the reference fix and the hidden oracle.
#[derive(Debug, Clone)]
pub struct TaskSource {
    /// `owner/name`.
    pub repo: String,
    /// Full fix commit.
    pub fix_commit: String,
    /// Its parent.
    pub vulnerable_commit: String,
    /// Go toolchain version.
    pub go_version: String,
    /// Source at the vulnerable commit (no `.git`).
    pub baseline_dir: PathBuf,
    /// Non-test files the fix changes.
    pub code_files: Vec<String>,
    /// Test files the fix adds or changes (the oracle).
    pub test_files: Vec<String>,
    /// Test functions the fix adds or changes.
    pub oracle_tests: Vec<String>,
    /// Packages of the oracle tests.
    pub oracle_packages: Vec<String>,
    /// The upstream fix, code only.
    pub reference_patch: String,
    /// The language profile (`go` for Go modules).
    pub profile: crate::profiles::Profile,
    /// Outside Go: the command that runs only the oracle tests.
    pub oracle_command: String,
}

/// A bare, blobless clone in the cache.
pub fn clone_bare(owner: &str, name: &str) -> Result<PathBuf, String> {
    let dst = cache_dir().join("git").join(format!("{owner}__{name}"));
    if !dst.join("HEAD").exists() {
        if let Some(parent) = dst.parent() {
            fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        }
        let url = format!("https://github.com/{owner}/{name}.git");
        git_ok(&["clone", "--bare", "--filter=blob:none", &url, &dst.to_string_lossy()], None)?;
    }
    Ok(dst)
}

fn extract_archive(repo: &Path, sha: &str, dst: &Path) -> Result<(), String> {
    if dst.exists() {
        fs::remove_dir_all(dst).map_err(|e| e.to_string())?;
    }
    fs::create_dir_all(dst).map_err(|e| e.to_string())?;
    let tar = git_bytes(&["archive", "--format=tar", sha], repo)?;
    let mut archive = tar::Archive::new(Cursor::new(tar));
    // Like Python's `filter="data"`: no absolute paths, no `..`, no devices.
    for entry in archive.entries().map_err(|e| e.to_string())? {
        let mut entry = entry.map_err(|e| e.to_string())?;
        let path = entry.path().map_err(|e| e.to_string())?.into_owned();
        let Some(rel) = path.to_str().and_then(patchlib::safe_relpath) else {
            continue;
        };
        let kind = entry.header().entry_type();
        if !(kind.is_file() || kind.is_dir() || kind.is_symlink()) {
            continue;
        }
        entry.set_preserve_permissions(false);
        entry.unpack(dst.join(rel)).map_err(|e| e.to_string())?;
    }
    Ok(())
}

fn pkg_of(path: &str) -> String {
    match path.rsplit_once('/') {
        Some((dir, _)) => format!("./{dir}"),
        None => ".".to_owned(),
    }
}

/// Stages 1-2 for a CVE: try each fix commit until one splits cleanly.
pub fn fetch_and_split(info: &CveInfo, out: &Path) -> Result<TaskSource, StopStage> {
    fetch_and_split_except(info, out, &[])
}

/// [`fetch_and_split`], skipping fix commits in `skip` (ones already tried).
pub fn fetch_and_split_except(info: &CveInfo, out: &Path, skip: &[String]) -> Result<TaskSource, StopStage> {
    if info.commits.is_empty() {
        return Err(StopStage::new("UNSUPPORTED", "no GitHub fix commit in the official references"));
    }
    let mut errors = Vec::new();
    for (owner, name, sha) in info.commits.iter().filter(|(_, _, sha)| !skip.iter().any(|s| s.starts_with(sha.as_str()))) {
        let short: String = sha.chars().take(10).collect();
        match split_one(owner, name, sha, out) {
            Ok(t) => return Ok(t),
            Err(e) => errors.push(format!("{owner}/{name}@{short}: {}", e.reason)),
        }
    }
    let lifecycle = if errors.iter().any(|e| e.contains("no oracle") || e.contains("test")) {
        "INCONCLUSIVE"
    } else {
        "UNSUPPORTED"
    };
    Err(StopStage::new(lifecycle, errors.join("; ")))
}

fn split_one(owner: &str, name: &str, sha: &str, out: &Path) -> Result<TaskSource, StopStage> {
    let err = |e: String| StopStage::new("UNSUPPORTED", e);
    let repo = clone_bare(owner, name).map_err(err)?;
    let _ = git(&["fetch", "origin", sha], Some(&repo));
    let full = git_ok(&["rev-parse", "--verify", &format!("{sha}^{{commit}}")], Some(&repo)).map_err(err)?.trim().to_owned();
    let parents_line = git_ok(&["rev-list", "--parents", "-n", "1", &full], Some(&repo)).map_err(err)?;
    let parents: Vec<&str> = parents_line.split_whitespace().skip(1).collect();
    if parents.len() != 1 {
        return Err(StopStage::new("UNSUPPORTED", format!("fix commit has {} parents (merge or root)", parents.len())));
    }
    let vuln = parents[0].to_owned();
    let status = git_ok(&["diff", "--no-ext-diff", "--no-textconv", "--name-status", "-M", &vuln, &full], Some(&repo)).map_err(err)?;
    let files: Vec<Vec<&str>> = status.lines().filter(|l| !l.trim().is_empty()).map(|l| l.split('\t').collect()).collect();
    if files.iter().any(|f| f[0].starts_with('R')) {
        return Err(StopStage::new("UNSUPPORTED", "fix renames files"));
    }
    let paths: Vec<(String, String)> = files.iter().map(|f| (f[0].to_owned(), f[f.len() - 1].to_owned())).collect();

    // The vulnerable source decides the language.
    let baseline = out.join("baseline");
    extract_archive(&repo, &vuln, &baseline).map_err(err)?;
    let profile = profiles::detect(&baseline);
    let lang = profile.lang.clone();
    let base_name = |p: &str| p.rsplit('/').next().unwrap_or(p).to_owned();
    let gomod = if lang == "go" {
        if paths.iter().any(|(_, p)| matches!(base_name(p).as_str(), "go.mod" | "go.sum")) {
            return Err(StopStage::new("UNSUPPORTED", "fix changes go.mod/go.sum (new dependencies)"));
        }
        fs::read_to_string(baseline.join("go.mod")).unwrap_or_default()
    } else {
        if let Some((_, p)) = paths.iter().find(|(_, p)| patchlib::MANIFESTS.contains(&base_name(p).as_str())) {
            return Err(StopStage::new("UNSUPPORTED", format!("fix changes {p} (new dependencies cannot be installed offline)")));
        }
        String::new()
    };
    let code: Vec<String> = paths.iter().filter(|(_, p)| patchlib::is_code_file(p, &lang)).map(|(_, p)| p.clone()).collect();
    let tests: Vec<String> = paths
        .iter()
        .filter(|(s, p)| patchlib::is_test_file(p, &lang) && !s.starts_with('D'))
        .map(|(_, p)| p.clone())
        .collect();
    if tests.is_empty() {
        return Err(StopStage::new("INCONCLUSIVE", "fix commit changes no test file (no oracle)"));
    }
    if code.is_empty() {
        return Err(StopStage::new("UNSUPPORTED", format!("fix changes no non-test {} code", profile.label)));
    }
    let mut args = vec!["diff", "--no-ext-diff", "--no-textconv", vuln.as_str(), full.as_str(), "--"];
    args.extend(tests.iter().map(String::as_str));
    let test_diff = git_ok(&args, Some(&repo)).map_err(err)?;
    let (funcs, oracle_command) = if lang == "go" {
        let funcs = patchlib::changed_test_funcs(&test_diff);
        if funcs.is_empty() {
            return Err(StopStage::new("INCONCLUSIVE", "could not tell which test functions the fix changed (no oracle)"));
        }
        (funcs, String::new())
    } else {
        let cmd = crate::oracle::command(&profile, &baseline, &tests).map_err(|e| StopStage::new("INCONCLUSIVE", format!("{e} (no oracle)")))?;
        (vec![], cmd)
    };
    let mut args = vec!["diff", "--no-ext-diff", "--no-textconv", "--no-color", vuln.as_str(), full.as_str(), "--"];
    args.extend(code.iter().map(String::as_str));
    let reference = git_ok(&args, Some(&repo)).map_err(err)?;

    // Hidden oracle: the fix's test files, as they are at the fix commit.
    let oracle = out.join("oracle");
    if oracle.exists() {
        fs::remove_dir_all(&oracle).map_err(|e| err(e.to_string()))?;
    }
    for p in &tests {
        let dst = oracle.join("files").join(p);
        fs::create_dir_all(dst.parent().unwrap()).map_err(|e| err(e.to_string()))?;
        let body = git_bytes(&["show", &format!("{full}:{p}")], &repo).map_err(err)?;
        fs::write(&dst, body).map_err(|e| err(e.to_string()))?;
    }
    let mut pkgs: Vec<String> = Vec::new();
    for p in &tests {
        let pkg = pkg_of(p);
        if !pkgs.contains(&pkg) {
            pkgs.push(pkg);
        }
    }
    let oracle_json = if lang == "go" {
        json!({"run": format!("^({})$", funcs.join("|")), "tests": funcs, "packages": pkgs})
    } else {
        json!({"lang": lang, "repro_command": oracle_command, "build_command": profile.build, "test_command": profile.test,
               "existing_tests_include_oracle": true, "timeout_reproduces": true})
    };
    crate::report::write_json(&oracle.join("oracle.json"), &oracle_json).map_err(|e| err(e.to_string()))?;
    fs::write(out.join("reference.patch"), &reference).map_err(|e| err(e.to_string()))?;
    Ok(TaskSource {
        repo: format!("{owner}/{name}"),
        fix_commit: full,
        vulnerable_commit: vuln,
        go_version: profiles::go_version(&gomod),
        baseline_dir: baseline,
        code_files: code,
        test_files: tests,
        oracle_tests: funcs,
        oracle_packages: pkgs,
        reference_patch: reference,
        profile,
        oracle_command,
    })
}

/// A GitHub repository checked out at a pinned commit.
#[derive(Debug, Clone)]
pub struct RepoCheckout {
    /// The URL given.
    pub url: String,
    /// Owner.
    pub owner: String,
    /// Name.
    pub name: String,
    /// Pinned commit.
    pub sha: String,
    /// The ref asked for, or `default branch`.
    pub git_ref: String,
    /// The extracted source (no `.git`).
    pub source_dir: PathBuf,
    /// Go toolchain version (1.21 when not a Go module).
    pub go_version: String,
    /// The most common language.
    pub language: String,
}

/// `(owner, name)` of a GitHub repository URL.
pub fn parse_github_url(url: &str) -> Result<(String, String), StopStage> {
    let m = GH_REPO
        .captures(url.trim())
        .ok_or_else(|| StopStage::new("UNSUPPORTED", format!("not a GitHub repo URL: '{url}'")))?;
    Ok((m[1].to_owned(), m[2].to_owned()))
}

/// Clone a GitHub repo at a pinned commit into `out/source`.
pub fn resolve_repo(url: &str, out: &Path, git_ref: Option<&str>) -> Result<RepoCheckout, StopStage> {
    let (owner, name) = parse_github_url(url)?;
    let err = |e: String| StopStage::new("UNSUPPORTED", e);
    let bare = clone_bare(&owner, &name).map_err(err)?;
    let sha = match git_ref.filter(|r| !r.is_empty()) {
        Some(r) => {
            let _ = git(&["fetch", "origin", r], Some(&bare));
            git_ok(&["rev-parse", "--verify", &format!("{r}^{{commit}}")], Some(&bare)).map_err(err)?.trim().to_owned()
        }
        None => {
            let head = git(&["symbolic-ref", "refs/remotes/origin/HEAD"], Some(&bare)).stdout.trim().to_owned();
            let branch = head.rsplit('/').next().filter(|b| !b.is_empty()).unwrap_or("HEAD").to_owned();
            let sha = git(&["rev-parse", "--verify", &format!("{branch}^{{commit}}")], Some(&bare)).stdout.trim().to_owned();
            if sha.is_empty() {
                git_ok(&["rev-parse", "HEAD"], Some(&bare)).map_err(err)?.trim().to_owned()
            } else {
                sha
            }
        }
    };
    let src = out.join("source");
    extract_archive(&bare, &sha, &src).map_err(err)?;
    let language = profiles::detect_language(&src);
    let gomod = src.join("go.mod");
    let go_version = if gomod.is_file() {
        profiles::go_version(&fs::read_to_string(&gomod).unwrap_or_default())
    } else {
        "1.21".to_owned()
    };
    Ok(RepoCheckout {
        url: url.to_owned(),
        owner,
        name,
        sha,
        git_ref: git_ref.filter(|r| !r.is_empty()).unwrap_or("default branch").to_owned(),
        source_dir: src,
        go_version,
        language,
    })
}

/// What `/patch` was pointed at.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Target {
    /// A CVE id (upper case).
    Cve(String),
    /// A GitHub repository URL and optional ref.
    Github {
        /// `https://github.com/owner/name`.
        url: String,
        /// Branch, tag or commit from `/tree/<ref>`.
        git_ref: Option<String>,
    },
    /// An absolute path to a local folder.
    Local(PathBuf),
}

/// Classify a `/patch` argument; relative folders resolve against `cwd`.
pub fn classify_target(target: &str, cwd: &Path) -> Result<Target, StopStage> {
    let t = target.trim().trim_matches(|c| c == '\'' || c == '"');
    if CVE_ID.is_match(&t.to_uppercase()) {
        return Ok(Target::Cve(t.to_uppercase()));
    }
    if let Some(m) = GH_TREE.captures(t) {
        return Ok(Target::Github {
            url: format!("https://github.com/{}/{}", &m[1], &m[2]),
            git_ref: m.get(3).map(|r| r.as_str().to_owned()),
        });
    }
    let expanded = match t.strip_prefix("~/") {
        Some(rest) => dirs::home_dir().map(|h| h.join(rest)).unwrap_or_else(|| PathBuf::from(t)),
        None => PathBuf::from(t),
    };
    let p = if expanded.is_absolute() { expanded } else { cwd.join(expanded) };
    if !t.is_empty() && p.is_dir() {
        return Ok(Target::Local(fs::canonicalize(&p).unwrap_or(p)));
    }
    Err(StopStage::new(
        "UNSUPPORTED",
        format!("not a CVE id, a GitHub repo URL or an existing folder: '{target}'"),
    ))
}

const MAX_LOCAL_FILES: usize = 50_000;
const MAX_LOCAL_BYTES: u64 = 2 * 1024 * 1024 * 1024;

/// Copy a local folder for the sandbox: no `.git`, installed dependencies or
/// build output, and symlinks only when they stay inside the folder.
pub fn copy_local(src: &Path, dst: &Path) -> Result<(), StopStage> {
    let src = fs::canonicalize(src).map_err(|e| StopStage::new("UNSUPPORTED", e.to_string()))?;
    let home = dirs::home_dir().and_then(|h| fs::canonicalize(h).ok());
    if src == Path::new("/") || Some(&src) == home.as_ref() {
        return Err(StopStage::new(
            "UNSUPPORTED",
            format!("refusing to copy {} (pick the repository folder)", src.display()),
        ));
    }
    let io = |e: std::io::Error| StopStage::new("UNSUPPORTED", e.to_string());
    if dst.exists() {
        fs::remove_dir_all(dst).map_err(io)?;
    }
    let skip = [
        ".git", "node_modules", ".venv", "venv", "__pycache__", ".tox", ".mypy_cache", ".pytest_cache",
        ".gradle", ".next", ".nuxt",
    ];
    let mut top_skip: Vec<&str> = Vec::new();
    if src.join("Cargo.toml").is_file() || src.join("pom.xml").is_file() {
        top_skip.push("target");
    }
    if src.join("composer.json").is_file() {
        top_skip.push("vendor");
    }
    fs::create_dir_all(dst).map_err(io)?;
    let (mut n, mut size) = (0usize, 0u64);
    let walker = walkdir::WalkDir::new(&src).follow_links(false).min_depth(1).into_iter().filter_entry(|e| {
        let name = e.file_name().to_str().unwrap_or("");
        !(e.file_type().is_dir() && (skip.contains(&name) || (e.depth() == 1 && top_skip.contains(&name))))
    });
    for entry in walker {
        let entry = entry.map_err(|e| StopStage::new("UNSUPPORTED", e.to_string()))?;
        let rel = entry.path().strip_prefix(&src).unwrap_or(entry.path());
        let target = dst.join(rel);
        let ft = entry.file_type();
        if ft.is_symlink() {
            if let Ok(real) = fs::canonicalize(entry.path()) {
                if real == src || real.starts_with(&src) {
                    let link = fs::read_link(entry.path()).map_err(io)?;
                    std::os::unix::fs::symlink(link, &target).map_err(io)?;
                }
            }
        } else if ft.is_dir() {
            fs::create_dir_all(&target).map_err(io)?;
        } else if ft.is_file() {
            n += 1;
            size += entry.metadata().map(|m| m.len()).unwrap_or(0);
            if n > MAX_LOCAL_FILES || size > MAX_LOCAL_BYTES {
                return Err(StopStage::new(
                    "UNSUPPORTED",
                    format!("folder too large (> {MAX_LOCAL_FILES} files or 2 GiB without .git)"),
                ));
            }
            fs::copy(entry.path(), &target).map_err(io)?;
        }
    }
    Ok(())
}

/// HEAD of a local git checkout, and whether it has uncommitted changes.
pub fn local_commit(src: &Path) -> (Option<String>, bool) {
    let sha = git(&["rev-parse", "HEAD"], Some(src));
    let sha = sha.ok().then(|| sha.stdout.trim().to_owned()).filter(|s| !s.is_empty());
    let dirty = sha.is_some() && !git(&["status", "--porcelain"], Some(src)).stdout.trim().is_empty();
    (sha, dirty)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classify_targets() {
        let cwd = tempfile::tempdir().unwrap();
        fs::create_dir(cwd.path().join("repo")).unwrap();
        assert_eq!(classify_target("cve-2022-36009", cwd.path()).unwrap(), Target::Cve("CVE-2022-36009".into()));
        assert_eq!(
            classify_target("https://github.com/soketlabs/tensorstudio-platform-backend/tree/feat/litellm", cwd.path()).unwrap(),
            Target::Github {
                url: "https://github.com/soketlabs/tensorstudio-platform-backend".into(),
                git_ref: Some("feat/litellm".into())
            }
        );
        assert_eq!(
            classify_target("github.com/o/r.git", cwd.path()).unwrap(),
            Target::Github { url: "https://github.com/o/r".into(), git_ref: None }
        );
        match classify_target("repo", cwd.path()).unwrap() {
            Target::Local(p) => assert!(p.ends_with("repo") && p.is_absolute()),
            other => panic!("{other:?}"),
        }
        assert!(classify_target("nope/nothing", cwd.path()).is_err());
    }

    #[test]
    fn github_url_forms() {
        assert_eq!(parse_github_url("https://github.com/a/b.git").unwrap(), ("a".into(), "b".into()));
        assert_eq!(parse_github_url("github.com:a/b").unwrap(), ("a".into(), "b".into()));
        assert!(parse_github_url("https://gitlab.com/a/b").is_err());
    }

    #[test]
    fn cve_records_parse_commits_and_cwes() {
        let rec = json!({
            "cveMetadata": {"state": "PUBLISHED", "datePublished": "2022-08-01"},
            "containers": {"cna": {
                "descriptions": [{"lang": "en", "value": "Bad thing. "}],
                "problemTypes": [{"descriptions": [{"cweId": "CWE-79"}]}],
                "references": [{"url": "https://github.com/o/r/commit/ABCDEF1234567"}]
            }}
        });
        let osv = json!({"references": [{"url": "https://github.com/o/r.git/commit/abcdef1234567"}],
                         "database_specific": {"cwe_ids": ["CWE-20", "CWE-79"]}});
        let info = parse_cve("CVE-2022-1", Some(&rec), Some(&osv)).unwrap();
        assert_eq!(info.description, "Bad thing.");
        assert_eq!(info.cwes, vec!["CWE-20", "CWE-79"]);
        assert_eq!(info.commits, vec![("o".into(), "r".into(), "abcdef1234567".into())]);
        assert_eq!(info.sources, vec!["cve.org", "osv.dev"]);
        let rejected = json!({"cveMetadata": {"state": "REJECTED"}});
        assert_eq!(parse_cve("CVE-1", Some(&rejected), None).unwrap_err().reason, "CVE is REJECTED");
    }

    #[test]
    fn copy_local_skips_deps_and_outside_links() {
        let src = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        fs::write(outside.path().join("secret"), "s").unwrap();
        fs::create_dir_all(src.path().join("node_modules/x")).unwrap();
        fs::create_dir_all(src.path().join("src")).unwrap();
        fs::write(src.path().join("src/a.js"), "a").unwrap();
        std::os::unix::fs::symlink(outside.path().join("secret"), src.path().join("leak")).unwrap();
        std::os::unix::fs::symlink("src/a.js", src.path().join("inside")).unwrap();
        let dst = tempfile::tempdir().unwrap();
        let out = dst.path().join("copy");
        copy_local(src.path(), &out).unwrap();
        assert!(out.join("src/a.js").is_file());
        assert!(!out.join("node_modules").exists());
        assert!(fs::symlink_metadata(out.join("leak")).is_err());
        assert!(fs::symlink_metadata(out.join("inside")).unwrap().file_type().is_symlink());
    }
}
