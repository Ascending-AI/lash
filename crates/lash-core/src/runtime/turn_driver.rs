use super::*;
use crate::runtime::turn_control::ActiveTurnControl;

mod capture_writer;
pub use capture_writer::deployment_turn_tool_capture;
mod context;
mod effects;
mod events;
mod failures;
mod handlers;
mod lease;
mod local_effects;
mod machine;
mod opener_groups;
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
    /// drives it in a follow-on turn. When this turn is cancelled that
    /// follow-on never runs: the final commit hands withheld turn input to the
    /// cancellation's undelivered disposition (FIG-3531) and releases withheld
    /// wakes (FIG-3543).
    pub(super) withheld_terminal_work: super::logical_turn::WithheldTerminalWork,
    pub(super) checkpoint_messages: crate::tool_dispatch::CheckpointMessageBuffer,
    /// The fence of the drive admission the turn's root runs under: the
    /// authority its checkpoint admissions present (FIG-3927). `None` for a
    /// turn that runs under no admitted root, which admits nothing.
    pub(super) drive_fence: Option<DriveFence>,
    /// The logical root the turn's checkpoint admissions bind rows to.
    pub(super) drive_root: Option<crate::TurnId>,
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
    /// guard is released at the end of `run`, after the turn's end has closed
    /// and finalized every group the turn held (ADR 0099 §7): finalization may
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
    /// journaled gate peek, and nothing else (FIG-3672 P9). Drive decisions
    /// that depend on the turn's cancellation read this, never a live token.
    pub(super) turn_cancel: Option<crate::TurnCancellationEvidence>,
    /// The cooperative stop the turn lends to the tool children its
    /// live-opener registration serves (FIG-2266). The drive fires it when it
    /// records the turn's cancellation, at a recorded point, so a replay
    /// fires it at the same point; the children record what they observed in
    /// their own settlements. No drive code reads it (FIG-3672 P9).
    pub(super) children_stop: CancellationToken,
    /// The turn-scope observation cursor: every host-facing emission the
    /// driver makes outside an effect body sequences under the turn scope's
    /// journal key, so no two emissions share an identity (ADR 0105 §1). An
    /// effect body's emissions key under that effect's invocation replay key
    /// instead.
    pub(super) turn_observations: crate::engine::ObservationCursor,
    /// Checkpoints this physical turn has issued: the capture base its next
    /// checkpoint's body advances to (ADR 0114 §3.1). Counted where the
    /// checkpoint is issued, so a replay counts the same.
    pub(super) capture_base: u32,
    /// The iteration's tool batch interrupted a call, by the recorded facts
    /// its outcome carries: a group wait lost to the turn's stop, or a call
    /// settled cancelled. The checkpoint that commits the cancelled calls
    /// leaves the capture base where it is, so a stop's partial keeps what
    /// the host saw of them (ADR 0114, Lane G amendment).
    pub(super) interrupted_calls: bool,
}

impl RuntimeTurnDriver<'_> {
    /// Records the cancellation this turn honours, from a journaled peek or a
    /// recorded outcome, and stops the children it lent its context to.
    pub(super) fn record_turn_cancel(&mut self, evidence: crate::TurnCancellationEvidence) {
        self.turn_cancel = Some(evidence);
        self.children_stop.cancel();
    }

    /// Whether the next checkpoint keeps the capture base: the turn honoured
    /// its stop, or the iteration it closes interrupted a call. Decided from
    /// recorded facts only, so a replay decides the same.
    pub(super) fn holds_capture_tail(&self) -> bool {
        self.turn_cancel.is_some() || self.interrupted_calls
    }
}
