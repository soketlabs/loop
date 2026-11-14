//! Access to Sessions by owner and tenant (task I3).
//!
//! The matrix: every Session method of `loop_runtime::access::METHODS` x the
//! caller kinds x the Session kinds, each call either allowed or refused as
//! "not found". A refusal is counted by the Runtime's access check, so the
//! test sees the decision, not what the method did after it.

use std::collections::{BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use loop_app_core::runtime::{BootstrapOpts, GlobalResources};
use loop_protocol::rpc::{methods, PeerInfo};
use loop_protocol::{
    AgentBackend, BackendError, BackendResult, Caller, ContentBlock, Principal, PrincipalKind,
    SessionKey, SessionRef, SessionSpec, Tenant, TurnOutcome,
};
use loop_runtime::access::{Need, Rule, METHODS};
use loop_runtime::log::{EventLog, MemoryEventLog};
use loop_runtime::{Runtime, RuntimeLimits};
use loop_wire::server::{serve_with_caller, serve_with_control, ServeOptions};
use loop_wire::transport::memory;
use loop_wire::{Authority, Connection, Incoming, RuntimeAdmin, RuntimeControl, Transport};
use serde_json::{json, Value};

fn opts(cwd: &Path) -> BootstrapOpts {
    BootstrapOpts {
        cwd: cwd.into(),
        provider: Some("faux".into()),
        model: Some("faux-model".into()),
        theme: None,
        system_prompt: None,
        append_system_prompt: None,
        no_context_files: true,
        interactive: false,
        session_id: None,
        no_web: true,
        permission_mode: Some("full-access".into()),
        agent: None,
    }
}

/// A Runtime whose model answers text, or calls the `subagent` tool once
/// when a prompt says `SPAWN`. `lock` puts a chat tool lock on `dir/ws`.
async fn runtime(dir: &Path, log: Arc<dyn EventLog>, lock: bool) -> Arc<Runtime> {
    use loop_ai::providers::faux::{FauxResponder, FauxResponse};
    let models = Arc::new(loop_ai::Models::create(Default::default()));
    let responder: FauxResponder = Arc::new(move |context: &loop_ai::Context| {
        let text = format!("{:?}", context.messages);
        let answered = context
            .messages
            .iter()
            .any(|m| matches!(m, loop_ai::Message::Assistant(_)));
        Box::pin(async move {
            if text.contains("SPAWN") && !answered {
                FauxResponse::ToolCalls(vec![loop_ai::ToolCall {
                    id: "call-spawn".into(),
                    name: "subagent".into(),
                    // `SPAWN-BOB` asks for a child in the sibling folder `bob`.
                    arguments: if text.contains("SPAWN-BOB") {
                        json!({"task": "child work", "cwd": "../bob"})
                    } else {
                        json!({"task": "child work"})
                    },
                    thought_signature: None,
                }])
            } else {
                FauxResponse::Text("done".into())
            }
        })
    });
    models.set_provider(loop_ai::providers::faux_provider_fn(responder));
    let ws = dir.join("ws");
    std::fs::create_dir_all(&ws).unwrap();
    let globals = GlobalResources::with_models(dir.join("agent"), models)
        .unwrap()
        .with_tool_lock(lock.then(|| {
            loop_app_core::permissions::tool_lock::RuntimeToolLock::new(
                loop_protocol::ToolLock::Chat,
                &ws,
            )
            .unwrap()
        }));
    let store = globals.open_runtime_session_store().unwrap();
    Runtime::new(
        globals,
        opts(&ws),
        log,
        store,
        RuntimeLimits {
            max_sessions: 64,
            max_active_turns: 8,
        },
    )
    .await
    .unwrap()
    .into_shared()
}

fn host() -> Caller {
    Caller::host(loop_wire::local_principal())
}
fn token() -> Caller {
    Caller::local_token(loop_wire::local_principal())
}
fn edge(id: &str, tenant: &str) -> Caller {
    let principal = Principal::new(PrincipalKind::Person, "https://idp.example", id).unwrap();
    Caller::asserted(principal, Tenant::new(tenant).unwrap()).unwrap()
}

fn folder(dir: &Path, name: &str) -> PathBuf {
    let path = dir.join("ws").join(name);
    std::fs::create_dir_all(&path).unwrap();
    path.canonicalize().unwrap()
}

fn spec(cwd: &Path) -> SessionSpec {
    SessionSpec {
        cwd: Some(cwd.to_path_buf()),
        ..SessionSpec::default()
    }
}

async fn run_turn(rt: &Runtime, session: &SessionRef, text: &str) {
    let turn = rt
        .prompt(session, vec![ContentBlock::text(text)])
        .await
        .unwrap();
    assert!(
        matches!(turn.outcome().await, TurnOutcome::Completed(_)),
        "turn did not complete"
    );
}

/// The Subagent `parent` opened, from the Session list.
async fn child_of(rt: &Runtime, parent: &SessionRef) -> SessionRef {
    rt.list()
        .await
        .unwrap()
        .into_iter()
        .find(|info| info.parent.as_ref().map(|p| &p.key) == Some(&parent.key))
        .map(|info| info.r#ref)
        .expect("a Subagent")
}

/// The callers of the matrix.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Who {
    /// The host's Unix socket.
    Host,
    /// The host's WebSocket with the Runtime token.
    Token,
    /// An outside user who owns `A` (tenant acme).
    Alice,
    /// An outside user of the same tenant who owns `B`.
    Bob,
    /// The id `alice` in another tenant.
    OtherTenant,
}
impl Who {
    const ALL: [Who; 5] = [
        Who::Host,
        Who::Token,
        Who::Alice,
        Who::Bob,
        Who::OtherTenant,
    ];
    fn caller(self) -> Caller {
        match self {
            Who::Host => host(),
            Who::Token => token(),
            Who::Alice => edge("alice", "acme"),
            Who::Bob => edge("bob", "acme"),
            Who::OtherTenant => edge("alice", "zeta"),
        }
    }
}

/// The Session kinds: local-owned, owned by Alice, owned by Bob, no owner
/// of record, and the Subagent of each.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
enum Kind {
    Local,
    LocalChild,
    Alice,
    AliceChild,
    Bob,
    BobChild,
    Unowned,
    UnownedChild,
}
impl Kind {
    const ALL: [Kind; 8] = [
        Kind::Local,
        Kind::LocalChild,
        Kind::Alice,
        Kind::AliceChild,
        Kind::Bob,
        Kind::BobChild,
        Kind::Unowned,
        Kind::UnownedChild,
    ];
    fn local(self) -> bool {
        matches!(
            self,
            Kind::Local | Kind::LocalChild | Kind::Unowned | Kind::UnownedChild
        )
    }
}

/// The rules, written out by hand (not with `access::decide`).
fn expected(who: Who, kind: Kind, need: Need) -> bool {
    match who {
        Who::Host | Who::Token => kind.local() || matches!(need, Need::List | Need::Close),
        Who::Alice => matches!(kind, Kind::Alice | Kind::AliceChild),
        Who::Bob => matches!(kind, Kind::Bob | Kind::BobChild),
        Who::OtherTenant => false,
    }
}

struct World {
    rt: Arc<Runtime>,
    sessions: HashMap<Kind, SessionRef>,
    folders: HashMap<&'static str, PathBuf>,
    _dir: tempfile::TempDir,
}

/// Every Session kind on one Runtime. The Sessions with no owner of record
/// are made by a first Runtime with an Event Log of its own, so the shared
/// Event Log has no record of them (as for a Session made before identity
/// was stored).
async fn world(lock: bool) -> World {
    let dir = tempfile::tempdir().unwrap();
    let mut folders = HashMap::new();
    for name in ["host", "alice", "bob", "none"] {
        folders.insert(name, folder(dir.path(), name));
    }
    let mut sessions = HashMap::new();
    {
        let first = runtime(dir.path(), Arc::new(MemoryEventLog::new()), false).await;
        let unowned = first.new_session(spec(&folders["none"])).await.unwrap();
        run_turn(&first, &unowned, "SPAWN").await;
        sessions.insert(Kind::UnownedChild, child_of(&first, &unowned).await);
        sessions.insert(Kind::Unowned, unowned);
        first.shutdown().await;
    }
    // Subagents run only without a tool lock: make them first.
    let log: Arc<dyn EventLog> = Arc::new(MemoryEventLog::new());
    let maker = runtime(dir.path(), log.clone(), false).await;
    for (who, root, child, name) in [
        (host(), Kind::Local, Kind::LocalChild, "host"),
        (
            edge("alice", "acme"),
            Kind::Alice,
            Kind::AliceChild,
            "alice",
        ),
        (edge("bob", "acme"), Kind::Bob, Kind::BobChild, "bob"),
    ] {
        let made = who
            .clone()
            .scope(maker.new_session(spec(&folders[name])))
            .await
            .unwrap();
        who.clone().scope(run_turn(&maker, &made, "SPAWN")).await;
        sessions.insert(child, child_of(&maker, &made).await);
        sessions.insert(root, made);
    }
    maker.shutdown().await;
    let rt = runtime(dir.path(), log, lock).await;
    // The roots loaded again by their owners, as a Surface would.
    for (who, kind) in [
        (host(), Kind::Local),
        (edge("alice", "acme"), Kind::Alice),
        (edge("bob", "acme"), Kind::Bob),
    ] {
        who.scope(rt.load_session(&sessions[&kind], SessionSpec::default()))
            .await
            .unwrap();
    }
    World {
        rt,
        sessions,
        folders,
        _dir: dir,
    }
}

fn ok<T>(_: T) {}

/// Call `method` on `session` for the caller in scope. `write` picks the
/// changing form of a method that reads or writes.
async fn call(rt: &Arc<Runtime>, method: &str, s: &SessionRef, write: bool) -> BackendResult<()> {
    use loop_protocol::rpc::*;
    let r = rt.as_ref();
    let session = s.clone();
    match method {
        methods::SESSION_LOAD => r.load_session(s, SessionSpec::default()).await.map(ok),
        methods::SESSION_CLOSE => RuntimeControl::close(r, s, false).await,
        methods::SESSION_ATTACH => RuntimeControl::attach_watching(r, 9_001, s, 0)
            .await
            .map(|_| ()),
        methods::SESSION_TRANSCRIPT => r.transcript(s).await.map(ok),
        methods::SESSION_PROMPT => r
            .prompt(s, vec![ContentBlock::text("hi")])
            .await
            .map(|_| ()),
        methods::SESSION_CONFIG => RuntimeControl::config(r, SessionParams { session })
            .await
            .map(ok),
        methods::SESSION_SKILL => RuntimeControl::skill(
            r,
            loop_protocol::session_services::SkillParams {
                session,
                name: "nope".into(),
                active: true,
                expected_revision: None,
            },
        )
        .await
        .map(ok),
        methods::SESSION_WORKFLOW => RuntimeControl::workflow(
            r,
            loop_protocol::session_services::WorkflowParams {
                session,
                goal: String::new(),
                concurrency: None,
                request_id: String::new(),
            },
        )
        .await
        .map(ok),
        methods::SESSION_TREE => RuntimeControl::tree(
            r,
            TreeParams {
                session,
                cursor: None,
                limit: None,
            },
        )
        .await
        .map(ok),
        methods::SESSION_VIEW => RuntimeControl::view(
            r,
            ViewParams {
                session,
                view_id: "none".into(),
                cursor: None,
                limit: None,
            },
        )
        .await
        .map(ok),
        methods::SESSION_NAVIGATE => RuntimeControl::navigate(
            r,
            NavigateParams {
                session,
                target_entry_id: Some("none".into()),
                summarize: false,
                expected_revision: Some(u64::MAX),
            },
        )
        .await
        .map(ok),
        methods::SESSION_RENAME => RuntimeControl::rename(
            r,
            RenameParams {
                session,
                name: "renamed".into(),
                expected_revision: Some(u64::MAX),
            },
        )
        .await
        .map(ok),
        methods::SESSION_FORK => RuntimeControl::fork(
            r,
            ForkParams {
                session,
                before_entry_id: "none".into(),
                name: None,
                request_id: format!("fork-{}", loop_ai::new_id()),
            },
        )
        .await
        .map(ok),
        methods::SESSION_CLONE => RuntimeControl::clone_session(
            r,
            CloneParams {
                session,
                name: None,
                request_id: format!("clone-{}", loop_ai::new_id()),
            },
        )
        .await
        .map(ok),
        methods::SESSION_EXPORT => RuntimeControl::export(r, SessionParams { session })
            .await
            .map(ok),
        methods::SESSION_MODELS => loop_protocol::ModelControl::models(r, s).await.map(ok),
        methods::SESSION_EFFORTS => loop_protocol::ModelControl::efforts(r, s).await.map(ok),
        methods::SESSION_MODES => loop_protocol::ModelControl::modes(r, s).await.map(ok),
        methods::SESSION_SET_MODEL => {
            loop_protocol::ModelControl::set_model(r, s, "faux/none", Some(u64::MAX))
                .await
                .map(ok)
        }
        methods::SESSION_SET_EFFORT => {
            loop_protocol::ModelControl::set_effort(r, s, "none", Some(u64::MAX))
                .await
                .map(ok)
        }
        methods::SESSION_SET_MODE => {
            loop_protocol::ModelControl::set_mode(r, s, "none", Some(u64::MAX))
                .await
                .map(ok)
        }
        methods::SESSION_TTFB => {
            let change = write.then_some(Some(1_000));
            loop_protocol::ModelControl::ttfb(r, s, change, Some(u64::MAX))
                .await
                .map(ok)
        }
        methods::SESSION_COMPACT => loop_protocol::Compaction::compact(r, s, None).await.map(ok),
        methods::SESSION_STATS => loop_protocol::SessionStats::stats(r, s).await.map(ok),
        methods::SESSION_SHELL => loop_protocol::LocalShell::run(r, s, "true", Some(1_000))
            .await
            .map(ok),
        methods::SESSION_SANDBOX => RuntimeAdmin::sandbox(
            r,
            loop_protocol::admin::SandboxParams {
                session,
                action: if write {
                    loop_protocol::admin::SandboxAction::Set
                } else {
                    loop_protocol::admin::SandboxAction::Status
                },
                config: None,
                expected_revision: Some(u64::MAX),
            },
        )
        .await
        .map(ok),
        methods::SESSION_PERMISSION_MODE => RuntimeAdmin::permission_mode(
            r,
            loop_protocol::admin::PermissionModeParams {
                session,
                mode: write.then(|| "nonsense".into()),
                expected_revision: Some(u64::MAX),
            },
        )
        .await
        .map(ok),
        other => panic!("the matrix has no call for {other}: add one"),
    }
}

/// Every Session method of the access table, with the right it needs; a
/// method that reads or writes appears twice.
fn session_methods() -> Vec<(&'static str, Need, bool)> {
    let mut list = Vec::new();
    for (method, rule) in METHODS {
        // Without the `orchestration` feature there are no workflows.
        if *method == methods::SESSION_WORKFLOW && !cfg!(feature = "orchestration") {
            continue;
        }
        match rule {
            Rule::Session(need) if *method != methods::SESSION_CANCEL => {
                list.push((*method, *need, false))
            }
            Rule::ReadOrWrite => {
                list.push((*method, Need::Read, false));
                list.push((*method, Need::Write, true));
            }
            Rule::Copy => list.push((*method, Need::Read, false)),
            _ => {}
        }
    }
    // Destructive last, so the other calls find loaded Sessions.
    list.sort_by_key(|(method, _, _)| *method == methods::SESSION_CLOSE);
    list
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn every_session_method_follows_the_access_rules() {
    tokio::time::timeout(Duration::from_secs(300), async {
        let world = world(true).await;
        let rt = &world.rt;
        let mut checked = 0;
        let mut wrong = Vec::new();
        // `session/cancel` first, while the roots are loaded: a refused
        // cancel is counted, an allowed one cancels nothing (idle).
        for who in Who::ALL {
            for kind in [Kind::Local, Kind::Alice, Kind::Bob] {
                let before = rt.access_refusals();
                let session = world.sessions[&kind].clone();
                let rt2 = rt.clone();
                who.caller()
                    .scope(async move { rt2.cancel(&session) })
                    .await;
                let refused = rt.access_refusals() > before;
                checked += 1;
                if refused == expected(who, kind, Need::Write) {
                    wrong.push(format!("{who:?} session/cancel {kind:?}: refused={refused}"));
                }
            }
        }
        for (method, need, write) in session_methods() {
            for kind in Kind::ALL {
                for who in Who::ALL {
                    let session = &world.sessions[&kind];
                    let before = rt.access_refusals();
                    let result = tokio::time::timeout(
                        Duration::from_secs(30),
                        who.caller().scope(call(rt, method, session, write)),
                    )
                    .await
                    .unwrap_or_else(|_| panic!("{who:?} {method} {kind:?} hung"));
                    let refused = rt.access_refusals() - before;
                    checked += 1;
                    let allow = expected(who, kind, need);
                    let ok = if allow {
                        refused == 0
                    } else {
                        refused == 1
                            && matches!(&result, Err(BackendError::SessionNotFound(k)) if k == &session.key)
                    };
                    if !ok {
                        wrong.push(format!(
                            "{who:?} {method} ({need:?}) on {kind:?}: expected {}, refusals {refused}, result {result:?}",
                            if allow { "allowed" } else { "not found" }
                        ));
                    }
                }
            }
        }
        assert!(wrong.is_empty(), "{} of {checked} wrong:\n{}", wrong.len(), wrong.join("\n"));
        assert!(checked > 1000, "only {checked} checks");
        rt.shutdown().await;
    })
    .await
    .unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn lists_show_only_what_the_caller_may_list() {
    tokio::time::timeout(Duration::from_secs(120), async {
        let world = world(true).await;
        let rt = &world.rt;
        let key = |kind: Kind| world.sessions[&kind].key.clone();
        for who in Who::ALL {
            let listed: BTreeSet<SessionKey> = who
                .caller()
                .scope(rt.list())
                .await
                .unwrap()
                .into_iter()
                .map(|info| info.r#ref.key)
                .collect();
            let want: BTreeSet<SessionKey> = Kind::ALL
                .into_iter()
                .filter(|kind| expected(who, *kind, Need::List))
                .map(key)
                .collect();
            assert_eq!(listed, want, "session/list for {who:?}");
            // What a row may show without `Read`: no title, name or model.
            for info in who.caller().scope(rt.list()).await.unwrap() {
                let kind = Kind::ALL
                    .into_iter()
                    .find(|kind| key(*kind) == info.r#ref.key)
                    .unwrap();
                if !expected(who, kind, Need::Read) {
                    assert_eq!(info.summary.first_prompt, None, "{who:?} {kind:?}");
                    assert_eq!(info.summary.selected_model, None, "{who:?} {kind:?}");
                    assert_eq!(info.name, None, "{who:?} {kind:?}");
                } else if matches!(kind, Kind::Local | Kind::Alice | Kind::Bob) {
                    assert!(info.summary.first_prompt.is_some(), "{who:?} {kind:?}");
                }
            }
            // `runtime/sessions`: the loaded ones (the three roots), and
            // totals of only the rows shown.
            let loaded = who
                .caller()
                .scope(RuntimeAdmin::sessions(rt.as_ref()))
                .await
                .unwrap();
            let rows: BTreeSet<SessionKey> = loaded
                .sessions
                .iter()
                .map(|r| r.session.key.clone())
                .collect();
            let want: BTreeSet<SessionKey> = [Kind::Local, Kind::Alice, Kind::Bob]
                .into_iter()
                .filter(|kind| expected(who, *kind, Need::List))
                .map(key)
                .collect();
            assert_eq!(rows, want, "runtime/sessions for {who:?}");
            assert_eq!(loaded.totals.sessions as usize, want.len());
        }
        rt.shutdown().await;
    })
    .await
    .unwrap();
}

/// A copy belongs to the caller that made it, in its tenant, after a read
/// check on the source; an import lands only in a folder the importer may
/// read.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_copy_belongs_to_its_copier() {
    use loop_protocol::rpc::{CloneParams, ImportParams, SessionParams};
    tokio::time::timeout(Duration::from_secs(120), async {
        let world = world(true).await;
        let rt = &world.rt;
        let alice = edge("alice", "acme");
        let a = world.sessions[&Kind::Alice].clone();
        let copy = alice
            .clone()
            .scope(RuntimeControl::clone_session(
                rt.as_ref(),
                CloneParams {
                    session: a.clone(),
                    name: None,
                    request_id: "alice-clone".into(),
                },
            ))
            .await
            .unwrap();
        let owner = rt.session_owner(&copy.session.key).await.unwrap().unwrap();
        assert!(owner.principal.same_principal(alice.principal()));
        assert_eq!(owner.tenant, Tenant::new("acme").unwrap());
        // A fork of hers, before her first message: hers too.
        let tree = alice
            .clone()
            .scope(RuntimeControl::tree(
                rt.as_ref(),
                loop_protocol::rpc::TreeParams {
                    session: a.clone(),
                    cursor: None,
                    limit: None,
                },
            ))
            .await
            .unwrap();
        let fork = alice
            .clone()
            .scope(RuntimeControl::fork(
                rt.as_ref(),
                loop_protocol::rpc::ForkParams {
                    session: a.clone(),
                    before_entry_id: tree.fork_points[0].entry_id.clone(),
                    name: None,
                    request_id: "alice-fork".into(),
                },
            ))
            .await
            .unwrap();
        let owner = rt.session_owner(&fork.session.key).await.unwrap().unwrap();
        assert!(owner.principal.same_principal(alice.principal()));
        assert_eq!(
            rt.session_tenant(&fork.session.key),
            Tenant::new("acme").unwrap()
        );
        // Bob cannot fork it, and its tree names no Session he could reach.
        let refused = edge("bob", "acme")
            .scope(RuntimeControl::fork(
                rt.as_ref(),
                loop_protocol::rpc::ForkParams {
                    session: a.clone(),
                    before_entry_id: tree.fork_points[0].entry_id.clone(),
                    name: None,
                    request_id: "bob-fork".into(),
                },
            ))
            .await;
        assert!(matches!(refused, Err(BackendError::SessionNotFound(_))));
        assert_eq!(
            rt.session_tenant(&copy.session.key),
            Tenant::new("acme").unwrap()
        );
        // The same request from Bob: the source is not found for him, and a
        // replay of Alice's receipt never names her copy to him.
        let replay = edge("bob", "acme")
            .scope(RuntimeControl::clone_session(
                rt.as_ref(),
                CloneParams {
                    session: a.clone(),
                    name: None,
                    request_id: "alice-clone".into(),
                },
            ))
            .await;
        assert!(
            matches!(replay, Err(BackendError::SessionNotFound(_))),
            "{replay:?}"
        );
        // The host's user cannot copy another tenant's Session.
        let host_copy = host()
            .scope(RuntimeControl::clone_session(
                rt.as_ref(),
                CloneParams {
                    session: a.clone(),
                    name: None,
                    request_id: "host-clone".into(),
                },
            ))
            .await;
        assert!(matches!(host_copy, Err(BackendError::SessionNotFound(_))));
        // Its own Session it copies into the local tenant.
        let local = world.sessions[&Kind::Local].clone();
        let mine = host()
            .scope(RuntimeControl::clone_session(
                rt.as_ref(),
                CloneParams {
                    session: local,
                    name: None,
                    request_id: "host-clone-2".into(),
                },
            ))
            .await
            .unwrap();
        assert_eq!(rt.session_tenant(&mine.session.key), Tenant::local());

        // Import: Alice into her own folder; Bob cannot name it.
        let exported = alice
            .clone()
            .scope(RuntimeControl::export(
                rt.as_ref(),
                SessionParams { session: a.clone() },
            ))
            .await
            .unwrap();
        let workspace_id = loop_runtime::workspace_id(&world.folders["alice"]);
        let import = |request_id: &str| ImportParams {
            workspace_id: workspace_id.clone(),
            jsonl: exported.jsonl.clone(),
            request_id: request_id.into(),
        };
        let imported = alice
            .clone()
            .scope(RuntimeControl::import(rt.as_ref(), import("alice-import")))
            .await
            .unwrap();
        let owner = rt
            .session_owner(&imported.session.key)
            .await
            .unwrap()
            .unwrap();
        assert!(owner.principal.same_principal(alice.principal()));
        let refused = edge("bob", "acme")
            .scope(RuntimeControl::import(rt.as_ref(), import("bob-import")))
            .await
            .unwrap_err();
        assert!(
            refused.to_string().contains("unknown workspace"),
            "{refused}"
        );
        rt.shutdown().await;
    })
    .await
    .unwrap();
}

/// The folder rule: an outside user never shares a folder
/// (or one inside or above it) with a Session it may not read.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_outside_user_never_opens_another_user_s_folder() {
    tokio::time::timeout(Duration::from_secs(120), async {
        let world = world(true).await;
        let rt = &world.rt;
        let bob = edge("bob", "acme");
        let alice_folder = world.folders["alice"].clone();
        let inside = alice_folder.join("deeper");
        std::fs::create_dir_all(&inside).unwrap();
        let root = alice_folder.parent().unwrap().to_path_buf();
        for cwd in [
            alice_folder.clone(),
            inside,
            root,
            world.folders["host"].clone(),
        ] {
            let refused = bob
                .clone()
                .scope(rt.new_session(spec(&cwd)))
                .await
                .unwrap_err();
            assert!(
                matches!(refused, BackendError::Forbidden(_)),
                "{}: {refused:?}",
                cwd.display()
            );
        }
        // Its own folder, and a new one, are fine.
        let own = bob
            .clone()
            .scope(rt.new_session(spec(&world.folders["bob"])))
            .await;
        assert!(own.is_ok(), "{own:?}");
        let fresh = folder(world._dir.path(), "bob-2");
        assert!(bob
            .clone()
            .scope(rt.new_session(spec(&fresh)))
            .await
            .is_ok());
        // The host's user is not limited.
        assert!(host()
            .scope(rt.new_session(spec(&alice_folder)))
            .await
            .is_ok());
        rt.shutdown().await;
    })
    .await
    .unwrap();
}

// ---- Over the wire -------------------------------------------------------

fn info() -> PeerInfo {
    PeerInfo {
        name: "test".into(),
        version: "test".into(),
    }
}

/// A raw client that keeps every notification it gets.
struct Client {
    conn: Arc<Connection>,
    seen: Arc<std::sync::Mutex<Vec<(String, Value)>>>,
}

fn client(transport: Transport) -> Client {
    let (conn, mut incoming) = Connection::new(transport);
    let resolver = Arc::clone(&conn);
    let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
    let sink = seen.clone();
    tokio::spawn(async move {
        while let Some(message) = incoming.recv().await {
            match message {
                Incoming::Response { id, result } => resolver.resolve(id, result).await,
                Incoming::Notification { method, params } => {
                    sink.lock().unwrap().push((method, params))
                }
                Incoming::Request { method, params, .. } => {
                    sink.lock().unwrap().push((method, params))
                }
            }
        }
    });
    Client { conn, seen }
}

async fn connect(rt: &Arc<Runtime>, caller: Option<Caller>) -> Client {
    let (server, client_end) = memory();
    let control: Arc<dyn RuntimeControl> = rt.clone();
    let options = |authority| ServeOptions {
        owns_backend: false,
        authority,
    };
    match caller {
        Some(caller) if caller.via().is_none() => {
            let _ = serve_with_control(
                rt.clone(),
                control,
                server,
                info(),
                options(Authority::HostLocal),
            );
        }
        Some(caller) => {
            let _ = serve_with_caller(
                rt.clone(),
                Some(control),
                server,
                info(),
                options(Authority::Remote),
                caller,
            );
        }
        None => {
            let _ = serve_with_control(
                rt.clone(),
                control,
                server,
                info(),
                options(Authority::Remote),
            );
        }
    }
    let client = client(client_end);
    client
        .conn
        .request(
            methods::INITIALIZE,
            json!({"protocol_version": 1, "client_info": {"name": "t", "version": "0"},
                   "client_capabilities": {"interact": true}}),
        )
        .await
        .unwrap();
    client
}

/// A remote connection with no caller is served nothing but `initialize`,
/// for every method of the table.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_remote_connection_with_no_caller_is_refused_everything() {
    tokio::time::timeout(Duration::from_secs(120), async {
        let world = world(true).await;
        let rt = &world.rt;
        let anon = connect(rt, None).await;
        let session = serde_json::to_value(&world.sessions[&Kind::Local]).unwrap();
        let before = rt.access_refusals();
        for (method, rule) in METHODS {
            if *method == methods::INITIALIZE || matches!(rule, Rule::ToClient(_)) {
                continue;
            }
            if *method == methods::SESSION_CANCEL {
                anon.conn
                    .notify(method, json!({"session": session}))
                    .unwrap();
                continue;
            }
            let error = anon
                .conn
                .request(method, json!({"session": session}))
                .await
                .unwrap_err();
            let text = format!("{error:?}");
            assert!(text.contains("no identity"), "{method}: {text}");
        }
        // Nothing reached the Runtime's check, and no update was sent.
        assert_eq!(rt.access_refusals(), before);
        assert!(anon
            .seen
            .lock()
            .unwrap()
            .iter()
            .all(|(m, _)| m != methods::SESSION_UPDATE));
        rt.shutdown().await;
    })
    .await
    .unwrap();
}

/// Two outside users on one Runtime, each attached to its own Session:
/// neither gets the other's updates or sees the other's Sessions.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn two_outside_users_never_see_each_other() {
    tokio::time::timeout(Duration::from_secs(120), async {
        let dir = tempfile::tempdir().unwrap();
        let rt = runtime(dir.path(), Arc::new(MemoryEventLog::new()), true).await;
        let alice = connect(&rt, Some(edge("alice", "acme"))).await;
        let bob = connect(&rt, Some(edge("bob", "acme"))).await;
        let mut mine = Vec::new();
        for (client, name) in [(&alice, "alice"), (&bob, "bob")] {
            let cwd = folder(dir.path(), name);
            let made = client
                .conn
                .request(methods::SESSION_NEW, json!({"cwd": cwd}))
                .await
                .unwrap();
            mine.push(made);
        }
        for (client, session) in [(&alice, &mine[0]), (&bob, &mine[1])] {
            client
                .conn
                .request(
                    methods::SESSION_ATTACH,
                    json!({"session": session, "after_seq": 0}),
                )
                .await
                .unwrap();
            client
                .conn
                .request(
                    methods::SESSION_PROMPT,
                    json!({"session": session, "content": [{"type": "text", "text": "hello"}]}),
                )
                .await
                .unwrap();
        }
        // Bob tries Alice's Session: not found, and still nothing of hers.
        let attach = bob
            .conn
            .request(
                methods::SESSION_ATTACH,
                json!({"session": mine[0], "after_seq": 0}),
            )
            .await
            .unwrap_err();
        assert!(format!("{attach:?}").contains("not found"), "{attach:?}");
        for (client, own, other) in [(&alice, &mine[0], &mine[1]), (&bob, &mine[1], &mine[0])] {
            let seen = client.seen.lock().unwrap().clone();
            let updates: Vec<_> = seen
                .iter()
                .filter(|(m, _)| m == methods::SESSION_UPDATE)
                .collect();
            assert!(!updates.is_empty());
            assert!(
                updates.iter().all(|(_, u)| u["session"] == own["key"]),
                "{updates:?}"
            );
            assert!(!format!("{seen:?}").contains(other["key"].as_str().unwrap()));
            let listed = client
                .conn
                .request(methods::SESSION_LIST, json!({}))
                .await
                .unwrap();
            let keys: Vec<_> = listed
                .as_array()
                .unwrap()
                .iter()
                .map(|i| i["ref"]["key"].clone())
                .collect();
            assert_eq!(keys, vec![own["key"].clone()]);
            let status = client
                .conn
                .request(methods::RUNTIME_STATUS, json!({}))
                .await
                .unwrap();
            assert_eq!(status["loaded"], 1, "{status}");
            assert_eq!(status["pid"], 0);
        }
        rt.shutdown().await;
    })
    .await
    .unwrap();
}

/// What the check costs per request, by caller kind: a cheap read
/// (`session/permission_mode`) timed in process (internal authority), as
/// the host's user, and as the outside owner. Ignored: run with
/// `--ignored --nocapture`; prints one `ACCESS|COST|` line.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "timing: run explicitly"]
async fn access_check_cost() {
    let world = world(true).await;
    let rt = &world.rt;
    const N: u32 = 20_000;
    let read = |session: SessionRef| loop_protocol::admin::PermissionModeParams {
        session,
        mode: None,
        expected_revision: None,
    };
    let mut line = String::from("ACCESS|COST");
    for (name, caller, kind) in [
        ("internal", None, Kind::Local),
        ("host", Some(host()), Kind::Local),
        ("outside_owner", Some(edge("alice", "acme")), Kind::Alice),
        ("outside_refused", Some(edge("bob", "acme")), Kind::Alice),
    ] {
        let session = world.sessions[&kind].clone();
        let started = std::time::Instant::now();
        for _ in 0..N {
            let call = RuntimeAdmin::permission_mode(rt.as_ref(), read(session.clone()));
            let _ = match &caller {
                Some(caller) => caller.clone().scope(call).await,
                None => call.await,
            };
        }
        let per = started.elapsed().as_nanos() / u128::from(N);
        line.push_str(&format!("|{name}_ns={per}"));
    }
    println!("{line}");
    rt.shutdown().await;
}

/// F1: a load never moves a Session to another folder. Bob closes his
/// Session and loads it "into" Dave's unused folder: refused, so Dave's
/// folder stays free and the list shows the folder the Session runs in.
/// The host's user is refused the same way (the CLI never sends a `cwd` on
/// a load); the stored folder itself is accepted.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_load_never_moves_a_session_to_another_folder() {
    tokio::time::timeout(Duration::from_secs(120), async {
        let world = world(true).await;
        let rt = &world.rt;
        let bob = edge("bob", "acme");
        let b = world.sessions[&Kind::Bob].clone();
        let dave_folder = folder(world._dir.path(), "dave");
        bob.clone()
            .scope(RuntimeControl::close(rt.as_ref(), &b, false))
            .await
            .unwrap();
        let moved = bob
            .clone()
            .scope(rt.load_session(&b, spec(&dave_folder)))
            .await;
        assert!(
            matches!(moved, Err(BackendError::Unsupported(_))),
            "{moved:?}"
        );
        // Its own stored folder is fine.
        bob.clone()
            .scope(rt.load_session(&b, spec(&world.folders["bob"])))
            .await
            .unwrap();
        let listed = bob.clone().scope(rt.list()).await.unwrap();
        let row = listed.iter().find(|i| i.r#ref.key == b.key).unwrap();
        assert_eq!(row.cwd, world.folders["bob"]);
        // Dave's folder is still his to take.
        let dave = edge("dave", "acme");
        dave.scope(rt.new_session(spec(&dave_folder)))
            .await
            .unwrap();
        // The host's user cannot move its Session either.
        let local = world.sessions[&Kind::Local].clone();
        host()
            .scope(RuntimeControl::close(rt.as_ref(), &local, false))
            .await
            .unwrap();
        let moved = host()
            .scope(rt.load_session(&local, spec(&world.folders["none"])))
            .await;
        assert!(
            matches!(moved, Err(BackendError::Unsupported(_))),
            "{moved:?}"
        );
        rt.shutdown().await;
    })
    .await
    .unwrap();
}

/// An outside user names a folder strictly inside the tool-lock root: no
/// `cwd`, and the root itself, are refused, so no user can take every
/// other user's folder.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_outside_user_cannot_claim_the_root() {
    tokio::time::timeout(Duration::from_secs(120), async {
        let dir = tempfile::tempdir().unwrap();
        let rt = runtime(dir.path(), Arc::new(MemoryEventLog::new()), true).await;
        let carol = edge("carol", "acme");
        let none = carol
            .clone()
            .scope(rt.new_session(SessionSpec::default()))
            .await;
        assert!(matches!(none, Err(BackendError::Forbidden(_))), "{none:?}");
        let root = dir.path().join("ws").canonicalize().unwrap();
        let at_root = carol.clone().scope(rt.new_session(spec(&root))).await;
        assert!(
            matches!(at_root, Err(BackendError::Forbidden(_))),
            "{at_root:?}"
        );
        // Nothing was claimed: users' own folders still open.
        for who in ["carol", "dan"] {
            edge(who, "acme")
                .scope(rt.new_session(spec(&folder(dir.path(), who))))
                .await
                .unwrap();
        }
        rt.shutdown().await;
    })
    .await
    .unwrap();
}

