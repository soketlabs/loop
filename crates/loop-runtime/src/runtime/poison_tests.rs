//! A poisoned lock never decides access: the access checks of
//! [`super::reach`] fail closed, and a Session the poisoned map does not
//! hold still answers from its own state.
use super::*;
use crate::log::MemoryEventLog;
use crate::sync::poison;
use loop_app_core::runtime::{BootstrapOpts, GlobalResources};
use loop_protocol::{PrincipalKind, SessionSpec};

async fn runtime(dir: &std::path::Path) -> Arc<Runtime> {
    let models = Arc::new(loop_ai::Models::create(Default::default()));
    models.set_provider(loop_ai::providers::faux_provider(Default::default()));
    let globals = GlobalResources::with_models(dir.join("agent"), models).unwrap();
    let opts = BootstrapOpts {
        cwd: dir.into(),
        provider: Some("faux".into()),
        model: Some("faux-model".into()),
        theme: None,
        system_prompt: None,
        append_system_prompt: None,
        no_context_files: true,
        interactive: false,
        session_id: None,
        no_web: true,
        permission_mode: Some("ask".into()),
        agent: None,
    };
    let store = globals.open_runtime_session_store().unwrap();
    Runtime::new(
        globals,
        opts,
        Arc::new(MemoryEventLog::new()),
        store,
        RuntimeLimits::default(),
    )
    .await
    .unwrap()
    .into_shared()
}

/// A user from outside the host.
fn outside_caller() -> Caller {
    let principal = Principal::new(PrincipalKind::Person, "https://idp", "ada").unwrap();
    Caller::asserted(principal, Tenant::new("acme").unwrap()).unwrap()
}

fn outside() -> Actor {
    Actor::Caller(outside_caller())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_poisoned_session_map_fails_access_closed() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().canonicalize().unwrap();
    let (x, own, empty) = (root.join("x"), root.join("own"), root.join("empty"));
    for dir in [&x, &own, &empty] {
        std::fs::create_dir_all(dir).unwrap();
    }
    let rt = runtime(&root).await;
    let session = rt
        .new_session(SessionSpec {
            cwd: Some(x.clone()),
            ..Default::default()
        })
        .await
        .unwrap();
    // A Session the outside user owns, in its own folder.
    outside_caller()
        .scope(rt.new_session(SessionSpec {
            cwd: Some(own.clone()),
            ..Default::default()
        }))
        .await
        .unwrap();
    let outside = outside();
    // Before: the outside user reads and may change its own folder, and an
    // empty one.
    assert_eq!(rt.readable_folders(&outside), Some(vec![own.clone()]));
    assert!(rt.may_change_folder(&outside, &own));
    assert!(rt.may_change_folder(&outside, &empty));
    rt.check_folder(&outside, &empty, None).await.unwrap();

    poison(&rt.sessions);
    assert!(rt.subject(&session.key).await.is_err());
    assert!(rt
        .check_as(&outside, &session.key, Need::Read)
        .await
        .is_err());
    assert!(rt.check_folder(&outside, &empty, None).await.is_err());
    assert_eq!(rt.readable_folders(&outside), Some(Vec::new()));
    assert!(!rt.may_change_folder(&outside, &own));
    assert!(!rt.may_change_folder(&outside, &empty));
    // Internal work and the host's user are never limited by these.
    assert_eq!(rt.readable_folders(&Actor::Internal), None);
    assert!(rt.may_change_folder(&Actor::Internal, &empty));

    // The test thread changed nothing in the map: let the Runtime shut down.
    rt.sessions.clear_poison();
}

/// A poisoned subject cache refuses a Session it would have to look up,
/// while a loaded Session still answers from its own state.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_poisoned_subject_cache_refuses_only_lookups() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().canonicalize().unwrap();
    let rt = runtime(&root).await;
    let loaded = rt.new_session(SessionSpec::default()).await.unwrap();

    poison(&rt.subjects);
    assert!(rt.subject(&SessionKey::from("unknown")).await.is_err());
    assert!(rt.subject(&loaded.key).await.is_ok());
    assert!(rt
        .check_as(&outside(), &SessionKey::from("unknown"), Need::Read)
        .await
        .is_err());
    rt.subjects.clear_poison();
}

/// A Subagent's turn acts for whoever started its parent's turn. When the
/// parent's turn caller is poisoned, the prompt is refused: it never runs
/// as the Runtime, and the child stays idle.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_poisoned_parent_turn_caller_refuses_the_subagent_prompt() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().canonicalize().unwrap();
    let rt = runtime(&root).await;
    let parent = rt.new_session(SessionSpec::default()).await.unwrap();
    let parent_live = rt.route(&parent).unwrap();
    let child = rt
        .open(
            None,
            SessionSpec::default(),
            Some(subagent::ChildOpen {
                parent: Arc::clone(&parent_live),
                cwd: root.clone(),
                max_turns: None,
            }),
        )
        .await
        .unwrap();

    poison(parent_live.escalations.turn_caller_lock());
    let refused = rt
        .prompt(&child, vec![ContentBlock::text("hello")])
        .await
        .err();
    match refused {
        Some(BackendError::Failed(info)) => assert_eq!(info.category, ErrorCategory::Internal),
        other => panic!(
            "the Subagent prompt was not refused: {:?}",
            other.map(|_| ())
        ),
    }
    assert!(!rt.route(&child).unwrap().busy.load(Ordering::Acquire));

    // A new parent turn replaces the caller; the child's prompt works again.
    parent_live.escalations.set_turn_caller(None);
    assert!(rt
        .prompt(&child, vec![ContentBlock::text("hello")])
        .await
        .is_ok());
}
