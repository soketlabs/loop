//! Adapter for the S3.3 Session services while the TUI owns one native harness.
//! Payloads and destinations use the existing S3.3 types and atomic store copy.
use async_trait::async_trait;
use loop_agent::harness::session::{
    fork_points_from_branch, list_session_summaries, read_session_entries_for_fork,
    session_tree_rows, SessionFile, SessionForkSelection, SessionStore, SessionTreeEntry,
};
use loop_agent::native_backend::NativeBackend;
use loop_protocol::rpc::{
    CloneParams, DestinationResult, ExportResult, ForkParams, ForkPoint, ImportParams,
    MutationResult, NavigateParams, NavigateResult, RenameParams, SessionInfo, SessionListSummary,
    SessionParams, SessionSnapshot, TreeEntry, TreeParams, TreeResult, TtfbResult,
    SESSION_RESPONSE_LIMIT, SESSION_TRANSFER_LIMIT,
};
use loop_protocol::{
    AgentBackend, BackendError, BackendInstanceId, BackendResult, Capabilities, Compaction,
    ContentBlock, ErrorCategory, LocalShell, ModelChoices, ModelControl, SessionRef, SessionSpec,
    SessionStats, SessionUpdate, TurnHandle, UpdateEnvelope,
};
use loop_wire::control::Attachment;
use loop_wire::RuntimeControl;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::Mutex;

