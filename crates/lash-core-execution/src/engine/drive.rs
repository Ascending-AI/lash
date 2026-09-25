//! The session drive's contract: what one drive of a session answers, and
//! the scopes its recorded steps run under (FIG-3600, ADR 0104 O1/O2/O6).
//!
//! A drive admits a root with a recorded `AdmitDrive` step, seals the
//! admission with a recorded `SealDriveAdmission` step, and runs the root's
//! turns to their terminal commit. Nothing here names an engine: an engine
//! runs the drive either in process (one controller, rescoped per step) or
//! split across its own handlers (admission in a per-session handler, each
//! root in a per-root one), and both reach the same kernel bodies.

use serde::{Deserialize, Serialize};

use super::admission::{Admitted, DriveRequestId, ParkRef, SealVerdict};
use super::commit::TurnCommitId;
use crate::{AdmittedScope, RuntimeError, SessionId, TurnId, TurnOutcome};

/// The prefix of the queue-drain id a drive's admission steps are recorded
/// under. A drive is never a queue drain; the scope only gives its admission
/// journal a session-bearing address.
const DRIVE_ADMISSION_SCOPE_PREFIX: &str = "drive:";

/// The scope a drive's `AdmitDrive` steps are recorded under: one per drive
/// request, so a redrive of the same request replays its admissions and a new
/// request admits afresh.
#[must_use]
pub fn drive_admission_scope(session: &SessionId, request: &DriveRequestId) -> AdmittedScope {
    AdmittedScope::queue_drain(
        session.clone(),
        format!("{DRIVE_ADMISSION_SCOPE_PREFIX}{}", request.as_str()),
    )
}

/// The scope an admitted root runs under: the root's own turn. Every turn a
/// drive runs is opened by its root, never by a queue drain (FIG-3607
/// contract 4).
#[must_use]
pub fn drive_root_scope(session: &SessionId, root: &TurnId) -> AdmittedScope {
    AdmittedScope::turn(session.clone(), root.clone())
}

/// The replay key of admission `ordinal` of drive `request`, inside
/// [`drive_admission_scope`]. It names the request as well as the scope does,
/// so a journal keyed by replay key alone never serves one request's
/// admission to another.
#[must_use]
pub fn drive_admission_replay_key(request: &DriveRequestId, ordinal: u32) -> String {
    format!("drive-admission:{}#{ordinal}", request.as_str())
}

/// The replay key of an admitted root's seal, inside [`drive_root_scope`].
#[must_use]
pub fn drive_seal_replay_key(admitted: &Admitted) -> String {
    format!("drive-seal:{}", admitted.admission().as_str())
}

/// How one admitted root ended.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "root_outcome", rename_all = "snake_case")]
pub enum RootOutcome {
    /// The root's turns ran and its terminal commit landed.
    Committed { root: TurnId, outcome: TurnOutcome },
    /// The seal refused the admission (another admission superseded it, or
    /// the root started under a history this execution cannot read), so
    /// nothing ran.
    Refused { root: TurnId, verdict: SealVerdict },
    /// The work admission named was answered by another driver or withdrawn
    /// before the root claimed it, so nothing ran.
    Ceded { root: TurnId },
}

impl RootOutcome {
    pub fn root(&self) -> &TurnId {
        match self {
            Self::Committed { root, .. } | Self::Refused { root, .. } | Self::Ceded { root } => {
                root
            }
        }
    }
}

/// Why a drive stopped admitting.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "stop", rename_all = "snake_case")]
pub enum DriveStop {
    /// Admission found nothing to drive. The drive re-checked before it
    /// stopped, so work admitted before this answer was driven.
    Idle,
    /// A parked root blocks the session; nothing is admitted until the park
    /// is resolved.
    Parked(ParkRef),
    /// The root admission would resume started under a history this
    /// execution cannot read. It is parked or abandoned, never re-run.
    SubstrateLost { root: TurnId },
    /// The root admission named already has its terminal (ADR 0105 L-S6).
    RootTerminal { root: TurnId, by: TurnCommitId },
    /// A pending follow-on of `root` holds the session and admission could
    /// not resume it (ADR 0101 §3, FIG-3542).
    Blocked { root: TurnId },
}

/// What one drive of a session did: the roots it ran, in order, and why it
/// stopped.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct DriveOutcome {
    pub ran: Vec<RootOutcome>,
    pub stop: DriveStop,
}

/// Why a drive ended without an outcome. The engine decides what to do with
/// the attempt; nothing here is a recorded result.
#[derive(Debug, thiserror::Error)]
pub enum DriveAbort {
    /// A live fault. The engine retries the attempt under its own policy,
    /// and the retry replays what was recorded.
    #[error("drive attempt failed and will be retried: {0}")]
    Retry(RuntimeError),
    /// The root parked (ADR 0104 O3): its park is durable, and the engine
    /// keeps its history for a restored build instead of retrying it.
    #[error("root `{root}` parked: {error}")]
    Parked { root: TurnId, error: RuntimeError },
    /// A refusal no retry changes. The engine ends the attempt terminally.
    #[error("drive refused: {0}")]
    Refused(RuntimeError),
}

impl DriveAbort {
    /// The error the abort carries, whatever its disposition.
    pub fn error(&self) -> &RuntimeError {
        match self {
            Self::Retry(error) | Self::Refused(error) | Self::Parked { error, .. } => error,
        }
    }

    pub fn into_error(self) -> RuntimeError {
        match self {
            Self::Retry(error) | Self::Refused(error) | Self::Parked { error, .. } => error,
        }
    }
}

/// Constructors for the admission executor alone.
///
/// An [`Admitted`] is minted only by the body of a recorded `AdmitDrive`
/// step, whose recorded verdict every replay decodes. No other caller may
/// build one.
#[doc(hidden)]
pub mod admission_body {
    use super::super::admission::{AdmissionId, Admitted, AdmittedWork, DriveRequestId};
    use crate::store::SessionHeadRef;
    use crate::{SessionId, TurnId};

    #[must_use]
    #[allow(clippy::too_many_arguments)]
    pub fn admitted(
        session: SessionId,
        root: TurnId,
        request: DriveRequestId,
        admission: AdmissionId,
        observed_epoch: u64,
        base: SessionHeadRef,
        turn_index: u64,
        work: AdmittedWork,
    ) -> Admitted {
        Admitted::minted(
            session,
            root,
            request,
            admission,
            observed_epoch,
            base,
            turn_index,
            work,
        )
    }
}
