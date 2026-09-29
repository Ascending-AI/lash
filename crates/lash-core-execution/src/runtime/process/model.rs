pub use lash_core_store::process_identity::*;
use lash_sansio::{CancelOrigin, CancelRequest};
use std::collections::BTreeSet;
use std::fmt;
use std::sync::Arc;

use serde::{Deserialize, Serialize};

use super::definition_ref::{ProcessDefinitionRef, ProcessDefinitionValue, ProcessEngineKind};
use super::events::{ProcessAwaitOutput, ProcessEventType, default_process_event_types};
use super::op_scope::ProcessOpScope;
use super::validation::prepare_process_registration;

mod execution;
pub use execution::*;
mod session_ids;
pub use session_ids::*;
mod scope_lifetime;
pub use scope_lifetime::*;
mod start_request;
pub use start_request::*;

pub use lash_sansio::handle::HandleId;
pub use lash_sansio::{ProcessId, SessionId};
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

    pub fn from_store_sequence(sequence: u64) -> Self {
        Self(sequence)
    }

    /// Exposes the opaque sequence to the process-store implementor that issued it; consumers must not compare cursors from different backends.
    pub fn store_sequence(self) -> u64 {
        self.0
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
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
        call: crate::ProcessToolCall,
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
        /// What the runner answers when the child turn finishes.
        result: SessionTurnResult,
    },
    External {
        #[serde(default)]
        metadata: serde_json::Value,
    },
}

/// What a `ProcessInput::SessionTurn` runner answers with when the child's
/// turn ends.
///
/// Failures and cancellations are the child's own under both modes: only a
/// finished turn is projected differently.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum SessionTurnResult {
    /// The runner answers the child's `AssembledTurn`, with the process and
    /// child-session ids beside it.
    Turn,
    /// The runner answers the child's final value, checked against `schema`
    /// when one is given.
    FinalValue {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        schema: Option<serde_json::Value>,
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
                result,
            } => Self::SessionTurn {
                definition_key: definition_key.clone(),
                create_request: create_request.clone(),
                turn_input: turn_input.clone(),
                result: result.clone(),
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

    pub fn engine_specific_kind(&self) -> Option<&str> {
        match self {
            Self::Engine { kind, .. } => Some(kind.as_str()),
            _ => None,
        }
    }

    /// Whether lash never executes a process of this input. An `External`
    /// input names work an actor outside lash runs and closes; every other
    /// input is executed by the effect engine, which owns its recovery
    /// (ADR 0110). The input class is the whole fact: there is no separate
    /// declaration that could contradict it.
    pub fn is_externally_owned(&self) -> bool {
        matches!(self, Self::External { .. })
    }
}

/// The store of process execution environments (ADR 0113 §2.1): immutable
/// bytes under a content-addressed reference, kept alive by referrer edges.
/// Only the cleanup executor severs edges, through
/// [`Self::end_process_env_referrer`].
#[async_trait::async_trait]
pub trait ProcessExecutionEnvStore: Send + Sync {
    /// Store `bytes` under `env_ref` if absent, verify they equal any stored
    /// bytes, and add the claim's edge, in one transaction that first takes
    /// the referrer's lock, checks its fence (`ReferrerEnded`) and arms the
    /// claim's guard if it has one and no row exists.
    async fn publish_process_execution_env(
        &self,
        claim: &crate::ReferrerClaim,
        env_ref: &ProcessExecutionEnvRef,
        bytes: &[u8],
    ) -> Result<(), crate::ArtifactStoreError>;

    /// Add the claim's edge to bytes already stored, with the same lock,
    /// fence check and guard arming. Absent bytes are `ArtifactMissing`.
    async fn acquire_process_execution_env(
        &self,
        claim: &crate::ReferrerClaim,
        env_ref: &ProcessExecutionEnvRef,
    ) -> Result<(), crate::ArtifactStoreError>;

    /// Apply one resolved cleanup in one transaction (ADR 0113 §2.3).
    async fn end_process_env_referrer(
        &self,
        cleanup: &crate::ResolvedArtifactCleanup,
    ) -> Result<(), crate::ArtifactStoreError>;

    async fn get_process_execution_env(
        &self,
        env_ref: &ProcessExecutionEnvRef,
    ) -> Result<Option<Vec<u8>>, crate::ArtifactStoreError>;
}

/// Map a store refusal raised by an artifact write to the session-facing
/// plugin error, keeping the typed refusals as runtime codes rather than
/// prose. Every other store failure stays a session error.
pub fn artifact_store_plugin_error(error: crate::StoreError) -> crate::PluginError {
    crate::ArtifactStoreError::from(error).into()
}

/// The fenced referrer an artifact store refused a publish or acquire under,
/// if `error` is that refusal (ADR 0113 §2.7), however many conversions it
/// has crossed. Every other error, store faults included, is `None`.
pub fn artifact_referrer_ended(error: &crate::PluginError) -> Option<&crate::ArtifactReferrer> {
    match error {
        crate::PluginError::Runtime(error) => error.ended_referrer(),
        crate::PluginError::RuntimeEffectController(error) => error.ended_referrer(),
        _ => None,
    }
}

