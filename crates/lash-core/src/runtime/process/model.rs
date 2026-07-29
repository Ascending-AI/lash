use std::collections::BTreeMap;
use std::fmt;
use std::sync::Arc;
use std::sync::Mutex;

use serde::{Deserialize, Serialize};

use super::events::{
    ProcessAwaitOutput, ProcessEventType, ProcessTerminalSemantics, ProcessTerminalState,
    default_process_event_types,
};
use super::op_scope::ProcessOpScope;
use super::validation::prepare_process_registration;

mod execution;
pub use execution::*;

pub type ProcessId = String;

/// Opaque position in a store's Process Change Feed.
///
/// The wrapped sequence is meaningful only to the registry backend that issued
/// it. Backends expose constructors/accessors so external store implementations
/// can persist and bind the position, but consumers should treat values as
/// cursors, not comparable timestamps.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ProcessChangeCursor(u64);

impl ProcessChangeCursor {
    pub fn initial() -> Self {
        Self(0)
    }

    pub fn from_store_sequence(sequence: u64) -> Self {
        Self(sequence)
    }

    pub fn store_sequence(self) -> u64 {
        self.0
    }
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct SessionScopeId(String);

impl SessionScopeId {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for SessionScopeId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl From<String> for SessionScopeId {
    fn from(value: String) -> Self {
        Self::new(value)
    }
}

impl From<&str> for SessionScopeId {
    fn from(value: &str) -> Self {
        Self::new(value)
    }
}

/// Durable executable input for a process.
///
/// `ToolCall`, `SessionTurn`, and `External` are kernel process primitives:
/// core owns their durable representation and execution semantics because they
/// are how the runtime coordinates tools, child sessions, and externally
/// completed work. `Engine` is the extension point for deployment-specific
/// process runtimes; those rows require a matching [`crate::ProcessEngine`] in
/// the host's process engine registry.
#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ProcessInput {
    ToolCall {
        call: crate::PreparedToolCall,
    },
    Engine {
        kind: String,
        #[serde(default)]
        payload: serde_json::Value,
    },
    SessionTurn {
        create_request: Box<crate::SessionCreateRequest>,
        turn_input: Box<crate::TurnInput>,
        output_contract: crate::ToolOutputContract,
    },
    External {
        #[serde(default)]
        metadata: serde_json::Value,
    },
}

impl Clone for ProcessInput {
    fn clone(&self) -> Self {
        match self {
            Self::ToolCall { call } => Self::ToolCall { call: call.clone() },
            Self::Engine { kind, payload } => Self::Engine {
                kind: kind.clone(),
                payload: payload.clone(),
            },
            Self::SessionTurn {
                create_request,
                turn_input,
                output_contract,
            } => Self::SessionTurn {
                create_request: create_request.clone(),
                turn_input: turn_input.clone(),
                output_contract: output_contract.clone(),
            },
            Self::External { metadata } => Self::External {
                metadata: metadata.clone(),
            },
        }
    }
}

impl PartialEq for ProcessInput {
    fn eq(&self, other: &Self) -> bool {
        serde_json::to_value(self).ok() == serde_json::to_value(other).ok()
    }
}

impl ProcessInput {
    pub fn engine_kind(&self) -> &'static str {
        match self {
            Self::ToolCall { .. } => "tool",
            Self::Engine { .. } => "engine",
            Self::SessionTurn { .. } => "session_turn",
            Self::External { .. } => "external",
        }
    }

    pub fn engine_specific_kind(&self) -> Option<&str> {
        match self {
            Self::Engine { kind, .. } => Some(kind.as_str()),
            _ => None,
        }
    }
}

/// Producer-declared contract stating what recovery may do with a process row
/// after owner loss. Required at registration and applied mechanically by the
/// sweep; never inferred at runtime. See ADR 0019.
///
/// There is deliberately no `Default` and no serde default: a producer that
/// forgets to declare a disposition must fail to compile rather than silently
/// inherit re-execution.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RecoveryDisposition {
    /// Another owner may re-execute the work — the contract for journaled,
    /// idempotent inputs (engine rows, session-turn rows).
    Rerunnable,
    /// The contract binds at first start: before any owner has begun execution
    /// any worker may claim the row; once execution has started, no other owner
    /// may ever re-execute it — abandonment is the only recovery.
    OwnerBound,
    /// Lash never executes the row at all. Closure comes from an external actor
    /// calling `complete_process`, or from a reconciled Abandon Request.
    ExternallyOwned,
}

/// Durable, non-terminal marker recording that a non-owner authorized
/// abandonment without proof the owner is gone. The sweep reconciles it into
/// [`ProcessTerminalState::Abandoned`](super::events::ProcessTerminalState::Abandoned)
/// only once the row's lease has lapsed; the marker never terminates anything
/// by itself and is visible to observers while pending. See ADR 0019.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AbandonRequest {
    pub requested_by: String,
    pub requested_at_ms: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ProcessExecutionEnvRef(String);

impl ProcessExecutionEnvRef {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for ProcessExecutionEnvRef {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProcessExecutionEnvSpec {
    #[serde(default)]
    pub plugin_options: crate::PluginOptions,
    #[serde(default)]
    pub policy: crate::SessionPolicy,
}

impl ProcessExecutionEnvSpec {
    pub fn new(plugin_options: crate::PluginOptions, policy: crate::SessionPolicy) -> Self {
        Self {
            plugin_options,
            policy,
        }
    }

