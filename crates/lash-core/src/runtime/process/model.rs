use std::collections::BTreeMap;
use std::fmt;
use std::sync::Arc;
use std::sync::Mutex;

use serde::{Deserialize, Serialize};

use super::events::{
    ProcessAwaitOutput, ProcessEventType, ProcessTerminalSemantics, default_process_event_types,
};
use super::op_scope::ProcessOpScope;
use super::validation::prepare_process_registration;

mod execution;
pub use execution::*;

pub type ProcessId = String;
pub type SessionId = String;
pub type ProcessOutcome = ProcessAwaitOutput;

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
    /// Constructs the backend-defined initial change-feed position for process-store implementors; callers must not treat it as a timestamp.
    pub fn initial() -> Self {
        Self(0)
    }

    /// Wraps an opaque backend change-feed sequence for process-store implementors without promising cross-backend comparability.
    pub fn from_store_sequence(sequence: u64) -> Self {
        Self(sequence)
    }

    /// Exposes the opaque sequence to the process-store implementor that issued it; consumers must not compare cursors from different backends.
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
        /// Caller-owned revision for the growable session request/input pair.
        /// Change this key whenever their executable meaning changes. The
        /// definition fingerprint deliberately excludes `create_request` and
        /// `turn_input`: keeping the key stable after changing either is a
        /// deliberate false merge, so the process id must otherwise be unique
        /// per definition.
        definition_key: String,
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
                definition_key,
                create_request,
                turn_input,
                output_contract,
            } => Self::SessionTurn {
                definition_key: definition_key.clone(),
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
    /// Exposes engine kind to store and process-engine implementors while persisting and coordinating durable process execution.
    pub fn engine_kind(&self) -> &'static str {
        match self {
            Self::ToolCall { .. } => "tool",
            Self::Engine { .. } => "engine",
            Self::SessionTurn { .. } => "session_turn",
            Self::External { .. } => "external",
        }
    }

    /// Exposes engine-specific kind to store and process-engine implementors, returning `None` for runtime-owned process primitives.
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
/// [`ProcessStatus::Abandoned`]
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
    /// Constructs a `ProcessExecutionEnvRef` for store and durable-substrate implementors suspending or resuming durable process execution.
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    /// Exposes the opaque stable reference for continuation-store implementors; its contents carry no ordering or backend-independent structure.
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
    /// Constructs a `ProcessExecutionEnvSpec` for protocol and process-engine implementors running a durable process.
    pub fn new(plugin_options: crate::PluginOptions, policy: crate::SessionPolicy) -> Self {
        Self {
            plugin_options,
            policy,
        }
    }

    /// Content-addresses the exact bytes persisted by [`Self::to_store_bytes`].
    ///
    /// Version 2 is a clean cutover from the former live-model serde hash.
    /// Store backends reject older schema versions and must be recreated; a
    /// future byte-format change requires a new textual family version and the
    /// same explicit old-row policy. These bytes follow the final binary's
    /// serde-json feature set; enabling order-preserving maps is therefore an
    /// identity-format change that requires a new family version.
    pub fn stable_ref(&self) -> Result<ProcessExecutionEnvRef, serde_json::Error> {
        self.to_store_bytes()
            .map(|bytes| process_execution_env_ref_for_bytes(&bytes))
    }

    /// Serializes a process execution environment for continuation-store implementors, preserving the stable reference alongside plugin and protocol state.
    pub fn to_store_bytes(&self) -> Result<Vec<u8>, serde_json::Error> {
        serde_json::to_vec(self)
    }

    /// Deserializes a stored execution environment for process-engine implementors and returns malformed payloads as plugin errors.
    pub fn from_store_bytes(bytes: &[u8]) -> Result<Self, serde_json::Error> {
        serde_json::from_slice(bytes)
    }
}