pub async fn publish_process_execution_env(
    env_store: &dyn ProcessExecutionEnvStore,
    claim: &crate::ReferrerClaim,
    spec: &ProcessExecutionEnvSpec,
) -> Result<ProcessExecutionEnvRef, crate::PluginError> {
    let bytes = spec.to_store_bytes().map_err(|err| {
        crate::PluginError::Session(format!("failed to encode process execution env: {err}"))
    })?;
    let env_ref = process_execution_env_ref_for_bytes(&bytes);
    env_store
        .publish_process_execution_env(claim, &env_ref, &bytes)
        .await?;
    Ok(env_ref)
}

/// Why a recorded process execution environment did not load.
///
/// The halves are distinct because they classify differently (FIG-3575). The
/// store failing to answer — a pool that timed out, a lost connection — is a
/// fact about this attempt, and a retry under a healthy store reads the
/// environment. Every other variant is a fact about what the store durably
/// holds under the reference, which every retry by this build meets again.
#[derive(Debug, thiserror::Error)]
pub enum ProcessExecutionEnvLoadError {
    /// The store did not answer the read; its own error says why.
    #[error(transparent)]
    Store(crate::PluginError),
    /// Nothing is stored under the reference.
    #[error("missing process execution env `{0}`")]
    Missing(ProcessExecutionEnvRef),
    /// The stored bytes are not the ones the reference names under this
    /// build's reference family.
    #[error(
        "unsupported or mismatched process execution env reference `{0}`; recreate the environment"
    )]
    Mismatched(ProcessExecutionEnvRef),
    /// The stored bytes do not decode as this build's environment.
    #[error("failed to decode process execution env `{env_ref}`: {message}")]
    Undecodable {
        env_ref: ProcessExecutionEnvRef,
        message: String,
    },
}

impl From<ProcessExecutionEnvLoadError> for crate::PluginError {
    fn from(error: ProcessExecutionEnvLoadError) -> Self {
        match error {
            ProcessExecutionEnvLoadError::Store(error) => error,
            unresolved => Self::Session(unresolved.to_string()),
        }
    }
}

