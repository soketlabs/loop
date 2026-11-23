//! Session store conformance suite.
//!
//! Each suite function takes a factory that returns a fresh, empty
//! `Arc<dyn SessionStore>` in a [`Fixture`], and a [`Capabilities`] value.
//! [`run_all`] runs every check, each on its own fresh store.
//!
//! The stores in this repository differ in a few places. The suite does not
//! skip those checks. A [`Capabilities`] field names each difference. For a
//! field, the suite asserts the behavior that the store declares. If a store
//! changes its behavior, the declared value fails the check, so the change is
//! seen and not hidden. [`Capabilities::strict`] declares the behavior that a
//! new store should aim for. The `memory`, `sqlite` and `jsonl` constructors
//! declare what those stores do today.

use std::collections::HashSet;
use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use loop_agent::harness::session::{
    PendingSessionWrite, SessionFile, SessionForkSelection, SessionMetadata, SessionReader,
    SessionStore, SessionTreeEntry,
};
use loop_agent::harness::types::SessionError;
use loop_agent::{AgentMessage, AgentThinkingLevel};
use serde_json::json;

use crate::Fixture;

/// The order in which `list` returns Sessions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ListOrder {
    /// No promise. The suite compares as a set.
    Unspecified,
    /// Oldest `created_at` first.
    CreatedAscending,
    /// Newest `created_at` first.
    CreatedDescending,
}

/// What `delete` does for an id that does not exist.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeleteMissing {
    /// It succeeds and does nothing.
    Succeeds,
    /// It fails with `SessionError::NotFound`.
    NotFound,
}

/// Behavior that legitimately differs between stores.
#[derive(Debug, Clone)]
pub struct Capabilities {
    /// Order of `list`.
    pub list_order: ListOrder,
    /// `read_entries(Some(n))` skips the first `n` entries. When false, the
    /// argument is ignored and every entry is returned.
    pub read_entries_filters_after_seq: bool,
    /// `delete` of a missing id.
    pub delete_missing: DeleteMissing,
    /// `fork` records the source Session id in `parent_session_id`. When
    /// false, the store records something else (a file path) there.
    pub fork_parent_is_session_id: bool,
    /// A fork keeps the entry ids of the source. When false, it writes new
    /// ids and rebuilds the parent links.
    pub fork_keeps_entry_ids: bool,
    /// `metadata().path` is set (a file-backed store).
    pub metadata_has_path: bool,
    /// The store has write leases: `write_cancellation()` returns a token.
    pub write_cancellation: bool,
    /// `create_surface_destination` works on a plain store. When false, it
    /// returns `SessionError::Invalid`, the trait default.
    pub surface_destination: bool,
    /// `read_path_to_root_or_compaction(Some(unknown_id))` fails. When false,
    /// it returns an empty path.
    pub unknown_leaf_is_error: bool,
    /// Concurrent appends to one Session form one linear chain: each entry's
    /// parent is the entry before it. When false, the store loses the chain
    /// (several entries may share a parent); it still keeps every entry.
    /// This is a defect of the JSONL store, not a design choice: its keyed
    /// operation queue lets two operations on one key run at once.
    pub concurrent_appends_form_one_chain: bool,
}

impl Capabilities {
    /// The behavior a new store should aim for.
    pub fn strict() -> Self {
        Self {
            list_order: ListOrder::Unspecified,
            read_entries_filters_after_seq: true,
            delete_missing: DeleteMissing::NotFound,
            fork_parent_is_session_id: true,
            fork_keeps_entry_ids: false,
            metadata_has_path: false,
            write_cancellation: false,
            surface_destination: true,
            unknown_leaf_is_error: true,
            concurrent_appends_form_one_chain: true,
        }
    }

    /// What `create_in_memory_session_store` does today.
    pub fn memory() -> Self {
        Self {
            list_order: ListOrder::Unspecified,
            read_entries_filters_after_seq: false,
            delete_missing: DeleteMissing::Succeeds,
            fork_parent_is_session_id: true,
            fork_keeps_entry_ids: true,
            metadata_has_path: false,
            write_cancellation: false,
            surface_destination: false,
            unknown_leaf_is_error: false,
            concurrent_appends_form_one_chain: true,
        }
    }

