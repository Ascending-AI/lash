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
/// `build_generation` is the sender's drain generation, the stamp the
/// drive's request carries ([`DriveRequest::build_generation`]): the
/// admission records it so the queued run it begins routes its resume by the
/// generation that admitted it (FIG-3795 S9).
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
}

/// The seal's decision.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "verdict", rename_all = "snake_case")]
pub enum SealVerdict {
    Sealed(DriveFence),
    SubstrateLost { root: TurnId },
    Superseded { epoch: u64 },
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
    /// The sender's drain generation the drive request carried, recorded
    /// with the admission (FIG-3795 S9): the root's admission stamps it, and
    /// the root's resume routes by it.
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

    /// The drain generation the drive's request was stamped with: the
    /// generation a queued run this root begins resumes through
    /// (FIG-3795 S9).
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