pub async fn load_process_execution_env(
    env_store: &dyn ProcessExecutionEnvStore,
    env_ref: &ProcessExecutionEnvRef,
) -> Result<ProcessExecutionEnvSpec, ProcessExecutionEnvLoadError> {
    let bytes = env_store
        .get_process_execution_env(env_ref)
        .await
        .map_err(|error| ProcessExecutionEnvLoadError::Store(error.into()))?
        .ok_or_else(|| ProcessExecutionEnvLoadError::Missing(env_ref.clone()))?;
    if process_execution_env_ref_for_bytes(&bytes) != *env_ref {
        return Err(ProcessExecutionEnvLoadError::Mismatched(env_ref.clone()));
    }
    ProcessExecutionEnvSpec::from_store_bytes(&bytes).map_err(|err| {
        ProcessExecutionEnvLoadError::Undecodable {
            env_ref: env_ref.clone(),
            message: err.to_string(),
        }
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
    /// Request-carried environment bytes handed to the replayable start
    /// command. Kept in options so the service contract does not prepublish a
    /// staging edge ahead of its journal.
    pub env_spec: Option<ProcessExecutionEnvSpec>,
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

    pub fn with_env_spec(mut self, env_spec: Option<ProcessExecutionEnvSpec>) -> Self {
        self.env_spec = env_spec;
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

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionScope {
    pub session_id: SessionId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_frame_id: Option<crate::FrameNodeId>,
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
        #[serde(default, skip_serializing_if = "Option::is_none")]
        agent_frame_id: Option<crate::FrameNodeId>,
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
            agent_frame_id: scope.agent_frame_id,
        }
    }

    pub(crate) fn id(&self) -> String {
        match self {
            Self::Host { scope } => scope
                .as_ref()
                .map(|scope| format!("host:{scope}"))
                .unwrap_or_else(|| "host".to_string()),
            Self::Session { session_id, .. } => session_id.to_string(),
        }
    }
}

impl SessionScope {
    pub fn new(session_id: impl Into<SessionId>) -> Self {
        Self {
            session_id: session_id.into(),
            agent_frame_id: None,
        }
    }

    /// Constructs a frame-scoped session identity for process-engine implementors binding work to
    /// one durable agent frame.
    pub fn for_agent_frame(
        session_id: impl Into<SessionId>,
        agent_frame_id: crate::FrameNodeId,
    ) -> Self {
        Self {
            session_id: session_id.into(),
            agent_frame_id: Some(agent_frame_id),
        }
    }

    pub fn id(&self) -> SessionScopeId {
        match self.agent_frame_id.as_deref() {
            Some(frame_id) => {
                SessionScopeId::new(format!("session:{}/frame:{frame_id}", self.session_id))
            }
            None => SessionScopeId::new(format!("session:{}", self.session_id)),
        }
    }

    /// Lets store, effect-host, and protocol implementors test whether this `SessionScope` is empty
    /// while materializing, executing, or persisting a session turn.
    pub fn is_empty(&self) -> bool {
        self.session_id.is_empty()
    }
}

/// Serializable process spec used to start or recover a runtime process.
///
/// Unknown fields are refused: the retired shape named its process with an
/// `id` of its own, and must not decode as a keyless start.
#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProcessRegistration {
    /// The start's idempotency key (ADR 0107). While a process registered
    /// under the same key is retained, registration returns that process
    /// instead of minting another. `None` is a keyless start: always new.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub start_key: Option<StartKey>,
    pub input: Arc<ProcessInput>,
    /// What ends the process: the recorded decision (FIG-3607 R4b).
    pub lifetime: LifetimeDecision,
    /// Where the start came from, nearest first; empty for a root start. A
    /// runtime start's is its admitted start context's, set by
    /// [`Self::with_start_cx`], never by the start's author.
    pub ancestry: Ancestry,
    /// The session the process's descendants may bind to, inherited through
    /// `Until` and `Detached` alike (FIG-3607 R10).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_capability: Option<SessionId>,
    pub identity: ProcessIdentity,
    #[serde(default)]
    pub event_types: Vec<ProcessEventType>,
    pub provenance: ProcessProvenance,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub env_ref: Option<ProcessExecutionEnvRef>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wake_session_id: Option<SessionId>,
    /// The parked call that consumes this process's terminal, when a
    /// declared start registered it (ADR 0116 §3.6). The registrar writes the
    /// hold with the row, and prune leaves a held row alone.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub consumer_hold: Option<ConsumerHold>,
}

impl Clone for ProcessRegistration {
    fn clone(&self) -> Self {
        Self {
            start_key: self.start_key.clone(),
            input: Arc::clone(&self.input),
            lifetime: self.lifetime.clone(),
            ancestry: self.ancestry.clone(),
            session_capability: self.session_capability.clone(),
            identity: self.identity.clone(),
            event_types: self.event_types.clone(),
            provenance: self.provenance.clone(),
            env_ref: self.env_ref.clone(),
            wake_session_id: self.wake_session_id.clone(),
            consumer_hold: self.consumer_hold.clone(),
        }
    }
}

/// A parked call's hold on the process whose terminal it consumes (ADR 0116
/// §3.6).
///
/// A declared start registers its child with the hold, in the registration
/// transaction, so the row cannot be pruned while the call may still redrive
/// its start: a redrive always finds the child under its key, and a start
/// whose receipt was lost can never register a second child. The call
/// releases the hold once its wait has ended; the close of the owning scope
/// releases every hold its calls still own, so an abandoned call leaks none.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConsumerHold {
    /// The consuming call's completion key id.
    pub key: String,
    /// The scope whose close releases the hold: the opener the call ran
    /// under.
    pub owner: ScopeId,
    /// Whether the call owes the process a cancel when it is abandoned
    /// (`CancelHint::CancelExternalWork`): an opener that cancels the call
    /// cancels the process it holds (ADR 0116 §3.4).
    #[serde(default)]
    pub cancels: bool,
}

impl ProcessRegistration {
    /// Constructs a `ProcessRegistration` for store and durable-substrate implementors while
    /// persisting and coordinating durable process execution.
    ///
    /// The registration carries no process id: the registrar mints one. A
    /// start that must be idempotent adds its key with [`Self::with_start_key`].
    pub fn new(
        input: ProcessInput,
        provenance: ProcessProvenance,
        lifetime: impl Into<LifetimeDecision>,
    ) -> Self {
        let identity = ProcessIdentity::from_process_input(&input);
        let lifetime = lifetime.into();
        // A root holds a session only through the host's lookup grant.
        let session_capability = match &lifetime {
            LifetimeDecision::Until {
                scope: ScopeId::Session(session_id),
                grant: ScopeGrant::HostSessionLookup,
            } => Some(session_id.clone()),
            LifetimeDecision::Until { .. } | LifetimeDecision::Detached => None,
        };
        Self {
            start_key: None,
            input: Arc::new(input),
            lifetime,
            ancestry: Ancestry::root(),
            session_capability,
            identity,
            event_types: default_process_event_types(),
            provenance,
            env_ref: None,
            wake_session_id: None,
            consumer_hold: None,
        }
    }

    /// The scopes whose close refuses this start (FIG-3607 R11, FIG-3948):
    /// its starter, the scope its lifetime names, and the session each of
    /// them lies inside, deduplicated in ledger-key order — the order a
    /// backend that locks each scope must take the locks in.
    ///
    /// The enclosing session is what fences a turn that never became a
    /// root: no root close ever records such a turn's row, and its session's
    /// close is the fact that it can no longer become one.
    #[must_use]
    pub fn closing_scopes(&self) -> Vec<ScopeId> {
        let mut scopes: Vec<ScopeId> = self
            .ancestry
            .starter()
            .into_iter()
            .chain(self.lifetime.scope())
            .flat_map(|scope| std::iter::once(scope.clone()).chain(scope.enclosing_session()))
            .collect();
        scopes.sort_by_key(|scope| (scope.storage_kind(), scope.storage_id()));
        scopes.dedup();
        scopes
    }