    /// What `create_sqlite_session_store` does today.
    pub fn sqlite() -> Self {
        Self {
            list_order: ListOrder::CreatedAscending,
            read_entries_filters_after_seq: false,
            delete_missing: DeleteMissing::Succeeds,
            fork_parent_is_session_id: true,
            fork_keeps_entry_ids: false,
            metadata_has_path: false,
            write_cancellation: false,
            surface_destination: false,
            unknown_leaf_is_error: true,
            concurrent_appends_form_one_chain: true,
        }
    }

    /// What `create_jsonl_session_store` does today.
    pub fn jsonl() -> Self {
        Self {
            list_order: ListOrder::CreatedDescending,
            read_entries_filters_after_seq: false,
            delete_missing: DeleteMissing::NotFound,
            fork_parent_is_session_id: false,
            fork_keeps_entry_ids: true,
            metadata_has_path: true,
            write_cancellation: false,
            surface_destination: false,
            unknown_leaf_is_error: false,
            concurrent_appends_form_one_chain: false,
        }
    }
}

/// Run every check below. The factory is called once per check.
pub async fn run_all<F, Fut>(factory: F, caps: &Capabilities)
where
    F: Fn() -> Fut,
    Fut: Future<Output = Fixture<dyn SessionStore>>,
{
    create_and_load(&factory, caps).await;
    list(&factory, caps).await;
    append_entry_order(&factory, caps).await;
    concurrent_appends(&factory, caps).await;
    read_entries_after_seq(&factory, caps).await;
    read_path(&factory, caps).await;
    leaf_moves(&factory, caps).await;
    fork(&factory, caps).await;
    delete(&factory, caps).await;
    open_writer(&factory, caps).await;
    trait_defaults(&factory, caps).await;
}

fn text(label: &str) -> PendingSessionWrite {
    PendingSessionWrite::Message {
        message: AgentMessage::user_text(label),
    }
}

fn is_not_found<T>(result: &Result<T, SessionError>) -> bool {
    matches!(result, Err(SessionError::NotFound(_)))
}

fn as_json(entries: &[SessionTreeEntry]) -> Vec<serde_json::Value> {
    entries
        .iter()
        .map(|e| serde_json::to_value(e).unwrap())
        .collect()
}

fn ids(entries: &[SessionTreeEntry]) -> Vec<String> {
    entries.iter().map(|e| e.id().to_string()).collect()
}

async fn append_texts(
    store: &dyn SessionStore,
    id: &str,
    labels: &[&str],
) -> Vec<SessionTreeEntry> {
    let mut out = Vec::new();
    for label in labels {
        out.push(store.append_entry(id, text(label)).await.unwrap());
    }
    out
}

/// A pause so that two Sessions never share a `created_at` millisecond.
async fn tick() {
    tokio::time::sleep(Duration::from_millis(3)).await;
}

/// `create` and `load`: metadata, missing ids and empty Sessions.
pub async fn create_and_load<F, Fut>(factory: &F, caps: &Capabilities)
where
    F: Fn() -> Fut,
    Fut: Future<Output = Fixture<dyn SessionStore>>,
{
    let fixture = factory().await;
    let store = fixture.store.as_ref();

    let created = store
        .create(Some("/proj/one".into()), Some("first".into()))
        .await
        .unwrap();
    let meta = created.metadata().clone();
    assert!(!meta.id.is_empty());
    assert_eq!(meta.cwd.as_deref(), Some("/proj/one"));
    assert_eq!(meta.name.as_deref(), Some("first"));
    assert_eq!(meta.parent_session_id, None);
    assert!(meta.created_at > 0);
    assert_eq!(meta.path.is_some(), caps.metadata_has_path);

    // A new Session is empty.
    assert_eq!(created.read_head().await.unwrap(), None);
    assert!(created.read_entries(None).await.unwrap().is_empty());
    assert!(created
        .read_path_to_root_or_compaction(None)
        .await
        .unwrap()
        .is_empty());
    assert!(created.read_entry("no-such-entry").await.unwrap().is_none());

    // `load` returns the same metadata.
    let loaded = store.load(&meta.id).await.unwrap();
    let again = loaded.metadata();
    assert_eq!(again.id, meta.id);
    assert_eq!(again.cwd, meta.cwd);
    assert_eq!(again.name, meta.name);
    assert_eq!(again.parent_session_id, None);
    assert_eq!(again.created_at, meta.created_at);

    // No cwd and no name stay absent. Ids are unique.
    let bare = store.create(None, None).await.unwrap();
    assert_eq!(bare.metadata().cwd, None);
    assert_eq!(bare.metadata().name, None);
    assert_ne!(bare.metadata().id, meta.id);
    let bare_loaded = store.load(&bare.metadata().id).await.unwrap();
    assert_eq!(bare_loaded.metadata().cwd, None);
    assert_eq!(bare_loaded.metadata().name, None);

    // A missing id is `NotFound`.
    assert!(is_not_found(&store.load("no-such-session").await));
}

