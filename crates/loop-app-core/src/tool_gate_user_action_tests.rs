//! The user's file actions (`session/files/*`) and terminals
//! (`session/terminal/*`): judged as the Session's tool calls, with the user
//! as the approver, and one record each.
use super::*;
use crate::permissions::decision::{DecisionContext, DecisionError};
use crate::permissions::{Capability, Layer, LayerSource, Setting, State};
use loop_protocol::{Principal, PrincipalKind, Tenant};
use PermissionMode::*;

#[derive(Default)]
struct Capture {
    records: Mutex<Vec<PolicyDecisionRecord>>,
}

#[async_trait::async_trait]
impl DecisionSink for Capture {
    fn context(&self, _escalation: Option<&str>) -> DecisionContext {
        DecisionContext {
            session: Some("s1".into()),
            turn: None,
            caller: Caller::host(Principal::local_uid(501)),
            answered_by: None,
        }
    }

    async fn record(&self, record: &PolicyDecisionRecord) -> Result<(), DecisionError> {
        self.records.lock().push(record.clone());
        Ok(())
    }
}

impl Capture {
    fn take(&self) -> Vec<PolicyDecisionRecord> {
        std::mem::take(&mut *self.records.lock())
    }
}

fn gate(
    workspace: &std::path::Path,
    mode: PermissionMode,
    rules: &[(&str, &str)],
    layers: Vec<Layer>,
) -> (Arc<ToolGate>, Arc<Capture>) {
    let gate = ToolGate::new(crate::tool_approval::permissions_from_settings(
        &rules
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect(),
    ));
    gate.set_workspace(workspace.to_path_buf());
    gate.set_mode(mode);
    gate.set_policy(crate::permissions::Policy::new(layers).0);
    let capture = Arc::new(Capture::default());
    gate.set_decision_sink(Some(capture.clone()));
    (gate, capture)
}

fn edge(id: &str) -> Caller {
    let principal = Principal::new(PrincipalKind::Person, "https://idp.example", id).unwrap();
    Caller::asserted(principal, Tenant::new("acme").unwrap()).unwrap()
}

fn shell_locked_off() -> Vec<Layer> {
    vec![Layer::new(LayerSource::Managed)
        .set(Capability::Shell, Setting::always(State::Off))
        .lock(Capability::Shell)]
}

fn workspace() -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    std::fs::create_dir_all(root.join(".git")).unwrap();
    std::fs::create_dir_all(root.join("src")).unwrap();
    std::fs::write(root.join("src/a.txt"), "a").unwrap();
    std::fs::write(root.join("secret.txt"), "s").unwrap();
    #[cfg(unix)]
    std::os::unix::fs::symlink(root.join(".git"), root.join("gitlink")).unwrap();
    (dir, root)
}

