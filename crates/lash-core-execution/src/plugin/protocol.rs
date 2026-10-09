//! Protocol-plugin traits and narrow session/runtime context wrappers.
//!
//! Protocol plugins register their implementations here; the runtime narrows
//! what a protocol plugin can poke at so external crates don't need direct access to
//! `Session` / `LashRuntime` internals.

pub use lash_core_store::execution_state::{
    CheckpointComponentKey, ExecutionLeafName, ExecutionStateCapture, HydratedExecutionState,
    InvalidExecutionLeafName, LeafChange, PluginOptions,
};

use crate::SessionId;
use std::sync::Arc;

use crate::runtime::RuntimeSessionState;
use crate::{
    ExecRequest, ExecResponse, LlmRequest, LlmUsage, RuntimeExecutionContext, SessionAppendNode,
    SessionReadView,
};

/// Session-scoped plugin that initializes, restores, and extends protocol
/// state across a session's lifecycle. External protocol crates implement
/// this via the [`ProtocolSessionContext`] wrapper so they don't need direct
/// access to
/// `Session`/`LashRuntime` internals — the context narrows what a
/// plugin can poke at to the capabilities any protocol reasonably needs.
#[async_trait::async_trait]
pub trait ProtocolSessionPlugin: Send + Sync {
    async fn initialize_session(
        &self,
        _ctx: ProtocolSessionContext<'_>,
    ) -> Result<(), crate::SessionError> {
        Ok(())
    }

    async fn restore_session(
        &self,
        _ctx: ProtocolSessionContext<'_>,
        _state: ProtocolSessionRestoreView,
    ) -> Result<(), crate::SessionError> {
        Ok(())
    }

    async fn append_session_nodes(
        &self,
        _ctx: ProtocolSessionContext<'_>,
        _nodes: &[SessionAppendNode],
    ) -> Result<(), crate::SessionError> {
        Ok(())
    }

    /// Runs inside a recorded turn effect. Replay serves its recorded decision
    /// without invoking the hook again. A crash before the effect commits may
    /// invoke it again, so writes made through the context must be idempotent.
    async fn before_llm_call(
        &self,
        _ctx: ProtocolBeforeLlmCallContext,
        _request: &LlmRequest,
    ) -> Result<Option<ProtocolLlmCallAction>, crate::PluginError> {
        Ok(None)
    }

    /// The protocol's facts for its own prompt sections (ADR 0133), derived
    /// from its committed execution state: RLM's bound variables, say. The
    /// runtime calls this where it builds a model call's prompt cut, and
    /// every section renderer of that call reads the result through
    /// [`PromptInput::protocol_facts`](super::prompt::PromptInput::protocol_facts).
    /// The composed text, not these facts, is what a call records, so a
    /// redriven call never asks again. `None` (the default) means the
    /// protocol derives none.
    async fn prompt_facts(
        &self,
        _ctx: ProtocolSessionContext<'_>,
    ) -> Result<Option<super::prompt::ProtocolPromptFacts>, crate::SessionError> {
        Ok(None)
    }
}

/// The protocol-owned inputs needed to restore a session.
///
/// This view contains no decoded plugin namespaces or handle to full runtime
/// state. Plugins access their own namespace through their registered store.
#[derive(Debug)]
pub struct ProtocolSessionRestoreView {
    /// Active frame identity, used to reset protocol-local execution on a switch.
    pub current_frame_node_id: Option<crate::FrameNodeId>,
    /// Protocol execution root and leaves, or the typed hydration failure.
    /// Restore implementations retain responsibility for reporting that failure.
    pub execution_state: Result<Option<HydratedExecutionState>, crate::StoreError>,
    /// Active history to replay protocol seed and globals events.
    pub active_events: Vec<crate::SessionHistoryRecord>,
}

impl ProtocolSessionRestoreView {
    pub fn new(state: &RuntimeSessionState) -> Self {
        Self {
            current_frame_node_id: state.current_frame_node_id.clone(),
            execution_state: state.execution_state_hydration(),
            active_events: state.read_model().active_events.to_vec(),
        }
    }
}