/// F2: request ids are per user. Bob can reuse Alice's copy request id for
/// his own copy (no "already used"), and so cannot learn or take hers.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn copy_request_ids_are_per_user() {
    use loop_protocol::rpc::CloneParams;
    tokio::time::timeout(Duration::from_secs(120), async {
        let world = world(true).await;
        let rt = &world.rt;
        let clone = |who: Caller, kind: Kind| {
            let session = world.sessions[&kind].clone();
            who.scope(RuntimeControl::clone_session(
                rt.as_ref(),
                CloneParams {
                    session,
                    name: None,
                    request_id: "shared-id".into(),
                },
            ))
        };
        // Bob takes the id first; Alice's copy with it still works.
        let his = clone(edge("bob", "acme"), Kind::Bob).await.unwrap();
        let hers = clone(edge("alice", "acme"), Kind::Alice).await.unwrap();
        assert_ne!(his.session.key, hers.session.key);
        // A retry by Alice answers from her own receipt.
        let again = clone(edge("alice", "acme"), Kind::Alice).await.unwrap();
        assert_eq!(again.session.key, hers.session.key);
        // The host's user has its own namespace too.
        let mine = clone(host(), Kind::Local).await.unwrap();
        assert_ne!(mine.session.key, hers.session.key);
        rt.shutdown().await;
    })
    .await
    .unwrap();
}

