//! Recorded admission values and inherited authority (ADR 0105 §2).

use serde::{Deserialize, Serialize};

use crate::{SessionId, TurnId};
pub use lash_core_store::store::{AdmissionId, DriveFence, RootStartNonce};

/// What a drive asks admission for.
///
/// It names no root: admission mints the root inside its recorded body, from
/// the work it admits (the unfinished run it resumes, or the first item of
/// the queue prefix it takes), so a replay decodes the same root and a fresh
/// execution never trusts a root the caller guessed.
///
/// `build_generation` is the drain generation of the build that admits: the
/// admission records it, so the root counts in that generation's drain and
/// its resume routes by the generation that admitted it (FIG-3795 S9). An
/// engine whose drive requests cross builds names the build serving the
/// admission, never the stamp the request was sent with (FIG-4742).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AdmitRequest {
    pub session: SessionId,
    pub request: DriveRequestId,
    pub build_generation: super::contracts::BuildGeneration,
}

/// Admission's decision.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "verdict", rename_all = "snake_case")]
pub enum AdmitVerdict {
    /// Admission may be sealed.
    Admit(Admitted),
    /// A parked root blocks the session; nothing is admitted.
    Parked(ParkRef),
    /// The root started under a history this execution cannot read. It is
    /// parked or abandoned, never re-run.
    SubstrateLost { root: TurnId },
    /// The root already has terminal evidence: a later execution adopts it
    /// instead of running the root again (ADR 0105 law L-S6). `commit` names
    /// the head commit that ended it, when one did.
    RootTerminal {
        root: TurnId,
        kind: crate::store::RootTerminalKind,
        commit: Option<crate::store::TurnCommitId>,
    },
    /// No work is pending.
    Idle,
    /// Work is pending and nothing is admitted: `generation`, the build the
    /// drive's invocation is pinned to, is marked draining (ADR 0106 §1), so
    /// the drive hands the rest to the newest build. The mark is read inside
    /// the recorded step and recorded with this answer, so a replay hands
    /// over where the first execution did, whatever the mark says by then.
    Draining {
        generation: super::contracts::BuildGeneration,
    },
}

/// The seal's decision.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "verdict", rename_all = "snake_case")]
pub enum SealVerdict {
    Sealed(DriveFence),
    Refused(SealRefusal),
}

/// Why a seal refused its admission: nothing ran. The refused root is the
/// admission's own, so a refusal names none.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "refusal", rename_all = "snake_case")]
pub enum SealRefusal {
    /// Another admission raised the drive epoch to `epoch`, past the one
    /// this admission observed.
    Superseded { epoch: u64 },
    /// Another execution of the root sealed this admission under a start
    /// marker this execution did not draw: it cannot read what that one did,
    /// so it must not run the root (ADR 0105 L-S8).
    ExecutionLost,
}

/// An admission granted by a recorded `AdmitDrive` step, to be sealed.
///
/// It has no public constructor: it is decoded only from a recorded
/// [`AdmitVerdict::Admit`].
///
/// It records no base. The head a root runs on, and its turn index, are
/// recorded once, by the root's `AdmitRoot` step, which a redrive replays
/// (ADR 0105 §2, FIG-3682): the admission is the one source of truth for the
/// base.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Admitted {
    session: SessionId,
    root: TurnId,
    request: DriveRequestId,
    admission: AdmissionId,
    observed_epoch: u64,
    /// The drain generation of the build that admitted the root, recorded
    /// with the admission (FIG-3795 S9, FIG-4742): the root's admission
    /// stamps it, and the root's resume routes by it.
    admitted_generation: super::contracts::BuildGeneration,
    /// What the root drives.
    work: AdmittedWork,
}

/// What an admitted root drives. Decided by admission and recorded with it,
/// so the root's run never re-reads the store to learn its own shape.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "work", rename_all = "snake_case")]
pub enum AdmittedWork {
    /// The prefix of accepted next-turn input headed by `head`.
    Input { head: crate::InputId },
    /// The prefix of ready queued work headed by `head`: a root like an
    /// input root, admitted and driven the same way (FIG-3927).
    Queued { head: crate::BatchId },
    /// The session's open command run, applied at this boundary before any
    /// turn-lane work (ADR 0101 §4). It admits no turn: the root names the
    /// application and ends when the command lane is empty. `head` is the
    /// `enqueue_seq` of the leading open command the admission saw, so a
    /// later admission naming the same head shows the lane made no progress.
    Commands { head: u64 },
    /// The follow-on the session head owes (ADR 0101 §3): its recovery, as
    /// recovery number `attempts + 1`. The recorded count is what the
    /// recovery raises from, so a redrive of the root never raises it twice.
    FollowOn { follow_on: TurnId, attempts: u32 },
}

impl Admitted {
    /// Only the `AdmitDrive` body mints an admission, through
    /// [`admission_body::admitted`](super::drive::admission_body::admitted).
    pub(super) fn minted(
        session: SessionId,
        root: TurnId,
        request: DriveRequestId,
        admission: AdmissionId,
        observed_epoch: u64,
        admitted_generation: super::contracts::BuildGeneration,
        work: AdmittedWork,
    ) -> Self {
        Self {
            session,
            root,
            request,
            admission,
            observed_epoch,
            admitted_generation,
            work,
        }
    }

    /// This admission as the build of `generation` runs its root (FIG-4742).
    ///
    /// An engine that pins a root's execution to the build that started it
    /// may start it on a newer build than the one whose drive admitted it.
    /// The root's stamp names the build its journal belongs to, so the build
    /// that runs the root restates the stamp before anything records it: the
    /// root's admission, its parks and the drain's count then all name the
    /// build that holds it.
    #[must_use]
    pub fn run_by(mut self, generation: super::contracts::BuildGeneration) -> Self {
        self.admitted_generation = generation;
        self
    }

    pub fn session(&self) -> &SessionId {
        &self.session
    }

    pub fn root(&self) -> &TurnId {
        &self.root
    }

    pub fn request(&self) -> &DriveRequestId {
        &self.request
    }

    /// The nonce the seal is keyed by.
    pub fn admission(&self) -> &AdmissionId {
        &self.admission
    }

    /// The drive epoch admission read; the seal advances it by one.
    pub fn observed_epoch(&self) -> u64 {
        self.observed_epoch
    }

    /// The drain generation of the build that holds the root: the one whose
    /// drive admitted it, or the one that runs it once that build restated
    /// the stamp ([`Self::run_by`]). A queued run this root begins resumes
    /// through it (FIG-3795 S9).
    pub fn admitted_generation(&self) -> &super::contracts::BuildGeneration {
        &self.admitted_generation
    }

    /// What the root drives.
    pub fn work(&self) -> &AdmittedWork {
        &self.work
    }
}

/// A logical drive request's id: application dedupe across engine runs.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct DriveRequestId(String);

impl DriveRequestId {
    pub fn new(id: impl Into<String>) -> Self {
        Self(id.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// A parked root that blocks admission.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ParkRef {
    pub session: SessionId,
    pub root: TurnId,
    pub park: crate::store::ParkId,
}
