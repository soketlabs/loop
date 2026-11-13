//! Paged reads (ADR-0007 §8). Defined for step B2. Nothing uses them yet.

use serde::{Deserialize, Serialize};

/// A read position: a `session_seq` or a `global_seq`, as the method says.
/// A read returns records strictly after the cursor.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Default, Serialize, Deserialize,
)]
#[serde(transparent)]
pub struct Cursor(pub u64);

/// One page of a read. `next` is `None` on the last page.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Page<T> {
    /// The items, in the order of the read.
    pub items: Vec<T>,
    /// Where the next page starts.
    pub next: Option<Cursor>,
}

impl<T> Page<T> {
    /// A page with no more pages after it.
    pub fn last(items: Vec<T>) -> Self {
        Self { items, next: None }
    }
}
