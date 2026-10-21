mod awaiter;
mod declared_start;
mod engine;
mod env_hold;
pub mod env_policy;
pub use env_hold::{HoldEnvError, ProcessEnvPorts, ProcessEnvSelection, hold_process_env};
pub(crate) use env_hold::{hold_launch, hold_launch_performing};
pub use env_policy::{
    EnclosingProcess, ProcessEnvChoice, ProcessEnvCx, ProcessEnvPolicy, ProcessEnvPolicyError,
    ProcessStartChoice, ResolvedInlineStart, ResolvedStartTarget,
};
#[cfg(test)]
mod guarded_surface_tests;
mod observer_intent;
mod op_scope;
mod service;
mod start_phases;
mod start_staging;
pub mod steps;
mod succession;
#[cfg(any(test, feature = "testing"))]
mod testing;
#[cfg(test)]
mod tests;
pub use awaiter::{
    ProcessChangeHub, ProcessChangeSubscription, ProcessEventSink, WatchedRegistry,
    watch_process_registry,
};
pub use declared_start::{
    DeclaredStartObligation, DeclaredStartObligationRefusal, DeclaredStartPhase,
    IsolatedStartRefusal, IsolatedToolStart, StartCancelDecision,
};
pub use engine::{ProcessEngineRegistration, ProcessEngineRegistry, WeakProcessEngineRegistry};
pub use observer_intent::{
    SessionObserverIntentSource, reconcile_session_process_observer_intents,
};
pub use op_scope::ProcessOpScope;
pub(crate) use op_scope::{LanguageCallAttribution, LanguageCallAttributions};
pub use service::{ProcessService, ProcessToolVisibilityFilter};
pub use start_phases::StartPhaseClock;
pub use start_staging::{
    ArtifactReferrerPorts, HostStartAdmission, PreparedProcessStart, ProcessStartStores,
    ReferrerAcquisition, RegisteredProcessStart, SessionTurnAdmission, StagedProcessStart,
    StartStaging, is_start_operation, register_process_start, stage_process_start,
    stage_store_local_start, start_operation_journal,
};
pub use steps::{
    ProcessStepTools, ProcessSteps, StepAdmission, StepRefusal, StepRuntime, StepToolsError,
};
pub use succession::request_process_successor;
pub use succession::succession_context;
pub(crate) use succession::{carry_refusal, linked_start, relinked_step};
#[cfg(any(test, feature = "testing"))]
pub use testing::*;
mod definition_acquisition;
mod definition_admission;
use lash_execution_contracts::process::model;
use lash_execution_contracts::process::registry_delegate;
use lash_execution_contracts::process::*;
pub fn current_epoch_ms() -> u64 {
    <crate::SystemClock as crate::ClockWallTime>::timestamp_ms(&crate::SystemClock)
}
