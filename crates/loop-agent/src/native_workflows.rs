//! Workflow admission, deduplication and ordered publication for NativeBackend.
use super::*;
use loop_protocol::session_services::*;

pub(super) struct WorkflowRun {
    pub fingerprint: String,
    pub id: String,
    pub done: tokio::sync::watch::Receiver<bool>,
}
impl NativeBackend {
    /// A prior admission for this request, without reserving execution again.
    pub fn known_workflow(&self, p: &WorkflowParams) -> BackendResult<Option<WorkflowResult>> {
        self.refuse_if_locked("session/workflow")?;
        let key = format!("{}:{}", p.session.key, p.request_id);
        let fingerprint = serde_json::to_string(p).map_err(|e| invalid(e.to_string()))?;
        let runs = self.workflows.lock();
        match runs.get(&key) {
            Some(run) if run.fingerprint == fingerprint => Ok(Some(WorkflowResult {
                workflow_id: run.id.clone(),
            })),
            Some(_) => Err(invalid(
                "request id was already used for another workflow".into(),
            )),
            None => Ok(None),
        }
    }
    /// Start once, retaining ownership when the requesting Surface disconnects.
    pub async fn workflow(self: &Arc<Self>, p: WorkflowParams) -> BackendResult<WorkflowResult> {
        self.refuse_if_locked("session/workflow")?;
        self.check_session(&p.session).await?;
        if p.goal.trim().is_empty()
            || p.goal.len() > 65536
            || p.request_id.is_empty()
            || p.request_id.len() > 256
            || p.concurrency.is_some_and(|n| n == 0 || n > 64)
        {
            return Err(invalid(
                "provide a goal, a request id, and concurrency from 1 through 64".into(),
            ));
        }
        let _admission = self.admission.lock().await;
        let key = format!("{}:{}", p.session.key, p.request_id);
        let fingerprint = serde_json::to_string(&p).map_err(|e| invalid(e.to_string()))?;
        {
            let runs = self.workflows.lock();
            if let Some(run) = runs.get(&key) {
                if run.fingerprint != fingerprint {
                    return Err(invalid(
                        "request id was already used for another workflow".into(),
                    ));
                }
                return Ok(WorkflowResult {
                    workflow_id: run.id.clone(),
                });
            }
            if runs.len() >= 1024 {
                return Err(invalid(
                    "workflow request history is full; start a new client".into(),
                ));
            }
        }
        if self.is_busy_now()
            || self
                .prompting
                .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
                .is_err()
        {
            return Err(BackendError::Busy);
        }
        let slot = PromptSlot(self.prompting.clone());
        let id = loop_ai::new_id();
        let (done, done_rx) = tokio::sync::watch::channel(false);
        self.workflows.lock().insert(
            key,
            WorkflowRun {
                fingerprint,
                id: id.clone(),
                done: done_rx,
            },
        );
        let native = self.clone();
        let workflow_id = id.clone();
        tokio::spawn(async move {
            let (progress_tx, mut progress_rx) = tokio::sync::mpsc::unbounded_channel();
            let publisher = native.clone();
            let progress_id = workflow_id.clone();
            let progress = tokio::spawn(async move {
                use crate::harness::orchestration::WorkflowProgressEvent as Event;
                while let Some(event) = progress_rx.recv().await {
                    let mut update = WorkflowProgress {
                        workflow_id: progress_id.clone(),
                        state: WorkflowState::Planned,
                        task_id: None,
                        description: None,
                        outline: None,
                        mermaid: None,
                        output: None,
                        error: None,
                        completion: None,
                    };
                    match event {
                        Event::GraphPlanned { outline, mermaid } => {
                            update.outline = Some(outline);
                            update.mermaid = Some(mermaid);
                        }
                        Event::TaskStarted {
                            task_id,
                            description,
                        } => {
                            update.state = WorkflowState::TaskStarted;
                            update.task_id = Some(task_id);
                            update.description = Some(description);
                        }
                        Event::TaskCompleted { task_id, output } => {
                            update.state = WorkflowState::TaskCompleted;
                            update.task_id = Some(task_id);
                            update.output = Some(output);
                        }
                        Event::TaskFailed { task_id, error } => {
                            update.state = WorkflowState::TaskFailed;
                            update.task_id = Some(task_id);
                            update.error = Some(error);
                        }
                    }
                    announce(
                        &publisher.instance,
                        &publisher.stream,
                        SessionUpdate::WorkflowProgress(update),
                    );
                }
            });
            let config = p
                .concurrency
                .map(|n| loop_orchestration::scheduler::SchedulerConfig {
                    max_concurrency: n as usize,
                    fail_fast: false,
                });
            let result = native
                .harness
                .start_workflow_from_goal(&p.goal, None, config, Some(progress_tx))
                .await;
            let _ = progress.await;
            if translate(
                &native.instance,
                &native.stream,
                &AgentEvent::AgentEnd { messages: vec![] },
            ) {
                apply_pending(&native.instance, &native.stream, &native.config);
            }
            let (completion, error) = match result {
                Ok(result) => (
                    Some(WorkflowCompletion {
                        success: result.success,
                        completed_count: result.task_results.len(),
                        failed_count: result.failed_tasks.len(),
                        total_count: result.total_task_count,
                        output: result.output_text(),
                        artifacts: result.artifact_paths(),
                    }),
                    None,
                ),
                Err(error) => (None, Some(error.to_string())),
            };
            announce(
                &native.instance,
                &native.stream,
                SessionUpdate::WorkflowProgress(WorkflowProgress {
                    workflow_id,
                    state: WorkflowState::Ended,
                    task_id: None,
                    description: None,
                    outline: None,
                    mermaid: None,
                    output: None,
                    error,
                    completion,
                }),
            );
            drop(slot);
            let _ = done.send(true);
        });
        Ok(WorkflowResult { workflow_id: id })
    }
    /// Wait for an accepted workflow's execution and final publication.
    pub async fn wait_workflow(&self, id: &str) {
        let done = self
            .workflows
            .lock()
            .values()
            .find(|r| r.id == id)
            .map(|r| r.done.clone());
        if let Some(mut done) = done {
            while !*done.borrow_and_update() {
                if done.changed().await.is_err() {
                    break;
                }
            }
        }
    }
}
