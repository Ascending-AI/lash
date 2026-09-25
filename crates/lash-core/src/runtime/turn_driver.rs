use super::*;
use crate::runtime::turn_control::ActiveTurnControl;

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
    queued_work_trace_payload, send_queued_work_started_event,
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
    pub(super) protocol_extension: Option<crate::ProtocolTurnExtensionHandle>,
    pub(super) turn_context: crate::TurnContext,
    pub(super) turn_causes: Vec<crate::TurnCause>,
    pub(super) pending_queue_claims: Vec<crate::QueuedWorkClaim>,
    pub(super) pending_turn_input_claims: Vec<crate::TurnInputClaim>,
    pub(super) pending_checkpoint_turn_input_claim: Option<crate::TurnInputClaim>,
    /// FIG-3157: work claimed at a terminal checkpoint and withheld from its
    /// delivery, so the committed finish stays this turn's answer. It is never
    /// settled as this turn's completed work. A finished turn's logical run
    /// drives it in a follow-on turn. When this turn is cancelled that
    /// follow-on never runs for turn input: the final commit settles withheld
    /// turn input through the cancellation's undelivered disposition, like an
    /// unclaimed active-turn row (FIG-3531). Withheld queued work keeps its
    /// own cancellation path, which is tracked separately.
    pub(super) withheld_terminal_work: super::logical_turn::WithheldTerminalWork,
    pub(super) checkpoint_messages: crate::tool_dispatch::CheckpointMessageBuffer,
    pub(super) session_execution_lease: Option<crate::SessionExecutionLeaseAuthority>,
    pub(super) runtime_lease_owner: crate::LeaseOwnerIdentity,
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
    /// The cooperative cancellation signal this turn's effect loop runs under.
    ///
    /// The lent opener context carries this token so a tool child's waits are
    /// cancelled with the turn that opened it (FIG-2266). Set at `run`; a
    /// registration taken before `run` — impossible today, since the first
    /// context is built inside the loop — would lend a token nobody cancels.
    pub(super) cooperative_cancel: CancellationToken,
}
