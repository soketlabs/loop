//! Path facts for the permission gate (task C2b,
//! `docs/design/workspace-driver.md` section 6).
//!
//! The decision code (path rules, protected paths, the tool lock) asks
//! three questions about a path: its canonical form, the target of the
//! link at it, and its `lstat` metadata (kind, owner, mode bits, hard
//! links). It asks them through a [`PathFacts`], so the same code decides
//! on any source of facts:
//!
//! - [`GatePaths`], the service a Session's gate uses: every fact comes
//!   from the Session's Workspace env ([`FileSystem`]), never from the
//!   Runtime host disk. The decision code is sync, so [`GatePaths::run`]
//!   runs it, fetches the first fact it missed through the env, and runs it
//!   again until nothing is missing. Facts are kept for one decision only:
//!   links can change between decisions.
//! - [`PathFacts::runtime_host`]: the Runtime host disk, for callers outside a
//!   Session's decision: the sync API (`decide`, `Call::resolve`,
//!   `search_may_show`), external agents (they run on the Runtime host),
//!   and the sync search filter of a Session whose paths are host paths.
//! - [`PathFacts::unknown`]: no facts. Every path is unresolvable, so deny
//!   and ask rules apply and allow rules and scopes do not (fail closed).
//!   The sync search filter of a guest-namespace Session uses it.
//!
//! An answer is [`Got::Yes`], [`Got::No`] (no such path, or not a link) or
//! [`Got::Unknown`] (the env could not say). Unknown fails closed in every
//! check: a path that cannot be resolved is not inside the workspace, is
//! protected, is outside the tool lock's folder, no allow rule applies to
//! it, and a deny or ask path rule that could not be compared with it
//! counts as matching.
//!
//! Limits: one decision makes at most [`MAX_ENV_CALLS`] env calls and
//! resolves no path of more than [`MAX_COMPONENTS`] components. Past a
//! limit [`GatePaths::run`] returns [`PathLimit`] and the gate refuses the
//! call, whatever the rules say. No real project path comes near them.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use loop_agent::harness::sandbox::PathNamespace;
use loop_agent::harness::types::{FileError, FileErrorCode, FileSystem, PathKind, PathMetadata};
use parking_lot::Mutex;

/// Env calls one decision may make. A decision on a real path makes fewer
/// than ten (one or two per missing path component, one per dangling link).
pub const MAX_ENV_CALLS: usize = 128;

/// Components of one path the gate resolves (a link's target included).
pub const MAX_COMPONENTS: usize = 256;

/// A decision went past a limit: the path is too deep, or has too many
/// links, to check. The gate refuses the call.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("the path is too deep or has too many links to check")]
pub struct PathLimit;

/// One answer about a path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Got<T> {
    /// The fact.
    Yes(T),
    /// No such path, or (for a link target) not a link.
    No,
    /// The source could not say. Fails closed.
    Unknown,
}

/// What the gate uses of `lstat`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Meta {
    pub(crate) kind: PathKind,
    pub(crate) uid: Option<u32>,
    pub(crate) mode: Option<u32>,
    pub(crate) nlink: Option<u64>,
}

impl From<PathMetadata> for Meta {
    fn from(meta: PathMetadata) -> Self {
        Self {
            kind: meta.kind,
            uid: meta.uid,
            mode: meta.mode,
            nlink: meta.nlink,
        }
    }
}

/// The three questions the decision code asks about a path.
pub(crate) trait Lookup: Send + Sync {
    /// The path with every link and `..` resolved; `No` when it does not
    /// exist.
    fn canonical(&self, path: &Path) -> Got<PathBuf>;
    /// The target of the link at `path`; `No` when it is not a link.
    fn read_link(&self, path: &Path) -> Got<PathBuf>;
    /// Metadata of `path` without following a final link.
    fn lstat(&self, path: &Path) -> Got<Meta>;
    /// Whether `~` names a known home folder. A guest env's home is not the
    /// Runtime's, so a `~` rule cannot be compared there (fails closed).
    fn home_known(&self) -> bool {
        true
    }
    /// The decision went past a limit ([`MAX_COMPONENTS`]).
    fn note_limit(&self) {}
}

