//! Admitted model calls (P2, FIG-5256; P5, FIG-5259; ADR 0133 §5, §6): each
//! admitted call's record, an audit root holding its prompt snapshot and its
//! exact provider body, and the text it references (section texts and body
//! chunks), stored once by content and shared across calls.
//!
//! A snapshot root outlives the turn that wrote it: ending the turn, which
//! prunes its phase rows, leaves the root and its text in place. Only
//! [`PromptWrite::Release`], the explicit retention, removes roots, and a
//! text goes with the last root that references it.

use lash_sansio::{SessionId, TurnId};

use crate::ids::Epoch;

/// One admitted model call: the session it belongs to and its identity
/// within the execution that owns it (ADR 0133 §6, §8).
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct PromptCallKey {
    /// The session.
    pub session: SessionId,
    /// The call.
    pub call: ModelCallId,
}

/// Which model call, under which owner. A turn's calls are its ordinals; a
/// compaction's or direct completion's call is owned by the execution that
/// makes it and keyed by its stable identity there, with no turn required.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum ModelCallId {
    /// Model call `ordinal` of turn `run`: distinct for every new call.
    Turn {
        /// The turn.
        run: TurnId,
        /// The call's ordinal among the turn's model calls.
        ordinal: u32,
    },
    /// A call the execution scope `owner` makes, keyed by `key`, its stable
    /// identity within that scope.
    Owned {
        /// The owning execution scope.
        owner: String,
        /// The call's stable key within its owner.
        key: String,
    },
}

impl ModelCallId {
    /// The stored owner column: `turn:<run>` or `owned:<scope>`.
    #[must_use]
    pub fn owner_column(&self) -> String {
        match self {
            Self::Turn { run, .. } => Self::turn_owner(run),
            Self::Owned { owner, .. } => format!("owned:{owner}"),
        }
    }

    /// The stored call column: the turn's ordinal, or the owned call's key.
    #[must_use]
    pub fn call_column(&self) -> String {
        match self {
            Self::Turn { ordinal, .. } => ordinal.to_string(),
            Self::Owned { key, .. } => key.clone(),
        }
    }

    /// The owner column of every call of turn `run`.
    #[must_use]
    pub fn turn_owner(run: &TurnId) -> String {
        format!("turn:{}", run.as_str())
    }
}

impl std::fmt::Display for ModelCallId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Turn { run, ordinal } => write!(f, "call {ordinal} of turn {run}"),
            Self::Owned { owner, key } => write!(f, "call {key} of {owner}"),
        }
    }
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
    /// or of its turn `run` alone (the calls [`ModelCallId::Turn`] names),
    /// and every text no remaining root references.
    Release {
        /// The session.
        session: SessionId,
        /// The one turn released, or `None` for every turn of the session.
        run: Option<TurnId>,
    },
}