fn process_execution_env_ref_for_bytes(bytes: &[u8]) -> ProcessExecutionEnvRef {
    ProcessExecutionEnvRef::new(format!(
        "process-env:v2:sha256:{}",
        crate::stable_hash::sha256_hex(bytes)
    ))
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
    let bytes = spec.to_store_bytes().map_err(|err| {
        crate::PluginError::Session(format!("failed to encode process execution env: {err}"))
    })?;
    let env_ref = process_execution_env_ref_for_bytes(&bytes);
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
    /// Explicit host-selected initial observer session ids.
    pub initial_observers: Vec<SessionId>,
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
/// originator and wake target. Observer membership remains an independent,
/// explicit start option.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProcessSpawnProvenance {
    pub originator: ProcessOriginator,
    pub wake_session_id: Option<SessionId>,
}

impl ProcessStartOptions {
    /// Constructs default start options for store and durable-substrate implementors coordinating durable process execution.
    pub fn new() -> Self {
        Self::default()
    }

    /// Sets the initial observer carried by a `ProcessStartOptions` for store and durable-substrate
    /// implementors while persisting and coordinating durable process execution.
    pub fn with_initial_observer(mut self, session_id: impl Into<SessionId>) -> Self {
        self.initial_observers.push(session_id.into());
        self
    }

    /// Sets the initial observers carried by a `ProcessStartOptions` for store and
    /// durable-substrate implementors while persisting and coordinating durable process execution.
    pub fn with_initial_observers(
        mut self,
        observers: impl IntoIterator<Item = impl Into<SessionId>>,
    ) -> Self {
        self.initial_observers = observers.into_iter().map(Into::into).collect();
        self
    }

    /// Sets the spawn provenance carried by a `ProcessStartOptions` for store and durable-substrate
    /// implementors while persisting and coordinating durable process execution.
    pub fn with_spawn_provenance(mut self, spawn_provenance: ProcessSpawnProvenance) -> Self {
        self.spawn_provenance = Some(spawn_provenance);
        self
    }

    /// Exposes execution context to store and durable-substrate implementors while persisting and
    /// coordinating durable process execution.
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
    pub identity: Option<ProcessIdentity>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wake_session_id: Option<SessionId>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub observers: Vec<SessionId>,
    #[serde(default)]
    pub event_types: Vec<ProcessEventType>,
}

impl ProcessStartRequest {
    /// Constructs a `ProcessStartRequest` for store and durable-substrate implementors while
    /// persisting and coordinating durable process execution.
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
            identity: None,
            wake_session_id: None,
            observers: Vec::new(),
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

    /// Sets the env spec carried by a `ProcessStartRequest` for store and durable-substrate
    /// implementors while persisting and coordinating durable process execution.
    pub fn with_env_spec(mut self, env_spec: ProcessExecutionEnvSpec) -> Self {
        self.env_spec = Some(env_spec);
        self
    }

    /// Sets the max attempts carried by a `ProcessStartRequest` for store and durable-substrate
    /// implementors while persisting and coordinating durable process execution.
    pub fn with_max_attempts(mut self, max_attempts: Option<u32>) -> Self {
        self.max_attempts = max_attempts;
        self
    }

    /// Sets the identity carried by a `ProcessStartRequest` for store and durable-substrate
    /// implementors while persisting and coordinating durable process execution.
    pub fn with_identity(mut self, identity: ProcessIdentity) -> Self {
        self.identity = Some(identity);
        self
    }

    /// Sets the wake session id carried by a `ProcessStartRequest` for store and durable-substrate
    /// implementors while persisting and coordinating durable process execution.
    pub fn with_wake_session_id(mut self, wake_session_id: Option<SessionId>) -> Self {
        self.wake_session_id = wake_session_id;
        self
    }

    /// Sets the observers carried by a `ProcessStartRequest` for store and durable-substrate
    /// implementors while persisting and coordinating durable process execution.
    pub fn with_observers(
        mut self,
        observers: impl IntoIterator<Item = impl Into<SessionId>>,
    ) -> Self {
        self.observers = observers.into_iter().map(Into::into).collect();
        self
    }

