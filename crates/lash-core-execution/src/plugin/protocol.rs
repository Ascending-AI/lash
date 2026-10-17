//! Protocol-plugin traits and narrow session/runtime context wrappers.
//!
//! Protocol plugins register their implementations here; the runtime narrows
//! what a protocol plugin can poke at so external crates don't need direct access to
//! `Session` / `LashRuntime` internals.

pub use lash_core_store::execution_state::{
    CheckpointComponentKey, ExecutionGraphCapture, ExecutionGraphPurpose, HydratedExecutionGraph,
    PluginOptions,
};
pub use lash_core_store::transcript::TranscriptDecoderPlugin;

/// A cell's ordered archival-body references, which its turn retains when
/// consuming the cell's saved root. The references contain no copied body.
pub use crate::runtime::actor::round::graph::ReportView as CellCallReferences;

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
    /// Refuse a candidate tool surface before it is published (FIG-5824).
    /// `state` supplies committed bindings for a capability preview that has
    /// not restored a live session; otherwise inspect the live execution.
    /// Validation changes neither bindings nor the offered surface.
    async fn validate_tool_catalog(
        &self,
        _catalog: &crate::ToolCatalog,
        _state: Option<&ProtocolSessionRestoreView>,
    ) -> Result<(), lash_execution_contracts::plugin_error::PluginError> {
        Ok(())
    }

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
    ) -> Result<Option<ProtocolLlmCallAction>, lash_execution_contracts::plugin_error::PluginError>
    {
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
    /// The session's SessionExecution graph, or the typed hydration
    /// failure. Restore implementations retain responsibility for reporting
    /// that failure.
    pub execution_state: Result<Option<HydratedExecutionGraph>, crate::StoreError>,
    /// Active history to replay protocol seed and globals events: the
    /// session's own records, shared, never copied for a restore.
    pub active_events: lash_protocol_types::AppendVec<crate::SessionHistoryRecord>,
    /// Where the session's committed head holds its execution state, when
    /// the caller reads it over a store: a stored-state check looks a
    /// tool's namespace root up in the stored binding tree through these
    /// pages (FIG-6418).
    pub pages: Option<SessionPages>,
}

/// The reader a session actor's pages are read through: its store and the
/// claim (actor, epoch) the read stands under. The durable session actor
/// sets it on a runtime every activation; a runtime not under a claim
/// holds none.
#[derive(Clone)]
pub struct SessionPageReader {
    pub store: Arc<dyn lash_durable::domain::pages::PageStore>,
    pub actor: lash_durable::ActorKey,
    pub epoch: lash_durable::Epoch,
}

impl std::fmt::Debug for SessionPageReader {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SessionPageReader")
            .field("actor", &self.actor)
            .field("epoch", &self.epoch)
            .finish_non_exhaustive()
    }
}

impl SessionPageReader {
    /// Where `state`'s committed head holds the session's execution state:
    /// the head revision's row, at its checkpoint. None for a session that
    /// committed no checkpoint yet.
    pub fn session_pages(&self, state: &RuntimeSessionState) -> Option<SessionPages> {
        let root =
            lash_core_store::checkpoint_graph::NodeId::from_hex(&state.checkpoint_ref.as_ref()?.0)?;
        Some(SessionPages {
            store: Arc::clone(&self.store),
            actor: self.actor.clone(),
            epoch: self.epoch,
            owner: lash_durable::domain::graph::RootOwner::SessionRevision {
                session: state.session_id.clone(),
                revision: state.head_revision,
            },
            root,
        })
    }
}

impl ProtocolSessionRestoreView {
    /// The view `state` builds, its stored-state pages read through
    /// `reader` — none where the session actor's claim does not reach.
    pub fn new(state: &RuntimeSessionState, reader: Option<&SessionPageReader>) -> Self {
        Self {
            current_frame_node_id: state.current_frame_node_id.clone(),
            execution_state: state.execution_state_hydration(),
            active_events: state.read_model().active_events.clone(),
            pages: reader.and_then(|reader| reader.session_pages(state)),
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
    session_pages: Option<SessionPages>,
}

/// Where a session's committed execution state stands on the persistent
/// heap, and the fence its executor reads it under (FIG-6193): the owner
/// row whose root reaches the session's pages, that root, and the store.
/// An executor leases the root once, when it first reads a committed
/// page, and reads every page of the session under that lease.
#[derive(Clone)]
pub struct SessionPages {
    pub store: Arc<dyn lash_durable::domain::pages::PageStore>,
    pub actor: lash_durable::ActorKey,
    pub epoch: lash_durable::Epoch,
    pub owner: lash_durable::domain::graph::RootOwner,
    pub root: lash_core_store::checkpoint_graph::NodeId,
}

impl std::fmt::Debug for SessionPages {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SessionPages")
            .field("actor", &self.actor)
            .field("epoch", &self.epoch)
            .field("owner", &self.owner)
            .field("root", &self.root)
            .finish_non_exhaustive()
    }
}

impl<'a> ProtocolSessionContext<'a> {
    pub fn new(session_id: &'a SessionId, fleet_format: crate::FleetFormat) -> Self {
        Self {
            session_id,
            fleet_format,
            recorded_render: None,
            prompt_history: None,
            session_pages: None,
        }
    }

