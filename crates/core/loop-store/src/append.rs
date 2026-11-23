//! The append request of the planned `EventWriter` (ADR-0007 §8).
//! Defined for steps B4 and B5. Nothing uses it yet.

use crate::event::Event;
use crate::lease::Fence;

/// One append request.
#[derive(Debug, Clone, PartialEq)]
pub struct Append {
    /// Records of one Session, or Runtime records only.
    pub events: Vec<Event>,
    /// Required for Session records of a leased Session. Its tenant must
    /// equal each event's tenant. Its epoch is stored as `lease_epoch`.
    pub fence: Option<Fence>,
    /// `true`: one transaction with no size cap (a turn's tree commit).
    pub atomic: bool,
    /// Moves the Session's recovery mark in the same transaction (§2.5).
    pub recovered_through: Option<u64>,
}

impl Append {
    /// An unfenced, non-atomic append of these events.
    pub fn new(events: Vec<Event>) -> Self {
        Self {
            events,
            fence: None,
            atomic: false,
            recovered_through: None,
        }
    }
}
