//! The bounded event buffer of one process.
//!
//! Producers (pipe and PTY readers, the exit) never wait: when the output
//! would pass the limit, the oldest output events are evicted. Each reader
//! has its own cursor; a reader whose next events were evicted gets one
//! `Dropped` event in their place (see `loop_workspace::process`).

use std::collections::VecDeque;

use loop_workspace::{ExitState, ProcessEvent, ProcessEventKind, StreamId};
use parking_lot::Mutex;
use tokio::sync::watch;

/// Bytes an event costs in the buffer besides its data, so a flood of tiny
/// events is bounded too.
const EVENT_OVERHEAD: usize = 64;

struct Stored {
    event: ProcessEvent,
    /// Output bytes produced up to and including this event.
    end: u64,
    cost: usize,
}

struct State {
    events: VecDeque<Stored>,
    cost: usize,
    limit: usize,
    next_seq: u64,
    produced: u64,
    /// `seq` of the newest evicted event; 0 when none was evicted.
    evicted_seq: u64,
    /// Output bytes produced up to and including that event.
    evicted_end: u64,
    exit: Option<(u64, ExitState)>,
}

/// Where one reader is.
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct Cursor {
    /// The `seq` of the last event this reader got.
    pub seq: u64,
    /// Output bytes up to and including that event; `None` when unknown
    /// (an attach to an evicted `seq`).
    pub end: Option<u64>,
}

/// The event buffer.
pub(crate) struct Ring {
    state: Mutex<State>,
    changed: watch::Sender<u64>,
}

/// What a reader gets now.
pub(crate) enum Next {
    /// An event.
    Event(ProcessEvent),
    /// Nothing yet; wait for a change.
    Pending,
    /// The exit event was read; nothing follows.
    Done,
}

impl Ring {
    /// A buffer that keeps about `limit` bytes of output.
    pub(crate) fn new(limit: usize) -> Self {
        Self {
            state: Mutex::new(State {
                events: VecDeque::new(),
                cost: 0,
                limit,
                next_seq: 1,
                produced: 0,
                evicted_seq: 0,
                evicted_end: 0,
                exit: None,
            }),
            changed: watch::channel(0).0,
        }
    }

    /// A receiver that wakes on every change.
    pub(crate) fn subscribe(&self) -> watch::Receiver<u64> {
        self.changed.subscribe()
    }

    fn notify(&self) {
        self.changed.send_modify(|v| *v = v.wrapping_add(1));
    }

    /// Add output. Never waits; evicts the oldest output when full. Output
    /// after the exit is ignored.
    pub(crate) fn push_output(&self, stream: StreamId, data: Vec<u8>) {
        if data.is_empty() {
            return;
        }
        {
            let mut s = self.state.lock();
            if s.exit.is_some() {
                return;
            }
            let cost = data.len() + EVENT_OVERHEAD;
            while s.cost + cost > s.limit {
                let Some(old) = s.events.pop_front() else {
                    break;
                };
                s.cost -= old.cost;
                s.evicted_seq = old.event.seq;
                s.evicted_end = old.end;
            }
            s.produced += data.len() as u64;
            let seq = s.next_seq;
            s.next_seq += 1;
            let end = s.produced;
            s.cost += cost;
            s.events.push_back(Stored {
                event: ProcessEvent {
                    seq,
                    stream,
                    kind: ProcessEventKind::Output(data),
                },
                end,
                cost,
            });
        }
        self.notify();
    }

    /// Add the exit event (once). It is never evicted.
    pub(crate) fn push_exit(&self, exit: ExitState) {
        {
            let mut s = self.state.lock();
            if s.exit.is_some() {
                return;
            }
            let seq = s.next_seq;
            s.next_seq += 1;
            s.exit = Some((seq, exit));
        }
        self.notify();
    }

    /// Whether `more` bytes of output would evict an event now.
    pub(crate) fn full(&self, more: usize) -> bool {
        let s = self.state.lock();
        s.cost + more + EVENT_OVERHEAD > s.limit
    }

    /// The exit, once it happened.
    pub(crate) fn exit(&self) -> Option<ExitState> {
        self.state.lock().exit.map(|(_, e)| e)
    }

    /// The `seq` of the newest event (0 before the first).
    pub(crate) fn last_seq(&self) -> u64 {
        self.state.lock().next_seq - 1
    }

    /// A cursor for a reader that starts after `after_seq`.
    pub(crate) fn cursor_after(&self, after_seq: u64) -> Cursor {
        let s = self.state.lock();
        let end = if after_seq == 0 {
            Some(0)
        } else if after_seq == s.evicted_seq {
            Some(s.evicted_end)
        } else {
            s.events
                .iter()
                .find(|e| e.event.seq == after_seq)
                .map(|e| e.end)
                .or_else(|| (after_seq >= s.next_seq - 1).then_some(s.produced))
        };
        Cursor {
            seq: after_seq,
            end,
        }
    }

    /// The next event for `cursor`, and move it.
    pub(crate) fn next(&self, cursor: &mut Cursor) -> Next {
        let s = self.state.lock();
        if cursor.seq < s.evicted_seq {
            let lost = s.evicted_end - cursor.end.unwrap_or(0).min(s.evicted_end);
            cursor.seq = s.evicted_seq;
            cursor.end = Some(s.evicted_end);
            return Next::Event(ProcessEvent {
                seq: s.evicted_seq,
                stream: StreamId::Control,
                kind: ProcessEventKind::Dropped { bytes: lost },
            });
        }
        // Events are in `seq` order, so the first after the cursor is at
        // a known index.
        let first = s.events.front().map(|e| e.event.seq);
        if let Some(first) = first {
            let index = cursor.seq.saturating_add(1).saturating_sub(first) as usize;
            if let Some(stored) = s.events.get(index) {
                cursor.seq = stored.event.seq;
                cursor.end = Some(stored.end);
                return Next::Event(stored.event.clone());
            }
        }
        match s.exit {
            Some((seq, exit)) if cursor.seq < seq => {
                cursor.seq = seq;
                Next::Event(ProcessEvent {
                    seq,
                    stream: StreamId::Control,
                    kind: ProcessEventKind::Exited(exit),
                })
            }
            Some(_) => Next::Done,
            None => Next::Pending,
        }
    }