    pub fn stable_ref(&self) -> Result<ProcessExecutionEnvRef, serde_json::Error> {
        crate::stable_hash::stable_json_sha256_hex(self)
            .map(|hash| ProcessExecutionEnvRef::new(format!("process-env:sha256:{hash}")))
    }

    pub fn to_store_bytes(&self) -> Result<Vec<u8>, serde_json::Error> {
        serde_json::to_vec(self)
    }

    pub fn from_store_bytes(bytes: &[u8]) -> Result<Self, serde_json::Error> {
        serde_json::from_slice(bytes)
    }
}

#[async_trait::async_trait]
pub trait ProcessExecutionEnvStore: Send + Sync {
    async fn put_process_execution_env(
        &self,
        env_ref: &ProcessExecutionEnvRef,
        bytes: &[u8],
    ) -> Result<(), crate::PluginError>;

    async fn get_process_execution_env(
        &self,
        env_ref: &ProcessExecutionEnvRef,
    ) -> Result<Option<Vec<u8>>, crate::PluginError>;
}

#[derive(Default)]
pub struct InMemoryProcessExecutionEnvStore {
    envs: Mutex<BTreeMap<String, Vec<u8>>>,
}

impl InMemoryProcessExecutionEnvStore {
    pub fn new() -> Self {
        Self::default()
    }
}

#[async_trait::async_trait]
impl ProcessExecutionEnvStore for InMemoryProcessExecutionEnvStore {
    async fn put_process_execution_env(
        &self,
        env_ref: &ProcessExecutionEnvRef,
        bytes: &[u8],
    ) -> Result<(), crate::PluginError> {
        self.envs
            .lock()
            .map_err(|_| {
                crate::PluginError::Session("process execution env store lock poisoned".to_string())
            })?
            .insert(env_ref.as_str().to_string(), bytes.to_vec());
        Ok(())
    }

    async fn get_process_execution_env(
        &self,
        env_ref: &ProcessExecutionEnvRef,
    ) -> Result<Option<Vec<u8>>, crate::PluginError> {
        Ok(self
            .envs
            .lock()
            .map_err(|_| {
                crate::PluginError::Session("process execution env store lock poisoned".to_string())
            })?
            .get(env_ref.as_str())
            .cloned())
    }
}

pub async fn persist_process_execution_env(
    env_store: &dyn ProcessExecutionEnvStore,
    spec: &ProcessExecutionEnvSpec,
) -> Result<ProcessExecutionEnvRef, crate::PluginError> {
    let env_ref = spec.stable_ref().map_err(|err| {
        crate::PluginError::Session(format!("failed to hash process execution env: {err}"))
    })?;
    let bytes = spec.to_store_bytes().map_err(|err| {
        crate::PluginError::Session(format!("failed to encode process execution env: {err}"))
    })?;
    env_store
        .put_process_execution_env(&env_ref, &bytes)
        .await?;
    Ok(env_ref)
}

pub async fn load_process_execution_env(
    env_store: &dyn ProcessExecutionEnvStore,
    env_ref: &ProcessExecutionEnvRef,
) -> Result<ProcessExecutionEnvSpec, crate::PluginError> {
    let bytes = env_store
        .get_process_execution_env(env_ref)
        .await?
        .ok_or_else(|| {
            crate::PluginError::Session(format!("missing process execution env `{env_ref}`"))
        })?;
    ProcessExecutionEnvSpec::from_store_bytes(&bytes).map_err(|err| {
        crate::PluginError::Session(format!(
            "failed to decode process execution env `{env_ref}`: {err}"
        ))
    })
}

#[derive(Clone, Debug, Default)]
pub struct ProcessStartOptions {
    pub descriptor: Option<ProcessHandleDescriptor>,
    /// Runtime-internal spawn provenance override. Set by process execution
    /// contexts so children started *by a process* inherit the parent's
    /// originator and wake target instead of being stamped with the ephemeral
    /// execution scope. `None` means the session start path stamps the
    /// creating session (the in-session meaning of "start"). This rides
    /// options — not the request — so in-session callers cannot forge
    /// provenance through the session surface.
    pub spawn_provenance: Option<ProcessSpawnProvenance>,
}

/// Provenance a process-run context hands to its children: the chain's
/// originator and the chain's wake target. Mirrors the trigger fire path,
/// where the spawned process inherits the registrant and the grant is derived
/// from the wake target.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProcessSpawnProvenance {
    pub originator: ProcessOriginator,
    pub wake_target: Option<SessionScope>,
}

impl ProcessStartOptions {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_descriptor(mut self, descriptor: ProcessHandleDescriptor) -> Self {
        self.descriptor = Some(descriptor);
        self
    }

    pub fn with_optional_descriptor(mut self, descriptor: Option<ProcessHandleDescriptor>) -> Self {
        self.descriptor = descriptor;
        self
    }

    pub fn with_spawn_provenance(mut self, spawn_provenance: ProcessSpawnProvenance) -> Self {
        self.spawn_provenance = Some(spawn_provenance);
        self
    }

    pub fn execution_context(&self, scope: &ProcessOpScope<'_>) -> ProcessExecutionContext {
        ProcessExecutionContext {
            causal_invocation: scope.parent_invocation.clone(),
            execution_write_authority: None,
        }
    }
}

/// Public host-facing request for starting a visible process handle.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ProcessStartRequest {
    pub id: ProcessId,
    pub input: ProcessInput,
    pub disposition: RecoveryDisposition,
    /// Maximum execution attempts. `None` delegates pacing indefinitely to the
    /// engine; deterministic failures then require host cancellation or
    /// abandonment to resolve awaiters.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_attempts: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub env_spec: Option<ProcessExecutionEnvSpec>,
    pub originator: ProcessOriginator,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wake_target: Option<SessionScope>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub grant: Option<ProcessStartGrant>,
    #[serde(default)]
    pub event_types: Vec<ProcessEventType>,
}