    /// Sets the event types carried by a `ProcessStartRequest` for store and durable-substrate
    /// implementors while persisting and coordinating durable process execution.
    pub fn with_event_types(
        mut self,
        event_types: impl IntoIterator<Item = ProcessEventType>,
    ) -> Self {
        self.event_types = event_types.into_iter().collect();
        self
    }

    /// Sets the extra event types carried by a `ProcessStartRequest` for store and
    /// durable-substrate implementors while persisting and coordinating durable process execution.
    pub fn with_extra_event_types(
        mut self,
        event_types: impl IntoIterator<Item = ProcessEventType>,
    ) -> Self {
        self.event_types.extend(event_types);
        self
    }

    /// Extracts the registration outcome for store and durable-substrate implementors while
    /// persisting and coordinating durable process execution.
    pub fn into_registration(self, env_ref: Option<ProcessExecutionEnvRef>) -> ProcessRegistration {
        let mut registration = ProcessRegistration::new(
            self.id,
            self.input,
            self.disposition,
            ProcessProvenance::new(self.originator),
        )
        .with_max_attempts(self.max_attempts)
        .with_event_types(self.event_types)
        .with_execution_env_ref(env_ref)
        .with_wake_session_id(self.wake_session_id);
        if let Some(identity) = self.identity {
            registration = registration.with_identity(identity);
        }
        registration
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
    /// Constructs a `ProcessProvenance` for store and process-engine implementors while persisting
    /// and coordinating durable process execution.
    pub fn new(originator: ProcessOriginator) -> Self {
        Self {
            originator,
            caused_by: None,
        }
    }

    /// Constructs a `ProcessProvenance` using host semantics for store and process-engine
    /// implementors while persisting and coordinating durable process execution.
    pub fn host() -> Self {
        Self::new(ProcessOriginator::host())
    }

    /// Constructs a `ProcessProvenance` using session semantics for store and process-engine
    /// implementors while persisting and coordinating durable process execution.
    pub fn session(scope: SessionScope) -> Self {
        Self::new(ProcessOriginator::session(scope))
    }

    /// Sets the caused by carried by a `ProcessProvenance` for store and process-engine
    /// implementors while persisting and coordinating durable process execution.
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
        session_id: SessionId,
    },
}

impl ProcessOriginator {
    /// Constructs a `ProcessOriginator` using host semantics for store and process-engine
    /// implementors while persisting and coordinating durable process execution.
    pub fn host() -> Self {
        Self::Host { scope: None }
    }

    /// Constructs a `ProcessOriginator` using host scoped semantics for store and process-engine
    /// implementors while persisting and coordinating durable process execution.
    pub fn host_scoped(scope: impl Into<String>) -> Self {
        Self::Host {
            scope: Some(scope.into()),
        }
    }

    /// Constructs a `ProcessOriginator` using session semantics for store and process-engine
    /// implementors while persisting and coordinating durable process execution.
    pub fn session(scope: SessionScope) -> Self {
        Self::Session {
            session_id: scope.session_id,
        }
    }

    pub(crate) fn id(&self) -> String {
        match self {
            Self::Host { scope } => scope
                .as_ref()
                .map(|scope| format!("host:{scope}"))
                .unwrap_or_else(|| "host".to_string()),
            Self::Session { session_id } => session_id.clone(),
        }
    }
}

impl SessionScope {
    /// Constructs a `SessionScope` for store, effect-host, and protocol implementors while
    /// materializing, executing, or persisting a session turn.
    pub fn new(session_id: impl Into<String>) -> Self {
        Self {
            session_id: session_id.into(),
            agent_frame_id: None,
        }
    }

    /// Constructs a frame-scoped session identity for process-engine implementors binding work to
    /// one durable agent frame.
    pub fn for_agent_frame(
        session_id: impl Into<String>,
        agent_frame_id: impl Into<crate::AgentFrameId>,
    ) -> Self {
        Self {
            session_id: session_id.into(),
            agent_frame_id: Some(agent_frame_id.into()),
        }
    }