/// Where a decision's path facts come from. There is no default: a caller
/// names the source ([`PathFacts::runtime_host`] only on the Runtime side).
#[derive(Clone)]
pub struct PathFacts(Source);

#[derive(Clone)]
enum Source {
    Host,
    Unknown,
    Probe(Arc<Probe>),
    /// Another source: the search memo (tests also count host calls).
    Other(Arc<dyn Lookup>),
}

impl PathFacts {
    /// Facts from the Runtime host disk. Only for callers outside a
    /// Session's decision (see the module notes).
    pub fn runtime_host() -> Self {
        Self(Source::Host)
    }

    /// No facts: every path is unresolvable (fail closed).
    pub fn unknown() -> Self {
        Self(Source::Unknown)
    }

    pub(crate) fn lookup(&self) -> &dyn Lookup {
        match &self.0 {
            Source::Host => &HostLookup,
            Source::Unknown => &UnknownLookup,
            Source::Probe(probe) => probe.as_ref(),
            Source::Other(lookup) => lookup.as_ref(),
        }
    }
}

impl std::fmt::Debug for PathFacts {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let name = match &self.0 {
            Source::Host => "host",
            Source::Unknown => "unknown",
            Source::Probe(_) => "env",
            Source::Other(_) => "other",
        };
        write!(f, "PathFacts({name})")
    }
}

impl PartialEq for PathFacts {
    fn eq(&self, other: &Self) -> bool {
        match (&self.0, &other.0) {
            (Source::Host, Source::Host) | (Source::Unknown, Source::Unknown) => true,
            (Source::Probe(a), Source::Probe(b)) => Arc::ptr_eq(a, b),
            _ => false,
        }
    }
}

/// The Runtime host disk.
struct HostLookup;

impl Lookup for HostLookup {
    fn canonical(&self, path: &Path) -> Got<PathBuf> {
        host_canonical(path)
    }
    fn read_link(&self, path: &Path) -> Got<PathBuf> {
        host_read_link(path)
    }
    fn lstat(&self, path: &Path) -> Got<Meta> {
        host_lstat(path)
    }
}

/// `canonicalize` on the Runtime host. An error is `No`, as the gate has
/// always read it.
fn host_canonical(path: &Path) -> Got<PathBuf> {
    match std::fs::canonicalize(path) {
        Ok(real) => Got::Yes(real),
        Err(_) => Got::No,
    }
}

/// `readlink` on the Runtime host.
fn host_read_link(path: &Path) -> Got<PathBuf> {
    match std::fs::read_link(path) {
        Ok(target) => Got::Yes(target),
        Err(_) => Got::No,
    }
}

/// `lstat` on the Runtime host.
fn host_lstat(path: &Path) -> Got<Meta> {
    let Ok(meta) = std::fs::symlink_metadata(path) else {
        return Got::No;
    };
    let kind = if meta.file_type().is_symlink() {
        PathKind::Symlink
    } else if meta.is_file() {
        PathKind::File
    } else if meta.is_dir() {
        PathKind::Dir
    } else {
        PathKind::Other
    };
    #[cfg(unix)]
    let (uid, mode, nlink) = {
        use std::os::unix::fs::MetadataExt;
        (
            Some(meta.uid()),
            Some(meta.mode() & 0o7777),
            Some(meta.nlink()),
        )
    };
    #[cfg(not(unix))]
    let (uid, mode, nlink) = (None, None, None);
    Got::Yes(Meta {
        kind,
        uid,
        mode,
        nlink,
    })
}

/// No facts at all.
struct UnknownLookup;