    /// Sets the start's idempotency key.
    pub fn with_start_key(mut self, start_key: Option<StartKey>) -> Self {
        self.start_key = start_key;
        self
    }

    /// The lineage this process's body starts children under (FIG-3607 R1,
    /// R10): the process, its own session when it runs one, then its recorded
    /// ancestry and session capability.
    pub fn lineage(&self, process_id: &ProcessId) -> ProcessLineage {
        recorded_lineage(
            process_id,
            &self.input,
            &self.ancestry,
            self.session_capability.as_ref(),
        )
    }

    /// Records the admitted start context a runtime start was made in: its
    /// ancestry and the session capability its descendants inherit. Only the
    /// runtime's realization calls this, with the context it materialized
    /// from the admitted scope; registration then checks the recorded
    /// lifetime against it (FIG-3607 R3).
    pub fn with_start_cx(mut self, cx: &StartCx) -> Self {
        self.ancestry = cx.ancestry();
        self.session_capability = cx.session_capability();
        self
    }

    /// How a refusal names this start: a registration carries no process id
    /// until the registrar mints one, so it is named by its key when it has
    /// one (ADR 0107).
    pub fn refusal_name(&self) -> String {
        self.start_key
            .as_ref()
            .map_or_else(|| "keyless start".to_string(), |key| format!("start {key}"))
    }

    /// Sets the process provenance carried by a `ProcessRegistration` for store and
    /// durable-substrate implementors while persisting and coordinating durable process execution.
    pub fn with_process_provenance(mut self, provenance: ProcessProvenance) -> Self {
        self.provenance = provenance;
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

    /// Registers the process under a parked call's hold (ADR 0116 §3.6).
    pub fn with_consumer_hold(mut self, consumer_hold: Option<ConsumerHold>) -> Self {
        self.consumer_hold = consumer_hold;
        self
    }

    /// Adopts the kind and label a host declared for this start.
    pub fn with_declared_identity(mut self, declared: DeclaredProcessIdentity) -> Self {
        self.identity = declared.into_identity();
        self
    }

    /// Adopts an identity the engine registry admitted, together with the
    /// signal event types the engine resolved for it.
    ///
    /// This is the only way a registration's derived identity is replaced, and
    /// [`AdmittedProcessIdentity`](crate::AdmittedProcessIdentity) is the only
    /// carrier that can hold a definition reference.
    ///
    /// It replaces the label too. A label already on the registration is not
    /// evidence that a host declared one: every derivation route puts a label
    /// here (`ProcessIdentity::from_process_input`), and a fixture or a caller
    /// may stamp an admitted identity more than once. Reading the label back
    /// off the registration to decide whether to keep it therefore lets the
    /// *first* stamp mask the second, which is how #1543 turned the
    /// `list_processes_filters_by_enriched_fields` conformance law red. A
    /// host-declared label is restored after admission, from the declaration
    /// that carried it, by [`Self::with_host_facing_label`].
    pub fn with_admitted_identity(mut self, admitted: crate::AdmittedProcessIdentity) -> Self {
        let (identity, signals) = admitted.into_parts();
        self.identity = identity;
        for signal in signals {
            if !self.event_types.contains(&signal) {
                self.event_types.push(signal);
            }
        }
        self
    }

    /// Restores the host-facing label a start *declared*, over the one the
    /// engine derived (FIG-3122).
    ///
    /// A label is display metadata, never an identity input, so this moves
    /// nothing else: admission stays the sole writer of the kind and of the
    /// definition reference only the engine can resolve. `None` — a start that
    /// declared no label — keeps the engine's derived label, which for a
    /// scripted-program engine is the lift digest.
    ///
    /// The declaration is the only authority for "the host declared this
    /// label", which is why the value is passed in rather than read back off
    /// the registration: by the time admission has run, a derived label and a
    /// declared one are indistinguishable on the row.
    pub fn with_host_facing_label(mut self, label: Option<String>) -> Self {
        if let Some(label) = label {
            self.identity.label = Some(label);
        }
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

/// Whether a durable write landed on this call, or coalesced onto a fact the
/// store already held under the same durable key.
///
/// Every identity-bearing store write in this runtime is idempotent under its
/// own durable key: a re-presented start, event append, signal, cancellation
/// request or trigger occurrence returns the recorded fact instead of writing a
/// second one. The store is the only layer that knows which of the two
/// happened, and callers that report replay to a host -- the tool-intent
/// ingress above all -- cannot infer it from a successful `Ok` (FIG-3070).
///
/// This is a store verdict, never a journal verdict: an effect journal that
/// replays a recorded outcome never reaches the store at all, so a caller
/// reporting "was this a replay?" folds both together.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StoreRealization {
    /// This call wrote the durable fact.
    #[default]
    Realized,
    /// The store already held this fact under the same durable key; this call
    /// wrote nothing and returned what was already there.
    Coalesced,
}

impl StoreRealization {
    /// Whether the store coalesced this call onto an already-recorded fact.
    pub fn is_coalesced(self) -> bool {
        matches!(self, Self::Coalesced)
    }

    /// Whether this call wrote the durable fact.
    ///
    /// Named for serde's `skip_serializing_if`, which keeps the common arm off
    /// the wire so adding this field leaves existing encodings byte-identical.
    pub fn is_realized(&self) -> bool {
        matches!(self, Self::Realized)
    }

    /// The verdict for a call that wrote when `wrote` and coalesced otherwise.
    pub fn from_wrote(wrote: bool) -> Self {
        if wrote {
            Self::Realized
        } else {
            Self::Coalesced
        }
    }
}

/// What a registration call did to the registry.
///
/// Registration is idempotent by fingerprint on every backend: an exact repeat
/// returns the recorded row instead of failing. `Created` therefore says
/// something a successful `Ok` does not — that this call, and no earlier one,
/// put the row there.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProcessRegistrationDisposition {
    /// This call inserted the row.
    Created,
    /// A row with this registration's fingerprint was already recorded; the
    /// returned record is that row, untouched.
    Existing,
}

/// A registered process record together with [`ProcessRegistrationDisposition`].
#[derive(Clone, Debug)]
pub struct ProcessRegistrationOutcome {
    /// The registered record, newly created or already recorded.
    pub record: ProcessRecord,
    pub disposition: ProcessRegistrationDisposition,
}

impl ProcessRegistrationOutcome {
    /// A record this call inserted.
    pub fn created(record: ProcessRecord) -> Self {
        Self {
            record,
            disposition: ProcessRegistrationDisposition::Created,
        }
    }