pub(super) struct NativeSessions {
    native: Arc<NativeBackend>,
    store: Option<Arc<dyn SessionStore>>,
    cwd: PathBuf,
    pub workspace_id: String,
    /// Serializes capture with prompt admission and native branch mutations.
    operation: Mutex<()>,
    /// The Session and branch head a running turn started from. Only the
    /// head is recorded at admission; the branch is read when a copy,
    /// export or tree needs it during the turn.
    committed: Mutex<Option<(String, Option<String>)>>,
    receipts: Mutex<HashMap<String, (String, DestinationResult)>>,
}
fn failed(error: impl std::fmt::Display) -> BackendError {
    BackendError::failed(ErrorCategory::Protocol, error.to_string())
}
fn fingerprint(params: &impl serde::Serialize) -> BackendResult<String> {
    use sha2::{Digest, Sha256};
    Ok(format!(
        "{:x}",
        Sha256::digest(serde_json::to_vec(params).map_err(failed)?)
    ))
}
fn request_key(kind: &str, request_id: &str) -> BackendResult<String> {
    if request_id.trim().is_empty() || request_id.len() > 256 {
        return Err(failed("request id must contain 1 to 256 bytes"));
    }
    Ok(format!("surface-{kind}:{request_id}"))
}
impl NativeSessions {
    pub fn new(
        native: Arc<NativeBackend>,
        store: Option<Arc<dyn SessionStore>>,
        cwd: PathBuf,
    ) -> Self {
        let workspace_id =
            loop_runtime::workspace_id(&cwd.canonicalize().unwrap_or_else(|_| cwd.clone()));
        Self {
            native,
            store,
            cwd,
            workspace_id,
            operation: Mutex::new(()),
            committed: Mutex::new(None),
            receipts: Mutex::new(HashMap::new()),
        }
    }
    /// Delete a copy this client created and never used. Refuses any other
    /// Session, so it cannot remove history.
    pub async fn discard_destination(&self, session: &SessionRef) -> BackendResult<()> {
        let _operation = self.operation.lock().await;
        let mut receipts = self.receipts.lock().await;
        if !receipts
            .values()
            .any(|(_, result)| result.session == *session)
        {
            return Err(failed(format!(
                "{} is not an unused copy made here",
                session.native_id
            )));
        }
        if self.native.current_session().await == *session {
            return Err(BackendError::Busy);
        }
        self.store()?
            .delete(&session.native_id)
            .await
            .map_err(failed)?;
        receipts.retain(|_, (_, result)| result.session != *session);
        Ok(())
    }
    async fn retry(
        &self,
        key: &str,
        fingerprint: &str,
    ) -> BackendResult<Option<DestinationResult>> {
        self.receipts
            .lock()
            .await
            .get(key)
            .map(|(previous, result)| {
                if previous != fingerprint {
                    Err(failed("request id already used with different parameters"))
                } else {
                    Ok(result.clone())
                }
            })
            .transpose()
    }
    fn store(&self) -> BackendResult<&Arc<dyn SessionStore>> {
        self.store
            .as_ref()
            .ok_or_else(|| failed("Session store unavailable"))
    }
    fn reference(&self, id: &str) -> SessionRef {
        SessionRef {
            instance: self.native.instance().clone(),
            key: id.into(),
            native_id: id.into(),
        }
    }
    async fn check(&self, session: &SessionRef) -> BackendResult<()> {
        if self.native.current_session().await != *session {
            return Err(BackendError::SessionNotFound(session.key.clone()));
        }
        Ok(())
    }
    async fn capture(&self, session: &SessionRef) -> BackendResult<SessionFile> {
        self.check(session).await?;
        let reader = self
            .store()?
            .load(&session.native_id)
            .await
            .map_err(failed)?;
        let mut file = SessionFile::from_reader(reader.as_ref())
            .await
            .map_err(failed)?;
        if self.native.is_busy(session) {
            // The running turn only appends below the head it started from.
            let head = self
                .committed
                .lock()
                .await
                .as_ref()
                .filter(|(id, _)| *id == session.native_id)
                .map(|(_, head)| head.clone())
                .ok_or_else(|| failed("committed Session snapshot unavailable"))?;
            let keep = match head {
                Some(head) => {
                    file.entries
                        .iter()
                        .position(|entry| entry.id() == head)
                        .ok_or_else(|| failed("committed Session head not on the branch"))?
                        + 1
                }
                None => 0,
            };
            file.entries.truncate(keep);
        }
        Ok(file)
    }
    async fn destination(
        &self,
        mut file: SessionFile,
        parent: Option<String>,
        key: String,
        fingerprint: String,
        draft: Option<String>,
    ) -> BackendResult<DestinationResult> {
        if let Some(result) = self.retry(&key, &fingerprint).await? {
            return Ok(result);
        }
        // The Runtime and this native adapter share the transactional copy helper:
        // fresh entry ids, parent metadata, a separate lease and no copied grants.
        file.cwd = Some(self.cwd.display().to_string());
        let receipt =
            serde_json::json!({"workspace_id":self.workspace_id,"draft":draft}).to_string();
        let id = self
            .store()?
            .create_surface_destination(
                file,
                self.cwd.display().to_string(),
                parent,
                key.clone(),
                fingerprint.clone(),
                receipt,
            )
            .await
            .map_err(failed)?;
        let result = DestinationResult {
            session: self.reference(&id),
            workspace_id: self.workspace_id.clone(),
            source_branch_revision: None,
            draft,
        };
        self.receipts
            .lock()
            .await
            .insert(key, (fingerprint, result.clone()));
        Ok(result)
    }
    async fn copy(
        &self,
        session: SessionRef,
        name: Option<String>,
        before: Option<String>,
        key: String,
        fingerprint: String,
    ) -> BackendResult<DestinationResult> {
        if let Some(result) = self.retry(&key, &fingerprint).await? {
            return Ok(result);
        }
        let mut file = self.capture(&session).await?;
        let draft = if let Some(before) = before {
            let point = fork_points_from_branch(&file.entries)
                .into_iter()
                .find(|p| p.entry_id == before)
                .ok_or_else(|| {
                    failed("fork point is not a user message on the committed branch")
                })?;
            file.entries = read_session_entries_for_fork(
                &file.entries,
                file.entries.last().map(SessionTreeEntry::id),
                SessionForkSelection::BeforeEntry,
                Some(&before),
            )
            .map_err(failed)?;
            Some(point.text)
        } else {
            None
        };
        file.name = name.or(file.name);
        self.destination(file, Some(session.native_id), key, fingerprint, draft)
            .await
    }
}