    /// Wait for and return the next event for `cursor`; `None` after the
    /// exit event.
    pub(crate) async fn next_event(
        &self,
        cursor: &mut Cursor,
        changes: &mut watch::Receiver<u64>,
    ) -> Option<ProcessEvent> {
        loop {
            changes.borrow_and_update();
            match self.next(cursor) {
                Next::Event(event) => return Some(event),
                Next::Done => return None,
                Next::Pending => {
                    // The sender lives as long as `self`.
                    if changes.changed().await.is_err() {
                        return None;
                    }
                }
            }
        }
    }

    /// Bytes the buffer holds now (with the per-event overhead).
    #[cfg(test)]
    pub(crate) fn held(&self) -> usize {
        self.state.lock().cost
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn out(ring: &Ring, cursor: &mut Cursor) -> Vec<(u64, ProcessEventKind)> {
        let mut seen = Vec::new();
        while let Next::Event(e) = ring.next(cursor) {
            seen.push((e.seq, e.kind));
        }
        seen
    }

    #[test]
    fn seq_starts_at_one_and_grows_by_one() {
        let ring = Ring::new(1 << 20);
        ring.push_output(StreamId::Stdout, b"a".to_vec());
        ring.push_output(StreamId::Stderr, b"b".to_vec());
        ring.push_exit(ExitState::Exited { code: 0 });
        let mut cursor = Cursor::default();
        let seen = out(&ring, &mut cursor);
        assert_eq!(seen.iter().map(|(s, _)| *s).collect::<Vec<_>>(), [1, 2, 3]);
        assert!(matches!(ring.next(&mut cursor), Next::Done));
        assert_eq!(ring.last_seq(), 3);
    }

    #[test]
    fn eviction_gives_one_dropped_event_with_the_exact_bytes() {
        // Room for about three 100-byte events.
        let ring = Ring::new(3 * (100 + EVENT_OVERHEAD));
        let mut live = Cursor::default();
        ring.push_output(StreamId::Stdout, vec![b'x'; 100]);
        let Next::Event(first) = ring.next(&mut live) else {
            panic!("first event");
        };
        assert_eq!(first.seq, 1);
        for _ in 0..9 {
            ring.push_output(StreamId::Stdout, vec![b'x'; 100]);
        }
        assert!(ring.held() <= 3 * (100 + EVENT_OVERHEAD));
        let seen = out(&ring, &mut live);
        // Events 2..=7 were evicted: 600 bytes this reader did not get.
        assert_eq!(seen[0], (7, ProcessEventKind::Dropped { bytes: 600 }));
        assert_eq!(
            seen[1..].iter().map(|(s, _)| *s).collect::<Vec<_>>(),
            [8, 9, 10]
        );

        // A new reader from the start loses all 700 evicted bytes.
        let mut fresh = ring.cursor_after(0);
        assert_eq!(
            out(&ring, &mut fresh)[0],
            (7, ProcessEventKind::Dropped { bytes: 700 })
        );
        // A reader after a kept event gets no Dropped.
        let mut after = ring.cursor_after(8);
        assert_eq!(
            out(&ring, &mut after)
                .iter()
                .map(|(s, _)| *s)
                .collect::<Vec<_>>(),
            [9, 10]
        );
        // After an evicted one: an upper bound.
        let mut inside = ring.cursor_after(3);
        assert_eq!(
            out(&ring, &mut inside)[0],
            (7, ProcessEventKind::Dropped { bytes: 700 })
        );
    }

    #[test]
    fn the_exit_is_never_evicted_and_output_after_it_is_ignored() {
        let ring = Ring::new(2 * (10 + EVENT_OVERHEAD));
        ring.push_output(StreamId::Stdout, vec![1; 10]);
        ring.push_exit(ExitState::Signaled { signal: 9 });
        ring.push_output(StreamId::Stdout, vec![2; 10]);
        ring.push_exit(ExitState::Lost);
        let mut c = Cursor::default();
        let seen = out(&ring, &mut c);
        assert_eq!(seen.len(), 2);
        assert_eq!(
            seen[1],
            (
                2,
                ProcessEventKind::Exited(ExitState::Signaled { signal: 9 })
            )
        );
        assert_eq!(ring.exit(), Some(ExitState::Signaled { signal: 9 }));
    }

    #[tokio::test]
    async fn a_waiting_reader_wakes_on_output() {
        let ring = std::sync::Arc::new(Ring::new(1 << 16));
        let reader = {
            let ring = ring.clone();
            tokio::spawn(async move {
                let mut cursor = Cursor::default();
                let mut changes = ring.subscribe();
                let mut got = Vec::new();
                while let Some(e) = ring.next_event(&mut cursor, &mut changes).await {
                    got.push(e.seq);
                }
                got
            })
        };
        for _ in 0..50 {
            ring.push_output(StreamId::Stdout, b"z".to_vec());
            tokio::task::yield_now().await;
        }
        ring.push_exit(ExitState::Exited { code: 0 });
        let got = reader.await.unwrap();
        assert_eq!(got, (1..=51).collect::<Vec<_>>());
    }
}