impl Lookup for UnknownLookup {
    fn canonical(&self, _path: &Path) -> Got<PathBuf> {
        Got::Unknown
    }
    fn read_link(&self, _path: &Path) -> Got<PathBuf> {
        Got::Unknown
    }
    fn lstat(&self, _path: &Path) -> Got<Meta> {
        Got::Unknown
    }
    fn home_known(&self) -> bool {
        false
    }
}

/// Host facts remembered across the calls of one search (the sync search
/// filter checks every walked file against the same cwd, bases and rule
/// prefixes). The memory is dropped when no call came for [`Self::GAP`]:
/// the next call belongs to another search, and links may have changed.
pub struct SearchMemo {
    state: Mutex<(std::time::Instant, HashMap<Query, Answer>)>,
}

impl Default for SearchMemo {
    fn default() -> Self {
        Self {
            state: Mutex::new((std::time::Instant::now(), HashMap::new())),
        }
    }
}

impl SearchMemo {
    /// A pause longer than this ends a search.
    pub const GAP: std::time::Duration = std::time::Duration::from_millis(100);
    /// Facts kept at most (walked files are asked about once each).
    const MAX: usize = 4096;

    fn get(&self, query: Query, fetch: impl FnOnce() -> Answer) -> Answer {
        let mut state = self.state.lock();
        let now = std::time::Instant::now();
        if now.duration_since(state.0) > Self::GAP || state.1.len() > Self::MAX {
            state.1.clear();
        }
        state.0 = now;
        if let Some(answer) = state.1.get(&query) {
            return answer.clone();
        }
        let answer = fetch();
        state.1.insert(query, answer.clone());
        answer
    }
}

impl Lookup for SearchMemo {
    fn canonical(&self, path: &Path) -> Got<PathBuf> {
        match self.get(Query::Canonical(path.to_path_buf()), || {
            Answer::Path(host_canonical(path))
        }) {
            Answer::Path(got) => got,
            Answer::Meta(_) => Got::Unknown,
        }
    }
    fn read_link(&self, path: &Path) -> Got<PathBuf> {
        match self.get(Query::ReadLink(path.to_path_buf()), || {
            Answer::Path(host_read_link(path))
        }) {
            Answer::Path(got) => got,
            Answer::Meta(_) => Got::Unknown,
        }
    }
    fn lstat(&self, path: &Path) -> Got<Meta> {
        match self.get(Query::Lstat(path.to_path_buf()), || {
            Answer::Meta(host_lstat(path))
        }) {
            Answer::Meta(got) => got,
            Answer::Path(_) => Got::Unknown,
        }
    }
}

/// One fact a decision asked for.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
enum Query {
    Canonical(PathBuf),
    ReadLink(PathBuf),
    Lstat(PathBuf),
}

#[derive(Debug, Clone)]
enum Answer {
    Path(Got<PathBuf>),
    Meta(Got<Meta>),
}

/// The facts of one decision, and the first one it missed.
#[derive(Default)]
struct Probe {
    facts: Mutex<HashMap<Query, Answer>>,
    miss: Mutex<Option<Query>>,
    /// No more env calls: a missing fact is unknown.
    full: Mutex<bool>,
    /// A limit was reached in this round.
    limited: Mutex<bool>,
    /// `~` names a home folder the facts can check (host namespace).
    home_known: bool,
}

impl Probe {
    fn get(&self, query: Query) -> Option<Answer> {
        if let Some(answer) = self.facts.lock().get(&query) {
            return Some(answer.clone());
        }
        if *self.full.lock() {
            *self.limited.lock() = true;
        } else {
            self.miss.lock().get_or_insert(query);
        }
        None
    }

    fn path(&self, query: Query) -> Got<PathBuf> {
        match self.get(query) {
            Some(Answer::Path(got)) => got,
            _ => Got::Unknown,
        }
    }
}