#[async_trait]
impl RuntimeControl for NativeSessions {
    async fn config(&self, p: SessionParams) -> BackendResult<SessionSnapshot> {
        self.check(&p.session).await?;
        let changed = self.native.config();
        Ok(SessionSnapshot {
            notices: vec![],
            session: p.session,
            workspace_id: self.workspace_id.clone(),
            at_seq: 0,
            revision: changed.revision,
            branch_revision: 0,
            view_id: None,
            metadata: changed.metadata,
            config: changed.config,
            effective: changed.effective,
            pending: changed.pending,
            capabilities: self.capabilities(),
            turn_state: None,
            stats: serde_json::Value::Null,
            workspace_revision: 0,
        })
    }

    async fn skill(
        &self,
        p: loop_protocol::session_services::SkillParams,
    ) -> BackendResult<MutationResult> {
        let _operation = self.operation.lock().await;
        self.native.skill(p).await
    }
    #[cfg(feature = "orchestration")]
    async fn workflow(
        &self,
        p: loop_protocol::session_services::WorkflowParams,
    ) -> BackendResult<loop_protocol::session_services::WorkflowResult> {
        let _operation = self.operation.lock().await;
        self.native.workflow(p).await
    }

    async fn list(&self) -> BackendResult<Vec<SessionInfo>> {
        let mut summaries: HashMap<_, _> =
            list_session_summaries(self.store()?.as_ref(), Some(&self.cwd.to_string_lossy()))
                .await
                .map_err(failed)?
                .into_iter()
                .map(|summary| (summary.id.clone(), summary))
                .collect();
        let current = self.native.current_session().await;
        let mut infos = Vec::new();
        for meta in self
            .store()?
            .list(Some(&self.cwd.to_string_lossy()))
            .await
            .map_err(failed)?
        {
            let summary = summaries.remove(&meta.id);
            let live = current.native_id == meta.id;
            infos.push(SessionInfo {
                r#ref: self.reference(&meta.id),
                cwd: self.cwd.clone(),
                name: summary
                    .as_ref()
                    .and_then(|s| s.name.clone())
                    .or(meta.name.clone()),
                parent: meta.parent_session_id.as_ref().map(|id| self.reference(id)),
                live,
                busy: live && self.native.is_busy(&current),
                created_ms: meta.created_at as u64,
                last_active_ms: summary.as_ref().map_or(meta.created_at, |s| s.updated_at) as u64,
                summary: SessionListSummary {
                    workspace_id: Some(self.workspace_id.clone()),
                    backend: Some("native".into()),
                    first_prompt: summary.as_ref().and_then(|s| s.title.clone()),
                    message_count: Some(summary.as_ref().map_or(0, |s| s.message_count as u64)),
                    selected_model: summary.and_then(|s| s.model),
                    fork_source: meta.parent_session_id.as_ref().map(|id| self.reference(id)),
                    ..Default::default()
                },
            });
        }
        Ok(infos)
    }
    async fn tree(&self, params: TreeParams) -> BackendResult<TreeResult> {
        let _operation = self.operation.lock().await;
        let file = self.capture(&params.session).await?;
        let entries = if self.native.is_busy(&params.session) {
            file.entries.clone()
        } else {
            self.store()?
                .load(&params.session.native_id)
                .await
                .map_err(failed)?
                .read_entries(None)
                .await
                .map_err(failed)?
        };
        let rows = session_tree_rows(&entries, file.entries.last().map(SessionTreeEntry::id));
        use std::hash::{Hash, Hasher};
        let mut signature = std::collections::hash_map::DefaultHasher::new();
        for row in &rows {
            (&row.entry_id, row.on_branch, row.current).hash(&mut signature);
        }
        let rows_id = format!("{:x}", signature.finish());
        let offset: usize = match params.cursor.as_deref() {
            None => 0,
            Some(cursor) => {
                let (id, offset) = cursor
                    .split_once(':')
                    .ok_or_else(|| failed("invalid tree cursor"))?;
                if id != rows_id {
                    return Err(failed("tree changed; reload the first page"));
                }
                offset.parse().map_err(failed)?
            }
        };
        let limit = params.limit.unwrap_or(100);
        if limit == 0 || limit > 1000 || offset > rows.len() {
            return Err(failed("invalid tree cursor or limit"));
        }
        let end = (offset + limit as usize).min(rows.len());
        let page = &rows[offset..end];
        let points = fork_points_from_branch(&file.entries)
            .into_iter()
            .filter(|point| page.iter().any(|row| row.entry_id == point.entry_id))
            .map(|point| ForkPoint {
                entry_id: point.entry_id,
                index: point.index,
                text: point.text,
                preview: point.preview,
            })
            .collect();
        let meta = self
            .store()?
            .load(&params.session.native_id)
            .await
            .map_err(failed)?
            .metadata()
            .clone();
        let mut family = self
            .store()?
            .list(Some(&self.cwd.to_string_lossy()))
            .await
            .map_err(failed)?
            .into_iter()
            .filter(|m| {
                m.parent_session_id.as_deref() == Some(&params.session.native_id)
                    || meta.parent_session_id.as_deref() == Some(&m.id)
            })
            .collect::<Vec<_>>();
        family.sort_by_key(|m| {
            (
                meta.parent_session_id.as_deref() != Some(&m.id),
                std::cmp::Reverse(m.created_at),
            )
        });
        if family.len() > 1000 {
            return Err(failed("Session family exceeds the page limit"));
        }
        let family = family
            .into_iter()
            .map(|meta| self.reference(&meta.id))
            .collect();
        Ok(TreeResult {
            entries: page
                .iter()
                .map(|row| TreeEntry {
                    entry_id: row.entry_id.clone(),
                    parent_id: row.parent_id.clone(),
                    depth: row.depth,
                    kind: format!("{:?}", row.kind).to_lowercase(),
                    preview: row.preview.clone(),
                    text: row.text.clone(),
                    on_branch: row.on_branch,
                    current: row.current,
                })
                .collect(),
            fork_points: points,
            family,
            branch_revision: 0,
            next_cursor: (end < rows.len()).then(|| format!("{rows_id}:{end}")),
        })
    }
    async fn navigate(&self, params: NavigateParams) -> BackendResult<NavigateResult> {
        let _operation = self.operation.lock().await;
        self.check(&params.session).await?;
        if self.native.is_busy(&params.session) {
            return Err(BackendError::Busy);
        }
        self.native
            .check_session_revision(params.expected_revision)?;
        self.native
            .harness()
            .navigate_tree_to(params.target_entry_id.as_deref(), params.summarize)
            .await
            .map_err(failed)?;
        self.native.restore_config().await?;
        let revision = self.native.config().revision;
        Ok(NavigateResult {
            revision,
            branch_revision: 0,
            seq: 0,
        })
    }
    async fn rename(&self, params: RenameParams) -> BackendResult<MutationResult> {
        let _operation = self.operation.lock().await;
        self.check(&params.session).await?;
        let name = params.name.trim();
        if name.is_empty() || name.len() > 4096 {
            return Err(failed("Session name must contain 1 to 4096 bytes"));
        }
        self.native
            .check_session_revision(params.expected_revision)?;
        self.store()?
            .append_entry(
                &params.session.native_id,
                loop_agent::harness::session::PendingSessionWrite::SessionInfo {
                    name: name.into(),
                },
            )
            .await
            .map_err(failed)?;
        let revision = self.native.record_change(Some(Some(name.into())));
        Ok(MutationResult {
            revision,
            seq: 0,
            pending: false,
            applies_after_turn: None,
        })
    }
    async fn fork(&self, params: ForkParams) -> BackendResult<DestinationResult> {
        let _operation = self.operation.lock().await;
        let fingerprint = fingerprint(&params)?;
        self.copy(
            params.session,
            params.name,
            Some(params.before_entry_id),
            request_key("copy", &params.request_id)?,
            fingerprint,
        )
        .await
    }
    async fn clone_session(&self, params: CloneParams) -> BackendResult<DestinationResult> {
        let _operation = self.operation.lock().await;
        let fingerprint = fingerprint(&params)?;
        self.copy(
            params.session,
            params.name,
            None,
            request_key("copy", &params.request_id)?,
            fingerprint,
        )
        .await
    }
    async fn export(&self, params: SessionParams) -> BackendResult<ExportResult> {
        let _operation = self.operation.lock().await;
        let file = self.capture(&params.session).await?;
        let result = ExportResult {
            format_version: 1,
            markdown: file.markdown(),
            jsonl: file
                .to_jsonl_bounded(SESSION_TRANSFER_LIMIT)
                .map_err(failed)?,
            branch_revision: 0,
        };
        struct SizeLimit(usize);
        impl std::io::Write for SizeLimit {
            fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
                self.0 = self.0.checked_sub(bytes.len()).ok_or_else(|| {
                    std::io::Error::other("encoded Session export exceeds 16 MiB")
                })?;
                Ok(bytes.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        serde_json::to_writer(SizeLimit(SESSION_RESPONSE_LIMIT), &result).map_err(failed)?;
        Ok(result)
    }

    async fn import(&self, params: ImportParams) -> BackendResult<DestinationResult> {
        let _operation = self.operation.lock().await;
        if params.workspace_id != self.workspace_id {
            return Err(failed("unknown workspace id"));
        }
        if params.jsonl.len() > SESSION_TRANSFER_LIMIT {
            return Err(failed("Session transfer exceeds 8 MiB"));
        }
        let file = SessionFile::from_jsonl(&params.jsonl).map_err(failed)?;
        let mut ids = std::collections::HashSet::new();
        let mut last = None;
        for entry in &file.entries {
            if entry.id().is_empty()
                || !ids.insert(entry.id())
                || entry.parent_id() != last
                || matches!(entry, SessionTreeEntry::Leaf { .. })
            {
                return Err(failed("invalid Session entry ids or parent links"));
            }
            if let SessionTreeEntry::Compaction {
                first_kept_entry_id: Some(id),
                ..
            } = entry
            {
                if !ids.contains(id.as_str()) {
                    return Err(failed("invalid compaction reference"));
                }
            }
            last = Some(entry.id());
        }
        let fingerprint = fingerprint(&params)?;
        self.destination(
            file,
            None,
            request_key("import", &params.request_id)?,
            fingerprint,
            None,
        )
        .await
    }
    /// Closing never deletes: a Session the TUI is not on holds nothing
    /// open here, and the one it is on stays (as on a Runtime, history is
    /// kept). Use [`NativeSessions::discard_destination`] for an unused copy.
    async fn close(&self, session: &SessionRef, _force: bool) -> BackendResult<()> {
        let _operation = self.operation.lock().await;
        if session.instance != *self.native.instance() {
            return Err(BackendError::SessionNotFound(session.key.clone()));
        }
        if self.native.current_session().await == *session {
            return Err(BackendError::Busy);
        }
        Ok(())
    }
    async fn attach(&self, _session: &SessionRef, _after_seq: u64) -> BackendResult<Attachment> {
        Err(BackendError::Unsupported(
            "native Event Log attachment".into(),
        ))
    }
}

// Prompt admission captures the branch before the model starts. Session copies
// and export can then read the committed source without its unfinished turn.
#[async_trait]
impl AgentBackend for NativeSessions {
    fn instance(&self) -> &BackendInstanceId {
        self.native.instance()
    }
    fn capabilities(&self) -> Capabilities {
        Capabilities {
            session_config: true,
            session_tree: true,
            session_rename: true,
            session_fork: true,
            session_transfer: true,
            ..self.native.capabilities()
        }
    }
    fn subscribe(&self) -> tokio::sync::broadcast::Receiver<UpdateEnvelope> {
        self.native.subscribe()
    }
    async fn new_session(&self, spec: SessionSpec) -> BackendResult<SessionRef> {
        let _operation = self.operation.lock().await;
        if self.native.is_busy(&self.native.current_session().await) {
            return Err(BackendError::Busy);
        }
        self.native.new_session(spec).await
    }
    async fn load_session(
        &self,
        session: &SessionRef,
        spec: SessionSpec,
    ) -> BackendResult<SessionRef> {
        let _operation = self.operation.lock().await;
        if self.native.is_busy(&self.native.current_session().await) {
            return Err(BackendError::Busy);
        }
        self.native.load_session(session, spec).await
    }
    async fn transcript(&self, session: &SessionRef) -> BackendResult<Vec<SessionUpdate>> {
        self.native.transcript(session).await
    }
    async fn prompt(
        &self,
        session: &SessionRef,
        input: Vec<ContentBlock>,
    ) -> BackendResult<TurnHandle> {
        let _operation = self.operation.lock().await;
        // One small read; a failure only affects copies during this turn.
        let head = self.native.harness().session_head().await.ok().flatten();
        *self.committed.lock().await = Some((session.native_id.clone(), head));
        self.native.prompt(session, input).await
    }
    fn cancel(&self, session: &SessionRef) {
        self.native.cancel(session);
    }
    fn is_busy(&self, session: &SessionRef) -> bool {
        self.native.is_busy(session)
    }
    async fn shutdown(&self) {
        self.native.shutdown().await;
    }
    fn compaction(&self) -> Option<&dyn Compaction> {
        self.native.compaction()
    }
    fn session_stats(&self) -> Option<&dyn SessionStats> {
        self.native.session_stats()
    }
    fn local_shell(&self) -> Option<&dyn LocalShell> {
        self.native.local_shell()
    }
    fn model_control(&self) -> Option<&dyn ModelControl> {
        Some(self)
    }
}

#[async_trait]
impl ModelControl for NativeSessions {
    async fn models(&self, session: &SessionRef) -> BackendResult<ModelChoices> {
        self.native.models(session).await
    }
    async fn set_model(
        &self,
        session: &SessionRef,
        model: &str,
        expected: Option<u64>,
    ) -> BackendResult<MutationResult> {
        let _operation = self.operation.lock().await;
        self.native.set_model(session, model, expected).await
    }
    async fn efforts(&self, session: &SessionRef) -> BackendResult<ModelChoices> {
        self.native.efforts(session).await
    }
    async fn set_effort(
        &self,
        session: &SessionRef,
        effort: &str,
        expected: Option<u64>,
    ) -> BackendResult<MutationResult> {
        let _operation = self.operation.lock().await;
        self.native.set_effort(session, effort, expected).await
    }
    async fn modes(&self, session: &SessionRef) -> BackendResult<ModelChoices> {
        self.native.modes(session).await
    }
    async fn set_mode(
        &self,
        session: &SessionRef,
        mode: &str,
        expected: Option<u64>,
    ) -> BackendResult<MutationResult> {
        let _operation = self.operation.lock().await;
        self.native.set_mode(session, mode, expected).await
    }
    async fn ttfb(
        &self,
        session: &SessionRef,
        change: Option<Option<u64>>,
        expected: Option<u64>,
    ) -> BackendResult<TtfbResult> {
        let _operation = self.operation.lock().await;
        self.native.ttfb(session, change, expected).await
    }
}