    /// A record that was already recorded before this call.
    pub fn existing(record: ProcessRecord) -> Self {
        Self {
            record,
            disposition: ProcessRegistrationDisposition::Existing,
        }
    }

    pub fn is_created(&self) -> bool {
        self.disposition == ProcessRegistrationDisposition::Created
    }
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
        match &self.kind {
            WaitKind::Signal { key, .. } => key,
        }
    }
}

/// The kind and label a host declares for a start whose input core owns
/// outright — a session turn, a tool call, an external placeholder.
///
/// A declaration carries no definition reference, by construction: only the
/// engine registry can put one on a durable row, and only after resolving it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeclaredProcessIdentity {
    pub kind: ProcessEngineKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
}

impl DeclaredProcessIdentity {
    pub fn new(kind: impl Into<ProcessEngineKind>) -> Self {
        Self {
            kind: kind.into(),
            label: None,
        }
    }

    pub fn labelled(kind: impl Into<ProcessEngineKind>, label: Option<impl Into<String>>) -> Self {
        Self {
            kind: kind.into(),
            label: label.map(Into::into),
        }
    }

    /// Widens the declaration to the durable identity shape, with no definition.
    pub fn into_identity(self) -> ProcessIdentity {
        ProcessIdentity::labelled(self.kind, self.label)
    }
}

/// Canonical process identity stored alongside every durable process row.
///
/// `ProcessInput::Engine` keeps its payload opaque to core. Identity is a pure
/// derivation: the non-engine input kinds derive it from the recorded input
/// itself, and an engine start derives it through the engine registry's
/// admission, which is the only thing that can name a definition reference.
/// There are no setters — a durable row's identity is never edited into place.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct ProcessIdentity {
    pub kind: ProcessEngineKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    /// The definition reference this row pins, for the engine starts that have
    /// one. It is the whole reference, not a bare blob: the engine kind that
    /// owns the definition, the definition value, and the signature claimed for
    /// it when the row was created.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub definition: Option<ProcessDefinitionRef>,
}

/// Reads a durable identity, including one written before the definition
/// reference was typed.
///
/// A pre-FIG-2992 row stored the definition as a bare engine-owned value with
/// no engine kind and no signature beside it. Such a row still names exactly
/// one definition, and the engine that owns it is the row's own `kind`, so it
/// reads back as an unclaimed reference to that engine's definition — no row is
/// unreadable, and no legacy row is credited with a signature it never carried.
impl<'de> Deserialize<'de> for ProcessIdentity {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        #[derive(Deserialize)]
        struct StoredIdentity {
            kind: ProcessEngineKind,
            #[serde(default)]
            label: Option<String>,
            #[serde(default)]
            definition: Option<serde_json::Value>,
        }

        let StoredIdentity {
            kind,
            label,
            definition,
        } = StoredIdentity::deserialize(deserializer)?;
        let definition = match definition {
            None | Some(serde_json::Value::Null) => None,
            Some(stored) => Some(
                match serde_json::from_value::<ProcessDefinitionRef>(stored.clone()) {
                    Ok(reference) => reference,
                    Err(_) => ProcessDefinitionRef::unclaimed(kind.clone(), stored),
                },
            ),
        };
        Ok(Self {
            kind,
            label,
            definition,
        })
    }
}