    /// Exposes id to store, effect-host, and protocol implementors while materializing, executing,
    /// or persisting a session turn.
    pub fn id(&self) -> SessionScopeId {
        match self.agent_frame_id.as_deref() {
            Some(frame_id) if !frame_id.is_empty() => {
                SessionScopeId::new(format!("session:{}/frame:{frame_id}", self.session_id))
            }
            _ => SessionScopeId::new(format!("session:{}", self.session_id)),
        }
    }

    /// Lets store, effect-host, and protocol implementors test whether this `SessionScope` is empty
    /// while materializing, executing, or persisting a session turn.
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
    pub wake_session_id: Option<SessionId>,
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
            wake_session_id: self.wake_session_id.clone(),
        }
    }
}

impl ProcessRegistration {
    /// Constructs a `ProcessRegistration` for store and durable-substrate implementors while
    /// persisting and coordinating durable process execution.
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
            wake_session_id: None,
        }
    }

    #[cfg(test)]
    pub(crate) fn session_start_draft(
        id: impl Into<ProcessId>,
        input: ProcessInput,
        disposition: RecoveryDisposition,
    ) -> Self {
        Self::new(id, input, disposition, ProcessProvenance::host())
    }

    /// Sets the process provenance carried by a `ProcessRegistration` for store and
    /// durable-substrate implementors while persisting and coordinating durable process execution.
    pub fn with_process_provenance(mut self, provenance: ProcessProvenance) -> Self {
        self.provenance = provenance;
        self
    }

    /// Sets the max attempts carried by a `ProcessRegistration` for store and durable-substrate
    /// implementors while persisting and coordinating durable process execution.
    pub fn with_max_attempts(mut self, max_attempts: Option<u32>) -> Self {
        self.max_attempts = max_attempts;
        self
    }

    /// Sets the execution env ref carried by a `ProcessRegistration` for store and
    /// durable-substrate implementors while persisting and coordinating durable process execution.
    pub fn with_execution_env_ref(mut self, env_ref: Option<ProcessExecutionEnvRef>) -> Self {
        self.env_ref = env_ref;
        self
    }

    /// Sets the wake session id carried by a `ProcessRegistration` for store and durable-substrate
    /// implementors while persisting and coordinating durable process execution.
    pub fn with_wake_session_id(mut self, wake_session_id: Option<SessionId>) -> Self {
        self.wake_session_id = wake_session_id;
        self
    }

    /// Sets the identity carried by a `ProcessRegistration` for store and durable-substrate
    /// implementors while persisting and coordinating durable process execution.
    pub fn with_identity(mut self, identity: ProcessIdentity) -> Self {
        self.identity = identity;
        self
    }

    /// Sets the event types carried by a `ProcessRegistration` for store and durable-substrate
    /// implementors while persisting and coordinating durable process execution.
    pub fn with_event_types(
        mut self,
        event_types: impl IntoIterator<Item = ProcessEventType>,
    ) -> Self {
        self.event_types = event_types.into_iter().collect();
        self
    }

    /// Sets the extra event types carried by a `ProcessRegistration` for store and
    /// durable-substrate implementors while persisting and coordinating durable process execution.
    pub fn with_extra_event_types(
        mut self,
        event_types: impl IntoIterator<Item = ProcessEventType>,
    ) -> Self {
        self.event_types.extend(event_types);
        self
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProcessStatus {
    #[default]
    Running,
    Waiting,
    Completed,
    Failed,
    Cancelled,
    Abandoned,
}

impl ProcessStatus {
    /// Maps a terminal process outcome to its durable status for process-store implementors;
    /// non-terminal variants remain running.
    pub fn from_terminal(terminal: ProcessTerminalSemantics) -> Self {
        terminal.status
    }

    /// Lets process-store implementors apply retention only to completed, failed, cancelled, or
    /// abandoned rows; running and waiting rows are never terminal.
    pub fn is_terminal(&self) -> bool {
        matches!(
            self,
            Self::Completed | Self::Failed | Self::Cancelled | Self::Abandoned
        )
    }

    /// Exposes label to store and process-engine implementors while persisting and coordinating
    /// durable process execution.
    pub fn label(&self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::Waiting => "waiting",
            Self::Completed => "completed",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
            Self::Abandoned => "abandoned",
        }
    }
}