/// Narrow wrapper around `Session` that protocol plugins use to
/// initialize, restore, and extend their per-session state.
///
/// Exposes only generic per-session lifecycle capabilities. Protocol-local
/// execution state is owned by the protocol plugin itself and is accessed
/// through [`ProtocolSessionPlugin`] callbacks.
/// Prevents protocol plugins from reaching into unrelated `Session`
/// internals.
pub struct ProtocolSessionContext<'a> {
    session_id: &'a SessionId,
    fleet_format: crate::FleetFormat,
    recorded_render: Option<&'a crate::RecordedRender>,
    prompt_history: Option<&'a SessionReadView>,
}

impl<'a> ProtocolSessionContext<'a> {
    pub fn new(session_id: &'a SessionId, fleet_format: crate::FleetFormat) -> Self {
        Self {
            session_id,
            fleet_format,
            recorded_render: None,
            prompt_history: None,
        }
    }

    pub fn with_recorded_render(mut self, recorded: &'a crate::RecordedRender) -> Self {
        self.recorded_render = Some(recorded);
        self
    }

    /// The committed history view used by this call's prompt cut.
    pub fn with_prompt_history(mut self, history: &'a SessionReadView) -> Self {
        self.prompt_history = Some(history);
        self
    }

    pub fn prompt_history(&self) -> Option<&SessionReadView> {
        self.prompt_history
    }

    pub fn recorded_render(&self) -> Option<&crate::RecordedRender> {
        self.recorded_render
    }

    /// ID of the session being initialized/restored. Equivalent to the
    /// `session_id` previously passed as a separate argument.
    pub fn session_id(&self) -> &str {
        self.session_id
    }

    /// The `F` the bound session's store recorded (FIG-3796): protocol
    /// plugins stamp durable envelopes at `F`'s writer version, never the bare
    /// build constant.
    pub fn fleet_format(&self) -> crate::FleetFormat {
        self.fleet_format
    }
}

pub struct ProtocolBeforeLlmCallContext {
    pub session_id: SessionId,
    pub sessions: Arc<dyn crate::plugin::SessionStateService>,
    pub session_graph: Arc<dyn crate::plugin::SessionGraphService>,
    pub processes: Arc<dyn crate::ProcessService>,
    pub state: SessionReadView,
    pub latest_prompt_usage: Option<LlmUsage>,
}

/// Minimum encoded body size at which a composite protocol-owned
/// execution-state value is persisted as its own content-addressed checkpoint
/// leaf instead of being inlined into the execution-state root.
///
/// This is a checkpoint-shape decision, not a storage decision, and it is
/// deliberately independent of any store's blob-compression profile: "is this
/// value worth its own component" and "should these bytes be compressed" are
/// different questions, and snapshot shape must not change because a different
/// backend is configured.
///
/// The line follows from what each choice costs *per commit*, because the root
/// is re-encoded in full on every commit while an unchanged leaf rides as a
/// body-free reference: an inline value costs its own encoded length plus its
/// root map entry, while a leaf costs its root reference plus its checkpoint
/// manifest row and nothing else. Measured against the budget accounting a
/// commit is actually charged for, a retained file leaf has 273 bytes of fixed
/// overhead and crosses the inline layout at a 272-byte body. This line stays
/// comfortably above that marginal point, which keeps every promotion a clear
/// win and keeps the manifest — the per-commit floor of a session made of short
/// values — small.
///
/// Above the line, per-commit bytes stop tracking retained state: a session of
/// 300 mid-size bindings (1.09 MB retained) commits ~100 KB when one binding
/// changes, where inlining them all commits the whole 1.09 MB every turn.
pub const EXECUTION_STATE_LEAF_MIN_BODY_BYTES: usize = 512;

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum ProtocolLlmCallAction {
    SwitchAgentFrame {
        frame_key: crate::FrameKey,
        task: String,
    },
}

