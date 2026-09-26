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
use sha2::{Digest, Sha256};

use std::collections::BTreeSet;

use super::admission::{Admitted, AdmittedWork, DriveRequestId, ParkRef, SealVerdict};
use super::commit::TurnCommitId;
use super::contracts::DriveRequest;
use crate::{AdmittedScope, RuntimeError, SessionId, TurnId, TurnOutcome};

/// The prefix of the queue-drain id a drive's admission steps are recorded
/// under. A drive is never a queue drain; the scope only gives its admission
/// journal a session-bearing address.
const DRIVE_ADMISSION_SCOPE_PREFIX: &str = "drive:";

/// Maximum roots admitted by one engine drive invocation before it hands
/// remaining work to a new request.
pub const MAX_ROOTS_PER_DRIVE: usize = 64;

/// The stable, fixed-size request id of a drive's next invocation.
#[must_use]
pub fn drive_continuation_request(request: &DriveRequest) -> DriveRequestId {
    let mut digest = Sha256::new();
    digest.update((request.session.as_str().len() as u64).to_be_bytes());
    digest.update(request.session.as_str().as_bytes());
    digest.update(request.request.as_str().as_bytes());
    DriveRequestId::new(format!("drive-next:{:x}", digest.finalize()))
}

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

/// The replay key of the start marker an execution of an admitted root draws
/// before its seal, inside [`drive_root_scope`] (ADR 0105 L-S8).
#[must_use]
pub fn drive_root_start_replay_key(admitted: &Admitted) -> String {
    format!("drive-root-start:{}", admitted.admission().as_str())
}

/// The replay key of an admitted root's seal, inside [`drive_root_scope`].
#[must_use]
pub fn drive_seal_replay_key(admitted: &Admitted) -> String {
    format!("drive-seal:{}", admitted.admission().as_str())
}

/// The replay key of a root's scope close, inside the root's scope: one
/// close per root, whichever admission ran it, so a redrive of a root that
/// already closed replays the recorded close.
#[must_use]
pub fn drive_close_root_replay_key(root: &TurnId) -> String {
    format!("drive-close:{}", root.as_str())
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
    /// The engine released the root's execution for good (an operator's
    /// cancel or fork killed it, or it ended terminally without a lash
    /// outcome). The drive goes on to its next admission, which reads what
    /// the store decided about the root.
    Released { root: TurnId },
}

impl RootOutcome {
    pub fn root(&self) -> &TurnId {
        match self {
            Self::Committed { root, .. }
            | Self::Refused { root, .. }
            | Self::Ceded { root }
            | Self::Released { root } => root,
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
    RootTerminal {
        root: TurnId,
        kind: crate::store::RootTerminalKind,
        commit: Option<TurnCommitId>,
    },
    /// A pending follow-on of `root` holds the session and admission could
    /// not resume it (ADR 0101 §3, FIG-3542).
    Blocked { root: TurnId },
    /// Admission named `root` again after the engine released its execution
    /// in this drive: the store still owes it work nothing will run. The
    /// drive stops rather than spin on it.
    RootAborted { root: TurnId },
    /// The drive stopped admitting with work possibly left: `root` ran
    /// nothing (its seal was superseded, or its queued run ceded), admission
    /// named it again after this drive already ran it, or the caller had what
    /// it waited for. Unlike [`Idle`](Self::Idle), admission did not answer
    /// that nothing is pending; the session's next ask to drive re-checks.
    Yielded { root: TurnId },
}

/// The stop rules every drive loop keeps, in process or split across an
/// engine's handlers: one copy, so no engine's loop can lose one.
///
/// - A root this drive already ran is not run again; what admission found
///   was left behind by that run, and the next ask answers it
///   ([`DriveStop::Yielded`]).
/// - A root whose execution the engine released is consumed, and the drive
///   goes on; if admission names it again the drive stops
///   ([`DriveStop::RootAborted`]) instead of calling it a second time.
/// - A root that ran nothing (a superseded seal, or a queued run that ceded)
///   stops the drive: another driver holds the session, or the queue has
///   nothing this drive can claim now ([`DriveStop::Yielded`]).
#[derive(Clone, Debug, Default)]
pub struct DriveLoop {
    ran: BTreeSet<TurnId>,
    released: BTreeSet<TurnId>,
}

impl DriveLoop {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Whether the drive runs `admitted`'s root: `Err(stop)` when it stops
    /// instead.
    pub fn before(&mut self, admitted: &Admitted) -> Result<(), DriveStop> {
        let root = admitted.root();
        if self.released.contains(root) {
            return Err(DriveStop::RootAborted { root: root.clone() });
        }
        if !self.ran.insert(root.clone()) {
            return Err(DriveStop::Yielded { root: root.clone() });
        }
        Ok(())
    }

    /// How the drive goes on after a root of `work` ended `outcome`:
    /// `Some(stop)` when it stops.
    pub fn after(&mut self, work: &AdmittedWork, outcome: &RootOutcome) -> Option<DriveStop> {
        match outcome {
            RootOutcome::Committed { .. } => None,
            RootOutcome::Ceded { root } => (!matches!(work, AdmittedWork::Input { .. }))
                .then(|| DriveStop::Yielded { root: root.clone() }),
            RootOutcome::Refused { root, .. } => Some(DriveStop::Yielded { root: root.clone() }),
            RootOutcome::Released { root } => {
                self.released.insert(root.clone());
                None
            }
        }
    }
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
    Parked {
        root: TurnId,
        error: Box<RuntimeError>,
    },
    /// A refusal no retry changes. The engine ends the attempt terminally.
    #[error("drive refused: {0}")]
    Refused(RuntimeError),
}

impl DriveAbort {
    /// The error the abort carries, whatever its disposition.
    pub fn error(&self) -> &RuntimeError {
        match self {
            Self::Retry(error) | Self::Refused(error) => error,
            Self::Parked { error, .. } => error,
        }
    }

    pub fn into_error(self) -> RuntimeError {
        match self {
            Self::Retry(error) | Self::Refused(error) => error,
            Self::Parked { error, .. } => *error,
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
    use crate::{SessionId, TurnId};

    #[must_use]
    pub fn admitted(
        session: SessionId,
        root: TurnId,
        request: DriveRequestId,
        admission: AdmissionId,
        observed_epoch: u64,
        work: AdmittedWork,
    ) -> Admitted {
        Admitted::minted(session, root, request, admission, observed_epoch, work)
    }
}
