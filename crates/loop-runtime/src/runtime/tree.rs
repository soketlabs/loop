//! Session-tree services. Destination creation never replaces a source harness.
use super::*;
use loop_agent::harness::session::{
    fork_points_from_branch, read_session_entries_for_fork, session_tree_rows, PendingSessionWrite,
    SessionFile, SessionForkSelection, SessionTreeEntry,
};
use loop_protocol::rpc::*;

pub(super) struct BranchView {
    session: SessionRef,
    updates: Vec<SessionUpdate>,
    branch_revision: u64,
    at_seq: u64,
    expires: std::time::Instant,
}
/// Most unexpired views a Runtime holds for Surfaces.
pub(super) const VIEW_LIMIT: usize = 128;
const VIEW_TTL: Duration = Duration::from_secs(300);

/// A reserved view id. Dropped before it is filled, it frees its slot.
pub(super) struct ViewSlot<'a> {
    views: &'a Mutex<HashMap<String, BranchView>>,
    session: SessionRef,
    id: String,
    filled: bool,
}
impl ViewSlot<'_> {
    fn fill(mut self, updates: Vec<SessionUpdate>, branch_revision: u64, at_seq: u64) -> String {
        self.views.lock().unwrap().insert(
            self.id.clone(),
            BranchView {
                session: self.session.clone(),
                updates,
                branch_revision,
                at_seq,
                expires: std::time::Instant::now() + VIEW_TTL,
            },
        );
        self.filled = true;
        self.id.clone()
    }
}
impl Drop for ViewSlot<'_> {
    fn drop(&mut self) {
        if !self.filled {
            self.views.lock().unwrap().remove(&self.id);
        }
    }
}

