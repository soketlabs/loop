//! Append-only Event Log shared by Runtime pumps and recovery.
//!
//! Appends return only after commit, with durable Update sequences and generation
//! stamped into the envelopes. Audit events never consume a Session cursor.
//!
//! The contract (records, errors and the `EventLog` trait) is in `loop-store`.
//! This module re-exports it at its old paths and holds the SQLite and memory
//! implementations.

mod memory;
mod sqlite;
pub use memory::MemoryEventLog;
pub use sqlite::SqliteEventLog;

/// The Event Log error. It is `loop_store::StoreError`; `WriterStopped` is
/// now `Stopped`.
pub use loop_store::StoreError as EventLogError;
pub use loop_store::{
    is_recovery_record, Event, EventKind, EventLog, LoggedEvent, Result, MAX_BATCH_EVENTS,
    WRITER_QUEUE_CAPACITY,
};

use loop_store::{stamp, validate_batch as validate};
