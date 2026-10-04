use super::*;
use crate::runtime::turn_control::ActiveTurnControl;

mod context;
pub(in crate::runtime) use context::register_live_opener;
mod effects;
pub(in crate::runtime) use effects::normalize_plugin_message_attachments;
mod events;
mod failures;
mod handlers;
mod lease;
mod local_effects;
mod machine;
mod opener_groups;
pub(in crate::runtime) use opener_groups::OpenerForCommit;
mod segment;
pub(in crate::runtime) use segment::{BoundaryTaken, TurnSegment};
mod streaming;
mod tool_catalog;
mod tools;
mod trace;

pub(in crate::runtime) use crate::runtime::turn_loop::{
    ingress_admitted_trace_payload, send_queued_work_started_event,
};
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
    pub(super) scoped_effect_controller: ScopedEffectController<'a>,
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
    /// The fence of the shift admission the turn's run executes under: the
    /// authority its checkpoint admissions present (FIG-3927). `None` for a
    /// turn that runs under no admitted run, which admits nothing.
    pub(super) shift_fence: Option<ShiftFence>,
    /// The logical run the turn's checkpoint admissions bind rows to.
    pub(super) shift_run: Option<crate::TurnId>,
    /// The build generation the turn's run invocation runs on: the one its
    /// admission stamped (FIG-4742), whose drain mark the turn reads at a
    /// quiet point (FIG-4739).
    pub(super) drive_generation: Option<crate::engine::BuildGeneration>,
    pub(super) turn_phase_probe: Option<Arc<dyn RuntimeTurnPhaseProbe>>,
    pub(super) turn_control: Arc<ActiveTurnControl>,
    /// Names the reply the protocol driver materialized, for the boundary's
    /// terminal materialization to recognize by identity.
    pub(super) protocol_reply: machine::ProtocolReplyTracker,
    /// This turn's registration in the host's live-opener registry
    /// (ADR 0099 §2, §3, FIG-2266).
    ///
    /// Registered the first time the turn builds a tool-execution context and
    /// re-registered on each later one, so a tool child of a group this turn
    /// opened borrows the turn's *current* live context. Deregistered when the
    /// guard is released at the end of `run`. A segment boundary registers
    /// again through its commit decision, and its continuation registers the
    /// same logical opener. A terminal closes and finalizes every held group
    /// before committing (ADR 0099 §7): finalization may
    /// have to run a child no process is running, and that child resolves its
    /// executor through this registration.
    pub(super) live_opener: std::sync::Mutex<Option<crate::facade_support::LiveOpenerGuard>>,
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
    /// The cooperative stop the turn lends to the tool children its
    /// live-opener registration serves (FIG-2266). The shift fires it when it
    /// records the turn's cancellation, at a recorded point, so a replay
    /// fires it at the same point; the children record what they observed in
    /// their own settlements. No shift code reads it (FIG-3672 P9).
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
}

impl RuntimeTurnDriver<'_> {
    /// Records the cancellation this turn honours, from a journaled peek or a
    /// recorded outcome, and stops the children it lent its context to.
    pub(super) fn record_turn_cancel(&mut self, evidence: crate::TurnCancellationEvidence) {
        self.turn_cancel = Some(evidence);
        self.children_stop.cancel();
    }
}
