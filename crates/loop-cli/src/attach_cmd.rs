//! Plain-text Surface for a shared Runtime. No application bootstrap.
use std::path::PathBuf;

#[derive(Debug, Default)]
pub struct AttachArgs {
    pub id: Option<String>,
    pub new: bool,
    pub cwd: Option<PathBuf>,
    pub socket: Option<PathBuf>,
    /// List finished Subagent children too.
    pub all: bool,
}

#[cfg(not(unix))]
pub async fn run(_: AttachArgs) -> anyhow::Result<()> {
    anyhow::bail!("loop attach is unsupported on this platform; Linux and macOS are supported")
}

#[cfg(unix)]
pub use unix::{connect, drive, AttachClient, Input, Terminal};

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
        SessionKey, SessionRef, SessionSpec,
    };
    use loop_wire::{transport::Transport, RemoteBackend};
    use std::{
        collections::{HashSet, VecDeque},
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
        stdin_ended: bool,
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
            Ok(Self {
                lines,
                interrupt,
                stdin_ended: false,
            })
        }
    }
    #[async_trait::async_trait]
    impl Terminal for StdioTerminal {
        async fn read(&mut self) -> anyhow::Result<Input> {
            tokio::select! {
                _ = self.interrupt.recv() => Ok(Input::Interrupt),
                line = self.lines.recv(), if !self.stdin_ended => match line {
                    Some(line) => Ok(Input::Line(line?)),
                    None => { self.stdin_ended = true; Ok(Input::Eof) },
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
        /// Which Session a `host/interact` request that follows came from.
        Origin(loop_protocol::InteractionId, SessionRef),
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

    /// Owns the local transport lifetime independently of cleanup RPC replies.
    pub struct AttachClient {
        backend: Arc<RemoteBackend>,
        closed: tokio_util::sync::CancellationToken,
    }
    impl Drop for AttachClient {
        fn drop(&mut self) {
            self.closed.cancel();
        }
    }
    impl AttachClient {
        async fn cleanup_rpc<T>(
            &self,
            rpc: impl std::future::Future<Output = T>,
        ) -> anyhow::Result<T> {
            tokio::select! {
                biased;
                _ = self.closed.cancelled() => Err(LostContact.into()),
                result = tokio::time::timeout(std::time::Duration::from_secs(2), rpc) => {
                    result.map_err(|_| anyhow::anyhow!("Runtime cleanup timed out; closing the local connection"))
                }
            }
        }
        async fn shutdown(&self) {
            let _ = self.cleanup_rpc(self.backend.shutdown()).await;
        }
    }

    /// Connect the approval host and expose withdrawals ignored by older wire clients.
    pub async fn connect(
        mut transport: Transport,
    ) -> anyhow::Result<(AttachClient, mpsc::UnboundedReceiver<HostEvent>)> {
        let (events_tx, events) = mpsc::unbounded_channel();
        let (tx, incoming) = mpsc::unbounded_channel();
        let (outgoing, mut from_wire) = mpsc::unbounded_channel();
        let notices = events_tx.clone();
        let closed = tokio_util::sync::CancellationToken::new();
        let pump_closed = closed.clone();
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    _ = pump_closed.cancelled() => break,
                    raw = transport.incoming.recv() => {
                        let Some(raw) = raw else { break; };
                        if let Ok(value) = serde_json::from_str::<serde_json::Value>(&raw) {
                            if value["method"] == methods::HOST_INTERACT_WITHDRAWN {
                                if let Ok(params) = serde_json::from_value(value["params"].clone()) {
                                    let _ = notices.send(HostEvent::Withdrawn(params));
                                }
                            }
                            // The wire client drops the request's Session; a
                            // Subagent's requests reach its ancestors' Surfaces.
                            if value["method"] == methods::HOST_INTERACT {
                                let params = &value["params"];
                                if let (Ok(id), Ok(session)) = (
                                    serde_json::from_value(params["id"].clone()),
                                    serde_json::from_value(params["session"].clone()),
                                ) {
                                    let _ = notices.send(HostEvent::Origin(id, session));
                                }
                            }
                        }
                        if tx.send(raw).is_err() { break; }
                    }
                    raw = from_wire.recv() => {
                        let Some(raw) = raw else { break; };
                        if transport.outgoing.send(raw).is_err() { break; }
                    }
                }
            }
            pump_closed.cancel();
            let _ = notices.send(HostEvent::Disconnected);
            // Dropping both transport directions closes locally even when the
            // peer never replies and the wire reader already drained requests.
        });
        let backend = match RemoteBackend::connect(
            Transport { incoming, outgoing },
            PeerInfo {
                name: "loop attach".into(),
                version: env!("CARGO_PKG_VERSION").into(),
            },
            Arc::new(Host(events_tx)),
        )
        .await
        {
            Ok(backend) => backend,
            Err(error) => {
                closed.cancel();
                return Err(error.into());
            }
        };
        Ok((AttachClient { backend, closed }, events))
    }

    fn list_text(sessions: &mut [SessionInfo]) -> String {
        sessions.sort_by_key(|s| std::cmp::Reverse(s.last_active_ms));
        if sessions.is_empty() {
            return "No Sessions. Use `loop attach --new` to create one.\n".into();
        }
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_millis() as u64);
        let mut rows =
            vec![["ID", "FOLDER", "TITLE", "BUSY", "LAST ACTIVE", "PARENT"].map(String::from)];
        for s in sessions.iter() {
            rows.push([
                s.r#ref.key.to_string(),
                s.cwd.display().to_string(),
                clip(s.name.as_deref().unwrap_or("(untitled)"), 40),
                if s.busy { "yes" } else { "no" }.into(),
                ago(now, s.last_active_ms),
                s.parent
                    .as_ref()
                    .map_or("-".into(), |p| p.key.as_str().chars().take(8).collect()),
            ]);
        }
        let mut widths = [0; 6];
        for row in &rows {
            for (width, cell) in widths.iter_mut().zip(row) {
                *width = (*width).max(cell.chars().count());
            }
        }
        let mut text = String::new();
        for row in rows {
            let mut line = String::new();
            for (i, cell) in row.iter().enumerate() {
                if i + 1 == row.len() {
                    line.push_str(cell);
                } else {
                    line.push_str(cell);
                    let pad = widths[i] - cell.chars().count() + 2;
                    line.extend(std::iter::repeat_n(' ', pad));
                }
            }
            text.push_str(line.trim_end());
            text.push('\n');
        }
        text
    }

    /// At most `max` characters, marking a cut with an ellipsis.
    fn clip(text: &str, max: usize) -> String {
        let line = text.lines().next().unwrap_or_default();
        if line.chars().count() <= max && line.len() == text.len() {
            return line.into();
        }
        let mut clipped: String = line.chars().take(max - 1).collect();
        clipped.push('…');
        clipped
    }

    /// How long ago `then_ms` was, for a person: "just now", "42s ago", "2m ago".
    pub(super) fn ago(now_ms: u64, then_ms: u64) -> String {
        let secs = now_ms.saturating_sub(then_ms) / 1000;
        match secs {
            0..10 => "just now".into(),
            10..60 => format!("{secs}s ago"),
            60..3600 => format!("{}m ago", secs / 60),
            3600..86400 => format!("{}h ago", secs / 3600),
            _ => format!("{}d ago", secs / 86400),
        }
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
        first_choice: usize,
        session: SessionKey,
        /// Set when the request comes from a Subagent of the attached Session.
        from: Option<String>,
    }
    impl Pending {
        fn show(&self) -> String {
            show(&self.request, self.first_choice, self.from.as_deref())
        }
    }
    fn show(request: &InteractionRequest, first_choice: usize, from: Option<&str>) -> String {
        let mut text = match from {
            Some(from) => format!("\nEscalation from {from}: "),
            None => String::from("\nEscalation: "),
        };
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
                    text.push_str(&format!("  {}. {}\n", i + first_choice, option.label));
                }
            }
            InteractionRequest::Question {
                prompt, choices, ..
            } => {
                text.push_str(&format!("{prompt}\n"));
                for (i, choice) in choices.iter().flatten().enumerate() {
                    text.push_str(&format!("  {}. {}\n", i + first_choice, choice.label));
                }
            }
            _ => text.push_str("This request cannot be presented.\n"),
        }
        text
    }
    fn answer(
        request: &InteractionRequest,
        line: &str,
        first_choice: usize,
    ) -> Option<InteractionOutcome> {
        let index = line
            .trim()
            .parse::<usize>()
            .ok()
            .and_then(|i| i.checked_sub(first_choice));
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

    /// "Subagent 1a2b3c4d (its task)" for a child Session's request.
    async fn subagent_label(backend: &RemoteBackend, child: &SessionRef) -> String {
        let short: String = child.key.as_str().chars().take(8).collect();
        let name = tokio::time::timeout(std::time::Duration::from_secs(2), backend.list())
            .await
            .ok()
            .and_then(Result::ok)
            .and_then(|list| list.into_iter().find(|s| s.r#ref.key == child.key))
            .and_then(|s| s.name);
        match name {
            Some(name) => format!("Subagent {short} ({name})"),
            None => format!("Subagent {short}"),
        }
    }

    /// One line on connect: which Session, where, and how to leave.
    fn header(what: &str, session: &SessionRef, cwd: &std::path::Path) -> String {
        format!(
            "{what} {} in {}. Ctrl+D or Ctrl+C twice detaches; the Session keeps running.\n",
            session.key,
            cwd.display()
        )
    }

    /// The lease refusal suggests `loop attach`, which is what just failed.
    fn load_error(error: loop_protocol::BackendError) -> anyhow::Error {
        let text = error.to_string();
        match text.split_once(" Use `loop attach") {
            Some((held, _)) if text.contains(" is in use by ") => anyhow::anyhow!(
                "{held} Close it there, or wait until that process has exited for 30 s, then attach again."
            ),
            _ => error.into(),
        }
    }

    pub async fn drive(
        client: AttachClient,
        mut events: mpsc::UnboundedReceiver<HostEvent>,
        args: AttachArgs,
        terminal: &mut dyn Terminal,
    ) -> anyhow::Result<()> {
        let backend = client.backend.clone();
        let (session, header) = if args.new {
            let cwd = std::path::absolute(args.cwd.unwrap_or(std::env::current_dir()?))?;
            let session = backend
                .new_session(SessionSpec {
                    cwd: Some(cwd.clone()),
                    ..Default::default()
                })
                .await?;
            let header = header("New Session", &session, &cwd);
            (session, header)
        } else {
            let mut sessions = backend.list().await?;
            let Some(id) = args.id else {
                let hidden = if args.all {
                    0
                } else {
                    let before = sessions.len();
                    sessions.retain(|s| s.live || s.parent.is_none());
                    before - sessions.len()
                };
                let mut text = list_text(&mut sessions);
                if hidden > 0 {
                    text.push_str(&format!(
                        "\n{hidden} finished Subagent Session(s) not shown; `loop attach --all` lists them.\n"
                    ));
                }
                terminal.write(&text)?;
                client.shutdown().await;
                return Ok(());
            };
            let mut found = matches(&sessions, &id);
            match found.len() {
                0 => {
                    client.shutdown().await;
                    anyhow::bail!("No Session matches `{id}`");
                }
                1 => {
                    let found = found.remove(0);
                    let header =
                        |session: &SessionRef| header("Attached to Session", session, &found.cwd);
                    // A Subagent runs only for its parent and is never reloaded;
                    // its Event Log stays readable.
                    if found.live || found.parent.is_some() {
                        let header = if found.live {
                            header(&found.r#ref)
                        } else {
                            format!(
                                "Replaying finished Subagent Session {} in {}. It cannot take prompts; Ctrl+D detaches.\n",
                                found.r#ref.key,
                                found.cwd.display()
                            )
                        };
                        (found.r#ref.clone(), header)
                    } else {
                        // Not loaded (e.g. after a Runtime restart). Loading takes
                        // its lease, so prompts typed here can run.
                        match backend
                            .load_session(&found.r#ref, SessionSpec::default())
                            .await
                        {
                            Ok(session) => {
                                let header = header(&session);
                                (session, header)
                            }
                            Err(error) => {
                                client.shutdown().await;
                                return Err(load_error(error));
                            }
                        }
                    }
                }
                _ => {
                    terminal.write(&list_text(&mut found))?;
                    client.shutdown().await;
                    anyhow::bail!("Ambiguous Session prefix `{id}`; use a longer id");
                }
            }
        };
        terminal.write(&header)?;
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
        let detached = client
            .cleanup_rpc(backend.detach(&session))
            .await
            .and_then(|result| result.map_err(anyhow::Error::from));
        // Never enqueue a fresh RPC after detach observes a dead transport.
        if detached.is_ok() {
            client.shutdown().await;
        }
        result?;
        detached
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
        let mut deferred = VecDeque::new();
        let mut pending: Vec<Pending> = Vec::new();
        // Numbers are request identities, never reused by this Surface. A
        // buffered answer remains tied to its original Escalation even if a
        // withdrawal wins the race with terminal input.
        let mut next_choice = 1;
        let mut withdrawn = HashSet::new();
        // Origins of requests not yet received, in arrival order.
        let mut origins: Vec<(loop_protocol::InteractionId, SessionRef)> = Vec::new();
        let mut labels: std::collections::HashMap<SessionKey, String> = Default::default();
        let mut interrupted = false;
        let mut renderer = crate::print_mode::TextRenderer::default();
        let mut prompts = tokio::task::JoinSet::new();
        loop {
            tokio::select! {
                Some(event) = events.recv() => match event {
                    HostEvent::Origin(id, from) => origins.push((id, from)),
                    HostEvent::Request(request, reply) => {
                        let request = *request;
                        let from = origins
                            .iter()
                            .position(|(id, _)| id == request.id())
                            .map(|i| origins.remove(i).1)
                            .unwrap_or_else(|| session.clone());
                        if withdrawn.remove(&(from.key.clone(), request.id().clone())) { let _ = reply.send(InteractionOutcome::Cancelled); continue; }
                        if !matches!(request, InteractionRequest::Permission {..} | InteractionRequest::Question {..}) {
                            let _ = reply.send(InteractionOutcome::Rejected { reason: "loop attach supports choices and text questions".into() }); continue;
                        }
                        let first_choice = next_choice;
                        next_choice += match &request {
                            InteractionRequest::Permission { options, .. } => options.len().max(1),
                            InteractionRequest::Question { choices, .. } => choices.as_ref().map_or(1, |c| c.len().max(1)),
                            _ => 1,
                        };
                        let label = if from.key == session.key {
                            None
                        } else {
                            if !labels.contains_key(&from.key) {
                                labels.insert(from.key.clone(), subagent_label(backend, &from).await);
                            }
                            labels.get(&from.key).cloned()
                        };
                        let item = Pending { request, reply, first_choice, session: from.key, from: label };
                        if pending.is_empty() { terminal.write(&item.show())?; }
                        pending.push(item);
                    }
                    HostEvent::Withdrawn(params) => {
                        let key = (params.session.key.clone(), params.id.clone());
                        let held = pending.iter().position(|p| p.session == key.0 && p.request.id() == &key.1);
                        if let Some(index) = held {
                            let item = pending.remove(index);
                            let _ = item.reply.send(InteractionOutcome::Cancelled);
                        } else if key.0 == session.key || origins.iter().any(|(id, s)| s.key == key.0 && *id == key.1) {
                            // It may arrive before its request does.
                            withdrawn.insert(key.clone());
                        }
                        if held.is_some() || key.0 == session.key {
                            terminal.write("\r\x1b[2KAnother client answered; Escalation withdrawn.\n")?;
                            if let Some(next) = pending.first() { terminal.write(&next.show())?; }
                        }
                    }
                    HostEvent::Disconnected => return Err(LostContact.into()),
                },
                update = updates.recv() => {
                    let envelope = update.map_err(|e| anyhow::anyhow!("Runtime updates interrupted ({e}); reattach to replay"))?;
                    if envelope.session != session.key { continue; }
                    last_seq = envelope.seq;
                    let text = renderer.render(&envelope.update);
                    if !text.is_empty() { terminal.write(&text)?; }
                    if let loop_protocol::SessionUpdate::TurnEnded { error, stop, .. } = envelope.update {
                        if stop == loop_protocol::StopReason::RuntimeRestarted {
                            terminal.write("Turn ended uncertain — Runtime restarted. Its tools may or may not have run; nothing was retried.\n")?;
                        }
                        if let Some(error) = error { terminal.write(&format!("loop: {}\n", error.message))?; }
                    }
                },
                Some(result) = prompts.join_next(), if !prompts.is_empty() => {
                    if let Err(error) = result? { terminal.write(&format!("loop: {error}\n"))?; }
                },
                // Lines and piped EOF wait for the snapshot, but controls must
                // stay responsive even while history is still being delivered.
                input = async {
                    if last_seq >= replay_until {
                        if let Some(input) = deferred.pop_front() { return Ok(input); }
                    }
                    terminal.read().await
                } => {
                    let input = input?;
                    if last_seq < replay_until && !matches!(input, Input::Interrupt) {
                        deferred.push_back(input);
                        continue;
                    }
                    match input {
                    Input::Eof => return Ok(()),
                    Input::Interrupt if interrupted => return Ok(()),
                    Input::Interrupt => {
                        interrupted = true;
                        backend.cancel(session);
                        terminal.write("\nTurn cancelled. Ctrl+C again to detach.\n")?;
                    }
                    Input::Line(line) => {
                        let explicit_prompt = line.starts_with("> ");
                        let line = line.strip_prefix("> ").unwrap_or(&line).to_owned();
                        if let Some(item) = pending.first().filter(|_| !explicit_prompt) {
                            if let Some(outcome) = answer(&item.request, &line, item.first_choice) {
                                let item = pending.remove(0);
                                let _ = item.reply.send(outcome);
                                if let Some(next) = pending.first() { terminal.write(&next.show())?; }
                            } else { terminal.write("Enter one of the numbered choices.\n")?; }
                        } else if !explicit_prompt && next_choice > 1 && line.trim().parse::<usize>().is_ok() {
                            terminal.write("Choice no longer active; ignored. Start a numeric prompt with `> `.\n")?;
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

    #[cfg(test)]
    mod tests {
        use super::ago;

        #[test]
        fn last_activity_reads_as_a_relative_time() {
            let now = 10_000_000_000;
            assert_eq!(ago(now, now), "just now");
            assert_eq!(ago(now, now + 5_000), "just now");
            assert_eq!(ago(now, now - 9_000), "just now");
            assert_eq!(ago(now, now - 42_000), "42s ago");
            assert_eq!(ago(now, now - 2 * 60_000 - 5_000), "2m ago");
            assert_eq!(ago(now, now - 3 * 3_600_000), "3h ago");
            assert_eq!(ago(now, now - 49 * 3_600_000), "2d ago");
        }
    }
}
