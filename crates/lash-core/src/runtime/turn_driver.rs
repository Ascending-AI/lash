use super::*;
use crate::ActorContext;

mod context;
mod effects;
pub(in crate::runtime) use effects::normalize_plugin_message_attachments;
mod events;
mod failures;
mod handlers;
pub(crate) mod issue;
mod lease;
mod local_effects;
mod machine;
mod tool_run_close;
pub(in crate::runtime) use tool_run_close::OpenerForCommit;
mod segment;
pub(in crate::runtime) use segment::{BoundaryTaken, TurnSegment};
mod streaming;
mod tool_catalog;
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
    /// The host-local stop the turn reads at its boundaries.
    pub(super) turn_control: crate::LocalTurnStop,
    /// Names the reply the protocol driver materialized, for the boundary's
    /// terminal materialization to recognize by identity.
    pub(super) protocol_reply: machine::ProtocolReplyTracker,
    /// The turn's opener state (ADR 0099 §6, §7): the once-only incorporation
    /// ledger and the groups the turn still holds after an aggregate stopped
    /// consuming early. Every phase context the turn builds shares it, so a
    /// loser the turn's end incorporates is charged against the same ledger
    /// the winning cell charged.
    pub(super) opener_state: crate::session::OpenerState,
    /// The cancellation this turn recorded honouring: the answer of a
    /// journaled gate peek, and nothing else (FIG-3672 P9). Shift decisions
    /// that depend on the turn's cancellation read this, never a live token.
    pub(super) turn_cancel: Option<crate::TurnCancellationEvidence>,
    /// Cooperative cancellation for recorded tool bodies in this turn.
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
    /// The turn's part in its run's segment boundaries (FIG-4739).
    pub(super) segment: TurnSegment,
    /// The lifetime this value is bound to; the context it carries is `'static`.
    pub(crate) run: std::marker::PhantomData<&'a ()>,
}

impl RuntimeTurnDriver<'_> {
    /// Records the cancellation this turn honours, from a journaled peek or a
    /// recorded outcome, and stops the children it lent its context to.
    pub(super) fn record_turn_cancel(&mut self, evidence: crate::TurnCancellationEvidence) {
        self.turn_cancel = Some(evidence);
        self.children_stop.cancel();
    }
}
