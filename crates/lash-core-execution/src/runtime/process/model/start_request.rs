use serde::{Deserialize, Serialize};

use super::super::events::{ProcessEventType, default_process_event_types};
use super::{
    LifetimeDecision, ProcessExecutionEnvRef, ProcessInput, ProcessProvenance, ProcessRegistration,
    RecoveryContract, SessionId,
};

/// A start request as a leaf tool attempt declares it: everything a process
/// start needs except its key.
///
/// The key is not declaration material. It is a pure function of the declaring
/// attempt's intent identity ([`crate::StartKey::for_tool_intent`]), so every
/// redrive of the declaration presents the same key and starts the same
/// process. The process id is minted by the registrar at realization and read
/// back off the recorded result (ADR 0107).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ProcessStartDeclaration {
    pub input: ProcessInput,
    pub disposition: RecoveryContract,
    /// The lifetime the declaring attempt chose from its start context,
    /// journaled with the declaration: realization never re-runs the policy
    /// (FIG-3607 R4b).
    pub lifetime: LifetimeDecision,
    /// `None` delegates pacing indefinitely to the engine; deterministic failures then require
    /// host cancellation or abandonment to resolve awaiters.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_attempts: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub env_spec: Option<super::ProcessExecutionEnvSpec>,
    pub originator: super::ProcessOriginator,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub identity: Option<super::DeclaredProcessIdentity>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wake_session_id: Option<SessionId>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub observers: Vec<SessionId>,
    #[serde(default)]
    pub event_types: Vec<ProcessEventType>,
}

impl ProcessStartDeclaration {
    /// Declare a process start for store and durable-substrate implementors.
    /// The key is absent by construction; realization derives it.
    pub fn new(
        input: ProcessInput,
        disposition: RecoveryContract,
        originator: super::ProcessOriginator,
        lifetime: impl Into<LifetimeDecision>,
    ) -> Self {
        Self {
            input,
            disposition,
            lifetime: lifetime.into(),
            max_attempts: None,
            env_spec: None,
            originator,
            identity: None,
            wake_session_id: None,
            observers: Vec::new(),
            event_types: default_process_event_types(),
        }
    }

    /// External placeholder declaration: `ProcessInput::External` is always
    /// [`RecoveryContract::ExternallyOwned`] — lash never executes it.
    pub fn external(
        originator: super::ProcessOriginator,
        metadata: serde_json::Value,
        lifetime: impl Into<LifetimeDecision>,
    ) -> Self {
        Self::new(
            ProcessInput::External { metadata },
            RecoveryContract::ExternallyOwned,
            originator,
            lifetime,
        )
    }

    pub fn with_env_spec(mut self, env_spec: super::ProcessExecutionEnvSpec) -> Self {
        self.env_spec = Some(env_spec);
        self
    }

    pub fn with_max_attempts(mut self, max_attempts: Option<u32>) -> Self {
        self.max_attempts = max_attempts;
        self
    }

    pub fn with_declared_identity(mut self, declared: super::DeclaredProcessIdentity) -> Self {
        self.identity = Some(declared);
        self
    }

    pub fn with_wake_session_id(mut self, wake_session_id: Option<SessionId>) -> Self {
        self.wake_session_id = wake_session_id;
        self
    }

    pub fn with_observers(
        mut self,
        observers: impl IntoIterator<Item = impl Into<SessionId>>,
    ) -> Self {
        self.observers = observers.into_iter().map(Into::into).collect();
        self
    }

    pub fn with_event_types(
        mut self,
        event_types: impl IntoIterator<Item = ProcessEventType>,
    ) -> Self {
        self.event_types = event_types.into_iter().collect();
        self
    }

    /// Adds event types to those already carried by this declaration.
    pub fn with_extra_event_types(
        mut self,
        event_types: impl IntoIterator<Item = ProcessEventType>,
    ) -> Self {
        self.event_types.extend(event_types);
        self
    }

    /// The only way a declaration becomes a request: every realization route
    /// passes `StartKey::for_tool_intent(identity)` here, so every redrive of
    /// one declaration starts the same process by construction.
    pub fn into_request(self, start_key: crate::StartKey) -> ProcessStartRequest {
        ProcessStartRequest {
            start_key: Some(start_key),
            input: self.input,
            disposition: self.disposition,
            lifetime: self.lifetime,
            max_attempts: self.max_attempts,
            env_spec: self.env_spec,
            originator: self.originator,
            identity: self.identity,
            wake_session_id: self.wake_session_id,
            observers: self.observers,
            event_types: self.event_types,
        }
    }
}

