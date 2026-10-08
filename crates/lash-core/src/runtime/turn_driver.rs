use super::*;
use crate::ActorContext;

mod abandoned_stream;
mod after_turn;
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
mod prepare;
mod prompt;
mod streaming;
mod tool_catalog;
mod tools;
mod trace;
pub(in crate::runtime) use trace::TurnPhaseSpan;

pub(super) use events::{emit_semantic_response_parts, send_turn_input_applications};
use handlers::foreground_exec_graph_key;
pub(super) use trace::protocol_step_trace_event;

pub(super) struct RuntimeTurnDriver<'a> {
    pub(super) session: Session,
    pub(super) policy: RuntimeSessionPolicy,
    /// The turn's committed content, recorded in program order from the
    /// machine's emissions and the driver's own terminal events.
    pub(super) recorded_assembly: RecordedTurnAssembly,
    /// The tool call records of the cell that last answered the machine,
    /// until the turn's next commit takes them (FIG-5330).
    pub(super) answered_cell_calls: Vec<crate::ToolCallRecord>,
    pub(super) host: RuntimeHost,
    pub(super) scoped_effect_controller: ActorContext,
    pub(super) session_id: SessionId,
    pub(super) turn_id: crate::TurnId,
    pub(super) turn_index: usize,
    pub(super) turn_pipeline: TurnBoundary,
    pub(super) prelude: Box<crate::runtime::effect::TurnPrelude>,
    /// Parent-session calls only. Child runtimes assemble their own ledgers.
    pub(super) llm_calls: Vec<crate::LlmCallRecord>,
    /// Non-transcript evidence from charge-safety-refused generations, with
    /// cardinality capped at one component per sealed provider attempt.
    pub(super) failure_evidence: Vec<crate::TurnFailureEvidence>,
    pub(super) session_services: Arc<RuntimeSessionServices>,
    /// What the turn's after-turn callbacks read sessions through: the
    /// committed head the turn started from, with its durable node
    /// identities, so a graph append they fence to its leaf lands on the
    /// branch the turn's commit extends. `Some` exactly when the session has
    /// after-turn callbacks.
    pub(super) after_turn_reads: Option<Arc<dyn crate::plugin::SessionReadService>>,
    pub(super) protocol_turn_options: crate::ProtocolTurnOptions,
    pub(super) turn_context: crate::TurnContext,
    pub(super) pending_turn_inputs: Vec<crate::AdmittedTurnInputs>,
    pub(super) pending_checkpoint_turn_inputs: Option<crate::AdmittedTurnInputs>,
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
    /// The admitted call the next model-call effect sends (ADR 0133 §6), its
    /// request template and recorded response context: handed to that
    /// effect's body, which fills the template's slots afresh and never
    /// rebuilds it.
    pub(super) admitted_body: Option<lash_sansio::llm::types::AdmittedSend>,
    /// The attempt of the admitted call the next model-call effect sends, its
    /// pin's: a re-sent attempt streams under an observation key of its own
    /// (`abandoned_stream::model_stream_key`).
    pub(super) model_attempt: u32,
    /// The lifetime this value is bound to; the context it carries is `'static`.
    pub(crate) run: std::marker::PhantomData<&'a ()>,
}
