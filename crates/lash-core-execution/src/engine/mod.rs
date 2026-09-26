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
mod control;
mod drive;
mod groups;
mod reconcile;
/// The determinism harness every slice that makes the drive deterministic
/// proves its change with (FIG-3672).
#[cfg(any(test, feature = "testing"))]
pub mod testing;

pub use crate::store::{
    ControlIntentId, ControlIntentKind, RootTerminal, RootTerminalCause, RootTerminalKind,
};
pub use admission::{
    AdmissionId, AdmitRequest, AdmitVerdict, Admitted, AdmittedWork, CancelGate, ChildOutcome,
    ChildStart, DriveAdmission, DriveContext, DriveFence, DriveRequestId, FenceSource, Fenced,
    InheritVerdict, InheritedAuthority, ParkRef, RootStartNonce, SealVerdict, TurnCancelSignal,
};
pub use commands::{
    AdmissionCommand, AdmissionExecutors, AdmissionResult, AdmissionStepContext, CooperativeCancel,
    EffectCommand, EffectExecutors, EffectResult, GatedObservationSink, Heartbeat,
    NullObservationSink, ObservationCursor, ObservationSink, SessionServices, StepContext,
};
pub use commit::{
    CancellationSettlement, CommitTurnOutcome, CommittedAttachments, CommittedGraphNode,
    DurableTurnState, ExecutionStateUpdate, IngressSettlement, ParkId, RecordedPluginStates,
    RootTerminalWrite, SessionGraphDelta, SessionHeadRef, TurnCommitId, TurnCommitRequest,
    UsageDelta,
};
pub use context::{
    Disposed, Disposition, DriveObservation, DurableOp, EngineContext, EngineFault, EngineRetry,
    EngineTerminal, EpochMs, ObservedEvent, ReplayKey, Winner, activity_projection,
};
pub use contracts::{
    BuildGeneration, BuildGenerationParseError, DriveHandover, DriveRequest, Never,
    PendingResolution, ResolveAck, RootProgress, TurnSegmentHandover, UnresolvedChild,
    UpgradePolicy,
};
pub use control::{
    EngineAck, EngineCursor, EnginePage, EngineParkRecorded, EngineRefusal, NoEngineControl,
    NoScopeClose, ParkReconcileReport, ParkRecoveryWriter, ParkTarget, RootRef, ScopeCloseSink,
    SessionControlEngine, begin_session_close_replay_key,
};
pub use drive::{
    DriveAbort, DriveLoop, DriveOutcome, DriveStop, MAX_ROOTS_PER_DRIVE, RootOutcome,
    admission_body, drive_admission_replay_key, drive_admission_scope, drive_close_root_replay_key,
    drive_continuation_request, drive_root_scope, drive_root_start_replay_key,
    drive_seal_replay_key,
};
pub use groups::{DriveGroups, GroupClosed, GroupKey};
pub use reconcile::{
    DriveReconcileReport, ReconcileArm, ReconcileCursor, ReconcileFailure, ReconcileTick, SlotPass,
};

#[cfg(test)]
mod tests;