/// Durable process lifecycle fold. Observer membership and wake subscription
/// are queryable edge state, audited by events but deliberately not projected
/// into this record.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ProcessRecord {
    pub id: ProcessId,
    pub registration_fingerprint: String,
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub outcome: Option<ProcessOutcome>,
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
    /// Exposes key to store and durable-substrate implementors while persisting and coordinating
    /// durable process execution.
    pub fn key(&self) -> &str {
        let WaitKind::Signal { key, .. } = &self.kind;
        key
    }
}

impl ProcessRecord {
    /// Builds a `ProcessRecord` from registration data for store and durable-substrate implementors
    /// while persisting and coordinating durable process execution.
    pub fn from_registration(registration: ProcessRegistration) -> Self {
        Self::from_registration_with_clock(registration, &crate::SystemClock)
    }

    /// Builds a `ProcessRecord` from registration with clock data for store and durable-substrate
    /// implementors while persisting and coordinating durable process execution.
    pub fn from_registration_with_clock(
        registration: ProcessRegistration,
        clock: &dyn crate::Clock,
    ) -> Self {
        let registration = prepare_process_registration(registration)
            .expect("process registration should be valid before record construction");
        let registration_fingerprint =
            super::validation::process_registration_fingerprint(&registration, &[]);
        Self::from_prepared_registration(
            registration,
            registration_fingerprint,
            clock.timestamp_ms(),
        )
    }

    /// Builds a `ProcessRecord` from prepared registration data for store and durable-substrate
    /// implementors while persisting and coordinating durable process execution.
    pub fn from_prepared_registration(
        registration: ProcessRegistration,
        registration_fingerprint: String,
        now_ms: u64,
    ) -> Self {
        Self {
            id: registration.id,
            registration_fingerprint,
            input: registration.input,
            disposition: registration.disposition,
            max_attempts: registration.max_attempts,
            identity: registration.identity,
            event_types: registration.event_types,
            provenance: registration.provenance,
            env_ref: registration.env_ref,
            created_at_ms: now_ms,
            updated_at_ms: now_ms,
            external_ref: None,
            first_started: None,
            abandon_request: None,
            wait: None,
            status: ProcessStatus::Running,
            outcome: None,
        }
    }

    /// Lets process-store implementors gate retention on the folded durable status rather than the
    /// presence of an incidental event.
    pub fn is_terminal(&self) -> bool {
        self.status.is_terminal()
    }

    /// Exposes originator id to store and durable-substrate implementors while persisting and
    /// coordinating durable process execution.
    pub fn originator_id(&self) -> String {
        self.provenance.originator.id()
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
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_present_json_value"
    )]
    pub definition: Option<serde_json::Value>,
}

fn deserialize_present_json_value<'de, D>(
    deserializer: D,
) -> Result<Option<serde_json::Value>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    serde_json::Value::deserialize(deserializer).map(Some)
}

impl ProcessIdentity {
    /// Constructs a `ProcessIdentity` for protocol and process-engine implementors while running a
    /// durable process.
    pub fn new(kind: impl Into<String>) -> Self {
        Self {
            kind: kind.into(),
            label: None,
            definition: None,
        }
    }

    /// Sets the label carried by a `ProcessIdentity` for protocol and process-engine implementors
    /// while running a durable process.
    pub fn with_label(mut self, label: Option<impl Into<String>>) -> Self {
        self.label = label.map(Into::into);
        self
    }

