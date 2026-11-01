//! Plain-text Surface for a shared Runtime. No application bootstrap.
use std::path::PathBuf;

#[derive(Debug, Default)]
pub struct AttachArgs {
    pub id: Option<String>,
    pub new: bool,
    pub cwd: Option<PathBuf>,
    pub socket: Option<PathBuf>,
}

#[cfg(not(unix))]
pub async fn run(_: AttachArgs) -> anyhow::Result<()> {
    anyhow::bail!("loop attach is unsupported on this platform; Linux and macOS are supported")
}

#[cfg(unix)]
pub use unix::{connect, drive, Input, Terminal};

#[cfg(unix)]
pub async fn run(args: AttachArgs) -> anyhow::Result<()> {
    let mut terminal = unix::StdioTerminal::new()?;
    let socket = match args.socket.clone() {
        Some(socket) => socket,
        None => crate::config::paths::runtime_socket_path(&crate::config::paths::get_agent_dir())?,
    };
    let stream = tokio::net::UnixStream::connect(&socket)
        .await
        .map_err(|e| {
            anyhow::anyhow!(
                "Cannot connect to Runtime at {}: {e}. Start one with `loop serve`.",
                socket.display()
            )
        })?;
    let (backend, events) = connect(loop_wire::socket::unix(stream)).await?;
    drive(backend, events, args, &mut terminal).await
}

#[cfg(unix)]
mod unix {
    use super::*;
    use loop_protocol::{
        rpc::{methods, InteractWithdrawnParams, PeerInfo, SessionInfo},
        AgentBackend, ContentBlock, HostServices, InteractionOutcome, InteractionRequest,
        SessionRef, SessionSpec,
    };
    use loop_wire::{transport::Transport, RemoteBackend};
    use std::{
        collections::HashSet,
        io::{BufRead, Write},
        sync::Arc,
    };
    use tokio::sync::{mpsc, oneshot};

    #[derive(Debug)]
    pub enum Input {
        Line(String),
        Interrupt,
        Eof,
    }

    /// Cancellation-safe input and flushed text output, scripted in tests.
    #[async_trait::async_trait]
    pub trait Terminal: Send {
        async fn read(&mut self) -> anyhow::Result<Input>;
        fn write(&mut self, text: &str) -> anyhow::Result<()>;
    }

