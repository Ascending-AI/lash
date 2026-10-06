//! Process actor rows (L6): the process actor and registry row, its state
//! revision, cancel, terminal and cascade cursor (ADR 0132 §10, §11).

use crate::ids::{DurableInstant, Epoch};
use lash_sansio::ProcessId;

use super::keys::ScopeKey;

/// One process actor's row.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProcessActorRow {
    /// The process.
    pub process: ProcessId,
    /// Its engine state's revision; advances with every committed
    /// transition.
    pub state_rev: u64,
    /// When its cancel was first requested.
    pub cancel_requested_at: Option<DurableInstant>,
    /// Whether it is terminal.
    pub terminal: bool,
    /// The terminal's cascade cursor while `Until` children remain to be
    /// marked.
    pub cascade_cursor: Option<String>,
    /// The epoch of the commit that last wrote it.
    pub written_epoch: Epoch,
}

/// The rows a process start writes: the registry row and the process actor
/// as ready, in the transaction that admits the start.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProcessStartRows {
    /// The process.
    pub process: ProcessId,
    /// Its lifetime scope, when it is `Until` one.
    pub until: Option<ScopeKey>,
    /// The registration (definition, engine kind, engine config with its
    /// cancel grace), encoded by its owner.
    pub registration_json: String,
}

/// A process write inside an owner commit.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ProcessWrite {
    /// Register a process and create its actor ready.
    Register(ProcessStartRows),
    /// Commit one engine transition: the state revision moves on.
    Advance {
        /// The process.
        process: ProcessId,
        /// The revision replaced.
        expected_rev: u64,
    },
    /// End the process with its terminal, encoded by its owner.
    Terminal {
        /// The process.
        process: ProcessId,
        /// The terminal outcome.
        outcome_json: String,
    },
    /// Mark the next batch of `scope`'s `Until` children for cancel and
    /// record where the cascade got to; `None` once it is done.
    CascadeBatch {
        /// The ending scope.
        scope: ScopeKey,
        /// The cursor after this batch.
        cursor: Option<String>,
    },
}

/// A request to cancel a process, from outside its owner.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CancelRequest {
    /// The process.
    pub process: ProcessId,
    /// Who asked and why, encoded by its owner.
    pub origin_json: String,
}

/// The answer to a [`CancelRequest`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CancelAnswer {
    /// The first request: recorded at `at`, and the process woken.
    Requested {
        /// When.
        at: DurableInstant,
    },
    /// An earlier request stands, with its own timestamp.
    AlreadyRequested {
        /// The earlier request's time.
        at: DurableInstant,
    },
    /// The process is already terminal.
    AlreadyEnded,
}