/// F3: the host's `runtime/sessions` row of another tenant's Session shows
/// no model and no tokens, and reading it refuses nothing.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_host_rows_of_other_tenants_show_no_model() {
    tokio::time::timeout(Duration::from_secs(120), async {
        let world = world(true).await;
        let rt = &world.rt;
        let before = rt.access_refusals();
        let rows = host()
            .scope(RuntimeAdmin::sessions(rt.as_ref()))
            .await
            .unwrap();
        assert_eq!(rt.access_refusals(), before, "no refused stats reads");
        for row in &rows.sessions {
            let local = row.session.key == world.sessions[&Kind::Local].key;
            assert_eq!(row.model.is_some(), local, "{row:?}");
            assert_eq!(row.tokens.is_some(), local, "{row:?}");
        }
        rt.shutdown().await;
    })
    .await
    .unwrap();
}

/// A Subagent opens only in folders its parent's owner may use: Alice's
/// agent asks for a child in Bob's folder (inside the same git root), and
/// no child opens there.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_subagent_stays_in_its_owner_s_folders() {
    tokio::time::timeout(Duration::from_secs(120), async {
        let dir = tempfile::tempdir().unwrap();
        // One git root above both users' folders: `resolve_cwd` alone would
        // let a child go anywhere below it.
        std::fs::create_dir_all(dir.path().join("ws/.git")).unwrap();
        let rt = runtime(dir.path(), Arc::new(MemoryEventLog::new()), false).await;
        let (alice, bob) = (edge("alice", "acme"), edge("bob", "acme"));
        let hers = alice
            .clone()
            .scope(rt.new_session(spec(&folder(dir.path(), "alice"))))
            .await
            .unwrap();
        let bob_folder = folder(dir.path(), "bob");
        bob.scope(rt.new_session(spec(&bob_folder))).await.unwrap();
        alice.clone().scope(run_turn(&rt, &hers, "SPAWN-BOB")).await;
        let children: Vec<_> = rt
            .list()
            .await
            .unwrap()
            .into_iter()
            .filter(|info| info.parent.is_some())
            .collect();
        assert!(
            children.iter().all(|child| child.cwd != bob_folder),
            "a child opened in Bob's folder: {children:?}"
        );
        // Control: a child in her own folder opens.
        let again = alice
            .clone()
            .scope(rt.new_session(spec(&folder(dir.path(), "alice"))))
            .await
            .unwrap();
        alice.scope(run_turn(&rt, &again, "SPAWN")).await;
        assert!(rt
            .list()
            .await
            .unwrap()
            .iter()
            .any(|info| info.parent.as_ref().map(|p| &p.key) == Some(&again.key)));
        rt.shutdown().await;
    })
    .await
    .unwrap();
}