/// Each mode × action × rule: the result and the record that says why.
#[tokio::test]
async fn a_file_action_is_judged_as_the_sessions_tool_call() {
    use UserFileAction::*;
    type Row = (
        PermissionMode,
        &'static [(&'static str, &'static str)],
        UserFileAction,
        &'static str,
        Option<DecidedBy>,
    );
    const NONE: &[(&str, &str)] = &[];
    const DENY_READ: &[(&str, &str)] = &[("read(*secret*)", "deny")];
    const DENY_LS: &[(&str, &str)] = &[("ls(*src*)", "deny")];
    const DENY_WRITE: &[(&str, &str)] = &[("write", "deny")];
    const ASK_WRITE: &[(&str, &str)] = &[("write(*src*)", "ask")];
    let rows: &[Row] = &[
        // Reads and lists: allowed in every mode, refused by a deny rule.
        (ReadOnly, NONE, Read, "src/a.txt", None),
        (Plan, NONE, List, "src", None),
        (Ask, NONE, Read, "src/a.txt", None),
        (FullAccess, NONE, List, "", None),
        (
            FullAccess,
            DENY_READ,
            Read,
            "secret.txt",
            Some(DecidedBy::Rule),
        ),
        (
            ReadOnly,
            DENY_READ,
            Read,
            "secret.txt",
            Some(DecidedBy::Rule),
        ),
        (FullAccess, DENY_LS, List, "src", Some(DecidedBy::Rule)),
        // Writes and deletes: the mode decides; the user approves an ask.
        (ReadOnly, NONE, Write, "src/b.txt", Some(DecidedBy::Off)),
        (Plan, NONE, Delete, "src/a.txt", Some(DecidedBy::Off)),
        (Ask, NONE, Write, "src/b.txt", None),
        (Ask, NONE, Delete, "src/a.txt", None),
        (AcceptEdits, NONE, Write, "src/b.txt", None),
        (FullAccess, NONE, Delete, "src/a.txt", None),
        (
            FullAccess,
            DENY_WRITE,
            Write,
            "src/b.txt",
            Some(DecidedBy::Rule),
        ),
        (
            FullAccess,
            DENY_WRITE,
            Delete,
            "src/a.txt",
            Some(DecidedBy::Rule),
        ),
        (FullAccess, ASK_WRITE, Write, "src/b.txt", None),
        // `.env*` asks in accept-edits: the user who clicks approves it.
        (AcceptEdits, NONE, Write, ".env", None),
        // `.git` and `.loop`: refused for every write, in every mode, also
        // by another case and through a link.
        (
            FullAccess,
            NONE,
            Write,
            ".git/config",
            Some(DecidedBy::Protected),
        ),
        (FullAccess, NONE, Delete, ".git", Some(DecidedBy::Protected)),
        (
            FullAccess,
            NONE,
            Write,
            ".GIT/hooks/x",
            Some(DecidedBy::Protected),
        ),
        (
            Ask,
            NONE,
            Write,
            ".loop/settings.json",
            Some(DecidedBy::Protected),
        ),
        (
            AcceptEdits,
            NONE,
            Delete,
            "src/.Loop",
            Some(DecidedBy::Protected),
        ),
        // Reading them is a read.
        (FullAccess, NONE, Read, ".git/config", None),
    ];
    let (_dir, root) = workspace();
    for (mode, rules, action, path, refused_by) in rows {
        let (gate, capture) = gate(&root, *mode, rules, vec![]);
        let got = gate
            .check_user_file_action(*action, &root.join(path), Some("s7".into()), None)
            .await;
        let row = format!("{mode:?} {rules:?} {action:?} `{path}`");
        let records = capture.take();
        assert_eq!(records.len(), 1, "{row}: one record");
        let record = &records[0];
        assert_eq!(record.source.to_string(), USER_FILES, "{row}");
        assert_eq!(record.session.as_deref(), Some("s7"), "{row}");
        assert!(
            record
                .subject
                .path
                .as_deref()
                .is_some_and(|p| p.ends_with(path.trim_end_matches('/'))),
            "{row}: {:?}",
            record.subject
        );
        match refused_by {
            Some(by) => {
                assert!(got.is_err(), "{row}: refused, got {got:?}");
                assert_eq!(record.outcome, DecisionOutcome::Deny, "{row}");
                assert_eq!(record.by, *by, "{row}");
            }
            None => {
                assert!(got.is_ok(), "{row}: allowed, got {got:?}");
                assert_eq!(record.outcome, DecisionOutcome::Allow, "{row}");
            }
        }
    }
}

/// A link to `.git` is `.git`: the resolved path counts.
#[cfg(unix)]
#[tokio::test]
async fn a_write_through_a_link_into_git_is_refused() {
    let (_dir, root) = workspace();
    let (gate, capture) = gate(&root, FullAccess, &[], vec![]);
    let refused = gate
        .check_user_file_action(
            UserFileAction::Write,
            &root.join("gitlink/config"),
            None,
            None,
        )
        .await
        .unwrap_err();
    assert!(refused.0.contains(".git"), "{refused}");
    assert_eq!(capture.take()[0].by, DecidedBy::Protected);
}

/// The user who clicks approves an ask; a caller from outside the host
/// cannot, so its ask is refused. The record names the caller.
#[tokio::test]
async fn only_the_host_user_approves_a_file_action_that_would_ask() {
    let (_dir, root) = workspace();
    let (gate, capture) = gate(&root, Ask, &[], vec![]);
    let host = Caller::host(Principal::local_uid(501));
    gate.check_user_file_action(UserFileAction::Write, &root.join("x"), None, Some(host))
        .await
        .unwrap();
    assert_eq!(capture.take()[0].by, DecidedBy::User);
    let refused = gate
        .check_user_file_action(
            UserFileAction::Write,
            &root.join("x"),
            None,
            Some(edge("alice")),
        )
        .await
        .unwrap_err();
    assert!(refused.0.contains("outside this host"), "{refused}");
    let record = capture.take().remove(0);
    assert_eq!(record.by, DecidedBy::NoApprover);
    assert_eq!(record.tenant.as_str(), "acme");
    // A read the mode allows needs no approver.
    gate.check_user_file_action(
        UserFileAction::Read,
        &root.join("src/a.txt"),
        None,
        Some(edge("alice")),
    )
    .await
    .unwrap();
}