impl Lookup for Probe {
    fn canonical(&self, path: &Path) -> Got<PathBuf> {
        self.path(Query::Canonical(path.to_path_buf()))
    }
    fn read_link(&self, path: &Path) -> Got<PathBuf> {
        self.path(Query::ReadLink(path.to_path_buf()))
    }
    fn lstat(&self, path: &Path) -> Got<Meta> {
        match self.get(Query::Lstat(path.to_path_buf())) {
            Some(Answer::Meta(got)) => got,
            _ => Got::Unknown,
        }
    }
    fn home_known(&self) -> bool {
        self.home_known
    }
    fn note_limit(&self) {
        *self.limited.lock() = true;
    }
}

/// How much one decision cost.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct LookupStats {
    /// Calls to the env.
    pub env_calls: usize,
    /// Times the decision code ran (one more than the env calls, unless
    /// the limit was reached).
    pub rounds: usize,
    /// A limit was reached ([`PathLimit`]).
    pub limited: bool,
}

/// The gate's path-resolution service: path facts from a Session's
/// Workspace env, in the env's namespace.
#[derive(Clone)]
pub struct GatePaths {
    env: Arc<dyn FileSystem>,
    namespace: PathNamespace,
}

impl std::fmt::Debug for GatePaths {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GatePaths")
            .field("cwd", &self.env.cwd())
            .field("namespace", &self.namespace)
            .finish()
    }
}

impl GatePaths {
    /// Facts from `env`, whose paths are in `namespace`.
    pub fn new(env: Arc<dyn FileSystem>, namespace: PathNamespace) -> Self {
        Self { env, namespace }
    }

    /// The Runtime host's env, for a gate that no Session bound to a
    /// Workspace env (tests, hosts that only gate agents). Its paths are
    /// host paths, so it reads the host disk through the env.
    pub fn host() -> Self {
        static HOST: std::sync::OnceLock<GatePaths> = std::sync::OnceLock::new();
        HOST.get_or_init(|| {
            Self::new(
                Arc::new(loop_agent::harness::HostExecutionEnv::new("/")),
                PathNamespace::Host,
            )
        })
        .clone()
    }

    /// The env.
    pub fn env(&self) -> &Arc<dyn FileSystem> {
        &self.env
    }

    /// The env's path namespace.
    pub fn namespace(&self) -> PathNamespace {
        self.namespace
    }

    /// Run `decide` with facts from the env: run it, fetch the first fact
    /// it missed, and run it again until nothing is missing. `decide` must
    /// depend only on its facts (it runs more than once). Past a limit the
    /// result is [`PathLimit`]: the caller must refuse.
    pub async fn run<T>(&self, decide: impl Fn(&PathFacts) -> T) -> Result<T, PathLimit> {
        match self.run_counted(decide).await {
            (_, stats) if stats.limited => Err(PathLimit),
            (out, _) => Ok(out),
        }
    }

    /// [`Self::run`], and what it cost; `stats.limited` says whether a
    /// limit was reached (then the result must not be used).
    pub async fn run_counted<T>(&self, decide: impl Fn(&PathFacts) -> T) -> (T, LookupStats) {
        let probe = Arc::new(Probe {
            home_known: self.namespace == PathNamespace::Host,
            ..Probe::default()
        });
        let facts = PathFacts(Source::Probe(Arc::clone(&probe)));
        let mut stats = LookupStats::default();
        loop {
            stats.rounds += 1;
            *probe.limited.lock() = false;
            let out = decide(&facts);
            let Some(query) = probe.miss.lock().take() else {
                stats.limited = *probe.limited.lock();
                return (out, stats);
            };
            let answer = self.fetch(&query).await;
            stats.env_calls += 1;
            probe.facts.lock().insert(query, answer);
            if stats.env_calls >= MAX_ENV_CALLS {
                *probe.full.lock() = true;
            }
        }
    }

    /// Facts for code that cannot wait for the env (the sync search
    /// filter): the Runtime host for a host-namespace env, remembered for
    /// one search in `memo`; none for a guest one, so the host is never
    /// read for a guest path.
    pub fn sync_facts(&self, memo: &Arc<SearchMemo>) -> PathFacts {
        match self.namespace {
            PathNamespace::Host => PathFacts(Source::Other(memo.clone())),
            _ => PathFacts::unknown(),
        }
    }