impl ProcessStartRequest {
    pub fn new(
        id: impl Into<ProcessId>,
        input: ProcessInput,
        disposition: RecoveryDisposition,
        originator: ProcessOriginator,
    ) -> Self {
        Self {
            id: id.into(),
            input,
            disposition,
            max_attempts: None,
            env_spec: None,
            originator,
            wake_target: None,
            grant: None,
            event_types: default_process_event_types(),
        }
    }

    /// External placeholder start: `ProcessInput::External` is always
    /// [`RecoveryDisposition::ExternallyOwned`] — lash never executes it.
    pub fn external(
        id: impl Into<ProcessId>,
        originator: ProcessOriginator,
        metadata: serde_json::Value,
    ) -> Self {
        Self::new(
            id,
            ProcessInput::External { metadata },
            RecoveryDisposition::ExternallyOwned,
            originator,
        )
    }

    pub fn with_env_spec(mut self, env_spec: ProcessExecutionEnvSpec) -> Self {
        self.env_spec = Some(env_spec);
        self
    }

    pub fn with_max_attempts(mut self, max_attempts: Option<u32>) -> Self {
        self.max_attempts = max_attempts;
        self
    }

    pub fn with_wake_target(mut self, wake_target: Option<SessionScope>) -> Self {
        self.wake_target = wake_target;
        self
    }

    pub fn with_grant(mut self, grant: Option<ProcessStartGrant>) -> Self {
        self.grant = grant;
        self
    }

    pub fn with_event_types(
        mut self,
        event_types: impl IntoIterator<Item = ProcessEventType>,
    ) -> Self {
        self.event_types = event_types.into_iter().collect();
        self
    }

    pub fn with_extra_event_types(
        mut self,
        event_types: impl IntoIterator<Item = ProcessEventType>,
    ) -> Self {
        self.event_types.extend(event_types);
        self
    }