    /// This context with `pages` as where the session's committed
    /// execution state stands.
    #[must_use]
    pub fn with_session_pages(mut self, pages: Option<SessionPages>) -> Self {
        self.session_pages = pages;
        self
    }

    /// Where the session's committed execution state stands, when the
    /// caller reads it over a store.
    pub fn session_pages(&self) -> Option<&SessionPages> {
        self.session_pages.as_ref()
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
    pub bindings: Arc<crate::plugin::ServiceBindings>,
    pub state: SessionReadView,
    pub latest_prompt_usage: Option<LlmUsage>,
    /// The spec the running turn's recorded shape resolved (FIG-6021).
    pub run_spec: crate::RunSpec,
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum ProtocolLlmCallAction {
    /// Switch to a fresh agent frame whose follow-on runs under `next_run`
    /// ([`ProtocolBeforeLlmCallContext::run_spec`] keeps the running
    /// turn's shape).
    SwitchAgentFrame {
        frame_key: crate::FrameKey,
        task: String,
        next_run: Box<crate::RunSpec>,
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

/// A cell's stored execution root as a turn restored on the cell reads it:
/// its row and every node it reaches, read under one protection
/// (FIG-5887 §3).
#[derive(Clone, Debug)]
pub struct StoredCell {
    pub read_limits: lash_core_store::checkpoint_graph::DecodeLimits,
    pub exec: lash_durable::domain::ExecKey,
    pub row: lash_durable::domain::ExecRootRow,
    pub graph: lash_durable::domain::graph::HydratedGraph,
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

    /// Whether this executor's cells commit the model response that asked
    /// for them with their first save, when their turn holds it
    /// ([`RuntimeExecutionContext::held_response`], FIG-6000): the save
    /// carries the held writes, and the cell publishes nothing before a
    /// commit holds them. An executor that does not commits the response
    /// on its own before the cell starts, as `model.done`.
    fn carries_held_response(&self) -> bool {
        false
    }

    /// The records of the tool calls a cell completed, in call order, as
    /// `cell`, its stored root, holds them. A turn that stops on the cell
    /// records them from here, since the cell never answers (FIG-5330).
    ///
    /// # Errors
    ///
    /// [`crate::SessionError`] when the root does not decode.
    fn snapshot_tool_calls(
        &self,
        _cell: &StoredCell,
    ) -> Result<(Vec<crate::ToolCallRecord>, CellCallReferences), crate::SessionError> {
        Ok(Default::default())
    }

    /// Check `snapshot`, the stored snapshot a cell resumes from, as this
    /// executor wrote it: whether this build decodes all of it, the state
    /// its language runtime resumes from included. A turn restored on the
    /// cell asks before the cell runs again, so a snapshot another build
    /// wrote is refused before any of the cell's work is re-delivered
    /// (FIG-5601, FIG-5613). The inner `Err` is the decoder's account of a
    /// snapshot this build does not decode. An executor whose cells
    /// snapshot nothing keeps the default.
    ///
    /// # Errors
    ///
    /// [`crate::RuntimeError`] when the check itself could not run: nothing
    /// is known of the snapshot, and the restore is tried again.
    async fn check_cell_snapshot(
        &self,
        _cell: &StoredCell,
    ) -> Result<Result<(), String>, crate::RuntimeError> {
        Ok(Ok(()))
    }

    /// Whether `cell`, a cell's stored root written by the build before
    /// this one, runs on under this build. A turn restored on the cell asks
    /// once every live node that serves sessions decodes this build's
    /// session set, before the cell runs again. The inner `Err` is the
    /// account of a root no carry this build ships moves forward: a parked
    /// cell's heap pages are carried by no migrator, so the cell stays
    /// parked, its rows intact, and resumes on a build that runs its
    /// kernel (FIG-6193). An executor whose cells snapshot nothing keeps
    /// the default.
    ///
    /// # Errors
    ///
    /// [`crate::RuntimeError`] when the check itself could not run: nothing
    /// is known of the snapshot, and the restore is tried again.
    async fn carried_cell_snapshot(
        &self,
        _cell: &StoredCell,
    ) -> Result<Result<(), String>, crate::RuntimeError> {
        Ok(Ok(()))
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

    /// The attachments `record`, a protocol record of this executor's
    /// session, references: a cell's retained outputs among them. A turn's
    /// commit reads them from the records it commits and holds each on the
    /// session, so what its history names outlives the upload that staged it
    /// (ADR 0124 §4), resumed or not. An executor whose records reference no
    /// attachment answers none.
    fn history_attachments(
        &self,
        _record: &crate::ProtocolEvent,
    ) -> Result<Vec<crate::AttachmentId>, crate::SessionError> {
        Ok(Vec::new())
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

    /// The session's descriptor as live state holds it, staging nothing:
    /// what an explicit administrative snapshot reads. Its heap pages are
    /// the store's or the turn's. `None` means the executor holds no
    /// execution state.
    async fn hydrated_execution_graph(
        &self,
        _ctx: ProtocolSessionContext<'_>,
    ) -> Result<Option<HydratedExecutionGraph>, crate::SessionError> {
        Ok(None)
    }

    /// The candidate that publishes `root`'s pages, staged under the
    /// pending capture, under `commit` (FIG-6418): what they name beyond
    /// the heap that the store may not hold (a helper release) is pinned in
    /// `tx`, after `reads` of whether the store pins it already. `None`
    /// when no pending capture stages `root`: the store holds its pages.
    ///
    /// # Errors
    ///
    /// [`crate::SessionError`] when the staged pages do not stream or a
    /// release they name is not held.
    async fn execution_candidate(
        &self,
        _root: lash_core_store::checkpoint_graph::NodeId,
        _commit: lash_core_store::checkpoint_graph::CommitId,
        _tx: &mut lash_durable::ActorTx,
        _reads: &dyn lash_durable::DurableReads,
    ) -> Result<Option<ExecutionCandidate>, crate::SessionError> {
        Ok(None)
    }

    /// The pending capture's commit was acknowledged: the pages it
    /// published leave the turn's staging.
    async fn acknowledge_execution_state_capture(&self) {}

    /// The pending capture's commit was not made: its pages stay staged for
    /// the next capture.
    async fn abort_execution_state_capture(&self) {}

    /// Resolves the pending capture whose commit's acknowledgement was
    /// lost: `committed` when the store recorded its publication. A
    /// committed capture is acknowledged; another is withdrawn, its pages
    /// kept staged for the next capture.
    async fn resolve_execution_state_capture(&self, committed: bool) {
        if committed {
            self.acknowledge_execution_state_capture().await;
        } else {
            self.abort_execution_state_capture().await;
        }
    }

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

    /// The session's execution state for `purpose`, its descriptor page
    /// (ADR 0145 §9), built from live state: a working capture after each
    /// successful cell, a final capture for the turn's commit. The heap
    /// pages staged under it are the commit's candidate
    /// ([`Self::execution_candidate`]), kept staged until
    /// [`Self::acknowledge_execution_state_capture`]; a refused commit
    /// publishes them with the next capture. An executor that holds no
    /// execution state keeps the default.
    async fn capture_execution_graph(
        &self,
        _ctx: ProtocolSessionContext<'_>,
        _purpose: ExecutionGraphPurpose,
    ) -> Result<ExecutionGraphCapture, crate::SessionError> {
        Ok(ExecutionGraphCapture::Clear)
    }

    /// Installs a session's execution state, its descriptor page, into the
    /// executor (ADR 0145 §9): a session revision's, or the working state a
    /// turn phase owns, before any model continuation or cell compiles. An
    /// executor that holds no execution state keeps the default.
    async fn restore_execution_graph(
        &self,
        _ctx: ProtocolSessionContext<'_>,
        _graph: &HydratedExecutionGraph,
        _purpose: ExecutionGraphPurpose,
    ) -> Result<(), crate::SessionError> {
        Ok(())
    }
}

/// The candidate that publishes a session capture's heap pages with the
/// commit that names its root (FIG-6418).
pub struct ExecutionCandidate {
    pub manifest: Box<dyn lash_core_store::checkpoint_graph::stage::CandidateManifest>,
}

impl std::fmt::Debug for ExecutionCandidate {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ExecutionCandidate")
            .field("summary", &self.manifest.summary())
            .finish()
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
    /// The mode's replacement for one attachment the target request cannot accept.
    /// This is request presentation only; the original ref stays in history.
    fn attachment_omission_text(&self, notice: &crate::AttachmentMaterializationNotice) -> String;

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
