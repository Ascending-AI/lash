use serde::{Deserialize, Serialize};

use super::super::events::{ProcessEventType, default_process_event_types};
use super::{
    ProcessExecutionEnvRef, ProcessId, ProcessInput, ProcessProvenance, ProcessRegistration,
    RecoveryContract, SessionId,
};
use crate::EffectOpener;

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
///
/// `ParentScope = Owned(EffectOpener) | Host` (FIG-3418, ADR 0094): the owned
/// arm is the shared opener vocabulary itself rather than a parallel enum
/// that re-spells it, so a parent cannot describe an owner `EffectOpener`
/// cannot name — and the one owner derivation (`EffectOpener::for_scope`)
/// stays the only place a scope becomes a parent.
///
/// The serialized shape is deliberately unlike the pre-cutover
/// `{kind, session_id, turn_id}` / `{kind, process_id, incarnation}` pairs:
/// a row, journal entry or remote envelope written against that vocabulary
/// fails this enum's decode rather than being silently reinterpreted.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(tag = "kind", content = "opener", rename_all = "snake_case")]
pub enum ParentScope {
    /// A durable opener — a turn, a queued-work drain or one process — owns
    /// the child.
    Owned(EffectOpener),
    /// Host-selected lifecycle ownership. A host scope never ends within a
    /// process's lifetime.
    Host,
}

/// The version stamped into [`ParentScope::storage_payload`].
///
/// The payload is the authority a ledger row or decode probe reads; the
/// `(kind, id)` columns beside it are only its index projection. A reader
/// refuses any other version rather than guessing at a shape it was not
/// built for.
///
/// Version 2: a process parent names its minted process id, with no
/// incarnation (ADR 0107).
pub const PARENT_SCOPE_STORAGE_PAYLOAD_VERSION: u16 = 2;

/// The versioned typed parent persisted beside the index projection.
///
/// Serialized as `{"version":1,"scope":{...}}`. Deliberately not the raw
/// `ParentScope` encoding: the version wrapper is what lets a future shape
/// refuse cleanly instead of colliding with this one's fields.
#[derive(Serialize, Deserialize)]
struct ParentScopeStoragePayload {
    version: u16,
    scope: ParentScope,
}

/// A stored parent-scope payload a reader refuses.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum ParentScopeStorageError {
    /// The payload decodes but names a version this build does not read.
    #[error("unsupported parent-scope payload version {found}")]
    UnsupportedVersion {
        /// The version found in the stored payload.
        found: u16,
    },
    /// The payload is not the versioned typed shape at all — including any
    /// pre-cutover `ParentScope` serialization.
    #[error("malformed parent-scope payload: {0}")]
    Malformed(String),
    /// The typed payload disagrees with the `(kind, id)` index projection
    /// stored beside it.
    #[error(
        "parent-scope payload does not match its index projection (kind `{kind}`, id `{id:?}`)"
    )]
    ProjectionMismatch {
        /// The stored `parent_scope_kind`/`parent_kind` value.
        kind: String,
        /// The stored `parent_scope_id`/`parent_id` value.
        id: Option<String>,
    },
}

impl ParentScope {
    /// The parent one turn is.
    #[must_use]
    pub fn turn(session_id: impl Into<SessionId>, turn_id: impl Into<crate::TurnId>) -> Self {
        Self::Owned(EffectOpener::turn(session_id, turn_id))
    }

    /// The parent one queued-work drain is.
    #[must_use]
    pub fn queue_drain(session_id: impl Into<SessionId>, drain_id: impl Into<String>) -> Self {
        Self::Owned(EffectOpener::queue_drain(session_id, drain_id))
    }

    /// The parent one process is.
    #[must_use]
    pub fn process(process_id: ProcessId) -> Self {
        Self::Owned(EffectOpener::process(process_id))
    }

    /// The owning opener, when the parent is owned rather than host-managed.
    #[must_use]
    pub fn opener(&self) -> Option<&EffectOpener> {
        match self {
            Self::Owned(opener) => Some(opener),
            Self::Host => None,
        }
    }

