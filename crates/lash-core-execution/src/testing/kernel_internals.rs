//! Crate-internal kernel surface the relocated store-backed tests reach for.
//!
//! Those tests were unit tests inside this crate until FIG-3582 moved every
//! test that needs a concrete store or effect host to
//! `tests/store_backed`, over a SQLite memory backend (ADR 0102): the store
//! depends on this crate, so an in-crate `cfg(test)` module could not use it.
//! This module is the `testing`-feature seam that keeps exactly the surface
//! they reach reachable without widening the crate's shipped API. A
//! crate-private function is reached through a wrapper here; a type or
//! function already declared `pub` inside a private module is re-exported.

// The crate-root short paths (`crate::X`) the relocated tests used in-crate
// that the root re-exports only to the crate. Each item is already public at
// its own path; these re-exports keep the short path the tests were written
// against.
pub use crate::attachments::{
    AttachmentProducer, AttachmentSourcePolicy, OpenAttachmentSourcePolicy,
};
pub use crate::plugin::{
    RuntimeServices, SessionObservedProcessOutcome, SessionObservedProcessReceipt,
    SessionObserverIntent,
};
pub use crate::runtime::UnavailableProcessService;
pub use crate::runtime::{
    artifact_owner_is_permanently_retired, artifact_staging_owner_edge_is_missing,
    load_process_execution_env, publish_process_execution_env,
};
pub use crate::session::{RuntimeExecutionTracing, Session};

use crate::runtime::process::{ObservedWorkItem, ProcessRecord, ProcessWorkObserver};
use crate::{PluginError, ProcessId};

/// `ProcessWorkObserver::work_item_from_record`: the bounded record/event-tail
/// retry the observation laws drive with a record read before a concurrent
/// write.
pub async fn work_item_from_record(
    observer: &ProcessWorkObserver,
    record: ProcessRecord,
) -> Result<ObservedWorkItem, PluginError> {
    observer.work_item_from_record(record).await
}

/// `process_terminal_resolution`: the resolution a process terminal delivers
/// to the waits armed on it, which the attach-terminal laws compare a parked
/// wait against.
pub fn process_terminal_resolution(output: crate::ProcessAwaitOutput) -> crate::Resolution {
    crate::runtime::effect::executor::process_terminal_resolution(output)
}

/// `EffectControllerTaskRequest::into_future`: serves one proxied request
/// against `controller`, the way the task that owns the proxy drives it; the
/// queued-lane round-trip law is that task.
pub async fn serve_effect_controller_task_request(
    request: crate::runtime::effect::executor::EffectControllerTaskRequest,
    controller: &dyn crate::RuntimeEffectController,
) {
    request.into_future(controller).await;
}

/// `PluginStateStore::bind` over a session's plugin-state registry: the
/// handle a plugin receives, which the checkpoint-generation laws write
/// through between captures and commits.
pub fn plugin_state_store(
    plugins: &crate::PluginSession,
    session_id: &crate::SessionId,
    plugin_id: &str,
) -> crate::PluginStateStore {
    plugins.plugin_state_store_for_testing(session_id, plugin_id)
}

/// `ToolDispatchContext::attempt_may_defer`: whether a call may park, from its
/// catalog entry or the grant that admits it.
pub fn dispatch_attempt_may_defer(
    context: &crate::tool_dispatch::ToolDispatchContext<'_>,
    tool_id: &crate::ToolId,
    grant: Option<&crate::ToolExecutionGrant>,
) -> bool {
    context.attempt_may_defer(tool_id, grant)
}

/// `RuntimeExecutionTracing::emit_tool_call_completed`: the trace a completed
/// tool call emits, which the retry-trace law reads back.
pub fn emit_tool_call_completed(
    tracing: &crate::session::RuntimeExecutionTracing,
    record: &crate::ToolCallRecord,
    attempts: &[lash_trace::TraceRetryAttempt],
    issuing_node_id: Option<&str>,
    clock: &dyn crate::Clock,
) {
    tracing.emit_tool_call_completed(record, attempts, issuing_node_id, clock);
}

/// `RuntimeExecutionContext::process_start_execution_env`: the reference and
/// spec a process start carries into its journaled command, which the
/// publish-ordering laws read before the effect runs.
pub fn process_start_execution_env(
    context: &crate::RuntimeExecutionContext<'_>,
    registration: crate::ProcessRegistration,
) -> (
    crate::ProcessRegistration,
    Option<crate::ProcessExecutionEnvSpec>,
) {
    context.process_start_execution_env(registration)
}

/// `ToolChildHost::child_controller`: the group-child-bound controller a
/// child's recorded admission mints, which the claim-pin law inspects.
pub fn tool_child_controller(
    host: &crate::runtime::effect::ToolChildHost,
    admitted: &crate::AdmittedScope,
    binding: crate::GroupChildBinding,
) -> Result<crate::ScopedEffectController<'static>, crate::RuntimeEffectControllerError> {
    host.child_controller(admitted, binding)
}

/// The recorded-authority checks a reopened tool child passes before it runs:
/// its cancellation binding and completion routing against this host.
pub async fn validate_recorded_authorities(
    host: &crate::runtime::effect::ToolChildHost,
    controller: &crate::ScopedEffectController<'_>,
    request: &crate::runtime::effect::ToolChildRequest,
) -> Result<(), crate::RuntimeEffectControllerError> {
    crate::runtime::effect::validate_recorded_authorities(host, controller, request).await
}