    /// The directory `accept-edits` lets Loop edit: the git root containing
    /// `cwd`, or `cwd` itself, found through the env. An env error counts
    /// as no `.git` there.
    pub async fn workspace_root(&self, cwd: &Path) -> PathBuf {
        for dir in cwd.ancestors() {
            if self.env.exists(&dir.join(".git")).await.unwrap_or(false) {
                return dir.to_path_buf();
            }
        }
        cwd.to_path_buf()
    }

    async fn fetch(&self, query: &Query) -> Answer {
        match query {
            Query::Canonical(path) => {
                Answer::Path(self.got(self.env.canonical_path(path).await, false))
            }
            Query::ReadLink(path) => Answer::Path(self.got(self.env.read_link(path).await, true)),
            Query::Lstat(path) => Answer::Meta(
                self.got(self.env.symlink_metadata(path).await, false)
                    .map(Meta::from),
            ),
        }
    }

    /// An env result as an answer. A host-namespace env answers as the host
    /// disk did: any error but a driver failure is `No`. A guest env's
    /// error is `No` only when it says the path is not there (or, for a
    /// link target, not a link); anything else is unknown.
    fn got<T>(&self, result: Result<T, FileError>, link: bool) -> Got<T> {
        let code = match result {
            Ok(value) => return Got::Yes(value),
            Err(error) => error.code,
        };
        let driver_failure = matches!(
            code,
            FileErrorCode::Unavailable
                | FileErrorCode::Gone
                | FileErrorCode::OutcomeUnknown
                | FileErrorCode::Unsupported
        );
        let absent = matches!(code, FileErrorCode::NotFound | FileErrorCode::NotADirectory)
            || (link && code == FileErrorCode::InvalidPath);
        match self.namespace {
            PathNamespace::Host if !driver_failure => Got::No,
            _ if absent => Got::No,
            _ => Got::Unknown,
        }
    }
}

impl<T> Got<T> {
    fn map<U>(self, f: impl FnOnce(T) -> U) -> Got<U> {
        match self {
            Got::Yes(value) => Got::Yes(f(value)),
            Got::No => Got::No,
            Got::Unknown => Got::Unknown,
        }
    }
}

#[cfg(all(test, unix))]
pub(crate) mod tests {
    use super::*;
    use crate::permissions::{Call, PermissionMode, Policy, ToolPermission};
    use std::collections::BTreeMap;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// Host facts with a count of the host calls.
    #[derive(Default)]
    pub(crate) struct Counted(AtomicUsize);

    impl Lookup for Counted {
        fn canonical(&self, path: &Path) -> Got<PathBuf> {
            self.0.fetch_add(1, Ordering::Relaxed);
            host_canonical(path)
        }
        fn read_link(&self, path: &Path) -> Got<PathBuf> {
            self.0.fetch_add(1, Ordering::Relaxed);
            host_read_link(path)
        }
        fn lstat(&self, path: &Path) -> Got<Meta> {
            self.0.fetch_add(1, Ordering::Relaxed);
            host_lstat(path)
        }
    }

    fn rules(extra: &[(&str, &str)]) -> BTreeMap<String, ToolPermission> {
        let settings = extra
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect();
        crate::tool_approval::permissions_from_settings(&settings)
    }

    /// A decision on `call` in `mode`, with facts from `facts`.
    fn decide(
        facts: &PathFacts,
        rules: &BTreeMap<String, ToolPermission>,
        mode: PermissionMode,
        tool: &str,
        path: &str,
        ws: &Path,
    ) -> crate::permissions::Verdict {
        let kind = loop_agent::protocol_events::tool_kind(tool);
        let args = serde_json::json!({ "path": path });
        let call = Call::resolve_with(facts, tool, kind, &args, ws, ws);
        Policy::default().decide_why(mode, rules, &call)
    }

