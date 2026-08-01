//! SQLite session store + optional FTS search (in-crate).

mod branch_cache;
mod materialize;
mod search;
mod store;

pub use search::create_sqlite_session_search;
pub use store::create_sqlite_session_store;
