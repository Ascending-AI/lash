//! A process's terminal transaction and its parks (ADR 0132 §3, §11).

use lash_durable::domain::{ParkEventWrite, ProcessWrite, ScopeKey, SnapshotWrite};
use lash_durable::{ActorTx, DomainWrite, DurableError, StoreFailure, StoreFailureKind};

use crate::runtime::actor::waits;
use crate::{CancelOrigin, ProcessId, ProcessOutcome};

/// Why a process actor parked: the park feed's typed reason. A parked
/// process runs no engine code until an operator redrives it; a cancel ends
/// it without running its engine.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "reason", rename_all = "snake_case", deny_unknown_fields)]
pub enum ProcessParkReason {
    /// Its activations crashed without progress `failed_activations` times
    /// in a row: the activation-loop budget.
    ActivationLoop {
        /// The claims in a row that committed nothing.
        failed_activations: u32,
    },
    /// No engine of its kind is installed on the node that claimed it.
    UnknownEngine {
        /// The engine kind its row names.
        kind: String,
    },
    /// Its engine state is in a format this node's engine does not read.
    UndecodableState {
        /// The format kind the state was written in.
        kind: String,
        /// Its version.
        version: u32,
    },
    /// Its driver row, what the actor records of its steps or of its child
    /// turn between transitions, does not decode on this node: another
    /// build wrote it. Nothing of the process ran or committed for it
    /// (FIG-5601).
    UndecodableDriver {
        /// The decoder's account.
        message: String,
    },
    /// Its engine refused a transition.
    AdvanceRefused {
        /// The engine's account.
        message: String,
    },
    /// It runs a turn in a child session, and the node that claimed it runs
    /// no session turns.
    UnservedSessionTurn,
    /// The store refused its commits as corrupt, its terminal too.
    CommitRefused {
        /// The store's account.
        message: String,
    },
}

impl ProcessParkReason {
    /// The park feed's encoding.
    #[expect(
        clippy::expect_used,
        reason = "a park reason is plain data whose encoding cannot fail"
    )]
    #[must_use]
    pub fn encode(&self) -> String {
        serde_json::to_string(self).expect("a park reason encodes")
    }
}

/// Why `process`'s actor is parked, read from its actor row; `None` while
/// it is not parked, or once it has no actor.
///
/// # Errors
///
/// The store's refusal, or a stored reason this build does not decode.
pub async fn park_of(
    store: &dyn lash_durable::DurableStore,
    process: &ProcessId,
) -> Result<Option<ProcessParkReason>, DurableError> {
    let corrupt = |message: String| {
        DurableError::Store(StoreFailure {
            kind: StoreFailureKind::Corrupt,
            message,
        })
    };
    let actor = lash_durable::ActorKey::process(process.as_str())
        .map_err(|error| corrupt(error.to_string()))?;
    let Some(park) = store.actor(&actor).await?.and_then(|actor| actor.park) else {
        return Ok(None);
    };
    serde_json::from_str(&park).map(Some).map_err(|error| {
        corrupt(format!(
            "the park of process {process} does not decode: {error}"
        ))
    })
}

/// The terminal of a process cancelled with `origin`: forced when lash
/// ended it at its grace.
#[must_use]
pub fn cancelled(origin: CancelOrigin, forced: bool) -> ProcessOutcome {
    let cancellation =
        crate::ToolCancellation::runtime("the process was cancelled").with_origin(origin);
    let cancellation = if forced {
        cancellation.forced()
    } else {
        cancellation
    };
    crate::ProcessAwaitOutput::from_tool_output(crate::ToolCallOutput::cancelled(cancellation))
}

/// Record `process`'s terminal transaction on `tx`, the same for every
/// cause: the terminal and its event, the process-terminal waits on it
/// resolved, its own pending waits revoked, its engine state deleted, and
/// its cascade begun. The cascade over its `Until` children runs in
/// further `cascade.batch` commits while the actor stays runnable.
///
/// # Errors
///
/// The outcome's encoding.
pub fn record_terminal(
    tx: &mut ActorTx,
    process: &ProcessId,
    outcome: &ProcessOutcome,
) -> Result<(), DurableError> {
    let outcome_json = serde_json::to_string(outcome).map_err(|error| {
        DurableError::Store(StoreFailure {
            kind: StoreFailureKind::Corrupt,
            message: format!("the terminal of process {process} does not encode: {error}"),
        })
    })?;
    tx.write(DomainWrite::Process(ProcessWrite::Terminal {
        process: process.clone(),
        outcome_json,
    }));
    waits::resolve_process_terminal_waits(tx, process, outcome)?;
    waits::revoke_scope(tx, &ScopeKey::Process(process.clone()));
    tx.write(DomainWrite::Snapshot(SnapshotWrite::Delete {
        exec: lash_durable::domain::ExecKey::Process(process.clone()),
    }));
    Ok(())
}

/// Park the committing actor with `reason` on `tx`.
pub fn record_park(tx: &mut ActorTx, reason: &ProcessParkReason) {
    tx.write(DomainWrite::ParkEvent(ParkEventWrite::Park {
        reason_json: reason.encode(),
    }));
    tx.give_up(lash_durable::Release::Parked);
}