    /// Storage discriminant for the scope, as written to `parent_scope_kind`.
    ///
    /// The owned arms take their opener's arm name, so a `WHERE
    /// parent_scope_kind = 'turn'` filter keeps its meaning. `Host` has no
    /// identity of its own; the id column stays `NULL` for it.
    pub fn storage_kind(&self) -> &'static str {
        match self {
            Self::Owned(EffectOpener::Turn { .. }) => "turn",
            Self::Owned(EffectOpener::QueueDrain { .. }) => "queue_drain",
            Self::Owned(EffectOpener::Process { .. }) => "process",
            Self::Host => "host",
        }
    }

    /// Index identity for the scope, as written to `parent_scope_id`.
    ///
    /// This is the *canonical projection* of the scope, not a codec: it is
    /// [`EffectOpener::identity_encoding`], whose length-prefixed components
    /// make it injective over valid ids — `Turn("a/b", "c")` and
    /// `Turn("a", "b/c")` can never share a key the way the retired
    /// `/`- and `#`-joined renderings could. Equality and index lookups use
    /// it; nothing parses it back. `Host` has no identity; the column is
    /// `NULL`, which the storage check constraint ties to the kind.
    pub fn storage_id(&self) -> Option<String> {
        match self {
            Self::Owned(opener) => Some(opener.identity_encoding()),
            Self::Host => None,
        }
    }

    /// The versioned typed payload stored beside the index projection.
    ///
    /// The payload — not the projection — is the authority a ledger row or a
    /// decode probe reads back. It survives facts the projection cannot
    /// express (a pruned parent process's row, a future arm), and its
    /// explicit version is what makes an incompatible row a refusal instead
    /// of a reinterpretation.
    pub fn storage_payload(&self) -> Result<String, serde_json::Error> {
        serde_json::to_string(&ParentScopeStoragePayload {
            version: PARENT_SCOPE_STORAGE_PAYLOAD_VERSION,
            scope: self.clone(),
        })
    }

    /// Rebuilds a scope from a stored row's index projection and typed
    /// payload.
    ///
    /// The payload is the authority; the `(kind, id)` pair is accepted only
    /// as the projection this build would have written for the decoded
    /// scope, so a row whose index columns and payload disagree is refused
    /// rather than trusted on either side. Any payload that is not the
    /// current versioned shape — including a pre-cutover `ParentScope`
    /// serialization — is refused, never reinterpreted.
    ///
    /// # Errors
    ///
    /// [`ParentScopeStorageError::Malformed`] for undecodable bytes,
    /// [`ParentScopeStorageError::UnsupportedVersion`] for a decodable
    /// payload at another version, and
    /// [`ParentScopeStorageError::ProjectionMismatch`] when the typed scope
    /// and the index columns disagree.
    pub fn from_storage_columns(
        kind: &str,
        id: Option<&str>,
        payload: &str,
    ) -> Result<Self, ParentScopeStorageError> {
        let decoded: ParentScopeStoragePayload = serde_json::from_str(payload)
            .map_err(|error| ParentScopeStorageError::Malformed(error.to_string()))?;
        if decoded.version != PARENT_SCOPE_STORAGE_PAYLOAD_VERSION {
            return Err(ParentScopeStorageError::UnsupportedVersion {
                found: decoded.version,
            });
        }
        let scope = decoded.scope;
        if scope.storage_kind() != kind || scope.storage_id().as_deref() != id {
            return Err(ParentScopeStorageError::ProjectionMismatch {
                kind: kind.to_string(),
                id: id.map(str::to_string),
            });
        }
        Ok(scope)
    }

    /// The lifecycle parent one owner identity maps to.
    ///
    /// [`crate::EffectOpener`] is the one owner vocabulary (ADR 0099 §1) —
    /// derived once, at admission, by [`crate::EffectOpener::for_scope`] from
    /// the admitted scope — and this is the only
    /// projection of that vocabulary into a `ParentScope`.
    ///
    /// A `QueueDrain` opener is a durable owner in exactly §1's sense, and
    /// since FIG-3419 it has an end protocol — the drain-end epilogue writes
    /// the receipt and the ledger row — so drain-owned children parent on it
    /// like any other owner.
    pub fn from_owner(opener: &crate::EffectOpener) -> Self {
        Self::Owned(opener.clone())
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
    /// The key is absent by construction; realization derives it.
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
    /// passes `StartKey::for_tool_intent(identity)` here, so every redrive of
    /// one declaration starts the same process by construction.
    pub fn into_request(self, start_key: crate::StartKey) -> ProcessStartRequest {
        ProcessStartRequest {
            start_key: Some(start_key),
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
    /// The start's idempotency key; `None` starts a new process every time.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub start_key: Option<crate::StartKey>,
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
    ///
    /// The request carries no process id: the registrar mints one. An
    /// idempotent start adds a key with [`Self::with_start_key`].
    pub fn new(
        input: ProcessInput,
        disposition: RecoveryContract,
        originator: super::ProcessOriginator,
        lifecycle: ProcessLifecyclePolicy,
    ) -> Self {
        Self {
            start_key: None,
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
            self.input,
            self.disposition,
            ProcessProvenance::new(self.originator),
            self.lifecycle,
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

#[cfg(test)]
mod tests {
    use super::*;

    fn turn_scope() -> ParentScope {
        ParentScope::turn(SessionId::from("s"), crate::TurnId::from("t"))
    }

    fn process_scope() -> ParentScope {
        ParentScope::process(crate::process_id_for_test("worker"))
    }

    /// The payload round-trips only against its own index projection.
    #[test]
    fn a_parent_scope_round_trips_through_its_versioned_payload() {
        for scope in [turn_scope(), process_scope(), ParentScope::Host] {
            let payload = scope.storage_payload().expect("encode the payload");
            let decoded = ParentScope::from_storage_columns(
                scope.storage_kind(),
                scope.storage_id().as_deref(),
                &payload,
            )
            .expect("the payload is the authority the projection agrees with");
            assert_eq!(decoded, scope);
        }
    }

    /// The canonical projection is injective exactly where the retired
    /// `{session}/{turn}` rendering was not: `("s/a","c")` and `("s","a/c")`
    /// rendered to one stored id and shared a ledger key before FIG-3418.
    #[test]
    fn scopes_that_render_identically_have_distinct_projections() {
        let first = ParentScope::turn(SessionId::from("s/a"), crate::TurnId::from("c"));
        let second = ParentScope::turn(SessionId::from("s"), crate::TurnId::from("a/c"));
        assert_ne!(first.storage_id(), second.storage_id());
    }

    /// A pre-cutover `ParentScope` serialization is not a payload: it lacks
    /// the version wrapper, so it is refused rather than reinterpreted.
    #[test]
    fn a_pre_cutover_parent_scope_is_refused_as_a_payload() {
        let old_shape = serde_json::json!({
            "kind": "turn",
            "session_id": "s",
            "turn_id": "t",
        })
        .to_string();
        let error = ParentScope::from_storage_columns("turn", Some("s/t"), &old_shape)
            .expect_err("an old-shape payload must not silently decode");
        assert!(
            matches!(error, ParentScopeStorageError::Malformed(_)),
            "old payloads refuse as malformed, not migrated: {error}"
        );
    }

    /// A payload whose version is not this build's is refused outright.
    #[test]
    fn an_unsupported_payload_version_is_refused() {
        let payload = serde_json::json!({
            "version": PARENT_SCOPE_STORAGE_PAYLOAD_VERSION + 1,
            "scope": { "kind": "host" },
        })
        .to_string();
        let error = ParentScope::from_storage_columns("host", None, &payload)
            .expect_err("a newer payload version must refuse");
        assert_eq!(
            error,
            ParentScopeStorageError::UnsupportedVersion {
                found: PARENT_SCOPE_STORAGE_PAYLOAD_VERSION + 1,
            }
        );
    }

    /// A payload is only trusted when the `(kind, id)` beside it is the
    /// projection this build would have written for the same scope.
    #[test]
    fn a_payload_that_disagrees_with_its_projection_is_refused() {
        let payload = turn_scope().storage_payload().expect("encode a turn");
        let error = ParentScope::from_storage_columns(
            "turn",
            process_scope().storage_id().as_deref(),
            &payload,
        )
        .expect_err("a mismatched projection must refuse");
        assert!(
            matches!(error, ParentScopeStorageError::ProjectionMismatch { .. }),
            "{error}"
        );
        let error = ParentScope::from_storage_columns("process", Some("s/t"), &payload)
            .expect_err("a mismatched kind must refuse");
        assert!(
            matches!(error, ParentScopeStorageError::ProjectionMismatch { .. }),
            "{error}"
        );
    }
}