/// `list` with and without a cwd filter.
pub async fn list<F, Fut>(factory: &F, caps: &Capabilities)
where
    F: Fn() -> Fut,
    Fut: Future<Output = Fixture<dyn SessionStore>>,
{
    let fixture = factory().await;
    let store = fixture.store.as_ref();
    assert!(store.list(None).await.unwrap().is_empty());
    assert!(store.list(Some("/alpha")).await.unwrap().is_empty());

    let mut created = Vec::new();
    for (cwd, name) in [
        (Some("/alpha"), "a1"),
        (Some("/beta"), "b1"),
        (Some("/alpha"), "a2"),
        (None, "none"),
    ] {
        let reader = store
            .create(cwd.map(str::to_string), Some(name.into()))
            .await
            .unwrap();
        created.push(reader.metadata().clone());
        tick().await;
    }
    let id_set = |list: &[SessionMetadata]| -> HashSet<String> {
        list.iter().map(|m| m.id.clone()).collect()
    };

    let all = store.list(None).await.unwrap();
    assert_eq!(all.len(), 4);
    assert_eq!(
        id_set(&all),
        created.iter().map(|m| m.id.clone()).collect::<HashSet<_>>()
    );

    let alpha = store.list(Some("/alpha")).await.unwrap();
    assert_eq!(alpha.len(), 2);
    assert!(alpha.iter().all(|m| m.cwd.as_deref() == Some("/alpha")));
    assert_eq!(
        id_set(&alpha),
        HashSet::from([created[0].id.clone(), created[2].id.clone()])
    );
    let beta = store.list(Some("/beta")).await.unwrap();
    assert_eq!(id_set(&beta), HashSet::from([created[1].id.clone()]));
    assert!(store.list(Some("/gamma")).await.unwrap().is_empty());

    // Listed metadata matches what `create` returned.
    for listed in &all {
        let original = created.iter().find(|m| m.id == listed.id).unwrap();
        assert_eq!(listed.cwd, original.cwd);
        assert_eq!(listed.name, original.name);
        assert_eq!(listed.created_at, original.created_at);
    }

    let names: Vec<_> = alpha.iter().map(|m| m.name.clone().unwrap()).collect();
    match caps.list_order {
        ListOrder::Unspecified => {}
        ListOrder::CreatedAscending => assert_eq!(names, ["a1", "a2"]),
        ListOrder::CreatedDescending => assert_eq!(names, ["a2", "a1"]),
    }
    let all_times: Vec<i64> = all.iter().map(|m| m.created_at).collect();
    match caps.list_order {
        ListOrder::Unspecified => {}
        ListOrder::CreatedAscending => assert!(all_times.windows(2).all(|w| w[0] <= w[1])),
        ListOrder::CreatedDescending => assert!(all_times.windows(2).all(|w| w[0] >= w[1])),
    }
}

