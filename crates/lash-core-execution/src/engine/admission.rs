//! Unfenced admission, separate from fenced execution (ADR 0105 §2).
//!
//! Only the three admission operations exist before a fence exists. Every
//! other durable operation is reachable only through [`Fenced`], which can be
//! built only from a recorded [`SealVerdict::Sealed`] or
//! [`InheritVerdict::Valid`], so an effect without a fence is unrepresentable.

use serde::{Deserialize, Serialize};

use super::commands::{EffectCommand, EffectResult};
use super::context::{EngineContext, EpochMs, ReplayKey};
use super::contracts::{DriveHandover, Never, ResolveAck};
use super::groups::GroupKey;
use crate::{
    AwaitEventKey, ExecutionScope, Resolution, SessionId, SessionStateVersionRefusal,
    TurnCancellationEvidence, TurnId,
};
pub use lash_core_store::store::{AdmissionId, DriveFence};

/// Unfenced admission. Each operation is a recorded step.
pub trait DriveAdmission: EngineContext {
    /// Reads the session head generation, the drive epoch, the parked-root
    /// set and the root's start marker.
    fn admit(&self, req: &AdmitRequest) -> Self::Op<'_, AdmitVerdict>;

    /// Advances the drive epoch by one with a compare-and-set and sets the
    /// root's start marker if absent, keyed by the admission nonce, and
    /// revalidates authority in the same transaction: a reset before the seal
    /// may retain [`admit`](Self::admit)'s result without rerunning it.
    fn seal(&self, admitted: Admitted) -> Self::Op<'_, SealVerdict>;

    /// A child (a group child, a child execution) validates the authority it
    /// inherited. It never mints an epoch.
    fn inherit(&self, authority: &InheritedAuthority) -> Self::Op<'_, InheritVerdict>;
}

/// Fenced execution: the durable primitives, reachable only through
/// [`Fenced`].
pub trait DriveContext: DriveAdmission {
    /// The fenced handle for a recorded verdict: `Some` only for
    /// [`SealVerdict::Sealed`] and [`InheritVerdict::Valid`].
    fn fenced(&self, verdict: FenceSource) -> Option<Fenced<'_, Self>>
    where
        Self: Sized,
    {
        let fence = match verdict {
            FenceSource::Sealed(SealVerdict::Sealed(fence))
            | FenceSource::Inherited(InheritVerdict::Valid(fence)) => fence,
            FenceSource::Sealed(_) | FenceSource::Inherited(_) => return None,
        };
        Some(Fenced { cx: self, fence })
    }

    /// Whether the engine suggests handing over to a fresh execution. It is
    /// deterministic on replay (ADR 0105 §7).
    fn handover_suggested(&self) -> bool;

    /// Hands the session over to a fresh execution at a turn boundary, or at
    /// a checkpoint boundary of an oversized turn (ADR 0105 §7). It never
    /// returns.
    fn continue_as_new(&self, handover: DriveHandover) -> Self::Op<'_, Never>;

    #[doc(hidden)]
    fn step(&self, f: &DriveFence, cmd: EffectCommand) -> Self::Op<'_, EffectResult>;
    #[doc(hidden)]
    fn timer(&self, f: &DriveFence, at: EpochMs, key: &ReplayKey) -> Self::Op<'_, ()>;
    #[doc(hidden)]
    fn await_key(&self, f: &DriveFence, key: &AwaitEventKey) -> Self::Op<'_, Resolution>;
    #[doc(hidden)]
    fn peek_key(&self, f: &DriveFence, key: &AwaitEventKey) -> Self::Op<'_, Option<Resolution>>;
    #[doc(hidden)]
    fn resolve_key(
        &self,
        f: &DriveFence,
        key: &AwaitEventKey,
        resolution: Resolution,
    ) -> Self::Op<'_, ResolveAck>;
    #[doc(hidden)]
    fn turn_cancel(
        &self,
        f: &DriveFence,
        scope: &ExecutionScope,
        gate: CancelGate,
    ) -> Self::Op<'_, TurnCancelSignal>;
    #[doc(hidden)]
    fn child(&self, f: &DriveFence, start: ChildStart) -> Self::Op<'_, ChildOutcome>;
}

/// The fenced view of an engine context.
pub struct Fenced<'c, C: DriveContext> {
    cx: &'c C,
    fence: DriveFence,
}

impl<'c, C: DriveContext> Fenced<'c, C> {
    /// A recorded step: the engine runs the registered executor for `cmd`
    /// inside its recorded body and returns the recorded result on replay.
    pub fn step(&self, cmd: EffectCommand) -> C::Op<'c, EffectResult> {
        self.cx.step(&self.fence, cmd)
    }

    /// A durable timer at an absolute deadline only.
    pub fn timer(&self, at: EpochMs, key: &ReplayKey) -> C::Op<'c, ()> {
        self.cx.timer(&self.fence, at, key)
    }

    /// Parks on a keyed promise (ADR 0105 §5).
    pub fn await_key(&self, key: &AwaitEventKey) -> C::Op<'c, Resolution> {
        self.cx.await_key(&self.fence, key)
    }

    /// A recorded, non-blocking read of a keyed promise.
    pub fn peek_key(&self, key: &AwaitEventKey) -> C::Op<'c, Option<Resolution>> {
        self.cx.peek_key(&self.fence, key)
    }

    /// Resolves a keyed promise; the first writer wins.
    pub fn resolve_key(
        &self,
        key: &AwaitEventKey,
        resolution: Resolution,
    ) -> C::Op<'c, ResolveAck> {
        self.cx.resolve_key(&self.fence, key, resolution)
    }

    /// The turn-cancel gate as an engine event.
    pub fn turn_cancel(
        &self,
        scope: &ExecutionScope,
        gate: CancelGate,
    ) -> C::Op<'c, TurnCancelSignal> {
        self.cx.turn_cancel(&self.fence, scope, gate)
    }

    /// Starts a child execution that carries this fence as inherited
    /// authority.
    pub fn child(&self, start: ChildStart) -> C::Op<'c, ChildOutcome> {
        self.cx.child(&self.fence, start)
    }

    /// The authority a child started from this drive inherits.
    pub fn inherited(
        &self,
        parent: ReplayKey,
        group: Option<(GroupKey, u32)>,
    ) -> InheritedAuthority {
        InheritedAuthority {
            fence: self.fence.clone(),
            parent,
            group,
        }
    }

    pub fn fence(&self) -> &DriveFence {
        &self.fence
    }

    pub fn context(&self) -> &'c C {
        self.cx
    }
}

