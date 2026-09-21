use serde::{Deserialize, Serialize};

use super::super::events::{ProcessEventType, default_process_event_types};
use super::{
    ProcessExecutionEnvRef, ProcessId, ProcessIncarnation, ProcessInput, ProcessProvenance,
    ProcessRegistration, RecoveryContract, SessionId,
};

/// The host-selected action when a process's parent scope ends.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum OnParentEnd {
    Abandon,
    Cancel,
}

impl OnParentEnd {
    /// Storage discriminant, as written to the `on_parent_end` column.
    pub fn storage_label(self) -> &'static str {
        match self {
            Self::Abandon => "abandon",
            Self::Cancel => "cancel",
        }
    }
}

/// Durable scope whose end controls a child's lifecycle.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ParentScope {
    Turn {
        session_id: SessionId,
        turn_id: crate::TurnId,
    },
    Process {
        process_id: ProcessId,
        incarnation: ProcessIncarnation,
    },
    Host,
}

impl ParentScope {
    /// Storage discriminant for the scope, as written to `parent_scope_kind`.
    pub fn storage_kind(&self) -> &'static str {
        match self {
            Self::Turn { .. } => "turn",
            Self::Process { .. } => "process",
            Self::Host => "host",
        }
    }

    /// Storage identity for the scope, as written to `parent_scope_id`.
    ///
    /// This rendering and [`Self::from_storage`] are the only codec for the
    /// column: a turn is `<session_id>/<turn_id>` and a process is
    /// `<process_id>#<incarnation>`, so a scope stays comparable by equality
    /// across every tier and an index on the pair serves a parent-scope
    /// query directly. `Host` has no identity; the column is `NULL`, which
    /// the storage check constraint ties to the kind.
    pub fn storage_id(&self) -> Option<String> {
        match self {
            Self::Turn {
                session_id,
                turn_id,
            } => Some(format!("{session_id}/{turn_id}")),
            Self::Process {
                process_id,
                incarnation,
            } => Some(format!("{process_id}#{incarnation}")),
            Self::Host => None,
        }
    }

    /// The lifecycle parent one owner identity maps to.
    ///
    /// [`crate::EffectOpener`] is the one owner vocabulary (ADR 0099 §1) —
    /// derived once, at admission, by [`crate::EffectOpener::for_scope`] from
    /// the admitted scope plus its pinned `ProcessRef` — and this is the only
    /// projection of that vocabulary into a `ParentScope`.
    ///
    /// A `QueueDrain` opener is a durable owner in exactly §1's sense, but it
    /// cannot yet parent a process: a drain has no end protocol, so there is
    /// no moment a child's `OnParentEnd` could fire. Until FIG-3419 lands that
    /// protocol, drain-owned children take `Host`.
    pub fn from_owner(opener: &crate::EffectOpener) -> Self {
        match opener {
            crate::EffectOpener::Turn {
                session_id,
                turn_id,
            } => Self::Turn {
                session_id: session_id.clone(),
                turn_id: turn_id.clone(),
            },
            crate::EffectOpener::Process { process_ref } => Self::Process {
                process_id: process_ref.process_id.clone(),
                incarnation: process_ref.incarnation,
            },
            // A drain is the logical owner, but parenting on it needs the
            // drain-end protocol FIG-3419 owns; until then its children attach
            // to the host.
            crate::EffectOpener::QueueDrain { .. } => Self::Host,
        }
    }

    /// Rebuilds a scope from the two stored columns.
    ///
    /// Returns `None` for any pair the storage check constraint forbids: an
    /// unknown kind, a `host` carrying an id, a non-`host` missing one, or an
    /// id whose separator or incarnation does not parse.
    pub fn from_storage(kind: &str, id: Option<&str>) -> Option<Self> {
        match (kind, id) {
            ("host", None) => Some(Self::Host),
            ("turn", Some(id)) => {
                let (session_id, turn_id) = id.split_once('/')?;
                (!session_id.is_empty() && !turn_id.is_empty()).then(|| Self::Turn {
                    session_id: SessionId::from(session_id.to_string()),
                    turn_id: crate::TurnId::from(turn_id.to_string()),
                })
            }
            ("process", Some(id)) => {
                let (process_id, incarnation) = id.rsplit_once('#')?;
                let incarnation = incarnation.parse::<u64>().ok()?;
                (!process_id.is_empty()).then(|| Self::Process {
                    process_id: ProcessId::from(process_id.to_string()),
                    incarnation: ProcessIncarnation::from_registration_sequence(incarnation),
                })
            }
            _ => None,
        }
    }
}

