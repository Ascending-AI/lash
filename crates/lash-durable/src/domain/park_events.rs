//! The operator's park feed (L6): one entry per park of an actor
//! (activation loop, undecodable formats) and per redrive.

use crate::ids::{ActorKey, DurableInstant};

/// A park-feed entry's position.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ParkEventSeq(pub i64);

/// One park-feed entry.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ParkEventRow {
    /// Its position.
    pub seq: ParkEventSeq,
    /// The actor.
    pub actor: ActorKey,
    /// What happened (`parked`, `redriven`, ...).
    pub kind: String,
    /// Its typed reason, encoded by its owner.
    pub reason_json: String,
    /// When.
    pub at: DurableInstant,
}

/// A park-feed write inside an owner commit.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ParkEventWrite {
    /// Append one entry for the committing actor.
    Append {
        /// What happened.
        kind: String,
        /// Its typed reason.
        reason_json: String,
    },
}