/// `append_entry`: ids, parent links, order, kinds and persistence.
pub async fn append_entry_order<F, Fut>(factory: &F, _caps: &Capabilities)
where
    F: Fn() -> Fut,
    Fut: Future<Output = Fixture<dyn SessionStore>>,
{
    let fixture = factory().await;
    let store = fixture.store.as_ref();
    let reader = store.create(Some("/w".into()), None).await.unwrap();
    let sid = reader.metadata().id.clone();

    let first = store.append_entry(&sid, text("one")).await.unwrap();
    assert_eq!(first.parent_id(), None);
    assert!(first.timestamp() > 0);
    assert_eq!(
        reader.read_head().await.unwrap().as_deref(),
        Some(first.id())
    );

    // Every kind of write becomes an entry that chains onto the leaf.
    let pendings = vec![
        text("two"),
        PendingSessionWrite::ThinkingLevelChange {
            thinking_level: AgentThinkingLevel::High,
        },
        PendingSessionWrite::ModelChange {
            provider: "faux".into(),
            model_id: "faux-model".into(),
        },
        PendingSessionWrite::ActiveToolsChange {
            tool_names: vec!["ls".into(), "bash".into()],
        },
        PendingSessionWrite::Label {
            label: "mark".into(),
        },
        PendingSessionWrite::SessionInfo {
            name: "renamed".into(),
        },
        PendingSessionWrite::BranchSummary {
            summary: "side trip".into(),
        },
        PendingSessionWrite::Custom {
            custom_type: "probe".into(),
            data: json!({"k": [1, 2, 3]}),
        },
        text("three"),
    ];
    let mut written = vec![first];
    for pending in pendings {
        let previous = written.last().unwrap().id().to_string();
        let entry = store.append_entry(&sid, pending).await.unwrap();
        assert_eq!(entry.parent_id(), Some(previous.as_str()));
        assert!(entry.timestamp() >= written.last().unwrap().timestamp());
        written.push(entry);
    }
    let unique: HashSet<_> = ids(&written).into_iter().collect();
    assert_eq!(unique.len(), written.len(), "entry ids are unique");

    // Reads return the entries in append order, equal to what append returned.
    let read = reader.read_entries(None).await.unwrap();
    assert_eq!(as_json(&read), as_json(&written));
    assert_eq!(
        reader.read_head().await.unwrap().as_deref(),
        Some(written.last().unwrap().id())
    );
    for entry in &written {
        let one = reader.read_entry(entry.id()).await.unwrap().unwrap();
        assert_eq!(
            serde_json::to_value(&one).unwrap(),
            serde_json::to_value(entry).unwrap()
        );
    }
    // A fresh reader sees the same entries.
    let fresh = store.load(&sid).await.unwrap();
    assert_eq!(
        as_json(&fresh.read_entries(None).await.unwrap()),
        as_json(&written)
    );
    assert_eq!(
        fresh.read_head().await.unwrap().as_deref(),
        Some(written.last().unwrap().id())
    );
    // The default path is the whole chain, root first.
    let path = fresh.read_path_to_root_or_compaction(None).await.unwrap();
    assert_eq!(ids(&path), ids(&written));

    // Appends to one Session do not touch another.
    let other = store.create(None, None).await.unwrap();
    assert!(other.read_entries(None).await.unwrap().is_empty());
    assert_eq!(other.read_head().await.unwrap(), None);

    // A missing Session is `NotFound`, and nothing is created by the attempt.
    assert!(is_not_found(
        &store.append_entry("no-such-session", text("x")).await
    ));
    assert!(is_not_found(&store.load("no-such-session").await));
}

/// Concurrent appends to one Session all land, in one linear chain.
pub async fn concurrent_appends<F, Fut>(factory: &F, caps: &Capabilities)
where
    F: Fn() -> Fut,
    Fut: Future<Output = Fixture<dyn SessionStore>>,
{
    let fixture = factory().await;
    let store = Arc::clone(&fixture.store);
    let reader = store.create(None, None).await.unwrap();
    let sid = reader.metadata().id.clone();
    let mut tasks = Vec::new();
    for worker in 0..8 {
        let store = Arc::clone(&store);
        let sid = sid.clone();
        tasks.push(tokio::spawn(async move {
            let mut mine = Vec::new();
            for n in 0..8 {
                let entry = store
                    .append_entry(&sid, text(&format!("w{worker}-{n}")))
                    .await
                    .unwrap();
                mine.push(entry.id().to_string());
            }
            mine
        }));
    }
    let mut returned = HashSet::new();
    for task in tasks {
        for id in task.await.unwrap() {
            returned.insert(id);
        }
    }
    assert_eq!(returned.len(), 64);
    let entries = reader.read_entries(None).await.unwrap();
    assert_eq!(entries.len(), 64);
    if caps.concurrent_appends_form_one_chain {
        assert_eq!(entries[0].parent_id(), None);
        for pair in entries.windows(2) {
            assert_eq!(pair[1].parent_id(), Some(pair[0].id()));
        }
    }
    assert_eq!(ids(&entries).into_iter().collect::<HashSet<_>>(), returned);
}

