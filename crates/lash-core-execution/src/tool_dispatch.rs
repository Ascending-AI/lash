mod admission;
mod atomic_attempt;
mod attempt_coordinator;
mod call_run;
mod context;
#[cfg(any(test, feature = "testing"))]
mod execution;
mod hooks;
mod intent_executor;
mod pending_resolver;
mod preparation;
mod production;
pub use production::parked_call_output;
mod realization;
mod retry;
pub use production::{CellCall, CellHostCalls, CellMember, CellMembers, HostCall};
mod singleton_run;

pub use crate::runtime::process::{
    DeclaredStartObligation, DeclaredStartObligationRefusal, IsolatedStartRefusal,
    IsolatedToolStart,
};
pub use crate::tool_run::RunCutRefusal;
pub use admission::{ToolRoundRefusal, admission_failure, admit_tool_round};
pub use call_run::{AdmittedToolCall, AttemptEnd, CallEnd};
pub use context::{ToolDispatchContext, ToolTriggerEffectOutcome};
pub use pending_resolver::{LaunchReceipt, model_visible_intent_outcomes};
pub use realization::{Realization, RealizationReceipt};
pub use singleton_run::{
    BeforeCheckReply, IsolatedBinding, IsolatedProcessDescriptor, SingletonAttempt,
    SingletonBodyOutcome, SingletonCapture, SingletonPreparedRequest, SingletonPresentationError,
    SingletonRunError, SingletonToolCall, SingletonToolHandlers, StartLaunch,
};

pub(crate) use atomic_attempt::AtomicToolAttempt;
pub use attempt_coordinator::{ToolAttemptLineage, coordinate_tool_invocation};
pub use context::{ToolCallIds, ToolDispatchOutcome, ToolPreparationOutcome};
#[cfg(feature = "testing")]
pub use context::{ToolCallLaunch, ToolTriggerOutcomeBuffer};
#[cfg(not(feature = "testing"))]
pub use context::{ToolCallLaunch, ToolTriggerOutcomeBuffer};
#[cfg(any(test, feature = "testing"))]
pub(crate) use execution::coordinate_prepared_tool_call_launch_with_execution_context;
pub use hooks::finalize_tool_result_with_execution_context;
pub(crate) use hooks::{attempt_occurrence, deferred_occurrence};
pub use intent_executor::IntentRealizationContext;
// Cross-crate execution seam used by lash-core's production realization runtime.
pub use intent_executor::execute_final_tool_intents;
#[cfg(any(test, feature = "testing"))]
pub(crate) use preparation::dispatch_tool_call_with_execution_context;
pub use preparation::resolve_callable_manifest_by_id;
#[cfg(any(test, feature = "testing"))]
pub use preparation::{dispatch_tool_call, resolve_tool_argument_projection_policy};
pub use preparation::{
    prepare_granted_tool_call_with_context, prepare_recorded_tool_call_with_context,
    prepare_tool_call_with_context, resolve_callable_manifest,
};
#[cfg(any(test, feature = "testing"))]
pub(crate) use retry::execute_once;
pub(crate) use retry::normalized_outcome;
pub(crate) use retry::settle_completed_pending_tool_call;