    /// Sets the definition carried by a `ProcessIdentity` for protocol and process-engine
    /// implementors while running a durable process.
    pub fn with_definition(mut self, definition: Option<serde_json::Value>) -> Self {
        self.definition = definition;
        self
    }

    /// Derives stable kind and definition identity for process-engine implementors from the
    /// executable input without executing it.
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
    /// Returns the newly acquired lease to process-store implementors and `None` when another
    /// holder remains busy; the busy holder is not discarded before this projection.
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
    /// Captures the process ID and exact lease token that process-store implementors must present
    /// to complete or release the claimed execution.
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

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProcessHandleSummary {
    #[serde(rename = "__handle__")]
    pub handle_type: String,
    pub id: ProcessId,
    pub process_id: ProcessId,
    pub kind: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub definition: Option<serde_json::Value>,
    pub status: ProcessStatus,
}

impl ProcessHandleSummary {
    /// Constructs a `ProcessHandleSummary` for store and durable-substrate implementors while
    /// persisting and coordinating durable process execution.
    pub fn new(
        process_id: impl Into<ProcessId>,
        identity: ProcessIdentity,
        status: ProcessStatus,
    ) -> Self {
        let process_id = process_id.into();
        Self {
            handle_type: "process".to_string(),
            id: process_id.clone(),
            process_id,
            kind: identity.kind,
            label: identity.label,
            definition: identity.definition,
            status,
        }
    }

    /// Sets the definition carried by a `ProcessHandleSummary` for store and durable-substrate
    /// implementors while persisting and coordinating durable process execution.
    pub fn with_definition(mut self, definition: Option<serde_json::Value>) -> Self {
        self.definition = definition;
        self
    }

