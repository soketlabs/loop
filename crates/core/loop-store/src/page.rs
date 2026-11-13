//! Paged reads (ADR-0007 §8). Live from step B2: `EventLog::read_session_page`
//! and `EventLog::recovery_page` return these.

use serde::{Deserialize, Serialize};

/// A read position: a `session_seq` or a `global_seq`, as the method says.
/// A read returns records strictly after the cursor.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Default, Serialize, Deserialize,
)]
#[serde(transparent)]
pub struct Cursor(pub u64);

/// One page of a read. `next` is `None` on the last page.
///
/// `C` is the type of the position. Reads of records page by a sequence
/// ([`Cursor`], the default). Lists of Sessions page by a key, for example
/// `Page<SessionSummary, SessionKey>`: the next page starts strictly after
/// that key.
///
/// `next` is `Some` when the read stopped at its limit and at least one more
/// item existed when the page was read. Then `next` is the position of the
/// last item. `None` means that no item after the last one existed. Items
/// that commit later are read by a new call that starts at the last item.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Page<T, C = Cursor> {
    /// The items, in the order of the read.
    pub items: Vec<T>,
    /// Where the next page starts.
    pub next: Option<C>,
}

impl<T, C> Page<T, C> {
    /// A page with no more pages after it.
    pub fn last(items: Vec<T>) -> Self {
        Self { items, next: None }
    }
}

/// The size limit of one page of records.
///
/// A page holds at most `events` records. It also stops before the record
/// that would take its payload above `bytes`, but it always holds at least
/// one record, so one large record cannot stop a reader. The byte count is
/// the store's encoded payload size; it is a limit on memory, not an exact
/// wire size.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct PageLimit {
    /// Most records in one page. A value of 0 is read as 1.
    pub events: u32,
    /// Most payload bytes in one page (soft, see above).
    pub bytes: usize,
}

impl PageLimit {
    /// 1,024 records or 4 MiB.
    pub const DEFAULT: Self = Self {
        events: 1024,
        bytes: 4 * 1024 * 1024,
    };

    /// At most `events` records, with the default byte limit.
    pub fn events(events: u32) -> Self {
        Self {
            events,
            ..Self::DEFAULT
        }
    }

    /// The record limit, never 0.
    pub fn max_events(&self) -> usize {
        self.events.max(1) as usize
    }
}

impl Default for PageLimit {
    fn default() -> Self {
        Self::DEFAULT
    }
}

/// Cut `items` (all items after the read's start, in order) to one page.
/// `size` gives an item's byte count and `position` its cursor. The default
/// and the memory implementations use it; it reads at most one item past
/// the page.
pub fn cut<T, C>(
    items: impl IntoIterator<Item = T>,
    limit: PageLimit,
    size: impl Fn(&T) -> usize,
    position: impl Fn(&T) -> C,
) -> Page<T, C> {
    let mut page = Vec::new();
    let mut bytes = 0usize;
    for item in items {
        let item_bytes = size(&item);
        let full = page.len() >= limit.max_events()
            || (!page.is_empty() && bytes.saturating_add(item_bytes) > limit.bytes);
        if full {
            let next = page.last().map(&position);
            return Page { items: page, next };
        }
        bytes = bytes.saturating_add(item_bytes);
        page.push(item);
    }
    Page::last(page)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cut_stops_at_events_and_bytes_and_keeps_one_large_item() {
        let by_events = cut(1..=5u64, PageLimit::events(2), |_| 1, |i| Cursor(*i));
        assert_eq!(by_events.items, vec![1, 2]);
        assert_eq!(by_events.next, Some(Cursor(2)));
        let limit = PageLimit {
            events: 10,
            bytes: 5,
        };
        let by_bytes = cut([3usize, 2, 1], limit, |n| *n, |n| Cursor(*n as u64));
        assert_eq!(by_bytes.items, vec![3, 2]);
        assert_eq!(by_bytes.next, Some(Cursor(2)));
        let large = cut([100usize, 1], limit, |n| *n, |n| Cursor(*n as u64));
        assert_eq!(large.items, vec![100]);
        assert_eq!(large.next, Some(Cursor(100)));
        let exact = cut(1..=2u64, PageLimit::events(2), |_| 1, |i| Cursor(*i));
        assert_eq!(exact.next, None, "no item after the last one");
        let empty = cut(
            Vec::<u64>::new(),
            PageLimit::events(0),
            |_| 1,
            |i| Cursor(*i),
        );
        assert_eq!(empty, Page::last(Vec::new()));
    }
}
