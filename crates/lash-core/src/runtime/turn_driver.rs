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
pub(in crate::runtime) use handlers::model_call_activity;
pub(super) use trace::protocol_step_trace_event;

pub(super) struct RuntimeTurnDriver<'a> {
    pub(super) session: Session,
    pub(super) policy: RuntimeSessionPolicy,
    /// The tool calls of the cell that last answered the machine, as the
    /// code executor's bound keeps them, until the turn's next commit takes
    /// them (FIG-5330).
    pub(super) answered_cell_calls: crate::runtime::durable::session::CellToolCalls,
    /// How the turn's cells hand their ends over (FIG-5999), shared with
    /// the effect bodies that run them.
    pub(super) cell_end: crate::CellEndHandoff,
    /// What the cell that last answered the machine left for the commit
    /// that consumes its answer.
    pub(super) answered_cell_end: Option<(crate::EffectId, crate::EndedCell)>,
    /// The model response that asked for the cell the turn runs next, held
    /// for the cell's first save (FIG-6000), until that cell runs.
    pub(super) held_response: Option<crate::HeldResponse>,
    /// The first start of the cell the turn's phase holds, until it
    /// answers: retained by the phase's checkpoint, and read back from it by
    /// a resume (§3.4).
    pub(super) cell_start: Option<crate::runtime::durable::session::CellStart>,
    /// The closing record of the cell that last answered the machine, held
    /// for the commit that consumes its answer (P1).
    pub(super) answered_cell_close:
        Option<(crate::EffectId, crate::runtime::durable::session::CellClose)>,
    /// Where the session's committed execution state stands on the
    /// persistent heap: the head this turn started from, then the working
    /// state its last committed cell left (FIG-6193), or the root a cell's
    /// recorded end left until a commit roots that state (FIG-6315).
    pub(super) session_pages: Option<crate::plugin::SessionPages>,
    pub(super) host: RuntimeHost,
    pub(super) scoped_effect_controller: ActorContext,
    pub(super) session_id: SessionId,
    pub(super) turn_id: crate::TurnId,
    pub(super) turn_index: usize,
    pub(super) turn_pipeline: TurnBoundary,
    pub(super) prelude: Box<crate::runtime::effect::TurnPrelude>,
    pub(super) prelude_base_messages: crate::MessageSequence,
    pub(super) prepared_context: crate::session_model::context::PreparedContext,
    /// Parent-session calls only. Child runtimes assemble their own ledgers.
    pub(super) llm_calls: Vec<crate::LlmCallRecord>,
    /// Contained provider panic, deferred until the terminal turn commits.
    pub(super) provider_panic: Option<String>,
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
    /// A live model call's typed deadline stop, shared with its effect body.
    pub(super) model_stop_error: Arc<std::sync::Mutex<Option<crate::LlmCallError>>>,
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
    /// The configuration the turn's run executes under, as its checkpoint
    /// records it: the resolved shape, and the tool surface the latest
    /// environment sync installed (FIG-6021).
    pub(super) run_config: crate::runtime::durable::run_config::RecordedRunConfig,
    /// Whether this drive installed a surface yet. Its first sync installs
    /// the surface the run recorded, when it recorded one, instead of
    /// advertising again (S5): only a later sync, of a new protocol
    /// iteration, advertises.
    pub(super) surface_installed: bool,
    /// The lifetime this value is bound to; the context it carries is `'static`.
    pub(crate) run: std::marker::PhantomData<&'a ()>,
}
