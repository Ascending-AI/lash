//! Protocol-plugin traits and narrow session/runtime context wrappers.
//!
//! Protocol plugins register their implementations here; the runtime narrows
//! what a protocol plugin can poke at so external crates don't need direct access to
//! `Session` / `LashRuntime` internals.
//!
//! Split out of `plugin/mod.rs` for file size; `pub use` there keeps
//! the outer module path.

use std::collections::BTreeMap;
use std::sync::Arc;

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

use crate::runtime::RuntimeSessionState;
use crate::{
    ExecRequest, ExecResponse, LlmRequest, PromptUsage, RuntimeExecutionContext, SessionAppendNode,
    SessionReadView,
};

/// Session-scoped plugin that initializes, restores, and extends protocol
/// state across a session's lifecycle. External protocol crates implement
/// this via context wrappers ([`ProtocolSessionContext`],
/// [`ProtocolRuntimeContext`]) so they don't need direct access to
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
        _state: &RuntimeSessionState,
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

    async fn apply_session_extension(
        &self,
        _extension: crate::ProtocolSessionExtensionHandle,
    ) -> Result<(), crate::SessionError> {
        Err(crate::SessionError::Protocol(
            "protocol does not accept session extensions".to_string(),
        ))
    }

    async fn validate_turn_extension(
        &self,
        _extension: &crate::ProtocolTurnExtensionHandle,
    ) -> Result<(), crate::SessionError> {
        Ok(())
    }

    /// Fires on every session materialization — root/builder open (including
    /// resume) and child create — so a protocol plugin can apply and default
    /// its per-session options at open time (apply-at-open semantics).
    ///
    /// The [`ProtocolSessionMaterialization`] descriptor carries the
    /// plugin-keyed options that reached this materialization (builder options
    /// for root opens, request options for child create) and whether this is a
    /// root session. The plugin reads/writes durable protocol turn options
    /// through [`ProtocolRuntimeContext`].
    fn configure_runtime_on_materialize(
        &self,
        _ctx: ProtocolRuntimeContext<'_>,
        _materialization: ProtocolSessionMaterialization<'_>,
    ) -> Result<(), crate::SessionError> {
        Ok(())
    }

    async fn before_llm_call(
        &self,
        _ctx: ProtocolBeforeLlmCallContext,
        _request: &LlmRequest,
    ) -> Result<Option<ProtocolLlmCallAction>, crate::PluginError> {
        Ok(None)
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
    session_id: &'a str,
}

impl<'a> ProtocolSessionContext<'a> {
    pub(crate) fn new(_session: &'a mut crate::Session, session_id: &'a str) -> Self {
        Self { session_id }
    }

    /// ID of the session being initialized/restored. Equivalent to the
    /// `session_id` previously passed as a separate argument.
    pub fn session_id(&self) -> &str {
        self.session_id
    }
}

pub struct ProtocolBeforeLlmCallContext {
    pub session_id: String,
    pub sessions: Arc<dyn crate::plugin::SessionStateService>,
    pub session_graph: Arc<dyn crate::plugin::SessionGraphService>,
    pub processes: Arc<dyn crate::ProcessService>,
    pub state: SessionReadView,
    pub latest_prompt_usage: Option<PromptUsage>,
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
/// commit is actually charged for, a retained file leaf has 296 bytes of fixed
/// overhead and crosses the inline layout at a 295-byte body. This line stays
/// comfortably above that marginal point, which keeps every promotion a clear
/// win and keeps the manifest — the per-commit floor of a session made of short
/// values — small.
///
/// Above the line, per-commit bytes stop tracking retained state: a session of
/// 300 mid-size bindings (1.09 MB retained) commits ~100 KB when one binding
/// changes, where inlining them all commits the whole 1.09 MB every turn.
pub const EXECUTION_STATE_LEAF_MIN_BODY_BYTES: usize = 512;

/// Complete protocol-owned execution-state component update for one checkpoint.
///
/// `root` is the well-known execution-state root body. `components` is the
/// complete leaf-key listing reachable from that root: a changed body submits
/// new logical bytes, while an unchanged key reuses its resident durable ref.
/// An absent key is deleted. An absent root requires an empty leaf set.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ExecutionStateSnapshot {
    pub root: Option<Vec<u8>>,
    pub components: BTreeMap<String, ExecutionStateComponentSnapshot>,
}

impl ExecutionStateSnapshot {
    pub fn from_root(root: Option<Vec<u8>>) -> Self {
        Self {
            root,
            components: BTreeMap::new(),
        }
    }

    pub fn changed_component(&mut self, key: impl Into<String>, body: Vec<u8>) {
        self.components
            .insert(key.into(), ExecutionStateComponentSnapshot::Changed(body));
    }