/// `read_entries(after_seq)`.
pub async fn read_entries_after_seq<F, Fut>(factory: &F, caps: &Capabilities)
where
    F: Fn() -> Fut,
    Fut: Future<Output = Fixture<dyn SessionStore>>,
{
    let fixture = factory().await;
    let store = fixture.store.as_ref();
    let reader = store.create(None, None).await.unwrap();
    let sid = reader.metadata().id.clone();
    let written = append_texts(store, &sid, &["1", "2", "3", "4", "5"]).await;
    let all = ids(&written);

    assert_eq!(ids(&reader.read_entries(None).await.unwrap()), all);
    assert_eq!(ids(&reader.read_entries(Some(0)).await.unwrap()), all);
    for after in [2_u64, 4, 5, 99] {
        let got = ids(&reader.read_entries(Some(after)).await.unwrap());
        if caps.read_entries_filters_after_seq {
            let skip = (after as usize).min(all.len());
            assert_eq!(got, all[skip..], "after_seq {after}");
        } else {
            assert_eq!(got, all, "after_seq {after} is ignored");
        }
    }
}

/// `read_path_to_root_or_compaction`: plain chain, explicit leaf, compaction.
pub async fn read_path<F, Fut>(factory: &F, caps: &Capabilities)
where
    F: Fn() -> Fut,
    Fut: Future<Output = Fixture<dyn SessionStore>>,
{
    let fixture = factory().await;
    let store = fixture.store.as_ref();
    let reader = store.create(None, None).await.unwrap();
    let sid = reader.metadata().id.clone();
    let chain = append_texts(store, &sid, &["m1", "m2", "m3"]).await;

    // An explicit leaf cuts the path there.
    let mid = reader
        .read_path_to_root_or_compaction(Some(chain[1].id()))
        .await
        .unwrap();
    assert_eq!(ids(&mid), ids(&chain[..2]));
    // The default leaf is the head.
    let full = reader.read_path_to_root_or_compaction(None).await.unwrap();
    assert_eq!(ids(&full), ids(&chain));
    let explicit = reader
        .read_path_to_root_or_compaction(Some(chain[2].id()))
        .await
        .unwrap();
    assert_eq!(ids(&explicit), ids(&chain));

    // An unknown leaf.
    let unknown = reader
        .read_path_to_root_or_compaction(Some("no-such-entry"))
        .await;
    if caps.unknown_leaf_is_error {
        assert!(unknown.is_err());
    } else {
        assert!(unknown.unwrap().is_empty());
    }

    // A compaction that keeps m2 onward: the path starts at m2.
    let compaction = store
        .append_entry(
            &sid,
            PendingSessionWrite::Compaction {
                summary: "summary".into(),
                first_kept_entry_id: Some(chain[1].id().to_string()),
                details: Some(json!({"tokens": 10})),
            },
        )
        .await
        .unwrap();
    let after = store.append_entry(&sid, text("m4")).await.unwrap();
    let path = reader.read_path_to_root_or_compaction(None).await.unwrap();
    assert_eq!(
        ids(&path),
        [chain[1].id(), chain[2].id(), compaction.id(), after.id()]
    );
    // The full tree keeps the compacted history.
    let all = reader.read_entries(None).await.unwrap();
    assert_eq!(all.len(), 5);
    // A path that ends at the compaction itself.
    let at = reader
        .read_path_to_root_or_compaction(Some(compaction.id()))
        .await
        .unwrap();
    assert_eq!(ids(&at), [chain[1].id(), chain[2].id(), compaction.id()]);

    // A legacy compaction without a kept range replaces all earlier entries.
    let legacy = store
        .append_entry(
            &sid,
            PendingSessionWrite::Compaction {
                summary: "legacy".into(),
                first_kept_entry_id: None,
                details: None,
            },
        )
        .await
        .unwrap();
    let tail = store.append_entry(&sid, text("m5")).await.unwrap();
    let path = reader.read_path_to_root_or_compaction(None).await.unwrap();
    assert_eq!(ids(&path), [legacy.id(), tail.id()]);
}