/// `InternalProcessAdmin` for a session over `processes`: the admin surface a
/// process tool receives, which the await-visibility law drives directly.
pub fn internal_process_admin<'run>(
    session_id: crate::SessionId,
    agent_frame_id: crate::FrameNodeId,
    processes: std::sync::Arc<dyn crate::ProcessService>,
    effect_controller: crate::runtime::RuntimeEffectControllerHandle<'run>,
    execution_env_spec: crate::ProcessExecutionEnvSpec,
) -> crate::InternalProcessAdmin<'run> {
    crate::InternalProcessAdmin::for_testing(
        session_id,
        agent_frame_id,
        processes,
        effect_controller,
        execution_env_spec,
    )
}

// `RuntimeExecutionContext`'s process-handle and process-await operations:
// what a language runtime's process host calls on a context, which the
// relocated handle, await and batch laws drive directly.

pub async fn await_process_handle(
    context: &crate::RuntimeExecutionContext<'_>,
    call_id: String,
    handle: serde_json::Value,
) -> crate::session::ToolInvocationReply {
    context.await_process_handle(call_id, handle).await
}

pub async fn signal_process_handle(
    context: &crate::RuntimeExecutionContext<'_>,
    call_id: String,
    handle: serde_json::Value,
    signal_name: String,
    payload: serde_json::Value,
) -> crate::session::ToolInvocationReply {
    context
        .signal_process_handle(call_id, handle, signal_name, payload)
        .await
}

pub async fn cancel_process_handle(
    context: &crate::RuntimeExecutionContext<'_>,
    call_id: String,
    handle: serde_json::Value,
) -> crate::session::ToolInvocationReply {
    context.cancel_process_handle(call_id, handle).await
}

pub async fn start_tool_process(
    context: &crate::RuntimeExecutionContext<'_>,
    call_id: String,
    tool_name: String,
    args: serde_json::Value,
) -> crate::session::ToolInvocationReply {
    context.start_tool_process(call_id, tool_name, args).await
}

pub fn record_started_process(
    context: &crate::RuntimeExecutionContext<'_>,
    process_id: &ProcessId,
) {
    context.record_started_process(process_id);
}

pub async fn await_process_with_cancellation(
    context: &crate::RuntimeExecutionContext<'_>,
    process_ref: &crate::ProcessRef,
    parent_invocation: Option<crate::RuntimeInvocation>,
    cancellation: Option<tokio_util::sync::CancellationToken>,
) -> Result<crate::ProcessAwaitOutput, PluginError> {
    context
        .await_process_with_cancellation(process_ref, parent_invocation, cancellation)
        .await
}

pub fn emit_tool_call_started(
    context: &crate::RuntimeExecutionContext<'_>,
    call_id: &str,
    name: &str,
    args: serde_json::Value,
    activity_id: crate::TurnActivityId,
) {
    context.emit_tool_call_started(call_id, name, args, activity_id);
}

/// `ToolRegistry::from_tool_registrations`: a registry over explicit source,
/// internal and orchestrating registrations, as a plugin session assembles it.
pub fn tool_registry_from_registrations(
    sources: Vec<(String, Vec<std::sync::Arc<dyn crate::ToolProvider>>)>,
    internal_tools: Vec<crate::InternalProcessToolDef>,
    orchestrating_tools: Vec<crate::facade_support::OrchestratingToolDef>,
) -> Result<crate::ToolRegistry, crate::tool_registry::ReconfigureError> {
    crate::ToolRegistry::from_tool_registrations(sources, internal_tools, orchestrating_tools)
}

/// The turn's cancellation-escalation await-event key, so a test can peek or
/// forge the escalation row a host journals.
pub async fn turn_escalation_key(
    resolver: &dyn crate::AwaitEventResolver,
    address: &crate::runtime::TurnAddress,
) -> Result<crate::AwaitEventKey, crate::RuntimeError> {
    crate::runtime::turn_control::escalation_key(resolver, address).await
}

/// The turn's base cancellation-gate await-event key.
pub async fn turn_cancel_gate_key(
    resolver: &dyn crate::AwaitEventResolver,
    address: &crate::runtime::TurnAddress,
) -> Result<crate::AwaitEventKey, crate::RuntimeError> {
    crate::runtime::turn_control::cancel_gate_key(resolver, address).await
}

/// The resolution a cancellation gate records when `evidence` wins it, so a
/// test can resolve a gate as a peer writer would.
pub fn cancel_requested_gate_resolution(
    evidence: crate::runtime::TurnCancellationEvidence,
) -> Result<crate::Resolution, crate::RuntimeError> {
    crate::runtime::turn_control::gate_resolution(
        crate::runtime::turn_control::TurnGateTerminal::CancelRequested(evidence),
    )
}

/// The evidence an active turn records when it cancels itself internally.
pub fn active_turn_internal_evidence(
    active: &crate::runtime::turn_control::ActiveTurnControl,
) -> crate::runtime::TurnCancellationEvidence {
    active.internal_evidence(None)
}
