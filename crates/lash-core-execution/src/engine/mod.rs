//! Engine-neutral shift values, recorded commands, and observations.
//!
//! The shift issues its effects through [`crate::ActorContext`].

mod admission;
mod commands;
mod context;
mod contracts;
mod control;
mod ingress;
mod reconcile;
mod shift;

pub use crate::store::{
    ControlIntentId, ControlIntentKind, RunTerminal, RunTerminalCause, RunTerminalKind,
};
pub use crate::store::{ParkId, RunTerminalWrite, SessionHeadRef, TurnCommitId};
pub use admission::{
    AdmissionId, AdmitRequest, AdmitVerdict, Admitted, AdmittedWork, ParkRef, RunStartNonce,
    SealRefusal, SealVerdict, ShiftFence, ShiftRequestId,
};
pub use commands::{GatedObservationSink, NullObservationSink, ObservationCursor, ObservationSink};
pub use context::{ObservedEvent, ReplayKey, ShiftObservation, activity_projection};
pub use contracts::{
    BuildGeneration, BuildGenerationParseError, EngineGeneration, GenerationRebound,
    GenerationUnbound, ShiftRequest, UpgradePolicy,
};
pub use control::{
    EngineAck, EngineCursor, EnginePage, EngineParkRecorded, EngineRefusal, NoEngineControl,
    NoScopeClose, OpenRun, ParkReconcileReport, ParkRecoveryWriter, ParkTarget, RefusalClass,
    RunLoss, RunRef, ScopeCloseSink, SessionControlEngine, StalledExecution,
    begin_session_close_replay_key,
};
pub use ingress::{FIRST_INGRESS_ATTEMPT, ingress_shift_request};
pub use reconcile::{
    ReconcileArm, ReconcileCursor, ReconcileFailure, ReconcileTick, RecoveryLeaseConfig,
    RecoveryLeaseTimings, RecoveryPassBudget, RelayPass, SlotPass,
};
pub use shift::{
    MAX_RUNS_PER_SHIFT, RunEnd, RunOutcome, SHIFT_CONTINUATION_PREFIX, ShiftAbort, ShiftHold,
    ShiftLoop, ShiftOutcome, ShiftStop, admission_body, shift_admission_replay_key,
    shift_admission_scope, shift_close_run_replay_key, shift_continuation_request, shift_run_scope,
    shift_run_start_replay_key, shift_seal_replay_key,
};