/// Leaf moves create branches; the head and the path follow the leaf.
pub async fn leaf_moves<F, Fut>(factory: &F, _caps: &Capabilities)
where
    F: Fn() -> Fut,
    Fut: Future<Output = Fixture<dyn SessionStore>>,
{
    let fixture = factory().await;
    let store = fixture.store.as_ref();
    let reader = store.create(None, None).await.unwrap();
    let sid = reader.metadata().id.clone();
    let chain = append_texts(store, &sid, &["m1", "m2", "m3"]).await;

    let moved = store
        .append_entry(
            &sid,
            PendingSessionWrite::Leaf {
                target_id: Some(chain[0].id().to_string()),
            },
        )
        .await
        .unwrap();
    assert_eq!(moved.parent_id(), Some(chain[2].id()));
    assert_eq!(
        reader.read_head().await.unwrap().as_deref(),
        Some(chain[0].id())
    );
    let branch = store.append_entry(&sid, text("branch")).await.unwrap();
    assert_eq!(branch.parent_id(), Some(chain[0].id()));
    assert_eq!(
        reader.read_head().await.unwrap().as_deref(),
        Some(branch.id())
    );
    let path = reader.read_path_to_root_or_compaction(None).await.unwrap();
    assert_eq!(ids(&path), [chain[0].id(), branch.id()]);
    // The old branch is still in the tree and still reachable by its leaf.
    let old = reader
        .read_path_to_root_or_compaction(Some(chain[2].id()))
        .await
        .unwrap();
    assert_eq!(ids(&old), ids(&chain));
    assert_eq!(reader.read_entries(None).await.unwrap().len(), 5);

    // Moving to the root empties the head. The next entry has no parent.
    store
        .append_entry(&sid, PendingSessionWrite::Leaf { target_id: None })
        .await
        .unwrap();
    assert_eq!(reader.read_head().await.unwrap(), None);
    let root = store.append_entry(&sid, text("new root")).await.unwrap();
    assert_eq!(root.parent_id(), None);
}

/// `fork`: every selection, parent link, name, cwd and errors.
pub async fn fork<F, Fut>(factory: &F, caps: &Capabilities)
where
    F: Fn() -> Fut,
    Fut: Future<Output = Fixture<dyn SessionStore>>,
{
    let fixture = factory().await;
    let store = fixture.store.as_ref();
    let source = store
        .create(Some("/src".into()), Some("source".into()))
        .await
        .unwrap();
    let sid = source.metadata().id.clone();
    let written = append_texts(store, &sid, &["first", "second", "third"]).await;

    let check_parent = |meta: &SessionMetadata| {
        if caps.fork_parent_is_session_id {
            assert_eq!(meta.parent_session_id.as_deref(), Some(sid.as_str()));
        } else {
            assert!(meta.parent_session_id.is_some());
            assert_ne!(meta.parent_session_id.as_deref(), Some(sid.as_str()));
        }
    };
    let check_entries = |forked: &[SessionTreeEntry], expected: &[SessionTreeEntry]| {
        assert_eq!(forked.len(), expected.len());
        if caps.fork_keeps_entry_ids {
            assert_eq!(ids(forked), ids(expected));
        } else {
            let source_ids: HashSet<_> = ids(expected).into_iter().collect();
            assert!(forked.iter().all(|e| !source_ids.contains(e.id())));
        }
        // The copy is a linear chain.
        for pair in forked.windows(2) {
            assert_eq!(pair[1].parent_id(), Some(pair[0].id()));
        }
        // Content is equal. Compare the messages only, so ids may differ.
        let body = |entries: &[SessionTreeEntry]| -> Vec<serde_json::Value> {
            entries
                .iter()
                .map(|e| match serde_json::to_value(e).unwrap() {
                    serde_json::Value::Object(mut map) => {
                        map.remove("id");
                        map.remove("parent_id");
                        map.remove("timestamp");
                        serde_json::Value::Object(map)
                    }
                    other => other,
                })
                .collect()
        };
        assert_eq!(body(forked), body(expected));
    };

    // All.
    let all = store
        .fork(&sid, SessionForkSelection::All, None, Some("all".into()))
        .await
        .unwrap();
    assert_ne!(all.metadata().id, sid);
    assert_eq!(all.metadata().name.as_deref(), Some("all"));
    assert_eq!(all.metadata().cwd.as_deref(), Some("/src"));
    check_parent(all.metadata());
    let forked = all.read_entries(None).await.unwrap();
    check_entries(&forked, &written);
    assert_eq!(
        all.read_head().await.unwrap().as_deref(),
        forked.last().map(|e| e.id())
    );
    // The fork is a stored Session of its own.
    let loaded = store.load(&all.metadata().id).await.unwrap();
    assert_eq!(loaded.read_entries(None).await.unwrap().len(), 3);
    assert_eq!(
        loaded.metadata().parent_session_id,
        all.metadata().parent_session_id
    );

    // ThroughEntry is inclusive.
    let through = store
        .fork(
            &sid,
            SessionForkSelection::ThroughEntry,
            Some(written[1].id()),
            None,
        )
        .await
        .unwrap();
    assert_eq!(through.metadata().name, None);
    check_entries(&through.read_entries(None).await.unwrap(), &written[..2]);

    // BeforeEntry is exclusive.
    let before = store
        .fork(
            &sid,
            SessionForkSelection::BeforeEntry,
            Some(written[2].id()),
            None,
        )
        .await
        .unwrap();
    check_entries(&before.read_entries(None).await.unwrap(), &written[..2]);
    let none = store
        .fork(
            &sid,
            SessionForkSelection::BeforeEntry,
            Some(written[0].id()),
            None,
        )
        .await
        .unwrap();
    assert!(none.read_entries(None).await.unwrap().is_empty());
    assert_eq!(none.read_head().await.unwrap(), None);

    // BeforeUserMessage stops before the first user message on the branch.
    let before_user = store
        .fork(&sid, SessionForkSelection::BeforeUserMessage, None, None)
        .await
        .unwrap();
    assert!(before_user.read_entries(None).await.unwrap().is_empty());

    // A fork of a fork records its own source.
    let nested = store
        .fork(
            &all.metadata().id,
            SessionForkSelection::All,
            None,
            Some("nested".into()),
        )
        .await
        .unwrap();
    if caps.fork_parent_is_session_id {
        assert_eq!(
            nested.metadata().parent_session_id.as_deref(),
            Some(all.metadata().id.as_str())
        );
    }

    // The source is not changed, and a fork never writes back to it.
    assert_eq!(source.read_entries(None).await.unwrap().len(), 3);
    store
        .append_entry(&all.metadata().id, text("only in fork"))
        .await
        .unwrap();
    assert_eq!(source.read_entries(None).await.unwrap().len(), 3);
    assert_eq!(
        store
            .load(&all.metadata().id)
            .await
            .unwrap()
            .read_entries(None)
            .await
            .unwrap()
            .len(),
        4
    );

    // Errors: missing source, missing target, missing id.
    assert!(is_not_found(
        &store
            .fork("no-such-session", SessionForkSelection::All, None, None)
            .await
    ));
    assert!(store
        .fork(&sid, SessionForkSelection::ThroughEntry, None, None)
        .await
        .is_err());
    assert!(store
        .fork(
            &sid,
            SessionForkSelection::ThroughEntry,
            Some("no-such-entry"),
            None
        )
        .await
        .is_err());
    assert!(store
        .fork(
            &sid,
            SessionForkSelection::BeforeEntry,
            Some("no-such-entry"),
            None
        )
        .await
        .is_err());
    // Failed forks leave no Session behind: source, fork, nested and the
    // four forks above that succeeded.
    let count = store.list(None).await.unwrap().len();
    assert_eq!(count, 1 + 6);
}

