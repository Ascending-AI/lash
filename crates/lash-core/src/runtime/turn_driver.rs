use super::*;
use crate::ActorContext;

mod context;
mod durable_drive;
pub(in crate::runtime) use durable_drive::{DriveParts, RuntimeDrive};
mod effects;
mod events;
mod failures;
mod handlers;
pub(crate) mod issue;
mod lease;
mod local_effects;
mod machine;
mod streaming;
mod tool_catalog;
mod tool_run_close;
mod tools;
mod trace;

pub(in crate::runtime) use crate::runtime::turn_loop::send_queued_work_started_event;
pub(super) use events::{emit_semantic_response_parts, send_turn_input_applications};
use handlers::foreground_exec_graph_key;
pub(super) use trace::protocol_step_trace_event;

pub(super) struct RuntimeTurnDriver<'a> {
    pub(super) tool_run_owner: Option<lash_core_execution::core_internal::ToolRunOwner>,
    pub(super) session: Session,
    pub(super) policy: RuntimeSessionPolicy,
    /// The turn's committed content, recorded in program order from the
    /// machine's emissions and the driver's own terminal events.
    pub(super) recorded_assembly: RecordedTurnAssembly,
    pub(super) host: RuntimeHost,
    pub(super) scoped_effect_controller: ActorContext,
    pub(super) session_id: SessionId,
    pub(super) turn_id: crate::TurnId,
    pub(super) turn_index: usize,
    pub(super) turn_pipeline: TurnBoundary,
    pub(super) prelude: Box<crate::runtime::effect::TurnPrelude>,
    /// Most recent provider usage observed during this execution attempt.
    ///
    /// The turn pipeline retains the projection basis captured before the
    /// logical turn began so a persisted continuation cannot rebuild history
    /// from a later call in the same turn.
    pub(super) latest_prompt_usage: Option<crate::TokenUsage>,
    /// Parent-session calls only. Child runtimes assemble their own ledgers.
    pub(super) llm_calls: Vec<crate::LlmCallRecord>,
    /// Non-transcript evidence from charge-safety-refused generations, with
    /// cardinality capped at one component per sealed provider attempt.
    pub(super) failure_evidence: Vec<crate::TurnFailureEvidence>,
    pub(super) session_services: Arc<RuntimeSessionServices>,
    pub(super) protocol_turn_options: crate::ProtocolTurnOptions,
    pub(super) turn_context: crate::TurnContext,
    pub(super) turn_causes: Vec<crate::TurnCause>,
    pub(super) pending_queued: Vec<crate::AdmittedQueuedWork>,
    pub(super) pending_turn_inputs: Vec<crate::AdmittedTurnInputs>,
    pub(super) pending_checkpoint_turn_inputs: Option<crate::AdmittedTurnInputs>,
    /// FIG-3157: work admitted at a terminal checkpoint and withheld from its
    /// delivery, so the committed finish stays this turn's answer. It is never
    /// settled as this turn's completed work. A finished turn's logical run
    /// executes it in a follow-on turn. When this turn is cancelled that
    /// follow-on never runs: the final commit hands withheld turn input to the
    /// cancellation's undelivered disposition (FIG-3531) and releases withheld
    /// wakes (FIG-3543).
    pub(super) withheld_terminal_work: super::logical_turn::WithheldTerminalWork,
    pub(super) checkpoint_messages: crate::tool_dispatch::CheckpointMessageBuffer,
    pub(super) turn_phase_probe: Option<Arc<dyn RuntimeTurnPhaseProbe>>,
    /// Names the reply the protocol driver materialized, for the boundary's
    /// terminal materialization to recognize by identity.
    pub(super) protocol_reply: machine::ProtocolReplyTracker,
    /// The turn's opener state (ADR 0099 §6, §7): the once-only incorporation
    /// ledger and the groups the turn still holds after an aggregate stopped
    /// consuming early. Every phase context the turn builds shares it, so a
    /// loser the turn's end incorporates is charged against the same ledger
    /// the winning cell charged.
    pub(super) opener_state: crate::session::OpenerState,
    /// Cooperative cancellation for the tool bodies this turn's cells start:
    /// fired when an accepted `Immediate` cancel stops a running cell.
    pub(super) children_stop: CancellationToken,
    /// The turn-scope observation cursor: every host-facing emission the
    /// driver makes outside an effect body sequences under the turn scope's
    /// journal key, so no two emissions share an identity (ADR 0105 §1). An
    /// effect body's emissions key under that effect's invocation replay key
    /// instead.
    pub(super) turn_observations: crate::engine::ObservationCursor,
    /// Where this driver stands when it observes: the turn's shift, which
    /// may emit once a step body of this attempt has really run, or, on the
    /// copy a recorded step's body runs on, that body's live step.
    pub(super) trace: crate::trace::TraceStanding,
    /// The lifetime this value is bound to; the context it carries is `'static`.
    pub(crate) run: std::marker::PhantomData<&'a ()>,
}