/// Public host-facing request for starting a visible process handle.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ProcessStartRequest {
    /// The start's idempotency key; `None` starts a new process every time.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub start_key: Option<crate::StartKey>,
    pub input: ProcessInput,
    pub disposition: RecoveryContract,
    /// What ends the process. A host start is a root: `Detached`, or `Until`
    /// a session the host looked up.
    pub lifetime: LifetimeDecision,
    /// `None` delegates pacing indefinitely to the engine; deterministic failures then require
    /// host cancellation or abandonment to resolve awaiters.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_attempts: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub env_spec: Option<super::ProcessExecutionEnvSpec>,
    pub originator: super::ProcessOriginator,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub identity: Option<super::DeclaredProcessIdentity>,
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
    ///
    /// The request carries no process id: the registrar mints one. An
    /// idempotent start adds a key with [`Self::with_start_key`].
    pub fn new(
        input: ProcessInput,
        disposition: RecoveryContract,
        originator: super::ProcessOriginator,
        lifetime: impl Into<LifetimeDecision>,
    ) -> Self {
        Self {
            start_key: None,
            input,
            disposition,
            lifetime: lifetime.into(),
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
    /// [`RecoveryContract::ExternallyOwned`] — lash never executes it.
    pub fn external(
        originator: super::ProcessOriginator,
        metadata: serde_json::Value,
        lifetime: impl Into<LifetimeDecision>,
    ) -> Self {
        Self::new(
            ProcessInput::External { metadata },
            RecoveryContract::ExternallyOwned,
            originator,
            lifetime,
        )
    }

    /// Sets the start's idempotency key.
    pub fn with_start_key(mut self, start_key: Option<crate::StartKey>) -> Self {
        self.start_key = start_key;
        self
    }

    /// Keys the start with a host-supplied key, scoped to the request's
    /// originator: the same key from two sessions starts two processes
    /// (ADR 0107).
    #[must_use]
    pub fn with_host_start_key(self, key: impl AsRef<[u8]>) -> Self {
        let start_key = crate::StartKey::for_host(self.originator.start_key_owner(), key);
        self.with_start_key(Some(start_key))
    }

    /// A host start as the host rails realize it under `scope`: the host's
    /// own key makes the start idempotent while its process is retained, and
    /// a keyless start is always new, keyed by the scope and its ordinal
    /// among the run's keyless starts, so a durable handler's replay re-issues
    /// the same key (ADR 0107).
    #[must_use]
    pub fn keyed_in(self, scope: &crate::ScopedEffectController<'_>) -> Self {
        if self.start_key.is_some() {
            self
        } else {
            let start_key = scope.next_keyless_start_key();
            self.with_start_key(Some(start_key))
        }
    }

    /// Sets the env spec carried by a `ProcessStartRequest` for store and durable-substrate
    /// implementors while persisting and coordinating durable process execution.
    pub fn with_env_spec(mut self, env_spec: super::ProcessExecutionEnvSpec) -> Self {
        self.env_spec = Some(env_spec);
        self
    }

    /// Sets the max attempts carried by a `ProcessStartRequest` for store and durable-substrate
    /// implementors while persisting and coordinating durable process execution.
    pub fn with_max_attempts(mut self, max_attempts: Option<u32>) -> Self {
        self.max_attempts = max_attempts;
        self
    }

    /// A request never pins a definition reference: only the engine registry can, and only
    /// after resolving it against the engine's stored artifact.
    pub fn with_declared_identity(mut self, declared: super::DeclaredProcessIdentity) -> Self {
        self.identity = Some(declared);
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

    /// Drops the key a caller happened to carry, leaving the declaration a leaf
    /// attempt records. The key is re-derived from the attempt identity at
    /// realization, never carried across the journal.
    pub fn into_declaration(self) -> ProcessStartDeclaration {
        ProcessStartDeclaration {
            input: self.input,
            disposition: self.disposition,
            lifetime: self.lifetime,
            max_attempts: self.max_attempts,
            env_spec: self.env_spec,
            originator: self.originator,
            identity: self.identity,
            wake_session_id: self.wake_session_id,
            observers: self.observers,
            event_types: self.event_types,
        }
    }

    /// Extracts the registration outcome for store and durable-substrate implementors while
    /// persisting and coordinating durable process execution.
    pub fn into_registration(self, env_ref: Option<ProcessExecutionEnvRef>) -> ProcessRegistration {
        let mut registration = ProcessRegistration::new(
            self.input,
            self.disposition,
            ProcessProvenance::new(self.originator),
            self.lifetime,
        )
        .with_start_key(self.start_key)
        .with_max_attempts(self.max_attempts)
        .with_event_types(self.event_types)
        .with_execution_env_ref(env_ref)
        .with_wake_session_id(self.wake_session_id);
        if let Some(identity) = self.identity {
            registration = registration.with_declared_identity(identity);
        }
        registration
    }
}
