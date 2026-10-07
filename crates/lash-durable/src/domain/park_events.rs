//! The operator's park feed (L6): one entry per park of an actor
//! (activation loop, undecodable state), per redrive and per end of a parked
//! actor.

use crate::ids::{ActorKey, DurableInstant};

/// A park-feed entry's position.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ParkEventSeq(pub i64);

/// What a park-feed entry records.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ParkEventKind {
    /// The actor parked.
    Parked,
    /// An operator redrove it.
    Redriven,
    /// It ended while parked.
    Ended,
}

impl ParkEventKind {
    /// The stored spelling.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Parked => "parked",
            Self::Redriven => "redriven",
            Self::Ended => "ended",
        }
    }

    /// The stored spelling read back; `None` for anything else.
    #[must_use]
    pub fn parse(stored: &str) -> Option<Self> {
        match stored {
            "parked" => Some(Self::Parked),
            "redriven" => Some(Self::Redriven),
            "ended" => Some(Self::Ended),
            _ => None,
        }
    }
}

/// One park-feed entry.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ParkEventRow {
    /// Its position.
    pub seq: ParkEventSeq,
    /// The actor.
    pub actor: ActorKey,
    /// What happened.
    pub kind: ParkEventKind,
    /// Its typed reason (a park's) or its requester (a redrive's), encoded
    /// by its owner.
    pub reason_json: String,
    /// When.
    pub at: DurableInstant,
}

/// A park-feed write inside an owner commit.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ParkEventWrite {
    /// Park the committing actor with `reason_json`, and append the feed's
    /// `parked` entry. The commit releases it [`Release::Parked`](crate::Release::Parked).
    Park {
        /// Its typed reason.
        reason_json: String,
    },
    /// The committing actor ends while parked: clear its park and append
    /// the feed's `ended` entry.
    Ended {
        /// The terminal, encoded by its owner.
        reason_json: String,
    },
}
