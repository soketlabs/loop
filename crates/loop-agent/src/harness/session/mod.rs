//! Session tree, stores, repository, and search.

mod fork;
mod jsonl_store;
mod keyed_operation_queue;
mod memory_store;
mod repository;
mod search;
pub mod types;

#[cfg(feature = "sqlite")]
pub mod sqlite;

pub use fork::{read_session_entries_for_fork, SessionForkSelection};
pub use jsonl_store::create_jsonl_session_store;
pub use memory_store::create_in_memory_session_store;
pub use repository::{create_session_repository, SessionRepository};
pub use search::{create_scanning_session_search, find_session_entry_matches, SessionSearch};
pub use types::*;

#[cfg(feature = "sqlite")]
pub use sqlite::{create_sqlite_session_search, create_sqlite_session_store};