/// Under the tool lock a write is off (the Runtime refuses it before the
/// gate too), and a read stays inside the Session's folder.
#[tokio::test]
async fn the_tool_lock_turns_file_writes_off() {
    let (_dir, root) = workspace();
    let (gate, capture) = gate(&root, FullAccess, &[], vec![]);
    gate.lock_tools(crate::permissions::tool_lock::ToolLock::Chat);
    assert!(gate
        .check_user_file_action(UserFileAction::Write, &root.join("src/b"), None, None)
        .await
        .is_err());
    assert_eq!(capture.take()[0].by, DecidedBy::Off);
}

/// Each mode × rule × Lock for a terminal.
#[tokio::test]
async fn a_terminal_needs_the_strongest_shell_right() {
    type Row = (
        PermissionMode,
        &'static [(&'static str, &'static str)],
        bool,
        Result<DecidedBy, DecidedBy>,
    );
    const NONE: &[(&str, &str)] = &[];
    const DENY: &[(&str, &str)] = &[("bash", "deny")];
    const PATTERN: &[(&str, &str)] = &[("bash(rm *)", "deny")];
    const ALLOW_ALL: &[(&str, &str)] = &[("bash(*)", "allow")];
    const READ_DENY: &[(&str, &str)] = &[("read(*secret*)", "deny")];
    let rows: &[Row] = &[
        (ReadOnly, NONE, false, Err(DecidedBy::Off)),
        (Plan, NONE, false, Err(DecidedBy::Off)),
        (Ask, NONE, false, Ok(DecidedBy::User)),
        (AcceptEdits, NONE, false, Ok(DecidedBy::User)),
        (FullAccess, NONE, false, Ok(DecidedBy::Preset)),
        (FullAccess, DENY, false, Err(DecidedBy::Rule)),
        (Ask, DENY, false, Err(DecidedBy::Rule)),
        // Any deny pattern: a terminal cannot be checked against it.
        (FullAccess, PATTERN, false, Err(DecidedBy::Unreadable)),
        (Ask, PATTERN, false, Err(DecidedBy::Unreadable)),
        // An allow pattern never matches the terminal: still the user's ask.
        (Ask, ALLOW_ALL, false, Ok(DecidedBy::User)),
        // Other tools' rules do not matter.
        (FullAccess, READ_DENY, false, Ok(DecidedBy::Preset)),
        // A Lock that turns the shell off.
        (FullAccess, NONE, true, Err(DecidedBy::Off)),
        (Ask, NONE, true, Err(DecidedBy::Off)),
    ];
    let (_dir, root) = workspace();
    for (mode, rules, lock, expected) in rows {
        let layers = if *lock { shell_locked_off() } else { vec![] };
        let (gate, capture) = gate(&root, *mode, rules, layers);
        let got = gate
            .check_user_terminal("term-1", Some("s7".into()), None)
            .await;
        let row = format!("{mode:?} {rules:?} lock={lock}");
        let records = capture.take();
        assert_eq!(records.len(), 1, "{row}: one record");
        let record = &records[0];
        assert_eq!(record.source.to_string(), USER_TERMINAL, "{row}");
        assert_eq!(record.call_id.as_deref(), Some("term-1"), "{row}");
        assert_eq!(record.capability, "shell", "{row}");
        match expected {
            Ok(by) => {
                assert!(got.is_ok(), "{row}: {got:?}");
                assert_eq!(record.by, *by, "{row}");
                assert!(gate.user_terminal_allowed().await.is_ok(), "{row}");
            }
            Err(by) => {
                assert!(got.is_err(), "{row}: {got:?}");
                assert_eq!(record.outcome, DecisionOutcome::Deny, "{row}");
                assert_eq!(record.by, *by, "{row}");
                assert!(gate.user_terminal_allowed().await.is_err(), "{row}");
            }
        }
        // The re-check writes no record.
        assert!(capture.take().is_empty(), "{row}");
    }
}

#[tokio::test]
async fn a_terminal_from_outside_the_host_cannot_be_approved_and_its_end_is_recorded() {
    let (_dir, root) = workspace();
    let (gate, capture) = gate(&root, Ask, &[], vec![]);
    let refused = gate
        .check_user_terminal("term-2", None, Some(edge("alice")))
        .await
        .unwrap_err();
    assert!(refused.0.contains("outside this host"), "{refused}");
    assert_eq!(capture.take()[0].by, DecidedBy::NoApprover);
    gate.record_user_terminal_end(
        "term-2",
        "closed by the user: exited with code 0",
        None,
        None,
    )
    .await
    .unwrap();
    let record = capture.take().remove(0);
    assert_eq!(record.call_id.as_deref(), Some("term-2"));
    assert_eq!(record.by, DecidedBy::User);
    assert_eq!(record.outcome, DecisionOutcome::Allow);
    assert!(record
        .reason
        .as_deref()
        .is_some_and(|r| r.contains("exited with code 0")));
}