impl ProcessIdentity {
    /// Constructs a `ProcessIdentity` naming only an engine kind, for protocol and process-engine
    /// implementors while running a durable process.
    pub fn new(kind: impl Into<ProcessEngineKind>) -> Self {
        Self {
            kind: kind.into(),
            label: None,
            definition: None,
        }
    }

    /// Constructs a labelled `ProcessIdentity` for protocol and process-engine implementors while
    /// running a durable process.
    pub fn labelled(kind: impl Into<ProcessEngineKind>, label: Option<impl Into<String>>) -> Self {
        Self {
            kind: kind.into(),
            label: label.map(Into::into),
            definition: None,
        }
    }

    /// The engine kind is taken from the reference, so the two can never drift.
    pub fn for_definition(
        reference: ProcessDefinitionRef,
        label: Option<impl Into<String>>,
    ) -> Self {
        Self {
            kind: reference.engine_kind.clone(),
            label: label.map(Into::into),
            definition: Some(reference),
        }
    }

    /// An engine input derives only its kind here: its label and definition
    /// reference come from the engine registry's admission, which is the only
    /// authority over an opaque engine payload.
    pub fn from_process_input(input: &ProcessInput) -> Self {
        match input {
            ProcessInput::ToolCall { call } => Self::labelled("tool", Some(call.tool_name.clone())),
            ProcessInput::Engine { kind, .. } => Self::new(kind.clone()),
            ProcessInput::SessionTurn { create_request, .. } => {
                let label = create_request
                    .subagent
                    .as_ref()
                    .map(|subagent| subagent.capability.clone())
                    .or_else(|| create_request.session_id.clone().map(Into::into));
                Self::labelled("session_turn", label)
            }
            ProcessInput::External { metadata } => {
                let label = metadata
                    .get("label")
                    .or_else(|| metadata.get("name"))
                    .or_else(|| metadata.get("title"))
                    .and_then(serde_json::Value::as_str)
                    .map(str::to_string);
                Self::labelled("external", label)
            }
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
    /// Execution segment this reference was minted for.
    ///
    /// A run that hands over to a successor is a new segment with its own
    /// backend identity, and the live host and the recovery pass can both
    /// submit one. The reference is therefore written compare-and-set on this
    /// ordinal: an absent or lower ordinal never displaces a higher one, so a
    /// slow writer for an earlier segment cannot overwrite the owner a later
    /// segment already recorded. `None` reads as segment zero: it is what a
    /// writer that predates segmented references wrote, and every such writer
    /// only ever minted the first segment's reference.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub segment_ordinal: Option<u64>,
}

impl ProcessExternalRef {
    /// The segment this reference belongs to, reading an absent ordinal as zero.
    pub fn segment_ordinal(&self) -> u64 {
        self.segment_ordinal.unwrap_or(0)
    }

    pub fn supersedes(&self, existing: &Self) -> bool {
        self.segment_ordinal() > existing.segment_ordinal()
    }
}

/// A process, as the holder of a handle to it sees it.
///
/// The view is the one handle record (ADR 0095) plus the summary fields a
/// holder is allowed to read. `id` is the opaque [`HandleId`], the marker field
/// is a constant, and `process_id` is the part that id carries, filled in only
/// by [`ProcessHandleView::new`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[expect(
    clippy::manual_non_exhaustive,
    reason = "the private unit field is the serialized ADR 0095 marker, not a non-exhaustive guard; `#[non_exhaustive]` would drop the `__handle__` field from the wire"
)]
pub struct ProcessHandleView {
    #[serde(rename = "__handle__", with = "handle_kind_field")]
    handle_kind: (),
    pub id: HandleId,
    pub process_id: ProcessId,
    pub kind: ProcessEngineKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub definition: Option<ProcessDefinitionRef>,
    pub status: ProcessStatus,
}

/// Writes the handle marker field as the one kind, and refuses any other.
///
/// Serializing a constant rather than a carried string is what stops a view
/// built here, or decoded from a peer, from claiming to be some other kind of
/// handle.
mod handle_kind_field {
    pub fn serialize<S: serde::Serializer>(_: &(), serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(lash_sansio::handle::HANDLE_KIND)
    }

    pub fn deserialize<'de, D: serde::Deserializer<'de>>(deserializer: D) -> Result<(), D::Error> {
        use serde::Deserialize as _;
        let kind = String::deserialize(deserializer)?;
        if kind == lash_sansio::handle::HANDLE_KIND {
            return Ok(());
        }
        Err(serde::de::Error::invalid_value(
            serde::de::Unexpected::Str(&kind),
            &lash_sansio::handle::HANDLE_KIND,
        ))
    }
}

