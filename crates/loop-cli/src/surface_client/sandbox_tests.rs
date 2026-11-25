//! A sandbox chosen in the single-process CLI applies from the next turn,
//! is announced and saved, and is released on every path that abandons it.
//! The sandboxes are fakes with their own identity, so the tests can see
//! which one a tool really ran in.
use super::*;
use loop_agent::harness::{SandboxMode, TaggedSandbox};
use loop_ai::providers::{faux_provider_fn, FauxResponse};
use loop_ai::{Context, Message, ToolCall};
use loop_protocol::admin::{SandboxAction, SandboxChoice, SandboxParams, SandboxResult};
use loop_protocol::{ContentBlock, SessionUpdate, UpdateEnvelope};
use std::time::Duration;
use tokio::sync::broadcast::Receiver;

const LIMIT: Duration = Duration::from_secs(30);

fn local(tag: &str) -> SandboxChoice {
    SandboxChoice {
        mode: "local".into(),
        isolation: None,
        runtime: Some(tag.into()),
    }
}

/// Every prompt asks for one `echo parked` command, then finishes.
fn command_per_prompt(client: &SurfaceClient) {
    let provider = faux_provider_fn(Arc::new(|context: &Context| {
        let response = if matches!(context.messages.last(), Some(Message::User(_))) {
            FauxResponse::ToolCalls(vec![ToolCall {
                id: format!("call{}", context.messages.len()),
                name: "bash".into(),
                arguments: serde_json::json!({"command": "echo parked"}),
                thought_signature: None,
            }])
        } else {
            FauxResponse::Text("finished".into())
        };
        Box::pin(std::future::ready(response))
    }));
    provider.set_models(client.host.inner.models.models(Some("faux")));
    client.host.inner.models.set_provider(provider);
}

fn use_sandboxes(client: &mut SurfaceClient, boxes: &[&TaggedSandbox]) {
    let boxes: std::collections::HashMap<String, TaggedSandbox> = boxes
        .iter()
        .map(|b| (b.tag().to_owned(), (*b).clone()))
        .collect();
    client.sandbox_starter = Some(Arc::new(move |choice| {
        let tag = choice.runtime.clone().unwrap_or_default();
        match boxes.get(&tag) {
            Some(b) if choice.mode == "local" => Ok(SandboxMode::Enabled {
                sandbox: Arc::new(b.clone()),
            }),
            _ if choice.mode == "off" => Ok(SandboxMode::Disabled),
            _ => Err(loop_protocol::BackendError::failed(
                loop_protocol::ErrorCategory::Other,
                format!("no sandbox `{tag}`"),
            )),
        }
    }));
}

async fn set(client: &SurfaceClient, choice: SandboxChoice) -> SandboxResult {
    client
        .sandbox_service()
        .sandbox(SandboxParams {
            session: client.session_ref(),
            action: SandboxAction::Set,
            config: Some(choice),
            expected_revision: None,
        })
        .await
        .unwrap()
}