/// `delete`.
pub async fn delete<F, Fut>(factory: &F, caps: &Capabilities)
where
    F: Fn() -> Fut,
    Fut: Future<Output = Fixture<dyn SessionStore>>,
{
    let fixture = factory().await;
    let store = fixture.store.as_ref();
    let doomed = store.create(Some("/d".into()), None).await.unwrap();
    let keeper = store.create(Some("/d".into()), None).await.unwrap();
    let doomed_id = doomed.metadata().id.clone();
    let keeper_id = keeper.metadata().id.clone();
    append_texts(store, &doomed_id, &["a", "b"]).await;
    append_texts(store, &keeper_id, &["k"]).await;

    store.delete(&doomed_id).await.unwrap();
    assert!(is_not_found(&store.load(&doomed_id).await));
    assert!(is_not_found(
        &store.append_entry(&doomed_id, text("late")).await
    ));
    let listed = store.list(None).await.unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].id, keeper_id);
    assert_eq!(store.list(Some("/d")).await.unwrap().len(), 1);
    // The other Session is untouched.
    assert_eq!(keeper.read_entries(None).await.unwrap().len(), 1);
    assert_eq!(
        store
            .load(&keeper_id)
            .await
            .unwrap()
            .read_entries(None)
            .await
            .unwrap()
            .len(),
        1
    );
    // A fork of a deleted Session fails; a fork made earlier survives.
    assert!(is_not_found(
        &store
            .fork(&doomed_id, SessionForkSelection::All, None, None)
            .await
    ));
    let child = store
        .fork(&keeper_id, SessionForkSelection::All, None, None)
        .await
        .unwrap();
    store.delete(&keeper_id).await.unwrap();
    assert_eq!(
        store
            .load(&child.metadata().id)
            .await
            .unwrap()
            .read_entries(None)
            .await
            .unwrap()
            .len(),
        1
    );

    // A missing id.
    let missing = store.delete("no-such-session").await;
    match caps.delete_missing {
        DeleteMissing::Succeeds => missing.unwrap(),
        DeleteMissing::NotFound => assert!(is_not_found(&missing)),
    }
    // Deleting twice behaves like deleting a missing id.
    let twice = store.delete(&doomed_id).await;
    match caps.delete_missing {
        DeleteMissing::Succeeds => twice.unwrap(),
        DeleteMissing::NotFound => assert!(is_not_found(&twice)),
    }
}

