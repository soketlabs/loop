//! SQLite session store + optional FTS search (in-crate).

mod branch_cache;
mod leases;
mod materialize;
mod search;
mod store;

pub use leases::{
    create_sqlite_session_store_for_resume, LeaseEnvironment, LeaseIdentity, PidLiveness,
    SessionLease, LEASE_RENEW_INTERVAL_MS, LEASE_STALE_AFTER_MS,
};
pub use search::create_sqlite_session_search;
pub use store::{create_sqlite_session_store, SqliteSessionStore};