    pub struct StdioTerminal {
        lines: mpsc::UnboundedReceiver<std::io::Result<String>>,
        interrupt: tokio::signal::unix::Signal,
    }
    impl StdioTerminal {
        pub fn new() -> anyhow::Result<Self> {
            let interrupt =
                tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt())?;
            let (tx, lines) = mpsc::unbounded_channel();
            // A detached OS thread avoids Tokio's blocking-stdin shutdown wait.
            std::thread::spawn(move || {
                for line in std::io::stdin().lock().lines() {
                    if tx.send(line).is_err() {
                        break;
                    }
                }
            });
            Ok(Self { lines, interrupt })
        }
    }
    #[async_trait::async_trait]
    impl Terminal for StdioTerminal {
        async fn read(&mut self) -> anyhow::Result<Input> {
            tokio::select! {
                _ = self.interrupt.recv() => Ok(Input::Interrupt),
                line = self.lines.recv() => match line {
                    Some(line) => Ok(Input::Line(line?)),
                    None => Ok(Input::Eof),
                }
            }
        }
        fn write(&mut self, text: &str) -> anyhow::Result<()> {
            let mut out = std::io::stdout().lock();
            out.write_all(text.as_bytes())?;
            out.flush()?;
            Ok(())
        }
    }

    #[derive(Debug, thiserror::Error)]
    #[error("Lost contact with Runtime; reattach to resume")]
    struct LostContact;

    pub enum HostEvent {
        Request(Box<InteractionRequest>, oneshot::Sender<InteractionOutcome>),
        Withdrawn(InteractWithdrawnParams),
        Disconnected,
    }
    struct Host(mpsc::UnboundedSender<HostEvent>);
    #[async_trait::async_trait]
    impl HostServices for Host {
        async fn interact(&self, request: InteractionRequest) -> InteractionOutcome {
            let (tx, rx) = oneshot::channel();
            if self
                .0
                .send(HostEvent::Request(Box::new(request), tx))
                .is_err()
            {
                return InteractionOutcome::Cancelled;
            }
            rx.await.unwrap_or(InteractionOutcome::Cancelled)
        }
    }

    /// Connect the approval host and expose withdrawals ignored by older wire clients.
    pub async fn connect(
        mut transport: Transport,
    ) -> anyhow::Result<(Arc<RemoteBackend>, mpsc::UnboundedReceiver<HostEvent>)> {
        let (events_tx, events) = mpsc::unbounded_channel();
        let (tx, incoming) = mpsc::unbounded_channel();
        let notices = events_tx.clone();
        tokio::spawn(async move {
            while let Some(raw) = transport.incoming.recv().await {
                if let Ok(value) = serde_json::from_str::<serde_json::Value>(&raw) {
                    if value["method"] == methods::HOST_INTERACT_WITHDRAWN {
                        if let Ok(params) = serde_json::from_value(value["params"].clone()) {
                            let _ = notices.send(HostEvent::Withdrawn(params));
                        }
                    }
                }
                if tx.send(raw).is_err() {
                    return;
                }
            }
            let _ = notices.send(HostEvent::Disconnected);
        });
        let backend = RemoteBackend::connect(
            Transport {
                incoming,
                outgoing: transport.outgoing,
            },
            PeerInfo {
                name: "loop attach".into(),
                version: env!("CARGO_PKG_VERSION").into(),
            },
            Arc::new(Host(events_tx)),
        )
        .await?;
        Ok((backend, events))
    }

    fn list_text(sessions: &mut [SessionInfo]) -> String {
        sessions.sort_by_key(|s| std::cmp::Reverse(s.last_active_ms));
        if sessions.is_empty() {
            return "No Sessions. Use `loop attach --new` to create one.\n".into();
        }
        let mut text = String::from("ID\tFOLDER\tTITLE\tBUSY\tLAST ACTIVE (ms)\tPARENT\n");
        for s in sessions {
            text.push_str(&format!(
                "{}\t{}\t{}\t{}\t{}\t{}\n",
                s.r#ref.key,
                s.cwd.display(),
                s.name.as_deref().unwrap_or("(untitled)"),
                s.busy,
                s.last_active_ms,
                s.parent.as_ref().map(|p| p.key.as_str()).unwrap_or("-")
            ));
        }
        text
    }

    fn matches(sessions: &[SessionInfo], id: &str) -> Vec<SessionInfo> {
        let exact: Vec<_> = sessions
            .iter()
            .filter(|s| s.r#ref.key.as_str() == id || s.r#ref.native_id == id)
            .cloned()
            .collect();
        if !exact.is_empty() {
            return exact;
        }
        sessions
            .iter()
            .filter(|s| s.r#ref.key.as_str().starts_with(id) || s.r#ref.native_id.starts_with(id))
            .cloned()
            .collect()
    }

    struct Pending {
        request: InteractionRequest,
        reply: oneshot::Sender<InteractionOutcome>,
    }
    fn show(request: &InteractionRequest) -> String {
        let mut text = String::from("\nEscalation: ");
        match request {
            InteractionRequest::Permission {
                tool_call, options, ..
            } => {
                text.push_str(&format!(
                    "{}\n",
                    tool_call
                        .as_ref()
                        .and_then(|t| t.title.as_deref())
                        .unwrap_or("Tool approval")
                ));
                for (i, option) in options.iter().enumerate() {
                    text.push_str(&format!("  {}. {}\n", i + 1, option.label));
                }
            }
            InteractionRequest::Question {
                prompt, choices, ..
            } => {
                text.push_str(&format!("{prompt}\n"));
                for (i, choice) in choices.iter().flatten().enumerate() {
                    text.push_str(&format!("  {}. {}\n", i + 1, choice.label));
                }
            }
            _ => text.push_str("This request cannot be presented.\n"),
        }
        text
    }
    fn answer(request: &InteractionRequest, line: &str) -> Option<InteractionOutcome> {
        let index = line
            .trim()
            .parse::<usize>()
            .ok()
            .and_then(|i| i.checked_sub(1));
        let id = match request {
            InteractionRequest::Permission { options, .. } => {
                options.get(index?)?.native_id.clone()
            }
            InteractionRequest::Question {
                choices: Some(choices),
                ..
            } => choices.get(index?)?.native_id.clone(),
            InteractionRequest::Question { choices: None, .. } => {
                return Some(InteractionOutcome::Answered { value: line.into() })
            }
            _ => return None,
        };
        Some(InteractionOutcome::Selected {
            native_id: id,
            note: None,
        })
    }

    pub async fn drive(
        backend: Arc<RemoteBackend>,
        mut events: mpsc::UnboundedReceiver<HostEvent>,
        args: AttachArgs,
        terminal: &mut dyn Terminal,
    ) -> anyhow::Result<()> {
        let session = if args.new {
            let cwd = std::path::absolute(args.cwd.unwrap_or(std::env::current_dir()?))?;
            backend
                .new_session(SessionSpec {
                    cwd: Some(cwd),
                    ..Default::default()
                })
                .await?
        } else {
            let mut sessions = backend.list().await?;
            let Some(id) = args.id else {
                terminal.write(&list_text(&mut sessions))?;
                backend.shutdown().await;
                return Ok(());
            };
            let mut found = matches(&sessions, &id);
            match found.len() {
                0 => {
                    backend.shutdown().await;
                    anyhow::bail!("No Session matches `{id}`");
                }
                1 => found.remove(0).r#ref,
                _ => {
                    terminal.write(&list_text(&mut found))?;
                    backend.shutdown().await;
                    anyhow::bail!("Ambiguous Session prefix `{id}`; use a longer id");
                }
            }
        };
        let mut updates = backend.subscribe();
        let snapshot = backend.attach(&session, 0).await?;
        let result = attached(
            &backend,
            &session,
            &mut updates,
            &mut events,
            terminal,
            snapshot.latest_seq,
        )
        .await;
        // A dead connection cannot acknowledge cleanup RPCs. The Runtime owns
        // the Session independently and will remove this recipient itself.
        if result
            .as_ref()
            .err()
            .is_some_and(|error| error.is::<LostContact>())
        {
            return result;
        }
        // Detach is observation only: never cancel or close a Session on exit.
        let detached = backend.detach(&session).await;
        backend.shutdown().await;
        result?;
        detached?;
        Ok(())
    }

    async fn attached(
        backend: &Arc<RemoteBackend>,
        session: &SessionRef,
        updates: &mut tokio::sync::broadcast::Receiver<loop_protocol::UpdateEnvelope>,
        events: &mut mpsc::UnboundedReceiver<HostEvent>,
        terminal: &mut dyn Terminal,
        replay_until: u64,
    ) -> anyhow::Result<()> {
        let mut last_seq = 0;
        let mut pending: Vec<Pending> = Vec::new();
        let mut withdrawn = HashSet::new();
        let mut interrupted = false;
        let mut prompts = tokio::task::JoinSet::new();
        loop {
            tokio::select! {
                Some(event) = events.recv() => match event {
                    HostEvent::Request(request, reply) => {
                        let request = *request;
                        if withdrawn.remove(request.id()) { let _ = reply.send(InteractionOutcome::Cancelled); continue; }
                        if !matches!(request, InteractionRequest::Permission {..} | InteractionRequest::Question {..}) {
                            let _ = reply.send(InteractionOutcome::Rejected { reason: "loop attach supports choices and text questions".into() }); continue;
                        }
                        if pending.is_empty() { terminal.write(&show(&request))?; }
                        pending.push(Pending { request, reply });
                    }
                    HostEvent::Withdrawn(params) if params.session == *session => {
                        withdrawn.insert(params.id.clone());
                        if let Some(index) = pending.iter().position(|p| p.request.id() == &params.id) {
                            let item = pending.remove(index);
                            let _ = item.reply.send(InteractionOutcome::Cancelled);
                        }
                        terminal.write("\r\x1b[2KAnother client answered; Escalation withdrawn.\n")?;
                        if let Some(next) = pending.first() { terminal.write(&show(&next.request))?; }
                    }
                    HostEvent::Withdrawn(_) => {},
                    HostEvent::Disconnected => return Err(LostContact.into()),
                },
                update = updates.recv() => {
                    let envelope = update.map_err(|e| anyhow::anyhow!("Runtime updates interrupted ({e}); reattach to replay"))?;
                    if envelope.session != session.key { continue; }
                    last_seq = envelope.seq;
                    if let Some(text) = crate::print_mode::agent_text_chunk(&envelope.update) { terminal.write(text)?; }
                    if let loop_protocol::SessionUpdate::TurnEnded { error, .. } = envelope.update {
                        terminal.write("\n")?;
                        if let Some(error) = error { terminal.write(&format!("loop: {}\n", error.message))?; }
                    }
                },
                Some(result) = prompts.join_next(), if !prompts.is_empty() => {
                    if let Err(error) = result? { terminal.write(&format!("loop: {error}\n"))?; }
                },
                // Finish the snapshot even when piped stdin immediately closes.
                input = terminal.read(), if last_seq >= replay_until => match input? {
                    Input::Eof => return Ok(()),
                    Input::Interrupt if interrupted => return Ok(()),
                    Input::Interrupt => {
                        interrupted = true;
                        backend.cancel(session);
                        terminal.write("\nTurn cancelled. Ctrl+C again to detach.\n")?;
                    }
                    Input::Line(line) => {
                        if let Some(item) = pending.first() {
                            if let Some(outcome) = answer(&item.request, &line) {
                                let item = pending.remove(0);
                                let _ = item.reply.send(outcome);
                                if let Some(next) = pending.first() { terminal.write(&show(&next.request))?; }
                            } else { terminal.write("Enter one of the numbered choices.\n")?; }
                        } else if !line.trim().is_empty() {
                            interrupted = false;
                            match backend.prompt(session, vec![ContentBlock::text(line)]).await {
                                Ok(turn) => { prompts.spawn(async move {
                                    match turn.outcome().await {
                                        loop_protocol::TurnOutcome::Failed(error) => Err(loop_protocol::BackendError::Failed(error)),
                                        loop_protocol::TurnOutcome::Uncertain => Err(loop_protocol::BackendError::failed(loop_protocol::ErrorCategory::Network, "Lost contact with Runtime")),
                                        _ => Ok(()),
                                    }
                                }); }
                                Err(error) => terminal.write(&format!("loop: {error}\n"))?,
                            }
                        }
                    }
                }
            }
        }
    }
}
