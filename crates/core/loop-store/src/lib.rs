//! Storage contracts for Loop (ADR-0007).
//!
//! This crate has the record types, the error type and the traits of Loop's
//! stores. It has no database driver and no `rusqlite` types. The SQLite
//! and memory Event Logs are in `loop-runtime` for now; a Postgres store can
//! live outside this repository and implement the same traits.
//!
//! Dependency rule: this crate depends only on `loop-protocol` (for
//! `SessionKey`, `TurnId`, `UpdateEnvelope` and the wire update types). It
//! must never depend on `loop-runtime` or `loop-agent`, so that both can
//! depend on it.
//!
//! # Live now
//!
//! - [`event`]: `Event`, `EventKind`, `LoggedEvent` (alias `Committed`),
//!   `MAX_BATCH_EVENTS`, `WRITER_QUEUE_CAPACITY`, `is_recovery_record`,
//!   `validate_batch`, `stamp`.
//! - [`log`]: the present `EventLog` trait.
//! - [`error`]: `StoreError` (was `EventLogError`) and `Result`.
//! - [`records`]: the typed `policy_decision` record (S4 §12), which the
//!   Runtime writes as `EventKind::PolicyDecision`.
//! - [`identity`]: `Tenant`, `Principal`, `PrincipalKind` and `Via`, the
//!   protocol's own types re-exported, so there is one definition of each.
//!
//! `loop_runtime::log` re-exports all of these at their old paths.
//!
//! # Defined for later steps, not used yet
//!
//! - [`identity`]: `Identity`, `SystemScope` (B3).
//! - [`lease`]: `Generation`, `OwnerInfo`, `Fence` (B4, chain D).
//! - [`page`]: `Cursor`, `Page` (B2).
//! - [`append`]: `Append` (B4, B5).
//! - [`records`]: payloads of `tree`, `session_deleted`, and the
//!   `session_opened` origin (B5).
//! - [`tree`]: `SessionTreeEntry`, `SessionMetadata` (B5).
//! - [`delta`]: `Delta` and the new `message_ended` fields (chain A; these
//!   move to `loop-protocol`).
//! - [`class`]: durable and live-only classes of updates and records (A0, A4).
//! - [`store`]: `EventWriter`, `EventReader`, `TreeReader`, `LeaseStore`,
//!   `ProjectionAdmin`, `Store`, `StoreCapabilities` (B1 to B7).

pub mod append;
pub mod class;
pub mod delta;
pub mod error;
pub mod event;
pub mod identity;
pub mod lease;
pub mod log;
pub mod page;
pub mod records;
pub mod store;
pub mod tree;

pub use append::Append;
pub use error::{Result, StoreError};
pub use event::{
    is_recovery_record, stamp, validate_batch, Committed, Event, EventKind, LoggedEvent,
    MAX_BATCH_EVENTS, WRITER_QUEUE_CAPACITY,
};
pub use identity::{Identity, Principal, PrincipalKind, SystemScope, Tenant, Via};
pub use lease::{Fence, Generation, OwnerInfo};
pub use log::EventLog;
pub use page::{Cursor, Page};
pub use records::{
    DecidedBy, DecisionAnswer, DecisionOutcome, DecisionSource, DecisionSubject,
    PolicyDecisionRecord,
};
pub use store::{
    EventReader, EventWriter, LeaseStore, ProjectionAdmin, RebuildReport, SessionSummary, Store,
    StoreCapabilities, TreeReader, VerifyReport,
};
pub use tree::{SessionMetadata, SessionTreeEntry};