/// `open_writer` admits a writer for an existing Session.
pub async fn open_writer<F, Fut>(factory: &F, _caps: &Capabilities)
where
    F: Fn() -> Fut,
    Fut: Future<Output = Fixture<dyn SessionStore>>,
{
    let fixture = factory().await;
    let store = fixture.store.as_ref();
    let created = store
        .create(Some("/o".into()), Some("opened".into()))
        .await
        .unwrap();
    let sid = created.metadata().id.clone();
    let written = append_texts(store, &sid, &["x", "y"]).await;

    let writer: Arc<dyn SessionReader> = store.open_writer(&sid).await.unwrap();
    assert_eq!(writer.metadata().id, sid);
    assert_eq!(writer.metadata().cwd.as_deref(), Some("/o"));
    assert_eq!(writer.metadata().name.as_deref(), Some("opened"));
    assert_eq!(
        as_json(&writer.read_entries(None).await.unwrap()),
        as_json(&written)
    );
    // The admitted writer can append, and the entry is visible to readers.
    let more = store.append_entry(&sid, text("z")).await.unwrap();
    assert_eq!(more.parent_id(), Some(written[1].id()));
    assert_eq!(writer.read_entries(None).await.unwrap().len(), 3);
    assert_eq!(
        store
            .load(&sid)
            .await
            .unwrap()
            .read_head()
            .await
            .unwrap()
            .as_deref(),
        Some(more.id())
    );
    // Opening twice is allowed for the same writer.
    store.open_writer(&sid).await.unwrap();
    // A missing Session is `NotFound`.
    assert!(is_not_found(&store.open_writer("no-such-session").await));
}

/// Defaults of the trait: cancellation token, lease release, Surface
/// destination.
pub async fn trait_defaults<F, Fut>(factory: &F, caps: &Capabilities)
where
    F: Fn() -> Fut,
    Fut: Future<Output = Fixture<dyn SessionStore>>,
{
    let fixture = factory().await;
    let store = fixture.store.as_ref();
    assert_eq!(
        store.write_cancellation().is_some(),
        caps.write_cancellation
    );
    // Releasing leases on a store that holds none succeeds, and the store
    // keeps working after it unless it has leases.
    store.release_leases().await.unwrap();
    if !caps.write_cancellation {
        let reader = store.create(None, None).await.unwrap();
        store
            .append_entry(&reader.metadata().id, text("after release"))
            .await
            .unwrap();
    }

    let entries = vec![SessionTreeEntry::Message {
        id: "e1".into(),
        parent_id: None,
        timestamp: 1,
        message: AgentMessage::user_text("hello"),
    }];
    let file = SessionFile {
        id: "exported".into(),
        name: Some("exported".into()),
        cwd: Some("/x".into()),
        created_at: 1,
        entries,
    };
    let result = store
        .create_surface_destination(
            file,
            "/x".into(),
            None,
            "request-key".into(),
            "fingerprint".into(),
            "receipt".into(),
        )
        .await;
    if caps.surface_destination {
        let id = result.unwrap();
        assert!(!id.is_empty());
        assert_eq!(store.load(&id).await.unwrap().metadata().id, id);
    } else {
        assert!(matches!(result, Err(SessionError::Invalid(_))));
        // A refused destination creates no Session.
        assert!(store
            .list(None)
            .await
            .unwrap()
            .iter()
            .all(|m| m.cwd.as_deref() != Some("/x")));
    }
}
