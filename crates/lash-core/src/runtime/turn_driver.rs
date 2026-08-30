use super::*;
use crate::runtime::turn_control::ActiveTurnControl;

mod context;
mod effects;
mod events;
mod failures;
mod handlers;
mod lease;
mod machine;
mod streaming;
mod tool_catalog;
mod tools;
mod trace;

pub(in crate::runtime) use crate::runtime::turn_loop::{
    queued_work_trace_payload, send_queued_work_started_event,
};
pub(super) use events::{
    emit_semantic_response_parts, send_session_event, send_turn_activity,
    send_turn_input_applications,
};
use handlers::foreground_exec_graph_key;
pub(super) use trace::protocol_step_trace_event;

pub(super) struct RuntimeTurnDriver<'a> {
    pub(super) session: Session,
    pub(super) policy: RuntimeSessionPolicy,
    pub(super) host: RuntimeHost,
    pub(super) scoped_effect_controller: ScopedEffectController<'a>,
    pub(super) session_id: String,
    pub(super) turn_id: crate::TurnId,
    pub(super) turn_index: usize,
    pub(super) turn_pipeline: TurnBoundary,
    pub(super) llm_stream_summaries: HashMap<usize, LlmStreamSummary>,
    /// Parent-session calls only. Child runtimes assemble their own ledgers.
    pub(super) llm_calls: Vec<crate::LlmCallRecord>,
    /// Non-transcript evidence from charge-safety-refused generations, with
    /// cardinality capped at one component per sealed provider attempt.
    pub(super) failure_evidence: Vec<crate::TurnFailureEvidence>,
    pub(super) next_llm_ordinal: usize,
    pub(super) session_services: Arc<RuntimeSessionServices>,
    pub(super) protocol_turn_options: crate::ProtocolTurnOptions,
    pub(super) protocol_extension: Option<crate::ProtocolTurnExtensionHandle>,
    pub(super) turn_context: crate::TurnContext,
    pub(super) turn_causes: Vec<crate::TurnCause>,
    pub(super) pending_queue_claims: Vec<crate::QueuedWorkClaim>,
    pub(super) pending_turn_input_claims: Vec<crate::runtime::turn_input_ingress::TurnInputDrive>,
    pub(super) pending_checkpoint_turn_input_claim: Option<crate::TurnInputClaim>,
    pub(super) checkpoint_messages: crate::tool_dispatch::CheckpointMessageBuffer,
    pub(super) recorded_intent_outcomes: crate::tool_dispatch::RecordedToolIntentOutcomeBuffer,
    pub(super) session_execution_lease: Option<crate::SessionExecutionLeaseAuthority>,
    pub(super) runtime_lease_owner: crate::LeaseOwnerIdentity,
    pub(super) turn_phase_probe: Option<Arc<dyn RuntimeTurnPhaseProbe>>,
    pub(super) turn_control: Arc<ActiveTurnControl>,
    pub(super) observes_durable_cancel_after_llm: bool,
}
