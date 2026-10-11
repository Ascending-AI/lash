//! Admitted model calls (P2, FIG-5256; P5, FIG-5259; ADR 0133 §5, §6): each
//! admitted call's root, holding its prompt snapshot (the audit record) and
//! its request (the exact provider body, its response context and an owned
//! call's deadline), and the text both reference (section texts, body and
//! contract chunks), stored once by content and shared across calls.
//!
//! The two parts live apart (FIG-5946). The request is read only to send the
//! call, so the commit that settles the call releases it
//! ([`PromptWrite::Settle`]): its edges go, and so does every text no other
//! edge names. The snapshot outlives the turn: only [`PromptWrite::Release`],
//! the explicit retention, removes it, and a text goes with the last edge
//! that references it.

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

/// One stored text (a section text, or a chunk of a body or contract),
/// under the content address its owner computed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PromptText {
    /// The text's content address.
    pub hash: String,
    /// The exact UTF-8 text.
    pub text: String,
}

/// One call's root, as its admission recorded it, while it is retained.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PromptSnapshotRow {
    /// The call.
    pub call: PromptCallKey,
    /// The prompt snapshot, encoded by its owner; `None` when the call
    /// composed no sections.
    pub snapshot: Option<String>,
    /// The request, encoded by its owner; `None` once the call settled.
    pub request: Option<String>,
    /// The content address of every text the snapshot references, sorted.
    pub snapshot_texts: Vec<String>,
    /// The content address of every text the request references, sorted;
    /// empty once the call settled.
    pub request_texts: Vec<String>,
    /// The epoch of the commit that recorded it.
    pub written_epoch: Epoch,
}

/// A prompt write inside an owner commit.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PromptWrite {
    /// Record `call`'s root: its `snapshot` over `snapshot_texts` and its
    /// `request` over `request_texts`. Each text carries its body, and the
    /// store binds only the bodies it does not already hold: a text already
    /// stored is shared, not written or sent again. Refused with
    /// [`DomainRefusal::PromptCallRecorded`](super::DomainRefusal::PromptCallRecorded)
    /// when the call already has a root: a call composes once.
    Record {
        /// The call.
        call: PromptCallKey,
        /// The prompt snapshot, encoded by its owner; `None` when the call
        /// composed no sections.
        snapshot: Option<String>,
        /// The request, encoded by its owner.
        request: String,
        /// Every distinct text the snapshot references.
        snapshot_texts: Vec<PromptText>,
        /// Every distinct text the request references.
        request_texts: Vec<PromptText>,
    },
    /// The settlement of turn `run`'s calls (FIG-5946): release the request
    /// of each of them but `open`, the call this commit admits, with every
    /// text no remaining edge references. A settled call's snapshot stays,
    /// and a root with no snapshot goes with its request. It belongs in
    /// every commit that leaves the turn's model phase, and in the one that
    /// admits a new call: no call is resent after either.
    Settle {
        /// The session.
        session: SessionId,
        /// The turn.
        run: TurnId,
        /// The call this commit admits, whose request stays.
        open: Option<u32>,
    },
    /// The explicit retention: release every root of `session`, or of its
    /// turn `run` alone (the calls [`ModelCallId::Turn`] names), and every
    /// text no remaining root references.
    Release {
        /// The session.
        session: SessionId,
        /// The one turn released, or `None` for every turn of the session.
        run: Option<TurnId>,
    },
}