async fn until(mut done: impl FnMut() -> bool) {
    while !done() {
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

fn ran(sandbox: &TaggedSandbox) -> usize {
    sandbox
        .commands()
        .iter()
        .filter(|c| c.contains("echo parked"))
        .count()
}

async fn go(client: &SurfaceClient) -> loop_protocol::TurnHandle {
    client
        .backend()
        .prompt(&client.session_ref(), vec![ContentBlock::text("go")])
        .await
        .unwrap()
}

/// Config events as `(pending, chosen, effective)` sandbox labels, up to
/// the first one whose effective sandbox is `until`.
async fn sandbox_events(
    rx: &mut Receiver<UpdateEnvelope>,
    until: &str,
) -> Vec<(bool, Option<String>, Option<String>)> {
    let mut seen = vec![];
    loop {
        let envelope = rx.recv().await.unwrap();
        if let SessionUpdate::ConfigChanged(changed) = envelope.update {
            let step = (
                changed.pending,
                changed.config.sandbox.clone(),
                changed.effective.sandbox.clone(),
            );
            let done = step.2.as_deref() == Some(until);
            seen.push(step);
            if done {
                return seen;
            }
        }
    }
}

/// A choice made while a tool call runs in the old sandbox leaves the call
/// there, is announced as pending and then effective, and the next turn
/// runs in the new sandbox.
#[tokio::test]
async fn a_sandbox_chosen_in_a_tool_call_is_used_by_the_next_turn() {
    tokio::time::timeout(LIMIT, async {
        let dir = tempfile::tempdir().unwrap();
        let mut client = test_support::fixture(dir.path()).await;
        command_per_prompt(&client);
        let gate = Arc::new(tokio::sync::Semaphore::new(0));
        let old = TaggedSandbox::gated("old", gate.clone());
        let new = TaggedSandbox::new("new");
        use_sandboxes(&mut client, &[&old, &new]);
        let mut rx = client.backend().subscribe();
        set(&client, local("old")).await;

        let turn = go(&client).await;
        until(|| ran(&old) == 1).await;
        let result = set(&client, local("new")).await;
        assert!(result.info.contains("next turn"), "{}", result.info);
        assert!(result.seq.is_some());
        assert_eq!(old.destroy_count(), 0);
        assert_eq!(new.commands().len(), 0);

        gate.add_permits(8);
        turn.outcome().await;
        until(|| !client.backend().is_busy(&client.session_ref())).await;
        // The next turn installs the waiting sandbox before it starts.
        let next = go(&client).await;
        let seen = sandbox_events(&mut rx, "local/new").await;
        let pending = seen.iter().position(|e| e.0).expect("a pending event");
        assert_eq!(
            seen[pending],
            (true, Some("local/new".into()), Some("local/old".into()))
        );
        // The turn's end leaves the effective sandbox alone; the choice is
        // announced as effective only once it is installed.
        assert!(seen[..seen.len() - 1]
            .iter()
            .all(|e| e.2.as_deref() != Some("local/new")));
        until(|| old.destroy_count() == 1).await;

        next.outcome().await;
        assert_eq!((ran(&new), ran(&old)), (1, 1));
        client.shutdown().run().await.unwrap();
    })
    .await
    .unwrap();
}

/// A busy choice A and an idle choice B: the later one wins, and A, queued
/// earlier, never reaches a tool.
#[tokio::test]
async fn an_older_queued_choice_never_overrides_a_newer_idle_one() {
    tokio::time::timeout(LIMIT, async {
        let dir = tempfile::tempdir().unwrap();
        let mut client = test_support::fixture(dir.path()).await;
        command_per_prompt(&client);
        let gate = Arc::new(tokio::sync::Semaphore::new(0));
        let (old, a, b) = (
            TaggedSandbox::gated("old", gate.clone()),
            TaggedSandbox::new("a"),
            TaggedSandbox::new("b"),
        );
        use_sandboxes(&mut client, &[&old, &a, &b]);
        set(&client, local("old")).await;
        let turn = go(&client).await;
        until(|| ran(&old) == 1).await;
        set(&client, local("a")).await;

        gate.add_permits(1);
        turn.outcome().await;
        until(|| !client.backend().is_busy(&client.session_ref())).await;
        let idle = set(&client, local("b")).await;
        assert!(!idle.info.contains("next turn"), "{}", idle.info);
        assert_eq!(a.destroy_count(), 1, "the stale choice is released");

        go(&client).await.outcome().await;
        assert_eq!((ran(&b), ran(&a)), (1, 0));
        client.shutdown().run().await.unwrap();
    })
    .await
    .unwrap();
}

/// Quitting with a choice still waiting releases the started sandbox once.
#[tokio::test]
async fn quitting_with_a_sandbox_waiting_releases_it() {
    tokio::time::timeout(LIMIT, async {
        let dir = tempfile::tempdir().unwrap();
        let mut client = test_support::fixture(dir.path()).await;
        command_per_prompt(&client);
        let gate = Arc::new(tokio::sync::Semaphore::new(0));
        let old = TaggedSandbox::gated("old", gate.clone());
        let waiting = TaggedSandbox::new("waiting");
        use_sandboxes(&mut client, &[&old, &waiting]);
        set(&client, local("old")).await;
        let turn = go(&client).await;
        until(|| ran(&old) == 1).await;
        set(&client, local("waiting")).await;

        gate.add_permits(1);
        turn.outcome().await;
        client.shutdown().run().await.unwrap();
        assert_eq!(waiting.destroy_count(), 1);
        client.shutdown().run().await.unwrap();
        assert_eq!(waiting.destroy_count(), 1, "released once");
        assert_eq!(ran(&waiting), 0);
    })
    .await
    .unwrap();
}

/// A choice waiting for the next turn is dropped, not applied, when the
/// Surface moves to another Session.
#[tokio::test]
async fn a_waiting_sandbox_does_not_follow_the_user_to_another_session() {
    tokio::time::timeout(LIMIT, async {
        let dir = tempfile::tempdir().unwrap();
        let mut client = test_support::fixture(dir.path()).await;
        command_per_prompt(&client);
        let gate = Arc::new(tokio::sync::Semaphore::new(0));
        let old = TaggedSandbox::gated("old", gate.clone());
        let waiting = TaggedSandbox::new("waiting");
        use_sandboxes(&mut client, &[&old, &waiting]);
        set(&client, local("old")).await;
        let turn = go(&client).await;
        until(|| ran(&old) == 1).await;
        set(&client, local("waiting")).await;
        gate.add_permits(8);
        turn.outcome().await;
        until(|| !client.backend().is_busy(&client.session_ref())).await;

        client.new_session().await.unwrap();
        assert_eq!(waiting.destroy_count(), 1);
        go(&client).await.outcome().await;
        assert_eq!(ran(&waiting), 0);
        client.shutdown().run().await.unwrap();
    })
    .await
    .unwrap();
}

/// The choice is in the Session's record: reopening the Session puts it
/// back in that sandbox.
#[tokio::test]
async fn a_sandbox_choice_survives_reopening_the_session() {
    tokio::time::timeout(LIMIT, async {
        let dir = tempfile::tempdir().unwrap();
        let mut client = test_support::fixture(dir.path()).await;
        command_per_prompt(&client);
        let chosen = TaggedSandbox::new("chosen");
        use_sandboxes(&mut client, &[&chosen]);
        let session = client.session_ref();
        set(&client, local("chosen")).await;
        client.new_session().await.unwrap();
        // The other Session starts on the host; reopening the first brings
        // its sandbox back.
        client
            .host
            .inner
            .harness
            .replace_sandbox(SandboxMode::Disabled)
            .await;
        client
            .open_native_session(&session.native_id)
            .await
            .unwrap();
        go(&client).await.outcome().await;
        assert_eq!(ran(&chosen), 1);
        client.shutdown().run().await.unwrap();
    })
    .await
    .unwrap();
}