fn page(cursor: Option<&str>, limit: Option<u32>, length: usize) -> BackendResult<(usize, usize)> {
    let start = cursor
        .map(str::parse::<usize>)
        .transpose()
        .map_err(failed)?
        .unwrap_or(0);
    let limit = limit.unwrap_or(100);
    if limit == 0 || limit > 1000 || start > length {
        return Err(failed("invalid page cursor or limit (use 1 to 1000)"));
    }
    Ok((start, start.saturating_add(limit as usize).min(length)))
}
pub(super) fn workspace_id(path: &std::path::Path) -> String {
    // Opaque, stable host-assigned identity; never interpreted as a path.
    use sha2::{Digest, Sha256};
    format!(
        "workspace-{:x}",
        Sha256::digest(path.as_os_str().as_encoded_bytes())
    )
}
fn fingerprint(params: &impl serde::Serialize) -> BackendResult<String> {
    use sha2::{Digest, Sha256};
    Ok(format!(
        "{:x}",
        Sha256::digest(serde_json::to_vec(params).map_err(failed)?)
    ))
}
fn destination_result(id: &str, receipt: &str) -> BackendResult<DestinationResult> {
    let receipt: serde_json::Value = serde_json::from_str(receipt).map_err(failed)?;
    Ok(DestinationResult {
        session: reference(id),
        workspace_id: receipt["workspace_id"].as_str().unwrap_or_default().into(),
        source_branch_revision: receipt["source_branch_revision"].as_u64(),
        draft: receipt["draft"].as_str().map(str::to_owned),
    })
}
/// `future` with the current request's caller ([`loop_protocol::Caller`])
/// in scope, for a task spawned on its behalf: the task-local caller is lost
/// across `tokio::spawn`.
fn in_caller_scope<F: std::future::Future>(
    future: F,
) -> impl std::future::Future<Output = F::Output> {
    let caller = loop_protocol::Caller::current();
    async move {
        match caller {
            Some(caller) => caller.scope(future).await,
            None => future.await,
        }
    }
}
/// A destination created but not yet admitted. Dropped early, it rolls back
/// before releasing the destination lock, so no retry reads its receipt first.
struct Unadmitted {
    created: Option<loop_agent::harness::session::DestinationCreation>,
    lock: Option<tokio::sync::OwnedMutexGuard<()>>,
}
impl Drop for Unadmitted {
    fn drop(&mut self) {
        if let (Some(mut created), Some(lock)) = (self.created.take(), self.lock.take()) {
            tokio::spawn(async move {
                if let Err(error) = created.rollback().await {
                    eprintln!(
                        "could not remove unadmitted Session {}: {error}",
                        created.id
                    );
                }
                drop(lock);
            });
        }
    }
}
fn validate_file(file: &SessionFile) -> BackendResult<()> {
    let mut ids = HashSet::new();
    let mut last = None;
    for entry in &file.entries {
        if entry.id().is_empty() || !ids.insert(entry.id()) || entry.parent_id() != last {
            return Err(failed("invalid Session entry ids or parent links"));
        }
        if matches!(entry, SessionTreeEntry::Leaf { .. }) {
            return Err(failed("a transfer must contain a branch, not leaf moves"));
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
    Ok(())
}
impl Runtime {
    pub(super) fn check_tree_live(&self, live: &LiveSession) -> BackendResult<()> {
        if self.stopped.load(Ordering::Acquire) || live.closing.load(Ordering::Acquire) {
            return Err(failed("Session closing"));
        }
        if let Some(error) = live.failure.lock().unwrap().clone() {
            return Err(failed(error));
        }
        Ok(())
    }
    /// Capture settled turn output, reading `busy` once. Caller holds the
    /// lifecycle lock, which also holds turn settlement, so `busy` stays as read.
    async fn capture(&self, live: &LiveSession) -> BackendResult<(SessionFile, bool)> {
        self.check_tree_live(live)?;
        let busy = live.busy.load(Ordering::Acquire);
        let mut file = if busy {
            live.committed
                .lock()
                .await
                .clone()
                .ok_or_else(|| failed("Session snapshot unavailable"))?
        } else {
            barrier(live).await?;
            live.opened.harness.export_session().await.map_err(failed)?
        };
        file.name = self
            .store
            .load(&live.reference.native_id)
            .await
            .map_err(failed)?
            .metadata()
            .name
            .clone();
        Ok((file, busy))
    }
    /// Settled turn output only. Caller holds the lifecycle lock.
    async fn committed_file(&self, live: &LiveSession) -> BackendResult<SessionFile> {
        Ok(self.capture(live).await?.0)
    }

    /// The transcript of a captured `file` through high-water mark `at_seq`.
    async fn transcript(
        &self,
        live: &LiveSession,
        file: &SessionFile,
        busy: bool,
        at_seq: u64,
    ) -> BackendResult<Vec<SessionUpdate>> {
        self.pauses.wait("view_capture").await;
        let entries = match file
            .entries
            .iter()
            .rposition(|entry| matches!(entry, SessionTreeEntry::Compaction { .. }))
        {
            Some(index) => {
                let start = match &file.entries[index] {
                    SessionTreeEntry::Compaction {
                        first_kept_entry_id: Some(id),
                        ..
                    } => file.entries[..index]
                        .iter()
                        .position(|entry| entry.id() == id)
                        .unwrap_or(index),
                    _ => index,
                };
                &file.entries[start..]
            }
            None => &file.entries,
        };
        let context = loop_agent::harness::session::default_context_from_entries(entries);
        let mut updates = loop_agent::protocol_events::replay(&context.messages);
        if busy {
            // Source copies exclude an unfinished turn. A Surface view also needs
            // that turn's already durable output through its exact high-water mark.
            let baseline = live.turn_start_seq.load(Ordering::Acquire);
            for record in self
                .log
                .read_after(&live.reference.key, baseline)
                .await
                .map_err(failed)?
            {
                if let EventKind::Update(envelope) = record.event.kind {
                    if envelope.seq <= at_seq
                        && !matches!(envelope.update, SessionUpdate::ConfigChanged { .. })
                    {
                        updates.push(envelope.update);
                    }
                }
            }
        }
        Ok(updates)
    }

    /// Whether the Event Log holds a branch reset at or after `after_seq`. A
    /// detached legacy Surface's cursor is the reset's own seq.
    pub(super) async fn reset_since(
        &self,
        session: &SessionRef,
        after_seq: u64,
    ) -> BackendResult<bool> {
        Ok(self
            .log
            .read_after(&session.key, after_seq.saturating_sub(1))
            .await
            .map_err(failed)?
            .iter()
            .any(|record| {
                matches!(&record.event.kind, EventKind::Update(envelope)
                    if envelope.seq >= after_seq
                        && matches!(envelope.update, SessionUpdate::BranchReset { .. }))
            }))
    }
    /// The current branch through `at_seq`, stamped at `at_seq`. Caller holds
    /// the lifecycle lock.
    pub(super) async fn baseline_locked(
        &self,
        live: &LiveSession,
        at_seq: u64,
    ) -> BackendResult<Vec<UpdateEnvelope>> {
        let (file, busy) = self.capture(live).await?;
        Ok(self
            .transcript(live, &file, busy, at_seq)
            .await?
            .into_iter()
            .map(|update| {
                let mut envelope = UpdateEnvelope::live(
                    live.reference.instance.clone(),
                    self.generation,
                    live.reference.key.clone(),
                    None,
                    0,
                    update,
                );
                envelope.seq = at_seq;
                envelope.origin = Origin::Replay;
                envelope
            })
            .collect())
    }
    /// Reserve a view before changing any state, so a full Runtime refuses the
    /// change rather than committing a branch it cannot announce. Unbounded
    /// reservations reconcile an already committed change: one per Session.
    fn reserve_view(&self, session: &SessionRef, bounded: bool) -> BackendResult<ViewSlot<'_>> {
        let id = loop_ai::new_id();
        let now = std::time::Instant::now();
        let mut views = self.views.lock().unwrap();
        views.retain(|_, view| view.expires > now);
        if bounded && views.len() >= VIEW_LIMIT {
            return Err(BackendError::Busy);
        }
        views.insert(
            id.clone(),
            BranchView {
                session: session.clone(),
                updates: Vec::new(),
                branch_revision: 0,
                at_seq: 0,
                expires: now + VIEW_TTL,
            },
        );
        Ok(ViewSlot {
            views: &self.views,
            session: session.clone(),
            id,
            filled: false,
        })
    }
    /// Add the branch state and an immutable transcript view at `public`'s
    /// sequence. Caller holds the lifecycle lock.
    pub(super) async fn snapshot_locked(
        &self,
        live: &LiveSession,
        public: SessionSnapshot,
    ) -> BackendResult<SessionSnapshot> {
        let state = self
            .store
            .control_state(&live.reference.native_id)
            .await
            .map_err(failed)?;
        let (file, busy) = self.capture(live).await?;
        let slot = self.reserve_view(&live.reference, true)?;
        let updates = self.transcript(live, &file, busy, public.at_seq).await?;
        let turn_state = if live.escalations.has_pending().await {
            "parked"
        } else if busy {
            "busy"
        } else {
            "idle"
        };
        Ok(SessionSnapshot {
            workspace_id: workspace_id(&live.opened.workspace.cwd),
            branch_revision: state.branch_revision,
            view_id: Some(slot.fill(updates, state.branch_revision, public.at_seq)),
            turn_state: Some(turn_state.into()),
            stats: json!({}),
            ..public
        })
    }
    /// Read an immutable transcript page. Expired views fail explicitly.
    pub async fn view(&self, params: ViewParams) -> BackendResult<ViewResult> {
        let views = self.views.lock().unwrap();
        let view = views
            .get(&params.view_id)
            .filter(|v| v.session == params.session && v.expires > std::time::Instant::now())
            .ok_or_else(|| failed("transcript view expired; attach again"))?;
        let (start, end) = page(params.cursor.as_deref(), params.limit, view.updates.len())?;
        Ok(ViewResult {
            updates: view.updates[start..end].to_vec(),
            next_cursor: (end < view.updates.len()).then(|| end.to_string()),
            branch_revision: view.branch_revision,
            at_seq: view.at_seq,
        })
    }
    /// Branches, fork points and related Sessions, without replacing the source.
    pub async fn tree(&self, params: TreeParams) -> BackendResult<TreeResult> {
        let live = self.route(&params.session)?;
        let _guard = live.lifecycle.lock().await;
        let (file, busy) = self.capture(&live).await?;
        let entries = if busy {
            file.entries.clone()
        } else {
            self.store
                .load(&params.session.native_id)
                .await
                .map_err(failed)?
                .read_entries(None)
                .await
                .map_err(failed)?
        };
        let rows = session_tree_rows(&entries, file.entries.last().map(SessionTreeEntry::id));
        let state = self
            .store
            .control_state(&params.session.native_id)
            .await
            .map_err(failed)?;
        // A cursor names the exact row set it pages. Turn admission and
        // completion change rows without a branch revision, so bind to both.
        let rows_id = {
            use sha2::{Digest, Sha256};
            let mut digest = Sha256::new();
            digest.update(state.branch_revision.to_le_bytes());
            for row in &rows {
                digest.update(row.entry_id.as_bytes());
                digest.update([0, row.on_branch as u8, row.current as u8]);
            }
            format!("{:x}", digest.finalize())
        };
        let cursor = params
            .cursor
            .as_deref()
            .map(|cursor| {
                let (rows, offset) = cursor
                    .split_once(':')
                    .ok_or_else(|| failed("invalid tree cursor"))?;
                if rows != rows_id {
                    return Err(failed("tree changed; reload the first page"));
                }
                Ok(offset)
            })
            .transpose()?;
        let (start, end) = page(cursor, params.limit, rows.len())?;
        let entries = rows[start..end]
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
            .collect();
        let page_ids: HashSet<_> = rows[start..end]
            .iter()
            .map(|row| row.entry_id.as_str())
            .collect();
        let fork_points = fork_points_from_branch(&file.entries)
            .into_iter()
            .filter(|point| page_ids.contains(point.entry_id.as_str()))
            .map(|point| ForkPoint {
                entry_id: point.entry_id,
                index: point.index,
                text: point.text,
                preview: point.preview,
            })
            .collect();
        let metadata = self
            .store
            .load(&params.session.native_id)
            .await
            .map_err(failed)?
            .metadata()
            .clone();
        let family: Vec<_> = self
            .store
            .list(None)
            .await
            .map_err(failed)?
            .into_iter()
            .filter(|meta| {
                meta.parent_session_id.as_deref() == Some(&params.session.native_id)
                    || metadata.parent_session_id.as_deref() == Some(&meta.id)
            })
            .map(|meta| reference(&meta.id))
            .collect();
        if family.len() > 1000 {
            return Err(failed("Session family exceeds the page limit"));
        }
        Ok(TreeResult {
            entries,
            fork_points,
            family,
            branch_revision: state.branch_revision,
            next_cursor: (end < rows.len()).then(|| format!("{rows_id}:{end}")),
        })
    }
    /// Rename metadata and publish an ordered ConfigChanged, without a branch reset.
    pub async fn rename(&self, params: RenameParams) -> BackendResult<MutationResult> {
        if params.name.trim().is_empty() || params.name.len() > 4096 {
            return Err(failed("Session name must contain 1 to 4096 bytes"));
        }
        let name = params.name.trim().to_owned();
        let live = self.route(&params.session)?;
        let _guard = live.lifecycle.lock().await;
        self.check_tree_live(&live)?;
        barrier(&live).await?;
        // The lifecycle lock serializes this with the config setters.
        live.opened
            .native
            .check_session_revision(params.expected_revision)?;
        let marker =
            json!({"id":loop_ai::new_id(),"kind":"rename","name":name.clone()}).to_string();
        self.store
            .control_mutation(
                &params.session.native_id,
                self.identity.clone(),
                false,
                PendingSessionWrite::SessionInfo { name: name.clone() },
                marker,
            )
            .await
            .map_err(failed)?;
        let revision = live.opened.native.record_change(Some(Some(name)));
        let seq = self.announce_control(&live, None).await?;
        Ok(MutationResult {
            revision,
            seq,
            pending: false,
            applies_after_turn: None,
        })
    }
    /// Move the active branch while idle, then announce its immutable replacement.
    pub async fn navigate(&self, params: NavigateParams) -> BackendResult<NavigateResult> {
        let live = self.route(&params.session)?;
        let _guard = live.lifecycle.lock().await;
        self.check_tree_live(&live)?;
        if live.busy.load(Ordering::Acquire) {
            return Err(BackendError::Busy);
        }
        barrier(&live).await?;
        live.opened
            .native
            .check_session_revision(params.expected_revision)?;
        let slot = self.reserve_view(&params.session, true)?;
        // Deterministic fallback summary, matching the native tree navigation.
        let summary = if params.summarize {
            Some(format!(
                "Branch with {} entries.",
                self.committed_file(&live).await?.entries.len()
            ))
        } else {
            None
        };
        let marker =
            json!({"id":loop_ai::new_id(),"kind":"navigate","summary":summary}).to_string();
        let state = self
            .store
            .control_mutation(
                &params.session.native_id,
                self.identity.clone(),
                true,
                PendingSessionWrite::Leaf {
                    target_id: params.target_entry_id,
                },
                marker,
            )
            .await
            .map_err(failed)?;
        let revision = live.opened.native.record_change(None);
        let seq = self.announce_control(&live, Some(slot)).await?;
        Ok(NavigateResult {
            revision,
            branch_revision: state.branch_revision,
            seq,
        })
    }
    /// Compact while idle: commit the summary as a branch change, then
    /// announce the compacted branch's immutable replacement.
    pub async fn compact(
        &self,
        session: &SessionRef,
        instructions: Option<&str>,
    ) -> BackendResult<loop_protocol::CompactionReport> {
        let live = self.loaded(session).await?;
        let head = {
            let _guard = live.lifecycle.lock().await;
            self.check_tree_live(&live)?;
            if live.busy.load(Ordering::Acquire) {
                return Err(BackendError::Busy);
            }
            barrier(&live).await?;
            live.opened.harness.session_head().await.map_err(failed)?
        };
        // The summary is a model call: it runs without the lifecycle lock,
        // so attach, close and other Session work do not wait for it. The
        // harness refuses turns meanwhile, and closing cancels it.
        let (mut report, write) = live
            .opened
            .native
            .prepare_compaction(session, instructions)
            .await?;
        let _guard = live.lifecycle.lock().await;
        self.check_tree_live(&live)?;
        if live.busy.load(Ordering::Acquire) {
            return Err(BackendError::Busy);
        }
        barrier(&live).await?;
        if live.opened.harness.session_head().await.map_err(failed)? != head {
            return Err(failed(
                "the Session changed while compacting; nothing was written",
            ));
        }
        let slot = self.reserve_view(session, true)?;
        // No `summary` key: that would add a branch summary entry.
        let marker = json!({"id":loop_ai::new_id(),"kind":"compact"}).to_string();
        let state = self
            .store
            .control_mutation(
                &session.native_id,
                self.identity.clone(),
                true,
                write,
                marker,
            )
            .await
            .map_err(failed)?;
        live.opened.native.record_change(None);
        report.seq = self.announce_control(&live, Some(slot)).await?;
        report.branch_revision = state.branch_revision;
        Ok(report)
    }
    /// Finish an unannounced committed tree write before admitting work or snapshots.
    /// A navigation passes the view it reserved; recovery reserves its own.
    pub(super) async fn announce_control(
        &self,
        live: &LiveSession,
        slot: Option<ViewSlot<'_>>,
    ) -> BackendResult<u64> {
        // `latest_seq` below must count appends abandoned while queued.
        self.log.sync().await.map_err(failed)?;
        let state = self
            .store
            .control_state(&live.reference.native_id)
            .await
            .map_err(failed)?;
        let Some(marker) = state.pending else {
            return self
                .log
                .latest_seq(&live.reference.key)
                .await
                .map_err(failed);
        };
        let marker_value: serde_json::Value = serde_json::from_str(&marker).map_err(failed)?;
        let at_seq = self
            .log
            .latest_seq(&live.reference.key)
            .await
            .map_err(failed)?;
        // The native backend counted the change, at commit or when the
        // Session was reopened from its tree.
        let changed = live.opened.native.config();
        let revision = changed.revision;
        let update = if marker_value["kind"] == "rename" {
            SessionUpdate::ConfigChanged(changed)
        } else {
            let slot = match slot {
                Some(slot) => slot,
                None => self.reserve_view(&live.reference, false)?,
            };
            let (file, busy) = self.capture(live).await?;
            let updates = self.transcript(live, &file, busy, at_seq + 1).await?;
            let view_id = slot.fill(updates, state.branch_revision, at_seq + 1);
            SessionUpdate::BranchReset {
                revision,
                branch_revision: state.branch_revision,
                view_id,
                // A recovered marker keeps the reason it was written with.
                reason: marker_value["kind"].as_str().unwrap_or("navigate").into(),
            }
        };
        let (tx, rx) = oneshot::channel();
        live.commands
            .send(Command::Publish(Box::new(update), tx))
            .map_err(failed)?;
        let result = rx.await.map_err(failed)?;
        if let Err(error) = result {
            fail(live, &error);
            return Err(error);
        }
        // No setter waits on this change's receipt.
        live.announced.lock().unwrap().receipts.remove(&revision);
        self.store
            .acknowledge_control(&live.reference.native_id, &marker)
            .await
            .map_err(failed)?;
        self.log
            .latest_seq(&live.reference.key)
            .await
            .map_err(failed)
    }
    /// Return a bounded, consistent payload. The caller writes its own files.
    pub async fn export(&self, session: &SessionRef) -> BackendResult<ExportResult> {
        let live = self.route(session)?;
        let _guard = live.lifecycle.lock().await;
        let file = self.committed_file(&live).await?;
        let jsonl = file
            .to_jsonl_bounded(SESSION_TRANSFER_LIMIT)
            .map_err(failed)?;
        let result = ExportResult {
            format_version: 1,
            markdown: file.markdown(),
            jsonl,
            branch_revision: self
                .store
                .control_state(&session.native_id)
                .await
                .map_err(failed)?
                .branch_revision,
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
    /// The shared handle, so destination creation runs as a Runtime-owned
    /// task: a dropped request never interrupts admission or its rollback.
    fn shared(&self) -> Option<Arc<Runtime>> {
        self.me.get().and_then(Weak::upgrade)
    }
    /// Fork before a user message. Its text is returned to the initiating draft.
    pub async fn fork(&self, params: ForkParams) -> BackendResult<DestinationResult> {
        match self.shared() {
            // The caller does not cross the spawn on its own.
            Some(me) => tokio::spawn(in_caller_scope(async move { me.fork_owned(params).await }))
                .await
                .map_err(failed)?,
            None => self.fork_owned(params).await,
        }
    }
    async fn fork_owned(&self, params: ForkParams) -> BackendResult<DestinationResult> {
        let fingerprint = fingerprint(&params)?;
        if let Some(result) = self
            .retry_destination("copy", &params.request_id, &fingerprint)
            .await?
        {
            return Ok(result);
        }
        self.copy_branch(
            params.session,
            params.name,
            params.request_id,
            Some(params.before_entry_id),
            fingerprint,
        )
        .await
    }
    /// Copy a full committed branch into another leased Session.
    pub async fn clone_session(&self, params: CloneParams) -> BackendResult<DestinationResult> {
        match self.shared() {
            // The caller does not cross the spawn on its own.
            Some(me) => tokio::spawn(in_caller_scope(async move {
                me.clone_session_owned(params).await
            }))
            .await
            .map_err(failed)?,
            None => self.clone_session_owned(params).await,
        }
    }
    async fn clone_session_owned(&self, params: CloneParams) -> BackendResult<DestinationResult> {
        let fingerprint = fingerprint(&params)?;
        if let Some(result) = self
            .retry_destination("copy", &params.request_id, &fingerprint)
            .await?
        {
            return Ok(result);
        }
        self.copy_branch(
            params.session,
            params.name,
            params.request_id,
            None,
            fingerprint,
        )
        .await
    }
    async fn copy_branch(
        &self,
        session: SessionRef,
        name: Option<String>,
        request_id: String,
        before: Option<String>,
        fingerprint: String,
    ) -> BackendResult<DestinationResult> {
        let live = self.route(&session)?;
        let (mut file, revision, draft) = {
            let _guard = live.lifecycle.lock().await;
            let mut file = self.committed_file(&live).await?;
            let revision = self
                .store
                .control_state(&session.native_id)
                .await
                .map_err(failed)?
                .branch_revision;
            let draft = if let Some(before) = &before {
                let point = fork_points_from_branch(&file.entries)
                    .into_iter()
                    .find(|p| &p.entry_id == before)
                    .ok_or_else(|| {
                        failed("fork point is not a user message on the committed branch")
                    })?;
                file.entries = read_session_entries_for_fork(
                    &file.entries,
                    file.entries.last().map(SessionTreeEntry::id),
                    SessionForkSelection::BeforeEntry,
                    Some(before),
                )
                .map_err(failed)?;
                Some(point.text)
            } else {
                None
            };
            (file, revision, draft)
        };
        file.name = name.or(file.name);
        self.destination(
            file,
            live.opened.workspace.cwd.clone(),
            Some(session.native_id),
            request_id,
            fingerprint,
            Some(revision),
            draft,
        )
        .await
    }
    /// Validate a bounded payload and import into a known workspace.
    pub async fn import(&self, params: ImportParams) -> BackendResult<DestinationResult> {
        match self.shared() {
            // The caller does not cross the spawn on its own.
            Some(me) => tokio::spawn(in_caller_scope(
                async move { me.import_owned(params).await },
            ))
            .await
            .map_err(failed)?,
            None => self.import_owned(params).await,
        }
    }
    async fn import_owned(&self, params: ImportParams) -> BackendResult<DestinationResult> {
        if params.jsonl.len() > SESSION_TRANSFER_LIMIT {
            return Err(failed("Session transfer exceeds 8 MiB"));
        }
        let fingerprint = fingerprint(&params)?;
        if let Some(result) = self
            .retry_destination("import", &params.request_id, &fingerprint)
            .await?
        {
            return Ok(result);
        }
        let cwd = self
            .workspaces
            .lock()
            .unwrap()
            .keys()
            .find(|cwd| workspace_id(cwd) == params.workspace_id)
            .cloned()
            .ok_or_else(|| failed("unknown workspace id"))?;
        let file = SessionFile::from_jsonl(&params.jsonl).map_err(failed)?;
        validate_file(&file)?;
        self.destination(file, cwd, None, params.request_id, fingerprint, None, None)
            .await
    }
    /// Answer a retried request from its receipt. A receipt commits before its
    /// Session is admitted, so wait until no admission is in flight: one that
    /// fails rolls the receipt back.
    async fn retry_destination(
        &self,
        kind: &str,
        request_id: &str,
        fingerprint: &str,
    ) -> BackendResult<Option<DestinationResult>> {
        self.pauses.signal("destination_retry_wait");
        let _destinations = self.destinations.lock().await;
        self.receipt(kind, request_id, fingerprint).await
    }
    /// Read a receipt. Caller holds the destination lock.
    async fn receipt(
        &self,
        kind: &str,
        request_id: &str,
        fingerprint: &str,
    ) -> BackendResult<Option<DestinationResult>> {
        if request_id.trim().is_empty() || request_id.len() > 256 {
            return Err(failed("request id must contain 1 to 256 bytes"));
        }
        self.store
            .destination_receipt(&format!("{kind}:{request_id}"), fingerprint)
            .await
            .map_err(failed)?
            .map(|(id, receipt)| destination_result(&id, &receipt))
            .transpose()
    }
    #[allow(clippy::too_many_arguments)]
    async fn destination(
        &self,
        file: SessionFile,
        cwd: PathBuf,
        parent: Option<String>,
        request_id: String,
        fingerprint: String,
        revision: Option<u64>,
        draft: Option<String>,
    ) -> BackendResult<DestinationResult> {
        if request_id.trim().is_empty() || request_id.len() > 256 {
            return Err(failed("request id must contain 1 to 256 bytes"));
        }
        let destinations = self.destinations.clone().lock_owned().await;
        let kind = if parent.is_some() { "copy" } else { "import" };
        if let Some(result) = self.receipt(kind, &request_id, &fingerprint).await? {
            return Ok(result);
        }
        // Reserve room before creating anything; a full Runtime leaves no orphan row.
        let full = || self.sessions.lock().unwrap().len() >= self.limits.max_sessions;
        if full() && (!self.unload_one().await || full()) {
            return Err(BackendError::Busy);
        }
        let receipt = json!({"workspace_id":workspace_id(&cwd),"source_branch_revision":revision,"draft":draft}).to_string();
        let key = format!(
            "{}:{request_id}",
            if parent.is_some() { "copy" } else { "import" }
        );
        let created = self
            .store
            .create_destination(
                file,
                cwd.to_string_lossy().into_owned(),
                parent,
                key,
                fingerprint,
                receipt,
                self.identity.clone(),
            )
            .await
            .map_err(failed)?;
        let id = created.id.clone();
        let mut unadmitted = Unadmitted {
            created: Some(created),
            lock: Some(destinations),
        };
        let session = reference(&id);
        let injected = self.pauses.wait("destination_admit").await;
        let admitted = if injected {
            Err(failed("injected admission failure"))
        } else {
            self.open(Some(&session), SessionSpec::default(), None)
                .await
        };
        let mut created = unadmitted.created.take().unwrap();
        if let Err(error) = admitted {
            created.rollback().await.map_err(failed)?;
            return Err(error);
        }
        created.admitted();
        destination_result(&id, &created.receipt)
    }
}