/// Required lifecycle facts selected by the process's author or host.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ProcessLifecyclePolicy {
    pub parent: ParentScope,
    pub on_parent_end: OnParentEnd,
}

impl ProcessLifecyclePolicy {
    pub fn new(parent: ParentScope, on_parent_end: OnParentEnd) -> Self {
        Self {
            parent,
            on_parent_end,
        }
    }
}

/// A start request as a leaf tool attempt declares it: everything a process
/// start needs except its id.
///
/// The id is not declaration material. It is a pure function of the declaring
/// attempt's intent identity ([`ProcessId::from_intent_identity`]), so the
/// attempt can name the child it is starting before the start commits, and the
/// executor derives the same id on every redrive. Carrying an id here instead
/// would hash a field the executor overwrites into the submission payload,
/// which is what tripped the duplicate-identity check on redrive (FIG-2994).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ProcessStartDeclaration {
    pub input: ProcessInput,
    pub disposition: RecoveryContract,
    pub lifecycle: ProcessLifecyclePolicy,
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
    /// The id is absent by construction; realization derives it.
    pub fn new(
        input: ProcessInput,
        disposition: RecoveryContract,
        originator: super::ProcessOriginator,
        lifecycle: ProcessLifecyclePolicy,
    ) -> Self {
        Self {
            input,
            disposition,
            lifecycle,
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
        lifecycle: ProcessLifecyclePolicy,
    ) -> Self {
        Self::new(
            ProcessInput::External { metadata },
            RecoveryContract::ExternallyOwned,
            originator,
            lifecycle,
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
    /// passes `ProcessId::from_intent_identity(identity)` here, so the id the
    /// attempt returned and the id the executor starts are the same value by
    /// construction rather than by convention.
    pub fn into_request(self, id: ProcessId) -> ProcessStartRequest {
        ProcessStartRequest {
            id,
            input: self.input,
            disposition: self.disposition,
            lifecycle: self.lifecycle,
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
    pub id: ProcessId,
    pub input: ProcessInput,
    pub disposition: RecoveryContract,
    pub lifecycle: ProcessLifecyclePolicy,
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
    pub fn new(
        id: impl Into<ProcessId>,
        input: ProcessInput,
        disposition: RecoveryContract,
        originator: super::ProcessOriginator,
        lifecycle: ProcessLifecyclePolicy,
    ) -> Self {
        Self {
            id: id.into(),
            input,
            disposition,
            lifecycle,
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
        id: impl Into<ProcessId>,
        originator: super::ProcessOriginator,
        metadata: serde_json::Value,
        lifecycle: ProcessLifecyclePolicy,
    ) -> Self {
        Self::new(
            id,
            ProcessInput::External { metadata },
            RecoveryContract::ExternallyOwned,
            originator,
            lifecycle,
        )
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

    /// Drops the id a caller happened to carry, leaving the declaration a leaf
    /// attempt records. The id is re-derived from the attempt identity at
    /// realization, never carried across the journal.
    pub fn into_declaration(self) -> ProcessStartDeclaration {
        ProcessStartDeclaration {
            input: self.input,
            disposition: self.disposition,
            lifecycle: self.lifecycle,
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
            self.id,
            self.input,
            self.disposition,
            ProcessProvenance::new(self.originator),
            self.lifecycle,
        )
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