    /// Host calls before (the sync code on the host disk, as the gate
    /// was) and env calls after, per decision, for the common cases; and
    /// the time per decision. Facts are kept for one decision only.
    #[tokio::test]
    async fn env_calls_per_decision_stay_bounded() {
        let dir = tempfile::tempdir().unwrap();
        let ws = dir.path().canonicalize().unwrap();
        std::fs::create_dir_all(ws.join(".git")).unwrap();
        std::fs::create_dir_all(ws.join("src")).unwrap();
        std::fs::write(ws.join("src/a.rs"), "a").unwrap();
        let none = rules(&[]);
        let five = rules(&[
            ("read(secrets/**)", "deny"),
            ("read(~/.ssh/**)", "deny"),
            ("edit(*.lock)", "ask"),
            ("write(src/**)", "allow"),
            ("grep(**/.env)", "deny"),
        ]);
        let env = GatePaths::new(
            Arc::new(loop_agent::harness::HostExecutionEnv::new(&ws)),
            PathNamespace::Host,
        );
        let cases = [
            (
                "read a file, no rules",
                &none,
                PermissionMode::Ask,
                "read",
                "src/a.rs",
            ),
            (
                "read a file, 5 path rules",
                &five,
                PermissionMode::Ask,
                "read",
                "src/a.rs",
            ),
            (
                "write a new file, no rules",
                &none,
                PermissionMode::Ask,
                "write",
                "src/new.rs",
            ),
            (
                "write a new file, 5 path rules",
                &five,
                PermissionMode::Ask,
                "write",
                "src/new.rs",
            ),
            (
                "write a new file, accept-edits",
                &none,
                PermissionMode::AcceptEdits,
                "write",
                "src/new.rs",
            ),
        ];
        const RUNS: u32 = 300;
        for (name, rules, mode, tool, path) in cases {
            let counted = Arc::new(Counted::default());
            let facts = PathFacts(Source::Other(counted.clone()));
            let before = decide(&facts, rules, mode, tool, path, &ws);
            let host_calls = counted.0.load(Ordering::Relaxed);
            let (after, stats) = env
                .run_counted(|facts| decide(facts, rules, mode, tool, path, &ws))
                .await;
            assert_eq!(before, after, "{name}");
            assert!(
                stats.env_calls <= host_calls,
                "{name}: {stats:?} > {host_calls}"
            );
            assert!(stats.env_calls <= 8, "{name}: {stats:?}");

            let started = std::time::Instant::now();
            for _ in 0..RUNS {
                decide(&PathFacts::runtime_host(), rules, mode, tool, path, &ws);
            }
            let sync = started.elapsed() / RUNS;
            let started = std::time::Instant::now();
            for _ in 0..RUNS {
                assert!(env
                    .run(|facts| decide(facts, rules, mode, tool, path, &ws))
                    .await
                    .is_ok());
            }
            let through_env = started.elapsed() / RUNS;
            eprintln!(
                "{name}: host calls {host_calls} (sync, {sync:?}/decision); env calls {} in {} \
                 rounds ({through_env:?}/decision)",
                stats.env_calls, stats.rounds
            );
        }
    }

    /// No path in the call, no fact: the env is not called.
    #[tokio::test]
    async fn a_call_without_a_path_needs_no_env_call() {
        let env = GatePaths::new(
            Arc::new(loop_agent::harness::HostExecutionEnv::new("/")),
            PathNamespace::Host,
        );
        let none = rules(&[]);
        let (_, stats) = env
            .run_counted(|facts| {
                let call = Call::resolve_with(
                    facts,
                    "bash",
                    loop_protocol::ToolKind::Execute,
                    &serde_json::json!({ "command": "ls" }),
                    Path::new("/ws"),
                    Path::new("/ws"),
                );
                Policy::default().decide_why(PermissionMode::Ask, &none, &call)
            })
            .await;
        assert_eq!(
            stats,
            LookupStats {
                env_calls: 0,
                rounds: 1,
                limited: false,
            }
        );
    }
}
