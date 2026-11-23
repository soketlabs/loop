//! Streaming processes, PTYs and file transfer of the host Workspace
//! (Unix only).

mod process;
mod pty;
mod ring;
mod transfer;
mod tree;

pub use process::{kill_all_host_processes, kill_tree_without_listing, leaked_host_processes};
pub(crate) use process::{Launch, ProcessTable};
pub(crate) use transfer::{Transfers, MAX_UPLOAD_BYTES, UPLOAD_IDLE};