    /// Builds a `ProcessHandleSummary` from record data for store and durable-substrate
    /// implementors while persisting and coordinating durable process execution.
    pub fn from_record(record: ProcessRecord) -> Self {
        Self::new(record.id, record.identity, record.status)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProcessCancelSummary {
    pub process_id: ProcessId,
    pub status: ProcessStatus,
}

impl ProcessCancelSummary {
    /// Builds a `ProcessCancelSummary` from record data for store and durable-substrate
    /// implementors while persisting and coordinating durable process execution.
    pub fn from_record(record: ProcessRecord) -> Self {
        Self {
            process_id: record.id,
            status: record.status,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ProcessStatusFilter {
    #[default]
    Running,
    Waiting,
    Completed,
    Failed,
    Cancelled,
    Abandoned,
    Any,
}

impl ProcessStatusFilter {
    /// Exposes label to store and process-engine implementors while persisting and coordinating
    /// durable process execution. Returns `None` when no label is present.
    pub fn label(self) -> Option<&'static str> {
        match self {
            Self::Running => Some("running"),
            Self::Waiting => Some("waiting"),
            Self::Completed => Some("completed"),
            Self::Failed => Some("failed"),
            Self::Cancelled => Some("cancelled"),
            Self::Abandoned => Some("abandoned"),
            Self::Any => None,
        }
    }

    /// Parses the process-status filter for store and protocol implementors, defaults absence to
    /// `running`, and rejects unknown labels.
    pub fn decode(value: Option<&str>) -> Result<Self, String> {
        match value.unwrap_or("running") {
            "running" => Ok(Self::Running),
            "waiting" => Ok(Self::Waiting),
            "completed" => Ok(Self::Completed),
            "failed" => Ok(Self::Failed),
            "cancelled" => Ok(Self::Cancelled),
            "abandoned" => Ok(Self::Abandoned),
            "any" => Ok(Self::Any),
            other => Err(format!(
                "processes.list status must be `running`, `waiting`, `completed`, `failed`, `cancelled`, `abandoned`, or `any`, got `{other}`"
            )),
        }
    }

    /// Requests the live-only store scan for running or waiting filters and the all-row scan for
    /// terminal or `any` filters.
    pub fn list_mode(self) -> ProcessListMode {
        match self {
            Self::Running | Self::Waiting => ProcessListMode::Live,
            Self::Completed | Self::Failed | Self::Cancelled | Self::Abandoned | Self::Any => {
                ProcessListMode::All
            }
        }
    }

    /// Applies exact status matching for process-store implementors, with `any` as the sole
    /// wildcard.
    pub fn matches(self, status: ProcessStatus) -> bool {
        match self {
            Self::Running => status == ProcessStatus::Running,
            Self::Waiting => status == ProcessStatus::Waiting,
            Self::Completed => status == ProcessStatus::Completed,
            Self::Failed => status == ProcessStatus::Failed,
            Self::Cancelled => status == ProcessStatus::Cancelled,
            Self::Abandoned => status == ProcessStatus::Abandoned,
            Self::Any => true,
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct ProcessListFilter {
    pub definition: Option<serde_json::Value>,
    pub status: ProcessStatusFilter,
    pub waiting: Option<bool>,
    pub originator_id: Option<String>,
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
    /// Parses the complete process-list filter for store implementors, rejecting unknown fields and
    /// ill-typed values rather than silently ignoring them.
    pub fn decode(args: &serde_json::Value) -> Result<Self, String> {
        let map = args
            .as_object()
            .ok_or_else(|| "processes.list expects a record of process filters".to_string())?;
        for key in map.keys() {
            match key.as_str() {
                "definition"
                | "status"
                | "waiting"
                | "originator_id"
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
        let originator_id = optional_string_filter(args, "originator_id")?;
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
            originator_id,
            identity_kind,
            identity_label,
            caused_by_occurrence_id,
            caused_by_subscription_id,
            created_at_start_ms,
            created_at_end_ms,
        })
    }

    /// Exposes list mode to store and durable-substrate implementors while persisting and
    /// coordinating durable process execution.
    pub fn list_mode(&self) -> ProcessListMode {
        self.status.list_mode()
    }

    /// Applies every populated process filter conjunctively for store and conformance implementors;
    /// the creation-time bounds form a half-open `[start, end)` range.
    pub fn matches_record(&self, record: &ProcessRecord) -> bool {
        self.status.matches(record.status)
            && self
                .definition
                .as_ref()
                .is_none_or(|definition| record.identity.definition.as_ref() == Some(definition))
            && self
                .waiting
                .is_none_or(|waiting| record.wait.is_some() == waiting)
            && self
                .originator_id
                .as_ref()
                .is_none_or(|originator_id| record.originator_id() == originator_id.as_str())
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
    /// Exposes the stable snake-case list mode for process-store implementors.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Live => "live",
            Self::All => "all",
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProcessSessionDeleteReport {
    pub session_id: String,
    pub removed_observer_count: usize,
    pub discarded_wake_delivery_count: usize,
    pub cleared_subscription_count: usize,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ProcessObserverBy {
    Host { operation_id: String },
    ForkInheritance,
}

impl ProcessObserverBy {
    /// Constructs a `ProcessObserverBy` using host semantics for store and durable-substrate
    /// implementors while persisting and coordinating durable process execution.
    pub fn host(operation_id: impl Into<String>) -> Self {
        Self::Host {
            operation_id: operation_id.into(),
        }
    }

    /// Returns the stable observer-authority component process-store implementors include in
    /// add/remove replay keys; fork inheritance uses one reserved literal.
    pub fn replay_component(&self) -> &str {
        match self {
            Self::Host { operation_id } => operation_id,
            Self::ForkInheritance => "fork_inheritance",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProcessTombstone {
    pub process_id: ProcessId,
    pub terminal_label: String,
    pub pruned_at_ms: u64,
    pub pruned_change_seq: u64,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ProcessChange {
    Upsert { record: Box<ProcessRecord> },
    Deleted { tombstone: ProcessTombstone },
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ObserverInheritance {
    #[default]
    All,
    None,
    Only(Vec<ProcessId>),
}