impl ProcessHandleView {
    /// Constructs a `ProcessHandleView` for store and durable-substrate implementors while
    /// persisting and coordinating durable process execution.
    ///
    /// This is the only way to build one, so the id and the parts it reports can
    /// never disagree.
    pub fn new(process_id: ProcessId, identity: ProcessIdentity, status: ProcessStatus) -> Self {
        Self {
            handle_kind: (),
            id: HandleId::process(&process_id),
            process_id,
            kind: identity.kind,
            label: identity.label,
            definition: identity.definition,
            status,
        }
    }

    /// Sets the definition carried by a `ProcessHandleView` for store and durable-substrate
    /// implementors while persisting and coordinating durable process execution.
    pub fn with_definition(mut self, definition: Option<ProcessDefinitionRef>) -> Self {
        self.definition = definition;
        self
    }

    /// Builds a `ProcessHandleView` from record data for store and durable-substrate
    /// implementors while persisting and coordinating durable process execution.
    pub fn from_record(record: ProcessRecord) -> Self {
        Self::new(record.id, record.identity, record.status)
    }
}

/// What a process start answers: the process it names, the key that made
/// the start idempotent, and whether this start created the process or found
/// it already registered under that key.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProcessStartReceipt {
    /// The minted id of the started process.
    pub process_id: ProcessId,
    /// The key the process is registered under, when the start carried one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub start_key: Option<StartKey>,
    /// Whether this start created the process.
    pub disposition: ProcessRegistrationDisposition,
}

