//! Prompt snapshots (P2, FIG-5256; ADR 0133 §5): each admitted model call's
//! snapshot, an audit root, and the section text it references, stored once
//! by content and shared across calls.
//!
//! A snapshot root outlives the turn that wrote it: ending the turn, which
//! prunes its phase rows, leaves the root and its text in place. Only
//! [`PromptWrite::Release`], the explicit retention, removes roots, and a
//! text goes with the last root that references it.

use lash_sansio::{SessionId, TurnId};

use crate::ids::Epoch;

/// One model call: the session, the turn and the call's ordinal within it.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct PromptCallKey {
    /// The session.
    pub session: SessionId,
    /// The turn.
    pub run: TurnId,
    /// The call's ordinal within the turn: distinct for every new call.
    pub call: u32,
}

/// One section text, under the content address its owner computed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PromptText {
    /// The text's content address.
    pub hash: String,
    /// The exact UTF-8 text.
    pub text: String,
}

/// One call's recorded snapshot.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PromptSnapshotRow {
    /// The call.
    pub call: PromptCallKey,
    /// The snapshot, encoded by its owner.
    pub snapshot: String,
    /// The content address of every text the snapshot references, sorted.
    pub texts: Vec<String>,
    /// The epoch of the commit that recorded it.
    pub written_epoch: Epoch,
}

/// A prompt write inside an owner commit.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PromptWrite {
    /// Record `call`'s snapshot as a root over `texts`. A text already
    /// stored is shared, not written again. Refused with
    /// [`DomainRefusal::PromptCallRecorded`](super::DomainRefusal::PromptCallRecorded)
    /// when the call already has a snapshot: a call composes once.
    Record {
        /// The call.
        call: PromptCallKey,
        /// The snapshot, encoded by its owner.
        snapshot: String,
        /// Every distinct text the snapshot references.
        texts: Vec<PromptText>,
    },
    /// The explicit retention: release every snapshot root of `session`,
    /// or of its turn `run` alone, and every text no remaining root
    /// references.
    Release {
        /// The session.
        session: SessionId,
        /// The one turn released, or `None` for every turn of the session.
        run: Option<TurnId>,
    },
}