    pub fn unchanged_component(&mut self, key: impl Into<String>) {
        self.components
            .insert(key.into(), ExecutionStateComponentSnapshot::Unchanged);
    }

    pub fn from_hydrated(state: HydratedExecutionState) -> Self {
        Self {
            root: Some(state.root),
            components: state
                .components
                .into_iter()
                .map(|(key, body)| (key, ExecutionStateComponentSnapshot::Changed(body)))
                .collect(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ExecutionStateComponentSnapshot {
    Changed(Vec<u8>),
    Unchanged,
}

/// Fully hydrated protocol-owned execution state supplied during restore.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct HydratedExecutionState {
    pub root: Vec<u8>,
    pub components: BTreeMap<String, Vec<u8>>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ProtocolLlmCallAction {
    SwitchAgentFrame { frame_id: String, task: String },
}

/// Narrow wrapper around `LashRuntime` that protocol plugins use when
/// configuring the runtime from a fresh `SessionCreateRequest`.
///
/// Exposes only the runtime-level capabilities protocols need to set
/// (termination contract, etc.) so plugins don't reach into unrelated
/// runtime internals.
pub struct ProtocolRuntimeContext<'a> {
    runtime: &'a mut crate::runtime::LashRuntime,
}

impl<'a> ProtocolRuntimeContext<'a> {
    pub(crate) fn new(runtime: &'a mut crate::runtime::LashRuntime) -> Self {
        Self { runtime }
    }

    /// The durable protocol turn options currently recorded on the session.
    /// Protocol plugins read these to preserve fields (e.g. termination) they
    /// are not overwriting.
    pub fn protocol_turn_options(&self) -> &crate::ProtocolTurnOptions {
        self.runtime.protocol_turn_options()
    }

    /// Set the durable protocol turn options and mirror them to the current
    /// agent frame only.
    pub fn set_protocol_turn_options(&mut self, options: crate::ProtocolTurnOptions) {
        self.runtime.set_protocol_turn_options(options);
    }

    /// Set the durable protocol turn options and mirror them to **every** agent
    /// frame. Apply-at-open semantics: the last applied value is recorded on the
    /// session and all frames.
    pub fn set_protocol_turn_options_all_frames(&mut self, options: crate::ProtocolTurnOptions) {
        self.runtime.set_protocol_turn_options_all_frames(options);
    }
}

/// Read-only descriptor of a session materialization handed to
/// [`ProtocolSessionPlugin::configure_runtime_on_materialize`].
pub struct ProtocolSessionMaterialization<'a> {
    /// Plugin-keyed options that reached this materialization: builder options
    /// for a root/builder open, request options for a child create.
    pub plugin_options: &'a PluginOptions,
    /// Whether this materialization is a root session (no parent).
    pub is_root_session: bool,
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

    async fn snapshot_execution_state(
        &self,
        _ctx: ProtocolSessionContext<'_>,
    ) -> Result<ExecutionStateSnapshot, crate::SessionError> {
        Ok(ExecutionStateSnapshot::default())
    }

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

    /// Complete live execution state, with every leaf body present.
    ///
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
    /// Build the `TurnDriverPreamble` (driver handle + prompt text + tool
    /// surface metadata) for a turn.
    fn build_preamble(&self, input: crate::ProtocolBuildInput) -> crate::TurnDriverPreamble;
}

/// Plugin-owned options carried on a `SessionCreateRequest`.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct PluginOptions {
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub plugins: BTreeMap<String, serde_json::Value>,
}

impl PluginOptions {
    /// Constructs an empty `PluginOptions` for protocol and process-engine implementors while
    /// preparing or executing plugin and tool work.
    pub fn empty() -> Self {
        Self::default()
    }

    /// Serializes one plugin's typed options for protocol implementors assembling session input.
    pub fn typed<T>(plugin_id: impl Into<String>, extras: T) -> Result<Self, serde_json::Error>
    where
        T: Serialize,
    {
        let mut options = Self::default();
        options.insert_typed(plugin_id, extras)?;
        Ok(options)
    }

    /// Inserts one plugin's typed options for protocol implementors composing a shared option map.
    pub fn insert_typed<T>(
        &mut self,
        plugin_id: impl Into<String>,
        extras: T,
    ) -> Result<(), serde_json::Error>
    where
        T: Serialize,
    {
        self.plugins
            .insert(plugin_id.into(), serde_json::to_value(extras)?);
        Ok(())
    }

    /// Decodes one plugin's typed options for protocol and process-engine implementors, returning
    /// `None` when that plugin supplied no entry.
    pub fn decode<T>(&self, plugin_id: &str) -> Result<Option<T>, serde_json::Error>
    where
        T: DeserializeOwned,
    {
        self.plugins
            .get(plugin_id)
            .cloned()
            .map(serde_json::from_value)
            .transpose()
    }
}