impl ProcessStartReceipt {
    /// The receipt of a start that registered `record` with `disposition`.
    pub fn of(record: &ProcessRecord, disposition: ProcessRegistrationDisposition) -> Self {
        Self {
            process_id: record.id.clone(),
            start_key: record.start_key.clone(),
            disposition,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProcessCancelReceipt {
    pub process_id: ProcessId,
    pub status: ProcessStatus,
    pub origin: CancelOrigin,
}

impl ProcessCancelReceipt {
    /// Builds a `ProcessCancelReceipt` from record data for store and durable-substrate
    /// implementors while persisting and coordinating durable process execution.
    pub fn from_record(record: ProcessRecord) -> Result<Self, crate::PluginError> {
        let request = record.cancel_request.ok_or_else(|| {
            crate::PluginError::Session(format!(
                "process `{}` has no cancellation request",
                record.id
            ))
        })?;
        Ok(Self {
            process_id: record.id,
            status: record.status,
            origin: request.origin,
        })
    }
}

/// Any-of selection over the closed process lifecycle vocabulary.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProcessStatusFilter {
    Any,
    In(BTreeSet<ProcessStatus>),
}

impl Default for ProcessStatusFilter {
    fn default() -> Self {
        Self::In(BTreeSet::from([ProcessStatus::Running]))
    }
}

impl ProcessStatusFilter {
    pub fn any_of(statuses: impl IntoIterator<Item = ProcessStatus>) -> Self {
        Self::In(statuses.into_iter().collect())
    }
    pub fn labels(&self) -> Option<Vec<&'static str>> {
        match self {
            Self::Any => None,
            Self::In(statuses) => Some(statuses.iter().map(ProcessStatus::label).collect()),
        }
    }
    pub fn decode(value: Option<&serde_json::Value>) -> Result<Self, String> {
        value
            .map(|value| {
                serde_json::from_value(value.clone())
                    .map_err(|error| format!("processes.list invalid status set: {error}"))
            })
            .unwrap_or_else(|| Ok(Self::default()))
    }
    /// Selects the live scan only when every selected status is live.
    pub fn list_mode(&self) -> ProcessListMode {
        match self {
            Self::In(statuses) if statuses.iter().all(|status| !status.is_retired()) => {
                ProcessListMode::Live
            }
            Self::In(_) | Self::Any => ProcessListMode::All,
        }
    }
    pub fn matches(&self, status: ProcessStatus) -> bool {
        match self {
            Self::Any => true,
            Self::In(statuses) => statuses.contains(&status),
        }
    }
}

mod list_filter;
pub use list_filter::{ProcessListFilter, ProcessOriginatorFilter};

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
pub struct ProcessSessionDeleteReport {
    pub session_id: SessionId,
    pub removed_observer_count: usize,
    pub discarded_wake_delivery_count: usize,
    pub cleared_subscription_count: usize,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ProcessObserverBy {
    Host { operation_id: String },
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
    /// add/remove replay keys.
    pub fn replay_component(&self) -> &str {
        match self {
            Self::Host { operation_id } => operation_id,
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

/// Durable process lifecycle fold. Observer membership and wake subscription
/// are queryable edge state, audited by events but deliberately not projected
/// into this record.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ProcessRecord {
    /// The minted id: the process's only identity, never reused (ADR 0107).
    pub id: ProcessId,
    /// The key the process was started under, while it is retained.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub start_key: Option<StartKey>,
    /// Sequence of the newest event folded into this record. Registration
    /// starts at zero; every event append advances the value in the same
    /// transaction that persists the event and projected record.
    pub last_event_sequence: u64,
    pub input: Arc<ProcessInput>,
    /// The recorded lifetime decision: never updated after registration.
    pub lifetime: LifetimeDecision,
    /// The recorded ancestry, nearest first; empty for a root.
    pub ancestry: Ancestry,
    /// The session capability descendants inherit.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_capability: Option<SessionId>,
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
    /// Durable execution-started fact (ADR 0110). `None` until a
    /// runner records it immediately before executing. Boxed so these
    /// usually-absent facts do not enlarge the pervasive `ProcessRecord` that
    /// flows through the runtime; serde treats `Option<Box<T>>` identically to
    /// `Option<T>`, so the persisted JSON is unchanged.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub first_started: Option<Box<ProcessStarted>>,
    /// The first accepted cancellation request, retained across retries.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cancel_request: Option<Box<CancelRequest>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wait: Option<WaitState>,
    /// The park the process is in, while its body refuses to replay its
    /// journal (FIG-3586, FIG-3659 NOW-B). Folded from `process.parked`
    /// facts and cleared by the first lifecycle fact past the refusal; never
    /// set on a terminal record. Boxed for the same reason as the other
    /// usually-absent facts.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub park: Option<Box<crate::store::ProcessPark>>,
    #[serde(default)]
    pub status: ProcessStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub outcome: Option<ProcessOutcome>,
}
/// The lineage a process's body starts children under, from the facts its
/// row records: the process, the session it runs of its own (a `SessionTurn`
/// child session), its ancestry and its session capability.
fn recorded_lineage(
    process_id: &ProcessId,
    input: &ProcessInput,
    ancestry: &Ancestry,
    session_capability: Option<&SessionId>,
) -> ProcessLineage {
    let own_session = match input {
        ProcessInput::SessionTurn { create_request, .. } => Some(
            create_request
                .session_id
                .clone()
                .unwrap_or_else(|| process_child_session_id(process_id)),
        ),
        ProcessInput::ToolCall { .. }
        | ProcessInput::Engine { .. }
        | ProcessInput::External { .. } => None,
    };
    ProcessLineage::of_process(
        process_id,
        ancestry,
        session_capability,
        own_session.as_ref(),
    )
}

impl ProcessRecord {
    /// The lineage this process's body starts children under, read back from
    /// its row (FIG-3607 R1).
    pub fn lineage(&self) -> ProcessLineage {
        recorded_lineage(
            &self.id,
            &self.input,
            &self.ancestry,
            self.session_capability.as_ref(),
        )
    }

    /// Builds the record of a process the registrar just minted `id` for.
    pub fn from_registration(registration: ProcessRegistration, id: ProcessId) -> Self {
        Self::from_registration_with_clock(registration, id, &crate::SystemClock)
    }

    /// Builds the record of a process the registrar just minted `id` for, at
    /// the clock's time.
    ///
    /// Panics when the registration is invalid, so callers that accept
    /// host-supplied registrations validate them with
    /// `prepare_process_registration` first.
    #[expect(clippy::expect_used, reason = "callers validate first")]
    pub fn from_registration_with_clock(
        registration: ProcessRegistration,
        id: ProcessId,
        clock: &dyn crate::Clock,
    ) -> Self {
        let registration = prepare_process_registration(registration)
            .expect("process registration should be valid before record construction");
        Self::from_prepared_registration(registration, id, clock.timestamp_ms())
    }

    /// Builds the record of a prepared registration under its minted `id`.
    pub fn from_prepared_registration(
        registration: ProcessRegistration,
        id: ProcessId,
        now_ms: u64,
    ) -> Self {
        Self {
            id,
            start_key: registration.start_key,
            last_event_sequence: 0,
            input: registration.input,
            lifetime: registration.lifetime,
            ancestry: registration.ancestry,
            session_capability: registration.session_capability,
            identity: registration.identity,
            event_types: registration.event_types,
            provenance: registration.provenance,
            env_ref: registration.env_ref,
            created_at_ms: now_ms,
            updated_at_ms: now_ms,
            external_ref: None,
            first_started: None,
            cancel_request: None,
            wait: None,
            park: None,
            status: ProcessStatus::Running,
            outcome: None,
        }
    }

    /// Lets process-store implementors gate retention on the folded durable status rather than the
    /// presence of an incidental event.
    pub fn is_terminal(&self) -> bool {
        self.status.is_terminal()
    }

    /// The key this process's park projection and park feed name it by. The
    /// one place a process record is mapped onto
    /// [`ProcessParkKey`](crate::store::ProcessParkKey).
    pub fn park_key(&self) -> crate::store::ProcessParkKey {
        self.id.clone()
    }

    /// Whether the process is parked and its latest run refused: the only
    /// state in which a recovery rerun is exempt from the attempt budget.
    pub fn is_refusing_park(&self) -> bool {
        self.park.as_deref().is_some_and(|park| park.refusing)
    }

    /// Exposes originator id to store and durable-substrate implementors while persisting and
    /// coordinating durable process execution.
    pub fn originator_id(&self) -> String {
        self.provenance.originator.id()
    }
}