/// How the runtime settles a code response or ends its logical Run.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CodeExecutionOutcome {
    /// The response was accepted for normal protocol processing.
    Accepted,
    /// The response could not be handed to the protocol, so cell-local
    /// mutations must be discarded.
    Discarded,
    /// A cancellation won at the response handoff, so cell-local mutations
    /// must be discarded.
    Cancelled,
    /// The logical Run has reached its terminal commit. Discard its code
    /// continuation before capturing execution state, preserving frame globals.
    Terminated,
}

#[async_trait::async_trait]
pub trait CodeExecutorPlugin: Send + Sync {
    async fn execute_code(
        &self,
        ctx: RuntimeExecutionContext<'_>,
        request: ExecRequest,
    ) -> Result<ExecResponse, crate::SessionError>;

    fn execution_state_dirty(&self) -> bool {
        false
    }

    /// The view of `records`, one cell's tool call records in call order,
    /// that a turn keeps of the cell: the executor's one bound on how many
    /// records and how much of each output a cell's round retains, with the
    /// records it leaves out accounted for. The turn records, streams and
    /// reports this view and nothing else of the cell's calls (FIG-5330).
    /// An executor that states no bound keeps every record whole.
    fn bound_tool_call_records(
        &self,
        records: Vec<crate::ToolCallRecord>,
    ) -> (Vec<crate::ToolCallRecord>, Option<crate::OmittedToolCalls>) {
        (records, None)
    }

    /// The records of the tool calls a cell completed, in call order, as
    /// `snapshot` holds them: the cell's stored snapshot, as this executor
    /// wrote it. A turn that stops on the cell records them from here, since
    /// the cell never answers (FIG-5330).
    ///
    /// # Errors
    ///
    /// [`crate::SessionError`] when the snapshot does not decode.
    fn snapshot_tool_calls(
        &self,
        _snapshot: &str,
    ) -> Result<Vec<crate::ToolCallRecord>, crate::SessionError> {
        Ok(Vec::new())
    }

    /// Check `snapshot`, the stored snapshot a cell resumes from, as this
    /// executor wrote it: whether this build decodes it. A turn restored on
    /// the cell asks before the cell runs again, so a snapshot another build
    /// wrote is refused before any of the cell's work is re-delivered
    /// (FIG-5601). An executor whose cells snapshot nothing keeps the
    /// default.
    ///
    /// # Errors
    ///
    /// [`crate::SessionError`] when the snapshot does not decode.
    fn check_cell_snapshot(&self, _snapshot: &str) -> Result<(), crate::SessionError> {
        Ok(())
    }

    /// The executable generation this executor runs cells under (FIG-3571):
    /// everything that decides how a cell compiles, what its nested effects
    /// are keyed by, and where its cancel checkpoints fall. A turn's admission
    /// records it, and a redrive under another generation is refused before
    /// any effect. `None` for an executor whose cells journal nothing a build
    /// change could move.
    fn executable_generation(&self) -> Option<crate::ExecutableGeneration> {
        None
    }

    async fn snapshot_execution_state(
        &self,
        _ctx: ProtocolSessionContext<'_>,
    ) -> Result<ExecutionStateCapture, crate::SessionError> {
        Ok(ExecutionStateCapture::default())
    }

    /// The artifacts a `continue_as` carries into the successor frame
    /// (ADR 0113 §3.1): every artifact the seed values in `initial_nodes`
    /// reference. The switching turn's final commit carries them onto the
    /// successor's frame environment and ends the frame it leaves; an
    /// executor that binds no artifacts answers none.
    async fn frame_switch_carries(
        &self,
        ctx: ProtocolSessionContext<'_>,
        _successor: &crate::FrameNodeId,
        initial_nodes: &[crate::SessionAppendNode],
    ) -> Result<Vec<crate::ArtifactName>, crate::SessionError>;