/// The recorded verdict a [`Fenced`] handle is built from.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FenceSource {
    Sealed(SealVerdict),
    Inherited(InheritVerdict),
}

/// What a drive asks admission for.
///
/// It names no root: admission mints the root inside its recorded body, from
/// the work it admits (the unfinished run it resumes, or the first item of
/// the queue prefix it takes), so a replay decodes the same root and a fresh
/// execution never trusts a root the caller guessed.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AdmitRequest {
    pub session: SessionId,
    pub request: DriveRequestId,
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
    /// The root already has a terminal: a later execution adopts it
    /// (ADR 0105 law L-S6).
    RootTerminal {
        root: TurnId,
        by: super::commit::TurnCommitId,
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

/// A child's validation of the authority it inherited.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "verdict", rename_all = "snake_case")]
pub enum InheritVerdict {
    Valid(DriveFence),
    Stale { current_epoch: u64 },
    GenerationRefused(SessionStateVersionRefusal),
}

/// An admission [`admit`](DriveAdmission::admit) granted, to be sealed.
///
/// It has no public constructor: it is decoded only from a recorded
/// [`AdmitVerdict::Admit`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Admitted {
    session: SessionId,
    root: TurnId,
    request: DriveRequestId,
    admission: AdmissionId,
    observed_epoch: u64,
    /// The head the root was admitted on. A replay rebuilds the root's input
    /// state from it, never from the live head (FIG-3682).
    base: crate::store::SessionHeadRef,
    /// The root's turn index, fixed at admission.
    turn_index: u64,
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
    /// The session's queued work: the unfinished queued run named by the
    /// root, or a new one under it.
    Queued,
    /// The follow-on the session head owes (ADR 0101 §3), which no queued
    /// run owns: its recovery, as recovery number `attempts + 1`. The
    /// recorded count is what the recovery raises from, so a redrive of the
    /// root never raises it twice.
    FollowOn { follow_on: TurnId, attempts: u32 },
}

impl Admitted {
    /// Only the `AdmitDrive` body mints an admission, through
    /// [`admission_body::admitted`](super::drive::admission_body::admitted).
    #[allow(clippy::too_many_arguments)]
    pub(super) fn minted(
        session: SessionId,
        root: TurnId,
        request: DriveRequestId,
        admission: AdmissionId,
        observed_epoch: u64,
        base: crate::store::SessionHeadRef,
        turn_index: u64,
        work: AdmittedWork,
    ) -> Self {
        Self {
            session,
            root,
            request,
            admission,
            observed_epoch,
            base,
            turn_index,
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

    /// The head the root was admitted on.
    pub fn base(&self) -> &crate::store::SessionHeadRef {
        &self.base
    }

    /// The root's turn index, fixed at admission and identical on every
    /// replay.
    pub fn turn_index(&self) -> u64 {
        self.turn_index
    }

    /// What the root drives.
    pub fn work(&self) -> &AdmittedWork {
        &self.work
    }
}

/// The authority a child inherits from the drive that started it. It is built
/// only from a [`Fenced`] handle, so it always names a sealed fence.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct InheritedAuthority {
    fence: DriveFence,
    parent: ReplayKey,
    group: Option<(GroupKey, u32)>,
}

impl InheritedAuthority {
    pub fn fence(&self) -> &DriveFence {
        &self.fence
    }

    /// The replay key of the parent operation that started the child.
    pub fn parent(&self) -> &ReplayKey {
        &self.parent
    }

    /// The group and position of a group child.
    pub fn group(&self) -> Option<&(GroupKey, u32)> {
        self.group.as_ref()
    }
}

/// Which turn-cancel gate a wait races.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CancelGate {
    /// The turn's gate: answers the first cancel request.
    Turn,
    /// The escalation gate: answers an `Immediate` request that follows an
    /// `AfterStep` one.
    Escalation,
}

/// What a turn-cancel gate delivered.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "signal", rename_all = "snake_case")]
pub enum TurnCancelSignal {
    Immediate(TurnCancellationEvidence),
    AfterStep(TurnCancellationEvidence),
    SessionRevoked,
}

/// A child execution's start: the authority it inherits and the command it
/// runs.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ChildStart {
    pub authority: InheritedAuthority,
    pub envelope: EffectCommand,
}

/// How a child execution ended.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum ChildOutcome {
    /// The child ran and recorded this result.
    Settled { result: EffectResult },
    /// The child refused its inherited authority as stale.
    Stale { current_epoch: u64 },
    /// The child refused the session's state generation.
    GenerationRefused(SessionStateVersionRefusal),
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
    pub park: super::commit::ParkId,
}
