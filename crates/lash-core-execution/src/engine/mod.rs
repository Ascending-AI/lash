//! The engine contract: the drive is deterministic workflow code
//! (ADR 0105, `docs/adr/0105-the-drive-is-deterministic-workflow-code.md`).
//!
//! A drive awaits only operations of an engine-supplied context. Admission is
//! unfenced and separate from fenced execution; every other durable operation
//! is reachable only through a [`Fenced`] handle built from a recorded
//! verdict. Commands are serializable, run by registered executors inside the
//! engine's recorded body, and never in drive code.
//!
//! This module names no engine. It holds traits and types only; engines
//! implement them in their own crates, and the drive body that consumes them
//! lands with the slices ADR 0105 lists.
//!
//! Rules every implementor and every drive-side caller keeps:
//!
//! - No `dyn Future` on the workflow-side chain. Every future a drive awaits
//!   is an engine's [`EngineContext::Op`], so a drive is `Send` exactly when
//!   its engine's ops are.
//! - A race reborrows both arms and keeps its loser; an op is given up only
//!   by [`EngineContext::dispose`], and a dropped op means abandon.
//! - Deadlines are absolute [`EpochMs`] read from [`EngineContext::now_ms`].
//! - Observation is synchronous and never decides anything.

mod admission;
mod commands;
mod commit;
mod context;
mod contracts;
mod groups;
/// The determinism harness every slice that makes the drive deterministic
/// proves its change with (FIG-3672).
#[cfg(any(test, feature = "testing"))]
pub mod testing;

pub use admission::{
    AdmissionId, AdmitRequest, AdmitVerdict, Admitted, CancelGate, ChildOutcome, ChildStart,
    DriveAdmission, DriveContext, DriveFence, DriveRequestId, FenceSource, Fenced, InheritVerdict,
    InheritedAuthority, ParkRef, SealVerdict, TurnCancelSignal,
};
pub use commands::{
    AdmissionCommand, AdmissionExecutors, AdmissionResult, AdmissionStepContext, CooperativeCancel,
    EffectCommand, EffectExecutors, EffectResult, GatedObservationSink, Heartbeat,
    NullObservationSink, ObservationCursor, ObservationSink, SessionServices, StepContext,
};
pub use commit::{
    CancellationSettlement, CommitTurnOutcome, CommittedAttachments, CommittedGraphNode,
    DurableTurnState, EngineParkRef, ExecutionStateUpdate, IngressSettlement, ParkId,
    ParkRecoveryWriter, ParkedWorkRef, RecordedPluginStates, SessionGraphDelta, SessionHeadRef,
    TurnCommitId, TurnCommitRequest, TurnTerminalEvidence, UsageDelta,
};
pub use context::{
    Disposed, Disposition, DriveObservation, DurableOp, EngineContext, EngineFault, EngineRetry,
    EngineTerminal, EpochMs, ObservedEvent, ReplayKey, Winner, activity_projection,
};
pub use contracts::{
    BuildGeneration, DriveHandover, DriveRequest, Never, PendingResolution, ResolveAck,
    RootProgress, TurnSegmentHandover, UnresolvedChild,
};
pub use groups::{DriveGroups, GroupClosed, GroupKey};

#[cfg(test)]
mod tests;