    /// Report whether a dirty execution-state capture *would* succeed, staging
    /// nothing.
    ///
    /// Only the final turn commit stages a capture, so a capture failure
    /// discovered there has already spent the turn's provider round trip and
    /// tool work. The runtime therefore asks this question at every
    /// prompt-resume-safe boundary before a provider call, and aborts the turn
    /// there if the answer is an error. An implementation must not stage,
    /// acknowledge, or roll back anything: it answers only whether the same
    /// capture attempted at this instant would fail. An executor that
    /// implements [`CodeExecutorPlugin::snapshot_execution_state`] with fallible
    /// encoding or I/O should implement this too; the default answers "no known
    /// obstacle".
    async fn probe_execution_state_capture(
        &self,
        _ctx: ProtocolSessionContext<'_>,
    ) -> Result<(), crate::SessionError> {
        Ok(())
    }

    /// [`CodeExecutorPlugin::snapshot_execution_state`] is a checkpoint delta:
    /// it reports unchanged leaves as body-free references, and the runtime
    /// releases their resident bodies once the durable refs are authoritative.
    /// Explicit administrative snapshot needs the whole state instead, so it
    /// asks the executor rather than reassembling one from resident checkpoint
    /// bodies. Implementations build this from live state and stage nothing.
    /// `None` means the executor holds no snapshotable state; an executor that
    /// implements `snapshot_execution_state` should implement this too.
    async fn hydrated_execution_state(
        &self,
        _ctx: ProtocolSessionContext<'_>,
    ) -> Result<Option<HydratedExecutionState>, crate::SessionError> {
        Ok(None)
    }

    async fn acknowledge_execution_state_capture(&self) {}

    async fn abort_execution_state_capture(&self) {}

    /// Settle a code response, or discard its continuation at logical termination.
    ///
    /// This closes the cancellation race between the executor's final token
    /// observation and the runtime consuming its response. Stateful executors
    /// can retain a cell checkpoint until this call and roll it back when the
    /// outcome is not [`CodeExecutionOutcome::Accepted`]. The runtime
    /// settles each returned response before starting another code effect for
    /// the same session. It also calls this with [`CodeExecutionOutcome::Terminated`]
    /// before a terminal commit captures state, including when cancellation
    /// replaces an accepted segment boundary during final settlement.
    async fn settle_code_execution(
        &self,
        _outcome: CodeExecutionOutcome,
    ) -> Result<(), crate::SessionError> {
        Ok(())
    }

    async fn restore_execution_state(
        &self,
        _ctx: ProtocolSessionContext<'_>,
        _state: &HydratedExecutionState,
    ) -> Result<(), crate::SessionError> {
        Ok(())
    }
}

pub trait AssistantProseProjectorPlugin: Send + Sync {
    fn project_assistant_prose(&self, text: &str) -> String;
}

/// Singleton kernel extension slot that owns the `ProtocolDriverHandle` and
/// associated preamble (prompt text, tool catalog, sync/async flag) for this
/// session.
///
/// Core owns the slot and the `HostTurnProtocol` state shape so the turn loop
/// can persist and resume protocol driver state generically. External protocol
/// crates own the concrete prompt policy and output parser. Plugin stack
/// construction must install exactly one implementation.
pub trait ProtocolDriverPlugin: Send + Sync {
    fn build_preamble(&self, input: crate::ProtocolBuildInput) -> crate::TurnDriverPreamble;

    /// The call's offered tools over its pinned catalog, using this protocol's
    /// discovery policy for native declarations and code-callable bindings alike.
    fn prompt_tools(&self, catalog: Arc<crate::ToolCatalog>) -> super::prompt::OfferedTools {
        super::prompt::OfferedTools::new(catalog, false)
    }

    /// The render a run's results present with, resolved from `namespace`,
    /// the protocol namespace the run executes under, which the driver reads
    /// as its owner's recorded type. A render it refuses is the run's
    /// refused shape, typed; a namespace it cannot read is corruption.
    fn resolve_render(
        &self,
        _namespace: &crate::ProtocolTurnOptions,
    ) -> Result<Option<crate::RecordedRender>, crate::RenderFault> {
        Ok(None)
    }
}

pub use lash_core_store::transcript::TranscriptDecoderPlugin;