    pub fn into_registration(self, env_ref: Option<ProcessExecutionEnvRef>) -> ProcessRegistration {
        ProcessRegistration::new(
            self.id,
            self.input,
            self.disposition,
            ProcessProvenance::new(self.originator),
        )
        .with_max_attempts(self.max_attempts)
        .with_event_types(self.event_types)
        .with_execution_env_ref(env_ref)
        .with_wake_target(self.wake_target)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionScope {
    pub session_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_frame_id: Option<crate::AgentFrameId>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProcessProvenance {
    pub originator: ProcessOriginator,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub caused_by: Option<crate::CausalRef>,
}

impl ProcessProvenance {
    pub fn new(originator: ProcessOriginator) -> Self {
        Self {
            originator,
            caused_by: None,
        }
    }

    pub fn host() -> Self {
        Self::new(ProcessOriginator::host())
    }

    pub fn session(scope: SessionScope) -> Self {
        Self::new(ProcessOriginator::session(scope))
    }

    pub fn with_caused_by(mut self, caused_by: Option<crate::CausalRef>) -> Self {
        self.caused_by = caused_by;
        self
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ProcessOriginator {
    Host {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        scope: Option<String>,
    },
    Session {
        scope: SessionScope,
    },
}

impl ProcessOriginator {
    pub fn host() -> Self {
        Self::Host { scope: None }
    }

    pub fn host_scoped(scope: impl Into<String>) -> Self {
        Self::Host {
            scope: Some(scope.into()),
        }
    }

    pub fn session(scope: SessionScope) -> Self {
        Self::Session { scope }
    }

    pub fn scope_id(&self) -> String {
        match self {
            Self::Host { scope } => scope
                .as_ref()
                .map(|scope| format!("host:{scope}"))
                .unwrap_or_else(|| "host".to_string()),
            Self::Session { scope } => scope.id().to_string(),
        }
    }
}

impl SessionScope {
    pub fn new(session_id: impl Into<String>) -> Self {
        Self {
            session_id: session_id.into(),
            agent_frame_id: None,
        }
    }

    pub fn for_agent_frame(
        session_id: impl Into<String>,
        agent_frame_id: impl Into<crate::AgentFrameId>,
    ) -> Self {
        Self {
            session_id: session_id.into(),
            agent_frame_id: Some(agent_frame_id.into()),
        }
    }

    pub fn id(&self) -> SessionScopeId {
        match self.agent_frame_id.as_deref() {
            Some(frame_id) if !frame_id.is_empty() => {
                SessionScopeId::new(format!("session:{}/frame:{frame_id}", self.session_id))
            }
            _ => SessionScopeId::new(format!("session:{}", self.session_id)),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.session_id.is_empty()
    }
}

/// Serializable process spec used to start or recover a runtime process.
#[derive(Debug, Serialize, Deserialize)]
pub struct ProcessRegistration {
    pub id: ProcessId,
    pub input: Arc<ProcessInput>,
    pub disposition: RecoveryDisposition,
    /// Maximum execution attempts, or `None` for engine-paced indefinite
    /// retry. A deterministic failure with `None` can remain non-terminal
    /// indefinitely; producers with deterministic failure modes should set an
    /// explicit budget.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_attempts: Option<u32>,
    pub identity: ProcessIdentity,
    #[serde(default)]
    pub event_types: Vec<ProcessEventType>,
    pub provenance: ProcessProvenance,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub env_ref: Option<ProcessExecutionEnvRef>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wake_target: Option<SessionScope>,
}

impl Clone for ProcessRegistration {
    fn clone(&self) -> Self {
        Self {
            id: self.id.clone(),
            input: Arc::clone(&self.input),
            disposition: self.disposition,
            max_attempts: self.max_attempts,
            identity: self.identity.clone(),
            event_types: self.event_types.clone(),
            provenance: self.provenance.clone(),
            env_ref: self.env_ref.clone(),
            wake_target: self.wake_target.clone(),
        }
    }
}

impl ProcessRegistration {
    pub fn new(
        id: impl Into<ProcessId>,
        input: ProcessInput,
        disposition: RecoveryDisposition,
        provenance: ProcessProvenance,
    ) -> Self {
        let identity = ProcessIdentity::from_process_input(&input);
        Self {
            id: id.into(),
            input: Arc::new(input),
            disposition,
            max_attempts: None,
            identity,
            event_types: default_process_event_types(),
            provenance,
            env_ref: None,
            wake_target: None,
        }
    }

    pub(crate) fn session_start_draft(
        id: impl Into<ProcessId>,
        input: ProcessInput,
        disposition: RecoveryDisposition,
    ) -> Self {
        Self::new(id, input, disposition, ProcessProvenance::host())
    }

    pub fn with_process_provenance(mut self, provenance: ProcessProvenance) -> Self {
        self.provenance = provenance;
        self
    }

    pub fn with_max_attempts(mut self, max_attempts: Option<u32>) -> Self {
        self.max_attempts = max_attempts;
        self
    }

    pub fn with_execution_env_ref(mut self, env_ref: Option<ProcessExecutionEnvRef>) -> Self {
        self.env_ref = env_ref;
        self
    }

    pub fn with_wake_target(mut self, wake_target: Option<SessionScope>) -> Self {
        self.wake_target = wake_target;
        self
    }

    pub fn with_identity(mut self, identity: ProcessIdentity) -> Self {
        self.identity = identity;
        self
    }

    pub fn with_event_types(
        mut self,
        event_types: impl IntoIterator<Item = ProcessEventType>,
    ) -> Self {
        self.event_types = event_types.into_iter().collect();
        self
    }

    pub fn with_extra_event_types(
        mut self,
        event_types: impl IntoIterator<Item = ProcessEventType>,
    ) -> Self {
        self.event_types.extend(event_types);
        self
    }
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum ProcessStatus {
    #[default]
    Running,
    Completed {
        await_output: ProcessAwaitOutput,
    },
    Failed {
        await_output: ProcessAwaitOutput,
    },
    Cancelled {
        await_output: ProcessAwaitOutput,
    },
    Abandoned {
        await_output: ProcessAwaitOutput,
    },
}

impl ProcessStatus {
    pub fn from_terminal(terminal: ProcessTerminalSemantics) -> Self {
        match terminal.state {
            ProcessTerminalState::Completed => Self::Completed {
                await_output: terminal.await_output,
            },
            ProcessTerminalState::Failed => Self::Failed {
                await_output: terminal.await_output,
            },
            ProcessTerminalState::Cancelled => Self::Cancelled {
                await_output: terminal.await_output,
            },
            ProcessTerminalState::Abandoned => Self::Abandoned {
                await_output: terminal.await_output,
            },
        }
    }

    pub fn is_terminal(&self) -> bool {
        !matches!(self, Self::Running)
    }

    pub fn label(&self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::Completed { .. } => "completed",
            Self::Failed { .. } => "failed",
            Self::Cancelled { .. } => "cancelled",
            Self::Abandoned { .. } => "abandoned",
        }
    }

    pub fn terminal_state(&self) -> Option<ProcessTerminalState> {
        match self {
            Self::Running => None,
            Self::Completed { .. } => Some(ProcessTerminalState::Completed),
            Self::Failed { .. } => Some(ProcessTerminalState::Failed),
            Self::Cancelled { .. } => Some(ProcessTerminalState::Cancelled),
            Self::Abandoned { .. } => Some(ProcessTerminalState::Abandoned),
        }
    }

    pub fn await_output(&self) -> Option<&ProcessAwaitOutput> {
        match self {
            Self::Running => None,
            Self::Completed { await_output }
            | Self::Failed { await_output }
            | Self::Cancelled { await_output }
            | Self::Abandoned { await_output } => Some(await_output),
        }
    }

    pub fn terminal_semantics(&self) -> Option<ProcessTerminalSemantics> {
        Some(ProcessTerminalSemantics {
            state: self.terminal_state()?,
            await_output: self.await_output()?.clone(),
        })
    }
}

/// Durable process row. Session-visible addressability lives in
/// [`ProcessHandleGrant`], not in the process record.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ProcessRecord {
    pub id: ProcessId,
    pub registration_hash: String,
    pub input: Arc<ProcessInput>,
    /// Declared recovery contract. Required with no serde default: pre-column
    /// durable rows cannot deserialize and are handled by each store's schema
    /// version bump (reject-and-recreate), never by an API/serde default.
    pub disposition: RecoveryDisposition,
    /// Persisted attempt budget; `None` retains engine-paced indefinite retry.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_attempts: Option<u32>,
    pub identity: ProcessIdentity,
    #[serde(default)]
    pub event_types: Vec<ProcessEventType>,
    pub provenance: ProcessProvenance,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub env_ref: Option<ProcessExecutionEnvRef>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wake_target: Option<SessionScope>,
    #[serde(default)]
    pub created_at_ms: u64,
    #[serde(default)]
    pub updated_at_ms: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub external_ref: Option<ProcessExternalRef>,
    /// Durable, lease-fenced execution-started fact (ADR 0019). `None` until a
    /// runner records it immediately before executing. Boxed so these
    /// usually-absent facts do not enlarge the pervasive `ProcessRecord` that
    /// flows through the runtime; serde treats `Option<Box<T>>` identically to
    /// `Option<T>`, so the persisted JSON is unchanged.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub first_started: Option<Box<ProcessStarted>>,
    /// Pending Abandon Request the sweep reconciles once the lease lapses.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub abandon_request: Option<Box<AbandonRequest>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wait: Option<WaitState>,
    #[serde(default)]
    pub status: ProcessStatus,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct WaitState {
    pub kind: WaitKind,
    pub since_ms: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum WaitKind {
    Signal {
        name: String,
        event_type: String,
        key: String,
        ordinal: u64,
    },
}

impl WaitState {
    pub fn key(&self) -> &str {
        let WaitKind::Signal { key, .. } = &self.kind;
        key
    }
}

impl ProcessRecord {
    pub fn from_registration(registration: ProcessRegistration) -> Self {
        Self::from_registration_with_clock(registration, &crate::SystemClock)
    }

    pub fn from_registration_with_clock(
        registration: ProcessRegistration,
        clock: &dyn crate::Clock,
    ) -> Self {
        let (registration, registration_hash) = prepare_process_registration(registration)
            .expect("process registration should be valid and hashable before record construction");
        Self::from_prepared_registration(registration, registration_hash, clock.timestamp_ms())
    }

    pub fn from_prepared_registration(
        registration: ProcessRegistration,
        registration_hash: String,
        now_ms: u64,
    ) -> Self {
        Self {
            id: registration.id,
            registration_hash,
            input: registration.input,
            disposition: registration.disposition,
            max_attempts: registration.max_attempts,
            identity: registration.identity,
            event_types: registration.event_types,
            provenance: registration.provenance,
            env_ref: registration.env_ref,
            wake_target: registration.wake_target,
            created_at_ms: now_ms,
            updated_at_ms: now_ms,
            external_ref: None,
            first_started: None,
            abandon_request: None,
            wait: None,
            status: ProcessStatus::Running,
        }
    }

    pub fn is_terminal(&self) -> bool {
        self.status.is_terminal()
    }

    pub fn clear_wake_target_for_session(&mut self, session_id: &str) -> bool {
        self.clear_wake_target_for_session_with_clock(session_id, &crate::SystemClock)
    }

    pub fn clear_wake_target_for_session_with_clock(
        &mut self,
        session_id: &str,
        clock: &dyn crate::Clock,
    ) -> bool {
        let should_clear = self
            .wake_target
            .as_ref()
            .is_some_and(|scope| scope.session_id == session_id);
        if should_clear {
            self.wake_target = None;
            self.updated_at_ms = clock.timestamp_ms();
        }
        should_clear
    }

    pub fn originator_scope_id(&self) -> String {
        self.provenance.originator.scope_id()
    }
}

/// Canonical process identity stored alongside every durable process row.
///
/// `ProcessInput::Engine` keeps its payload opaque to core. Engines therefore
/// publish their visible kind, display label, and definition identity at the
/// registration boundary; list, summary, trigger, and observation paths read
/// this durable field instead of decoding engine payload conventions.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProcessIdentity {
    pub kind: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub definition: Option<serde_json::Value>,
}

impl ProcessIdentity {
    pub fn new(kind: impl Into<String>) -> Self {
        Self {
            kind: kind.into(),
            label: None,
            definition: None,
        }
    }

    pub fn with_label(mut self, label: Option<impl Into<String>>) -> Self {
        self.label = label.map(Into::into);
        self
    }

    pub fn with_definition(mut self, definition: Option<serde_json::Value>) -> Self {
        self.definition = definition;
        self
    }

    pub fn from_process_input(input: &ProcessInput) -> Self {
        match input {
            ProcessInput::ToolCall { call } => {
                Self::new("tool").with_label(Some(call.tool_name.clone()))
            }
            ProcessInput::Engine { kind, .. } => Self::new(kind.clone()),
            ProcessInput::SessionTurn { create_request, .. } => {
                let label = create_request
                    .subagent
                    .as_ref()
                    .map(|subagent| subagent.capability.clone())
                    .or_else(|| create_request.usage_source.clone())
                    .or_else(|| create_request.session_id.clone());
                Self::new("session_turn").with_label(label)
            }
            ProcessInput::External { metadata } => {
                let label = metadata
                    .get("label")
                    .or_else(|| metadata.get("name"))
                    .or_else(|| metadata.get("title"))
                    .and_then(serde_json::Value::as_str)
                    .map(str::to_string);
                Self::new("external").with_label(label)
            }
        }
    }
}

/// Wire-format version stamped on every persisted [`ProcessLease`].
///
/// Bump when the on-wire shape of `ProcessLease` changes in a way that older
/// code cannot safely deserialize. Version 2 replaced the bare `owner_id`
/// string with a full [`LeaseOwnerIdentity`](crate::LeaseOwnerIdentity)
/// carrying incarnation and liveness metadata for fenced reclaim.
pub const PROCESS_LEASE_SCHEMA_VERSION: u32 = 2;

/// Durable session stores owned exclusively by one process execution.
pub fn process_runtime_session_ids(process_id: &str) -> [String; 2] {
    [
        format!("process-env:{process_id}"),
        format!("process-session-turn:{process_id}"),
    ]
}

/// Durable lease over a non-terminal background process.
///
/// The lease pair `(owner, lease_token)` plus `fencing_token` are how lash guarantees that
/// one non-terminal process is re-executed by exactly one worker at a time —
/// even after a crash, even across two workers that both sweep the same
/// registry for recoverable work. The durable backend
/// (`lash-sqlite-store`) uses these to serialize concurrent claims on the same
/// `process_id`; future distributed durable backends use the *same* fields to
/// coordinate workers that don't share a file system.
///
/// The owner is a full [`LeaseOwnerIdentity`](crate::LeaseOwnerIdentity):
/// its persisted liveness metadata is what lets a sweeping worker prove a
/// busy holder is *definitely dead* and reclaim the lease before the TTL
/// through [`ProcessRegistry::reclaim_process_lease`](super::ProcessRegistry::reclaim_process_lease),
/// mirroring the session execution lane.
///
/// **This is not single-process theatre.** The owner / fencing-token /
/// lease-token triple is the public contract that lets any backend detect and
/// reject stale writers. Treat it as load-bearing, not defensive.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ProcessLease {
    pub schema_version: u32,
    pub process_id: ProcessId,
    pub owner: crate::LeaseOwnerIdentity,
    pub lease_token: String,
    pub fencing_token: u64,
    pub claimed_at_epoch_ms: u64,
    pub expires_at_epoch_ms: u64,
}

/// Outcome of claiming (or reclaiming) a [`ProcessLease`].
///
/// Mirrors [`SessionExecutionLeaseClaimOutcome`](crate::SessionExecutionLeaseClaimOutcome):
/// a busy outcome carries the observed holder so the claimant can assess its
/// liveness and perform a fenced reclaim on exactly the lease it observed.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum ProcessLeaseClaimOutcome {
    Acquired(ProcessLease),
    Busy { holder: ProcessLease },
}

impl ProcessLeaseClaimOutcome {
    pub fn acquired(self) -> Option<ProcessLease> {
        match self {
            Self::Acquired(lease) => Some(lease),
            Self::Busy { .. } => None,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ProcessLeaseCompletion {
    pub process_id: ProcessId,
    pub lease_token: String,
}

impl ProcessLeaseCompletion {
    pub fn from_lease(lease: &ProcessLease) -> Self {
        Self {
            process_id: lease.process_id.clone(),
            lease_token: lease.lease_token.clone(),
        }
    }
}

/// Durable backend reference for background work accepted outside the local process.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct ProcessExternalRef {
    pub backend: String,
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metadata: Option<serde_json::Value>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProcessHandleDescriptor {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
}

impl ProcessHandleDescriptor {
    pub fn new(kind: Option<impl Into<String>>, label: Option<impl Into<String>>) -> Self {
        Self {
            kind: kind.map(Into::into),
            label: label.map(Into::into),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProcessHandleGrant {
    pub session_id: String,
    pub process_id: ProcessId,
    pub descriptor: ProcessHandleDescriptor,
}

pub type ProcessHandleGrantEntry = (ProcessHandleGrant, ProcessRecord);

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProcessLifecycleStatus {
    #[default]
    Running,
    Completed,
    Failed,
    Cancelled,
    Abandoned,
}

impl ProcessLifecycleStatus {
    pub fn label(self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::Completed => "completed",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
            Self::Abandoned => "abandoned",
        }
    }

    pub fn is_terminal(self) -> bool {
        !matches!(self, Self::Running)
    }

    pub fn terminal_state(self) -> Option<ProcessTerminalState> {
        match self {
            Self::Running => None,
            Self::Completed => Some(ProcessTerminalState::Completed),
            Self::Failed => Some(ProcessTerminalState::Failed),
            Self::Cancelled => Some(ProcessTerminalState::Cancelled),
            Self::Abandoned => Some(ProcessTerminalState::Abandoned),
        }
    }
}

impl From<&ProcessStatus> for ProcessLifecycleStatus {
    fn from(status: &ProcessStatus) -> Self {
        match status {
            ProcessStatus::Running => Self::Running,
            ProcessStatus::Completed { .. } => Self::Completed,
            ProcessStatus::Failed { .. } => Self::Failed,
            ProcessStatus::Cancelled { .. } => Self::Cancelled,
            ProcessStatus::Abandoned { .. } => Self::Abandoned,
        }
    }
}

impl From<ProcessStatus> for ProcessLifecycleStatus {
    fn from(status: ProcessStatus) -> Self {
        Self::from(&status)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProcessHandleSummary {
    #[serde(rename = "__handle__")]
    pub handle_type: String,
    pub id: ProcessId,
    pub process_id: ProcessId,
    pub descriptor: ProcessHandleDescriptor,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub definition: Option<serde_json::Value>,
    pub status: ProcessLifecycleStatus,
}

impl ProcessHandleSummary {
    pub fn new(
        process_id: impl Into<ProcessId>,
        descriptor: ProcessHandleDescriptor,
        status: ProcessLifecycleStatus,
    ) -> Self {
        let process_id = process_id.into();
        Self {
            handle_type: "process".to_string(),
            id: process_id.clone(),
            process_id,
            descriptor,
            definition: None,
            status,
        }
    }

    pub fn with_definition(mut self, definition: Option<serde_json::Value>) -> Self {
        self.definition = definition;
        self
    }

    pub fn from_grant_record(grant: ProcessHandleGrant, record: ProcessRecord) -> Self {
        let definition = record.identity.definition.clone();
        Self::new(
            record.id,
            grant.descriptor,
            ProcessLifecycleStatus::from(record.status),
        )
        .with_definition(definition)
    }
}

impl From<ProcessHandleGrantEntry> for ProcessHandleSummary {
    fn from((grant, record): ProcessHandleGrantEntry) -> Self {
        Self::from_grant_record(grant, record)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProcessCancelSummary {
    pub process_id: ProcessId,
    pub status: ProcessLifecycleStatus,
}

impl ProcessCancelSummary {
    pub fn from_record(record: ProcessRecord) -> Self {
        Self {
            process_id: record.id,
            status: ProcessLifecycleStatus::from(record.status),
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ProcessStatusFilter {
    #[default]
    Running,
    Completed,
    Failed,
    Cancelled,
    Abandoned,
    Any,
}

impl ProcessStatusFilter {
    pub fn decode(value: Option<&str>) -> Result<Self, String> {
        match value.unwrap_or("running") {
            "running" => Ok(Self::Running),
            "completed" => Ok(Self::Completed),
            "failed" => Ok(Self::Failed),
            "cancelled" => Ok(Self::Cancelled),
            "abandoned" => Ok(Self::Abandoned),
            "any" => Ok(Self::Any),
            other => Err(format!(
                "processes.list status must be `running`, `completed`, `failed`, `cancelled`, `abandoned`, or `any`, got `{other}`"
            )),
        }
    }

    pub fn list_mode(self) -> ProcessListMode {
        match self {
            Self::Running => ProcessListMode::Live,
            Self::Completed | Self::Failed | Self::Cancelled | Self::Abandoned | Self::Any => {
                ProcessListMode::All
            }
        }
    }

    pub fn matches(self, status: ProcessLifecycleStatus) -> bool {
        match self {
            Self::Running => status == ProcessLifecycleStatus::Running,
            Self::Completed => status == ProcessLifecycleStatus::Completed,
            Self::Failed => status == ProcessLifecycleStatus::Failed,
            Self::Cancelled => status == ProcessLifecycleStatus::Cancelled,
            Self::Abandoned => status == ProcessLifecycleStatus::Abandoned,
            Self::Any => true,
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct ProcessListFilter {
    pub definition: Option<serde_json::Value>,
    pub status: ProcessStatusFilter,
    pub waiting: Option<bool>,
    pub originator_scope_id: Option<String>,
    pub identity_kind: Option<String>,
    pub identity_label: Option<String>,
    pub caused_by_occurrence_id: Option<String>,
    pub caused_by_subscription_id: Option<String>,
    /// Inclusive lower bound for `created_at_ms`; paired with
    /// `created_at_end_ms` this is a half-open `[start, end)` range.
    pub created_at_start_ms: Option<u64>,
    /// Exclusive upper bound for `created_at_ms`; paired with
    /// `created_at_start_ms` this is a half-open `[start, end)` range.
    pub created_at_end_ms: Option<u64>,
}

impl ProcessListFilter {
    pub fn decode(args: &serde_json::Value) -> Result<Self, String> {
        let map = args
            .as_object()
            .ok_or_else(|| "processes.list expects a record of process filters".to_string())?;
        for key in map.keys() {
            match key.as_str() {
                "definition"
                | "status"
                | "waiting"
                | "originator_scope_id"
                | "identity_kind"
                | "identity_label"
                | "caused_by_occurrence_id"
                | "caused_by_subscription_id"
                | "created_at_start_ms"
                | "created_at_end_ms" => {}
                _ => return Err(format!("processes.list unknown filter `{key}`")),
            }
        }
        let definition = args.get("definition").cloned();
        let status =
            ProcessStatusFilter::decode(args.get("status").and_then(serde_json::Value::as_str))?;
        let waiting = args
            .get("waiting")
            .map(|value| {
                value
                    .as_bool()
                    .ok_or_else(|| "processes.list `waiting` filter must be a boolean".to_string())
            })
            .transpose()?;
        let originator_scope_id = optional_string_filter(args, "originator_scope_id")?;
        let identity_kind = optional_string_filter(args, "identity_kind")?;
        let identity_label = optional_string_filter(args, "identity_label")?;
        let caused_by_occurrence_id = optional_string_filter(args, "caused_by_occurrence_id")?;
        let caused_by_subscription_id = optional_string_filter(args, "caused_by_subscription_id")?;
        let created_at_start_ms = optional_u64_filter(args, "created_at_start_ms")?;
        let created_at_end_ms = optional_u64_filter(args, "created_at_end_ms")?;
        Ok(Self {
            definition,
            status,
            waiting,
            originator_scope_id,
            identity_kind,
            identity_label,
            caused_by_occurrence_id,
            caused_by_subscription_id,
            created_at_start_ms,
            created_at_end_ms,
        })
    }

    pub fn list_mode(&self) -> ProcessListMode {
        self.status.list_mode()
    }

    pub fn matches_entry(&self, entry: &ProcessHandleGrantEntry) -> bool {
        let (_grant, record) = entry;
        self.matches_record(record)
    }

    pub fn matches_record(&self, record: &ProcessRecord) -> bool {
        let status = ProcessLifecycleStatus::from(&record.status);
        self.status.matches(status)
            && self
                .definition
                .as_ref()
                .is_none_or(|definition| record.identity.definition.as_ref() == Some(definition))
            && self
                .waiting
                .is_none_or(|waiting| record.wait.is_some() == waiting)
            && self
                .originator_scope_id
                .as_ref()
                .is_none_or(|scope_id| record.originator_scope_id() == scope_id.as_str())
            && self
                .identity_kind
                .as_ref()
                .is_none_or(|kind| record.identity.kind.as_str() == kind.as_str())
            && self
                .identity_label
                .as_ref()
                .is_none_or(|label| record.identity.label.as_deref() == Some(label.as_str()))
            && self
                .caused_by_occurrence_id
                .as_ref()
                .is_none_or(|occurrence_id| caused_by_occurrence_matches(record, occurrence_id))
            && self
                .caused_by_subscription_id
                .as_ref()
                .is_none_or(|subscription_id| {
                    caused_by_subscription_matches(record, subscription_id)
                })
            && self
                .created_at_start_ms
                .is_none_or(|start_ms| record.created_at_ms >= start_ms)
            && self
                .created_at_end_ms
                .is_none_or(|end_ms| record.created_at_ms < end_ms)
    }
}

fn optional_string_filter(args: &serde_json::Value, key: &str) -> Result<Option<String>, String> {
    args.get(key)
        .map(|value| {
            value
                .as_str()
                .map(str::to_string)
                .ok_or_else(|| format!("processes.list `{key}` filter must be a string"))
        })
        .transpose()
}

fn optional_u64_filter(args: &serde_json::Value, key: &str) -> Result<Option<u64>, String> {
    args.get(key)
        .map(|value| {
            value
                .as_u64()
                .ok_or_else(|| format!("processes.list `{key}` filter must be an integer"))
        })
        .transpose()
}

fn caused_by_occurrence_matches(record: &ProcessRecord, occurrence_id: &str) -> bool {
    matches!(
        record.provenance.caused_by.as_ref(),
        Some(crate::CausalRef::TriggerOccurrence { occurrence_id: actual, .. }) if actual == occurrence_id
    )
}

fn caused_by_subscription_matches(record: &ProcessRecord, subscription_id: &str) -> bool {
    matches!(
        record.provenance.caused_by.as_ref(),
        Some(crate::CausalRef::TriggerOccurrence {
            subscription_id: Some(actual),
            ..
        }) if actual == subscription_id
    )
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProcessListMode {
    #[default]
    Live,
    All,
}

impl ProcessListMode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Live => "live",
            Self::All => "all",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProcessStartGrant {
    pub session_scope: SessionScope,
    pub descriptor: ProcessHandleDescriptor,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProcessSessionDeleteReport {
    pub session_id: String,
    pub revoked_handle_count: usize,
    pub discarded_wake_delivery_count: usize,
    pub orphaned_process_ids: Vec<String>,
    pub preserved_process_ids: Vec<String>,
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn process_value(component: &str, pos: usize, name: &str) -> serde_json::Value {
        json!({
            "component": component,
            "pos": pos,
            "name": name,
        })
    }

    fn engine_entry(
        process_id: &str,
        definition: serde_json::Value,
        process_name: &str,
        status: ProcessStatus,
    ) -> ProcessHandleGrantEntry {
        let mut record = ProcessRecord::from_registration(
            ProcessRegistration::new(
                process_id,
                ProcessInput::Engine {
                    kind: "test-engine".to_string(),
                    payload: json!({
                        "definition": definition.clone(),
                        "label": process_name,
                    }),
                },
                RecoveryDisposition::Rerunnable,
                ProcessProvenance::host(),
            )
            .with_identity(
                ProcessIdentity::new("test-engine")
                    .with_label(Some(process_name))
                    .with_definition(Some(definition)),
            )
            .with_execution_env_ref(Some(ProcessExecutionEnvRef::new(format!(
                "process-env:test:{process_id}"
            )))),
        );
        record.status = status;
        (
            ProcessHandleGrant {
                session_id: "session".to_string(),
                process_id: process_id.to_string(),
                descriptor: ProcessHandleDescriptor::new(Some("test-engine"), Some(process_name)),
            },
            record,
        )
    }

    #[test]
    fn process_list_filter_matches_waiting_facet() {
        let process_ref = process_value("target", 0, "target");
        let mut waiting_entry = engine_entry(
            "waiting",
            process_ref.clone(),
            "target",
            ProcessStatus::Running,
        );
        waiting_entry.1.wait = Some(WaitState {
            since_ms: 42,
            kind: WaitKind::Signal {
                name: "ready".to_string(),
                event_type: "signal.ready".to_string(),
                key: "process:waiting:signal.ready:1".to_string(),
                ordinal: 1,
            },
        });
        let idle_entry = engine_entry("idle", process_ref, "target", ProcessStatus::Running);
        let waiting_filter =
            ProcessListFilter::decode(&json!({ "waiting": true })).expect("decode waiting filter");
        let idle_filter =
            ProcessListFilter::decode(&json!({ "waiting": false })).expect("decode idle filter");

        assert_eq!(waiting_filter.list_mode(), ProcessListMode::Live);
        assert!(waiting_filter.matches_entry(&waiting_entry));
        assert!(!waiting_filter.matches_entry(&idle_entry));
        assert!(!idle_filter.matches_entry(&waiting_entry));
        assert!(idle_filter.matches_entry(&idle_entry));
        assert!(
            ProcessListFilter::decode(&json!({ "waiting": "yes" }))
                .expect_err("invalid waiting filter")
                .contains("must be a boolean")
        );
    }
}
