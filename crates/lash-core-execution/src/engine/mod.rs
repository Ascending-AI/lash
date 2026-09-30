//! Engine-neutral drive values, recorded commands, and observations.
//!
//! The drive uses [`crate::RuntimeEffectController`]. Restate records its
//! effects and cancel races (ADR 0105).

mod admission;
mod commands;
mod context;
mod contracts;
mod control;
mod drive;
mod ingress;
mod reconcile;
/// The determinism harness every slice that makes the drive deterministic
/// proves its change with (FIG-3672).
#[cfg(any(test, feature = "testing"))]
pub mod testing;

pub use crate::store::{
    ControlIntentId, ControlIntentKind, RootTerminal, RootTerminalCause, RootTerminalKind,
};
pub use crate::store::{ParkId, RootTerminalWrite, SessionHeadRef, TurnCommitId};
pub use admission::{
    AdmissionId, AdmitRequest, AdmitVerdict, Admitted, AdmittedWork, DriveFence, DriveRequestId,
    ParkRef, RootStartNonce, SealVerdict,
};
pub use commands::{GatedObservationSink, NullObservationSink, ObservationCursor, ObservationSink};
pub use context::{DriveObservation, ObservedEvent, ReplayKey, activity_projection};
pub use contracts::{BuildGeneration, BuildGenerationParseError, DriveRequest, UpgradePolicy};
pub use control::{
    EngineAck, EngineCursor, EnginePage, EngineParkRecorded, EngineRefusal, NoEngineControl,
    NoScopeClose, ParkReconcileReport, ParkRecoveryWriter, ParkTarget, RootRef, ScopeCloseSink,
    SessionControlEngine, StalledExecution, begin_session_close_replay_key,
};
pub use drive::{
    DRIVE_CONTINUATION_PREFIX, DriveAbort, DriveHold, DriveLoop, DriveOutcome, DriveStop,
    MAX_ROOTS_PER_DRIVE, RootOutcome, RootRunEnd, admission_body, drive_admission_replay_key,
    drive_admission_scope, drive_close_root_replay_key, drive_continuation_request,
    drive_root_scope, drive_root_start_replay_key, drive_seal_replay_key,
};
pub use ingress::{FIRST_INGRESS_ATTEMPT, ingress_drive_request};
pub use reconcile::{
    ReconcileArm, ReconcileCursor, ReconcileFailure, ReconcileTick, RecoveryLeaseConfig,
    RecoveryLeaseTimings, RecoveryPassBudget, RelayPass, SlotPass,
};
