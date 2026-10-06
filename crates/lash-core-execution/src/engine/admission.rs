//! Recorded admission values and inherited authority (ADR 0105 §2).

use serde::{Deserialize, Serialize};

use crate::{SessionId, TurnId};
pub use lash_core_store::store::{AdmissionId, RunStartNonce, ShiftFence};

/// What a shift asks admission for.
///
/// It names no run: admission mints the run inside its recorded body, from
/// the work it admits (the unfinished run it resumes, or the first item of
/// the queue prefix it takes), so a replay decodes the same run and a fresh
/// execution never trusts a run the caller guessed.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AdmitRequest {
    pub session: SessionId,
    pub request: ShiftRequestId,
    pub run_start: RunStartNonce,
}

/// Admission's decision.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "verdict", rename_all = "snake_case")]
pub enum AdmitVerdict {
    /// Admission recorded its seal and work.
    Admit(Admitted),
    /// A parked run blocks the session; nothing is admitted.
    Parked(ParkRef),
    /// The run started under a history this execution cannot read. It is
    /// parked or abandoned, never re-run.
    SubstrateLost { run: TurnId },
    /// The run already has terminal evidence: a later execution adopts it
    /// instead of running the run again (ADR 0105 law L-S6). `commit` names
    /// the head commit that ended it, when one did.
    RunTerminal {
        run: TurnId,
        kind: crate::store::RunTerminalKind,
        commit: Option<crate::store::TurnCommitId>,
    },
    /// No work is pending.
    Idle,
}

/// The seal's decision.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "verdict", rename_all = "snake_case")]
pub enum SealVerdict {
    Sealed(ShiftFence),
    Refused(SealRefusal),
}

/// Why a seal refused its admission: nothing ran. The refused run is the
/// admission's own, so a refusal names none.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "refusal", rename_all = "snake_case")]
pub enum SealRefusal {
    /// Another admission raised the shift epoch to `epoch`, past the one
    /// this admission observed.
    Superseded { epoch: u64 },
    /// Another execution of the run sealed this admission under a start
    /// marker this execution did not draw: it cannot read what that one did,
    /// so it must not run the run (ADR 0105 L-S8).
    ExecutionLost,
}

/// The work and atomic receipt granted by a recorded root admission.
/// Only the admission body mints it; execution requires the retained receipt.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Admitted {
    session: SessionId,
    request: ShiftRequestId,
    admission: AdmissionId,
    receipt: Box<lash_core_store::store::ShiftAdmissionReceipt>,
}

pub use lash_core_store::store::AdmittedWork;

impl Admitted {
    /// The atomic receipt retained by this recorded root admission.
    pub fn root(&self) -> &lash_core_store::store::ShiftAdmissionReceipt {
        &self.receipt
    }

    /// Only the `AdmitShift` body mints an admission, through
    /// [`admission_body::admitted`](super::shift::admission_body::admitted).
    pub(super) fn minted(
        session: SessionId,
        request: ShiftRequestId,
        admission: AdmissionId,
        receipt: lash_core_store::store::ShiftAdmissionReceipt,
    ) -> Self {
        Self {
            session,
            request,
            admission,
            receipt: Box::new(receipt),
        }
    }

    pub fn session(&self) -> &SessionId {
        &self.session
    }

    pub fn run(&self) -> &TurnId {
        &self.receipt.selection.run
    }

    pub fn request(&self) -> &ShiftRequestId {
        &self.request
    }

    /// The nonce the seal is keyed by.
    pub fn admission(&self) -> &AdmissionId {
        &self.admission
    }

    /// The shift epoch admission read; the seal advances it by one.
    pub fn observed_epoch(&self) -> u64 {
        self.receipt.selection.observed_epoch
    }

    /// What the run executes.
    pub fn work(&self) -> &AdmittedWork {
        &self.receipt.selection.work
    }

    /// The host operation the run executes, when it executes one.
    pub fn operation(&self) -> Option<lash_core_store::tool_run::OperationRun> {
        match self.work() {
            AdmittedWork::Operation { operation } => {
                Some(lash_core_store::tool_run::OperationRun {
                    session_id: self.session.clone(),
                    operation_id: operation.to_string(),
                })
            }
            AdmittedWork::Input { .. }
            | AdmittedWork::Queued { .. }
            | AdmittedWork::Commands { .. }
            | AdmittedWork::FollowOn { .. } => None,
        }
    }
}

/// A logical shift request's id: application dedupe across engine runs.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ShiftRequestId(String);

impl ShiftRequestId {
    pub fn new(id: impl Into<String>) -> Self {
        Self(id.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// A parked run that blocks admission.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ParkRef {
    pub session: SessionId,
    pub run: TurnId,
    pub park: crate::store::ParkId,
}

impl PartialEq for Admitted {
    fn eq(&self, other: &Self) -> bool {
        match (serde_json::to_value(self), serde_json::to_value(other)) {
            (Ok(left), Ok(right)) => left == right,
            _ => false,
        }
    }
}
impl Eq for Admitted {}
