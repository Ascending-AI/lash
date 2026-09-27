//! The run-spec rules both SQL backends apply inside the transaction that
//! admits a pending turn input (FIG-3838).
//!
//! A non-default spec is interned in the session's spec table once per hash:
//! a backend inserts the canonical bytes if the hash is new, reads the row
//! back, and refuses different bytes under the same hash. An input addressed
//! to a running turn joins that turn's recorded shape, so an explicit spec
//! that differs from the spec of the input the turn was started by is
//! refused. Both refusals come before the input row is written, and roll the
//! whole admission back.

use crate::run_spec::RunSpecHash;
use crate::store::StoreError;
use crate::{PendingTurnInputDraft, SessionId, TurnId};

/// What admitting one draft writes for its spec: nothing for the default
/// spec, or the hash the input row names and the bytes the session interns.
#[derive(Clone, Debug)]
pub struct RunSpecAdmission {
    pub hash: Option<RunSpecHash>,
    canonical_json: Option<String>,
}

impl RunSpecAdmission {
    /// The spec facts `draft` admits under.
    pub fn of(draft: &PendingTurnInputDraft) -> Result<Self, StoreError> {
        let encoding = |error: serde_json::Error| StoreError::RecordEncodingFailed {
            record_kind: "RunSpec".to_string(),
            message: error.to_string(),
        };
        let hash = draft.run_spec.hash().map_err(encoding)?;
        let canonical_json = match hash {
            Some(_) => Some(draft.run_spec.canonical_json().map_err(encoding)?),
            None => None,
        };
        Ok(Self {
            hash,
            canonical_json,
        })
    }

    /// The `(hash, canonical bytes)` pair to intern, when the spec is not
    /// the default.
    pub fn interned(&self) -> Option<(&str, &str)> {
        self.hash
            .as_ref()
            .map(RunSpecHash::as_str)
            .zip(self.canonical_json.as_deref())
    }

    /// The `run_spec_hash` column value.
    pub fn column(&self) -> Option<&str> {
        self.hash.as_ref().map(RunSpecHash::as_str)
    }

    /// Refuse the row an intern read back when it holds other bytes than
    /// this spec's under the same hash.
    pub fn check_interned(&self, session_id: &SessionId, stored: &str) -> Result<(), StoreError> {
        match self.interned() {
            Some((hash, canonical)) if canonical != stored => {
                Err(StoreError::RunSpecHashCollision {
                    session_id: session_id.clone(),
                    hash: hash.to_string(),
                })
            }
            _ => Ok(()),
        }
    }
}

/// The turn a steering draft addresses, when its explicit spec must be
/// checked against that turn's: `None` for a next-turn input and for an
/// omitted spec, which inherits the running root's shape.
pub fn steering_run_spec_target(draft: &PendingTurnInputDraft) -> Option<&TurnId> {
    if draft.run_spec.is_default() {
        return None;
    }
    draft.ingress.active_turn_id()
}

/// The steering verdict over the input that started addressed turn
/// `turn_id` (the input filed under the turn's id as its source key), read in
/// the admitting transaction: `state` is its lifecycle spelling and
/// `run_spec_hash` its spec column.
///
/// A turn whose starting input is already delivered has ended its first
/// physical turn, so the steering input is a next-turn input by rule and
/// resolves fresh from its own spec. A turn no input started carries no
/// input spec to differ from.
pub fn check_steering_run_spec(
    session_id: &SessionId,
    turn_id: &TurnId,
    admission: &RunSpecAdmission,
    addressed: Option<(&str, Option<&str>)>,
) -> Result<(), StoreError> {
    let Some((state, run_spec_hash)) = addressed else {
        return Ok(());
    };
    let delivered = crate::TurnInputStateKind::from_wire_str(state)
        .is_none_or(crate::TurnInputStateKind::is_terminal);
    if delivered || run_spec_hash == admission.column() {
        return Ok(());
    }
    Err(StoreError::PendingTurnInputRunSpecMismatch {
        session_id: session_id.clone(),
        turn_id: turn_id.clone(),
    })
}

/// The steering verdict over `turn_id` when the running root resolved under
/// spec hash `running` (`None` = the default spec), read in the admitting
/// transaction for the root kinds whose starting input is not filed under
/// the turn's id as a source key (FIG-3877): the follow-on the head owes,
/// which runs under the shape its fact recorded at the switch, and the
/// current position of a queued run, which resolves under the spec its
/// member inputs carry. The turn is running, so a steering spec must equal
/// the recorded shape — there is no delivered escape.
pub fn check_running_root_run_spec(
    session_id: &SessionId,
    turn_id: &TurnId,
    admission: &RunSpecAdmission,
    running: Option<&str>,
) -> Result<(), StoreError> {
    if admission.column() == running {
        return Ok(());
    }
    Err(StoreError::PendingTurnInputRunSpecMismatch {
        session_id: session_id.clone(),
        turn_id: turn_id.clone(),
    })
}
