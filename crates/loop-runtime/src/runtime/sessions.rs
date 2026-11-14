//! Opening, closing and unloading Sessions, and the Session cap.
use super::leases::OpeningLease;
use super::publish::{publish, pump};
use super::*;

impl Runtime {
    pub(super) async fn open(
        &self,
        existing: Option<&SessionRef>,
        spec: SessionSpec,
        child: Option<subagent::ChildOpen>,
    ) -> BackendResult<SessionRef> {
        self.open_as(existing, spec, child, false).await
    }
    /// Open a Session. `fresh` marks an `existing` Session that this request
    /// has just created (a fork, clone or import destination): like a new
    /// one, it is in the caller's tenant and its opening is the creating
    /// record (the owner of record).
    pub(super) async fn open_as(
        &self,
        existing: Option<&SessionRef>,
        mut spec: SessionSpec,
        child: Option<subagent::ChildOpen>,
        fresh: bool,
    ) -> BackendResult<SessionRef> {
        // The connection's caller, read at entry: it does not cross spawns.
        let caller = loop_protocol::Caller::current();
        let who = crate::access::Actor::of(caller.clone());
        // An attach checks this Surface's credentials, but never changes an
        // existing Session's environment while another Surface may use it.
        let caller_env = spec.env.clone();
        if existing.is_some() {
            spec.env = None;
        }
        let _admission = loop {
            let admission = self.admission.lock().await;
            if self.stopped.load(Ordering::Acquire) {
                return Err(failed("Runtime stopped"));
            }
            let Some(session) = existing else {
                break admission;
            };
            if session.instance != self.instance || session.key.0 != session.native_id {
                return Err(BackendError::SessionNotFound(session.key.clone()));
            }
            match self.route(session) {
                // Being closed or unloaded: load it again once that is done,
                // without holding up other admissions meanwhile.
                Ok(live) if live.closing.load(Ordering::Acquire) => {
                    drop(admission);
                    self.wait_removed(&live).await;
                }
                Ok(live) => {
                    let model = live.opened.native.config().config.model;
                    let notice = self
                        .credential_notice(model.as_deref(), caller_env.as_ref())
                        .await;
                    *live.credential_notice.lock().unwrap() = notice;
                    return Ok(live.reference.clone());
                }
                Err(_) => break admission,
            }
        };
        if let Some(session) = existing {
            if let Some(split) = self.split.get(&session.key).filter(|_| !fresh) {
                return Err(failed(split));
            }
            if let Some(parent) = self.subagent_parent(session).await? {
                // Reopened alone it would lose its parent's ceiling and depth.
                return Err(failed(format!(
                    "Session {} is a Subagent of Session {parent}; a Subagent runs only for its \
                     parent and cannot be reopened. Its Event Log stays readable with `loop session attach`.",
                    session.native_id
                )));
            }
        }
        let full = || self.sessions.lock().unwrap().len() >= self.limits.max_sessions;
        if full() && (!self.unload_one().await || full()) {
            return Err(BackendError::Busy);
        }
        if let Some(existing) = existing {
            if let Some(saved) = self
                .launch_specs
                .lock()
                .unwrap()
                .get(&existing.key)
                .cloned()
            {
                if spec.env.is_none() {
                    spec.env = saved.env;
                }
                if spec.options.is_none() {
                    spec.options = saved.options;
                }
            }
        }
        if let Some(child) = &child {
            spec.env = child.parent.opened.harness.session_environment();
        }
        let saved_spec = spec.clone();
        let mut opts = self.session_opts();
        if let Some(child) = &child {
            opts.cwd = child.cwd.clone();
        } else if let Some(cwd) = spec.cwd {
            opts.cwd = cwd;
        } else if let Some(session) = existing {
            let known = self
                .factory
                .globals
                .repository
                .list(None)
                .await
                .map_err(failed)?;
            let meta = known
                .iter()
                .find(|m| m.id == session.native_id)
                .ok_or_else(|| BackendError::SessionNotFound(session.key.clone()))?;
            if let Some(cwd) = &meta.cwd {
                opts.cwd = PathBuf::from(cwd);
            }
        }
        // Under a tool lock no Session opens outside the lock root, by any
        // entry: a new Session, a load from the store, a fork or a child.
        // The workspace loader checks again.
        if let Some(lock) = self.factory.globals.tool_lock_config() {
            if lock.admit(&opts.cwd).is_err() {
                return Err(BackendError::ToolLocked {
                    what: format!(
                        "a Session in {} (outside the tool lock root)",
                        opts.cwd.display()
                    ),
                    lock: lock.lock(),
                });
            }
        }
        // An outside user's Session never shares a folder (or a folder
        // inside or above it) with a Session that user may not read.
        self.check_folder(&who, &opts.cwd, existing.map(|s| &s.key))
            .await?;
        if let Some(options) = &spec.options {
            opts.provider = options.provider.clone();
            opts.model = options.model.clone();
            opts.system_prompt = options.system_prompt.clone();
            opts.append_system_prompt = options.append_system_prompt.clone();
            opts.no_context_files = options.no_context_files;
            opts.no_web = options.no_web;
            opts.permission_mode = options.permission_mode.clone();
        }
        let session_workspace_flags = spec.options.as_ref().is_some_and(|options| {
            options.no_context_files
                || options.no_web
                || options.system_prompt.is_some()
                || options.append_system_prompt.is_some()
        });
        let workspace = if session_workspace_flags {
            Arc::new(
                WorkspaceResources::load(&self.factory.globals, &opts)
                    .await
                    .map_err(failed)?,
            )
        } else {
            self.factory.workspace(&opts).await.map_err(failed)?
        };
        let created = existing.is_none();
        let id = match existing {
            Some(session) => session.native_id.clone(),
            None if child.is_some() => self
                .store
                .create_child(
                    Some(workspace.cwd.to_string_lossy().into_owned()),
                    spec.name,
                    &child.as_ref().unwrap().parent.reference.native_id,
                )
                .await
                .map_err(failed)?
                .metadata()
                .id
                .clone(),
            None => self
                .factory
                .globals
                .repository
                .create(
                    Some(workspace.cwd.to_string_lossy().into_owned()),
                    spec.name,
                )
                .await
                .map_err(failed)?
                .metadata()
                .id
                .clone(),
        };
        let mut lease = match OpeningLease::acquire(
            self.store.clone(),
            self.identity.clone(),
            id.clone(),
        )
        .await
        {
            Ok(lease) => lease,
            Err(error) => {
                self.discard_new_row(created, &id).await;
                return Err(error);
            }
        };
        opts.session_id = Some(id.clone());
        let result = async {
            let mut opened = self
                .factory
                .open_with_environment(workspace, &opts, spec.env.as_ref())
                .await
                .map_err(failed)?;
            let credential_notice = self
                .credential_notice(
                    opened.native.config().config.model.as_deref(),
                    caller_env.as_ref(),
                )
                .await;
            let sandbox = opened
                .harness
                .ensure_sandbox_ready()
                .await
                .map_err(failed)?;
            let sandbox_choice = Self::sandbox_choice(&opened.settings, sandbox.is_some());
            {
                let mut workspaces = self.workspaces.lock().unwrap();
                let state = workspaces
                    .entry(opened.workspace.cwd.clone())
                    .or_insert_with(|| WorkspaceState {
                        resources: opened.workspace.clone(),
                        generation: admin::Generation::new(opened.workspace.clone()),
                        sandbox: None,
                        revision: 0,
                    });
                // A reload keeps its revision; a Session opened meanwhile
                // brings the resources it was opened with.
                if state.sandbox.is_none() {
                    state.sandbox = sandbox;
                }
            }
            let reference = opened.native.current_session().await;
            if !created && child.is_none() {
                // The Session's own model and effort win over the Runtime's
                // startup defaults.
                opened.native.restore_config().await?;
            }
            let mut sandbox_choice = sandbox_choice;
            if !created && child.is_none() {
                // The sandbox the Session last chose outlives a restart.
                let recorded = opened
                    .native
                    .recorded_sandbox()
                    .and_then(|v| {
                        serde_json::from_value::<loop_protocol::admin::SandboxChoice>(v).ok()
                    })
                    .filter(|choice| *choice != sandbox_choice);
                if let Some(choice) = recorded {
                    match self.restore_sandbox(&opened, &choice).await {
                        Ok(()) => {
                            sandbox_choice = if choice.mode == "local" {
                                choice
                            } else {
                                Self::sandbox_choice(&opened.settings, false)
                            };
                        }
                        Err(error) => opened
                            .startup_notices
                            .push(format!("Could not restore this Session's sandbox: {error}")),
                    }
                }
            }
            // Who opens it: the connection's caller; a Subagent acts for the
            // caller of its parent's turn (ADR-0009); in process, the Runtime.
            let acting = match &child {
                Some(child) => child.parent.turn_caller(),
                None => caller.clone().unwrap_or_else(in_process_caller),
            };
            // The Session's tenant, fixed for its life: a Subagent's is its
            // parent's; a Session this request created is the caller's; a
            // loaded one keeps the tenant the Event Log recorded.
            let creating = created || fresh;
            let tenant = match &child {
                Some(child) => child.parent.tenant.clone(),
                None if creating => acting.tenant().clone(),
                None => self.session_tenant(&reference.key),
            };
            let actor = child
                .as_ref()
                .map(|child| child.parent.reference.key.0.clone());
            if !created {
                // Appends abandoned while queued (a torn-down pump) commit first.
                self.log.sync().await.map_err(failed)?;
                // Turn ids stay unique per Session across Runtime generations.
                let turns = self
                    .log
                    .turn_ids(&tenant, &reference.key)
                    .await
                    .map_err(failed)?;
                let last = turns
                    .iter()
                    .filter_map(|turn| turn.as_str().strip_prefix("turn-")?.parse::<u64>().ok())
                    .max()
                    .unwrap_or(0);
                // A turn lost before its start was logged left only its prompt.
                let prompts = self
                    .log
                    .prompts_sent(&tenant, &reference.key)
                    .await
                    .map_err(failed)?;
                opened.native.continue_turns_after(last.max(prompts));
            }
            // Who may use it (I3): a Subagent its parent's users; a Session
            // this request created its creator; a loaded one its owner of
            // record.
            let access = match &child {
                Some(child) => child.parent.access.clone(),
                None if creating => {
                    crate::access::Subject::owned(tenant.clone(), acting.principal().clone())
                }
                None => self.subject(&reference.key).await?,
            };
            let escalations = crate::EscalationHost::new(
                reference.clone(),
                self.log.clone(),
                self.generation,
                tenant.clone(),
                actor.clone(),
            );
            escalations.set_access(access.clone());
            // Runtime grants are live Session state, never restored after a crash.
            opened.approval_bridge.clear_session_grants();
            Arc::make_mut(&mut opened.approval_bridge).set_host(Arc::new(escalations.clone()));
            opened
                .tool_gate
                .set_approver(Some(opened.approval_bridge.clone()));
            opened
                .tool_gate
                .set_mode_recorder(Some(loop_app_core::tool_gate::mode_recorder(
                    &opened.native,
                )));
            // Every gate decision of this Session goes to the Event Log.
            let decisions = decisions::LogDecisions::new(
                self.log.clone(),
                self.generation,
                reference.key.clone(),
                tenant.clone(),
                actor.clone(),
                escalations.clone(),
                &opened.native,
            );
            opened.tool_gate.set_decision_sink(Some(decisions.clone()));
            let (depth, workspace, child_state) = match &child {
                Some(child) => (
                    child.parent.depth + 1,
                    subagent::confine(&child.parent, &opened).await,
                    Some(subagent::ChildState::install(&opened, child.max_turns)),
                ),
                None => (0, workspace_root(&opened.workspace.cwd), None),
            };
            let parent = child.as_ref().map(|c| c.parent.reference.key.clone());
            // A Subagent acts for the person its parent acts for (ADR-0009),
            // so it copies the parent's `loaded_by`. That is the parent's last
            // loader, not an owner, and carries no authority (I3 adds owners).
            let loaded_by = match &child {
                Some(child) => child.parent.loaded_by.clone(),
                None => caller.clone(),
            };
            let receiver = opened.native.subscribe();
            let (commands, rx) = mpsc::unbounded_channel();
            let (cancel, _) = watch::channel(0);
            let ts_ms = now();
            let mut payload =
                json!({"cwd": opened.workspace.cwd, "parent": parent, "depth": depth});
            if creating {
                // The creating opening: its identity is the owner of record.
                payload[loop_store::SESSION_CREATED_KEY] = json!(true);
            }
            let opened_by = Identity::acting(&acting, tenant.clone()).with_actor(actor.clone());
            let opened_identity = opened_by.clone();
            self.log
                .append_batch(vec![Event {
                    generation: self.generation,
                    session_key: Some(reference.key.clone()),
                    turn: None,
                    ts_ms,
                    kind: EventKind::SessionOpened(payload),
                    identity: opened_by,
                }])
                .await
                .map_err(failed)?;
            touch(&self.activity, &reference.key, ts_ms);
            self.tenants
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .insert(reference.key.clone(), tenant.clone());
            if parent.is_some() {
                if let Some(keys) = self.subagents.lock().await.as_mut() {
                    keys.insert(reference.key.clone());
                }
            }
            let initial_config = opened.native.config();
            // The tree is written before the Event Log; a change whose
            // announcement was lost is announced before anyone is served.
            // A committed rename or branch move whose marker is still pending
            // is announced by its own update before anyone is served.
            let marked = !created
                && self
                    .store
                    .control_state(&reference.native_id)
                    .await
                    .map_err(failed)?
                    .pending
                    .is_some();
            let unannounced = !created
                && initial_config.revision
                    > self.logged_revision(&tenant, &reference.key).await? + u64::from(marked);
            let published = self
                .log
                .latest_seq(&tenant, &reference.key)
                .await
                .map_err(failed)?;
            let live = Arc::new(LiveSession {
                lifecycle: AsyncMutex::new(()),
                committed: AsyncMutex::new(None),
                turn_start_seq: std::sync::atomic::AtomicU64::new(0),
                pauses: self.pauses.clone(),
                opened,
                escalations,
                reference: reference.clone(),
                busy: AtomicBool::new(false),
                closing: AtomicBool::new(false),
                failure: Mutex::new(None),
                permit: AsyncMutex::new(None),
                cancel,
                commands,
                pump: AsyncMutex::new(None),
                turn_task: Mutex::new(None),
                activity: Arc::clone(&self.activity),
                tail: Mutex::new(VecDeque::new()),
                depth,
                parent: parent.clone(),
                workspace,
                child: child_state,
                waiting: AsyncMutex::new(0),
                turn_epoch: std::sync::atomic::AtomicU64::new(0),
                announced: Mutex::new(Announced {
                    published,
                    revision: initial_config.revision,
                    latest: initial_config.clone(),
                    receipts: HashMap::new(),
                }),
                pending_workspace: Mutex::new(None),
                credential_notice: Mutex::new(credential_notice),
                generation: Mutex::new(None),
                sandbox_choice: Mutex::new(sandbox_choice),
                pending_sandbox: Mutex::new(None),
                loaded_by,
                tenant,
                access,
                actor,
                acting: Mutex::new(Acting::new(acting, opened_identity)),
            });
            decisions.bind(&live);
            if unannounced {
                // Announced for the request that opens the Session.
                let identity = live.identity(&live.turn_caller());
                publish(
                    &live,
                    UpdateEnvelope::live(
                        self.instance.clone(),
                        self.generation,
                        reference.key.clone(),
                        None,
                        0,
                        SessionUpdate::ConfigChanged(initial_config),
                    ),
                    identity,
                    self.log.as_ref(),
                    self.generation,
                    &self.updates,
                )
                .await?;
            }
            if depth < self.subagent_depth {
                if let Some(me) = self.me.get() {
                    subagent::offer(me.clone(), &live).await.map_err(failed)?;
                }
            }
            // Attachments outlive unloading. Serialize restoration with new
            // registration and disconnect, so none can miss this host. A
            // Subagent's Escalations also go to Surfaces watching an ancestor.
            let recipients = self.recipients.lock().await;
            let mut lineage = vec![reference.key.clone()];
            lineage.extend(parent.iter().flat_map(|p| self.lineage(p)));
            for (connection, recipient) in wanted(&recipients, &lineage) {
                live.escalations.attach(connection, recipient).await;
            }
            let task = tokio::spawn(pump(
                Arc::clone(&live),
                receiver,
                rx,
                Arc::clone(&self.log),
                self.generation,
                self.updates.clone(),
                Arc::clone(&self.store),
                self.identity.clone(),
            ));
            *live.pump.lock().await = Some(task);
            self.pauses.wait("open_publishing").await;
            let _publishing = self.reload_gate.read().await;
            self.adopt_generation(&live).await;
            self.sessions
                .lock()
                .unwrap()
                .insert(reference.key.clone(), live.clone());
            self.unloaded.lock().unwrap().remove(&reference.key);
            self.launch_specs
                .lock()
                .unwrap()
                .insert(reference.key.clone(), saved_spec);
            self.idle_epoch.fetch_add(1, Ordering::AcqRel);
            lease.armed = false;
            Ok(reference)
        }
        .await;
        if result.is_err()
            && self
                .store
                .lease(&id)
                .await
                .map_err(failed)?
                .is_some_and(|lease| lease.identity == self.identity)
        {
            self.store
                .release_lease(&id, &self.identity)
                .await
                .map_err(failed)?;
            lease.armed = false;
        }
        if result.is_err() {
            self.discard_new_row(created, &id).await;
        }
        result
    }
    // A failed open of a Session this call created must not leave an empty row.
    pub(super) async fn discard_new_row(&self, created: bool, id: &str) {
        if created {
            if let Err(error) = self.store.delete(id).await {
                eprintln!("Runtime could not discard Session {id}: {error}");
            }
        }
    }
    /// Unload a Session, settling any forced cancellation before releasing ownership.
    /// Every wait is bounded by the close grace; a turn that ignores cancellation
    /// is aborted so the lease is always released.
    pub async fn close(&self, key: &SessionKey, force: bool) -> BackendResult<()> {
        self.check_as(
            &crate::access::Actor::current(),
            key,
            crate::access::Need::Close,
        )
        .await?;
        self.close_inner(key, force).await
    }
    pub(super) async fn close_inner(&self, key: &SessionKey, force: bool) -> BackendResult<()> {
        let live = self
            .sessions
            .lock()
            .unwrap()
            .get(key)
            .cloned()
            .ok_or_else(|| BackendError::SessionNotFound(key.clone()))?;
        {
            // Decided under the lifecycle lock, so no prompt is admitted
            // between the busy check and `closing`.
            let _lifecycle = live.lifecycle.lock().await;
            if !force && live.busy.load(Ordering::Acquire) {
                return Err(BackendError::Busy);
            }
            if live.closing.swap(true, Ordering::AcqRel) {
                drop(_lifecycle);
                // Another close owns the teardown; wait for it instead of repeating it.
                self.wait_removed(&live).await;
                return Ok(());
            }
        }
        let (reason, by) = if self.stopped.load(Ordering::Acquire) {
            // The Runtime stops its Sessions on its own.
            ("shutdown", in_process_caller())
        } else {
            // A Subagent is closed by its parent's agent, for the caller of
            // the parent's turn.
            let by = match loop_protocol::Caller::current() {
                Some(caller) => caller,
                None if live.parent.is_some() => live.turn_caller(),
                None => in_process_caller(),
            };
            ("closed", by)
        };
        self.teardown(key, live, force, reason, &by).await
    }
    /// Wait until `live` is no longer the loaded instance of its Session.
    pub(super) async fn wait_removed(&self, live: &Arc<LiveSession>) {
        while self
            .sessions
            .lock()
            .unwrap()
            .get(&live.reference.key)
            .is_some_and(|current| Arc::ptr_eq(current, live))
        {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    }
    pub(super) fn was_unloaded(&self, key: &SessionKey) -> bool {
        self.unloaded.lock().unwrap().contains(key)
    }
    /// Whether nothing needs `live` loaded: no turn, no parked Escalation, no
    /// attached Surface, no running Subagent below it, and not itself a
    /// Subagent (its `subagent` call closes it). Call under its lifecycle lock.
    pub(super) async fn unloadable(&self, live: &LiveSession) -> bool {
        let key = &live.reference.key;
        if live.busy.load(Ordering::Acquire)
            || live.closing.load(Ordering::Acquire)
            || live.child.is_some()
            || live.parent.is_some()
            || *live.waiting.lock().await > 0
            || live.escalations.has_pending().await
        {
            return false;
        }
        if self
            .watchers
            .lock()
            .unwrap()
            .get(key)
            .is_some_and(|watching| !watching.is_empty())
            || self
                .recipients
                .lock()
                .await
                .get(key)
                .is_some_and(|attached| !attached.is_empty())
        {
            return false;
        }
        !self
            .sessions
            .lock()
            .unwrap()
            .values()
            .any(|s| s.parent.as_ref() == Some(key))
    }
    /// Unload the least recently active Session nothing needs, to make room.
    /// Runs under admission. Returns whether one was unloaded.
    pub(super) async fn unload_one(&self) -> bool {
        let mut candidates: Vec<(i64, Arc<LiveSession>)> = {
            let activity = self.activity.lock().unwrap();
            self.sessions
                .lock()
                .unwrap()
                .values()
                .map(|live| {
                    let ts = activity.get(&live.reference.key).copied().unwrap_or(0);
                    (ts, live.clone())
                })
                .collect()
        };
        candidates.sort_by_key(|(ts, _)| *ts);
        for (_, live) in candidates {
            let Ok(lifecycle) = live.lifecycle.try_lock() else {
                continue;
            };
            if !self.unloadable(&live).await {
                continue;
            }
            let key = live.reference.key.clone();
            live.closing.store(true, Ordering::Release);
            self.unloaded.lock().unwrap().insert(key.clone());
            drop(lifecycle);
            eprintln!(
                "Runtime: unloading idle Session {} (least recently active) to make room",
                key.0
            );
            // The Runtime unloads on its own.
            if let Err(error) = self
                .teardown(&key, live, false, "unloaded", &in_process_caller())
                .await
            {
                eprintln!("Runtime: unloading Session {}: {error}", key.0);
            }
            return true;
        }
        false
    }
    /// Load a Session that was unloaded to make room, for a prompt or attach.
    pub(super) async fn reload(&self, session: &SessionRef) -> BackendResult<()> {
        self.open(Some(session), SessionSpec::default(), None)
            .await
            .map(|_| ())
    }
    /// Close a Session whose `closing` the caller set. Always removes it.
    /// `by` is who closes it; its records name `by`.
    pub(super) async fn teardown(
        &self,
        key: &SessionKey,
        live: Arc<LiveSession>,
        force: bool,
        reason: &str,
        by: &Caller,
    ) -> BackendResult<()> {
        if force {
            live.cancel.send_modify(|n| *n += 1);
            live.opened.native.cancel(&live.reference);
        }
        let grace = self.close_grace;
        // The host may be holding its decision mutex while an Event Log
        // acknowledgement stalls. Bound that wait too, then interrupt decisions
        // without discarding the independent durable withdrawal task.
        let escalations = live.escalations.clone();
        let withdrawn_by = by.clone();
        let mut withdrawing = tokio::spawn(async move {
            escalations
                .withdraw("Session closed", true, &withdrawn_by)
                .await
        });
        let withdrawal = match tokio::time::timeout(grace, &mut withdrawing).await {
            Ok(result) => result
                .map_err(failed)
                .and_then(|result| result.map_err(failed)),
            Err(_) => {
                live.escalations.interrupt_decisions();
                match tokio::time::timeout(grace, &mut withdrawing).await {
                    Ok(result) => result.map_err(failed).and_then(|result| result.map_err(failed)),
                    Err(_) => Err(failed("Session Escalation withdrawal exceeded close grace; durable cleanup continues")),
                }
            }
        };
        if force && live.commands.send(Command::Cancel(by.clone())).is_err() {
            live.cancel.send_modify(|n| *n += 1);
            live.opened.native.cancel(&live.reference);
        }
        let settled = tokio::time::timeout(grace, async {
            while live.busy.load(Ordering::Acquire) {
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
        })
        .await
        .is_ok();
        if !settled {
            eprintln!(
                "Runtime close: turn of Session {} ignored cancellation; aborting",
                key.0
            );
            if let Some(turn) = live.turn_task.lock().unwrap().take() {
                turn.abort();
            }
            live.permit.lock().await.take();
            let _lifecycle = live.lifecycle.lock().await;
            live.busy.store(false, Ordering::Release);
        }
        let _ = tokio::time::timeout(grace, live.opened.native.shutdown()).await;
        self.release_admin(&live).await;
        let (tx, rx) = oneshot::channel();
        let _ = live.commands.send(Command::Stop(tx));
        let stopped = tokio::time::timeout(grace, rx).await.is_ok();
        let mut pump_error = None;
        if let Some(task) = live.pump.lock().await.take() {
            if !stopped {
                task.abort();
            }
            if let Err(error) = task.await {
                if !error.is_cancelled() {
                    pump_error = Some(failed(error));
                }
            }
        }
        let ts_ms = now();
        let logged = tokio::time::timeout(
            grace,
            self.log.append_batch(vec![Event {
                generation: self.generation,
                session_key: Some(key.clone()),
                turn: None,
                ts_ms,
                kind: EventKind::SessionClosed(
                    json!({"force": force, "aborted": !settled || !stopped, "reason": reason}),
                ),
                identity: live.identity(by),
            }]),
        )
        .await
        .map_err(failed)
        .and_then(|result| result.map_err(failed));
        touch(&self.activity, key, ts_ms);
        let released = self
            .store
            .release_lease(&key.0, &self.identity)
            .await
            .map_err(failed);
        self.sessions.lock().unwrap().remove(key);
        if let Some(error) = pump_error {
            return Err(error);
        }
        withdrawal?;
        logged?;
        released?;
        if let Some(error) = live.failure.lock().unwrap().clone() {
            return Err(failed(error));
        }
        Ok(())
    }
}
