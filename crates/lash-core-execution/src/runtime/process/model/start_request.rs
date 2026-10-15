use serde::{Deserialize, Serialize};

use super::{
    DeclaredLaunch, EngineLaunch, LifetimeDecision, ProcessInput, ProcessProvenance,
    ProcessStartRegistration, ProcessStartTarget, SessionId,
};

/// A start request as a leaf tool attempt declares it: everything a process
/// start needs except its key.
///
/// The key is not declaration material. It is a pure function of the declaring
/// attempt's intent identity ([`crate::StartKeyDerivation::for_tool_intent`]), so every
/// redrive of the declaration presents the same key and starts the same
/// process. The process id is minted by the registrar at realization and read
/// back off the recorded result (ADR 0107).
///
/// It does not decode: only its constructors make one, so an engine-bearing
/// declaration carries a [`DeclaredLaunch`] its declaring attempt held
/// (D10). A journaled declaration decodes through the crate-private
/// [`ProcessStartDeclarationWire`] only the runtime reads, and every launch
/// a tool body returns is checked against what its attempt held before the
/// outcome is recorded.
#[derive(Clone, Debug, Serialize)]
pub struct ProcessStartDeclaration {
    /// What the start runs: an input, or the definition one is resolved from.
    /// An engine-bearing target carries the launch its declaring attempt
    /// held ([`DeclaredLaunch`]); only the constructors set it.
    input: ProcessStartTarget,
    /// The lifetime the declaring attempt chose from its start context,
    /// journaled with the declaration: realization never re-runs the policy
    /// (FIG-3607 R4b).
    pub lifetime: LifetimeDecision,
    pub originator: super::ProcessOriginator,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub identity: Option<super::DeclaredProcessIdentity>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub observers: Vec<SessionId>,
}

/// The stored form of a [`ProcessStartDeclaration`], which the runtime
/// decodes a journaled declaration from. Crate-private: a tool cannot mint a
/// declaration by decoding one.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ProcessStartDeclarationWire {
    input: ProcessStartTarget,
    lifetime: LifetimeDecision,
    originator: super::ProcessOriginator,
    #[serde(default)]
    identity: Option<super::DeclaredProcessIdentity>,
    #[serde(default)]
    observers: Vec<SessionId>,
}

impl From<ProcessStartDeclarationWire> for ProcessStartDeclaration {
    fn from(wire: ProcessStartDeclarationWire) -> Self {
        Self {
            input: wire.input,
            lifetime: wire.lifetime,
            originator: wire.originator,
            identity: wire.identity,
            observers: wire.observers,
        }
    }
}

impl ProcessStartDeclaration {
    fn of_target(
        input: ProcessStartTarget,
        originator: super::ProcessOriginator,
        lifetime: impl Into<LifetimeDecision>,
    ) -> Self {
        Self {
            input,
            lifetime: lifetime.into(),
            originator,
            identity: None,
            observers: Vec::new(),
        }
    }

    /// Declare an engine start under `launch`, the launch the declaring
    /// attempt held. The key is absent by construction; realization derives
    /// it.
    pub fn engine(
        kind: impl Into<String>,
        payload: serde_json::Value,
        launch: DeclaredLaunch,
        originator: super::ProcessOriginator,
        lifetime: impl Into<LifetimeDecision>,
    ) -> Self {
        Self::of_target(
            ProcessStartTarget::Input(ProcessInput::Engine {
                kind: kind.into(),
                payload,
                launch: launch.into_launch(),
            }),
            originator,
            lifetime,
        )
    }

    /// Declare a start of the definition `launch` was held for, under that
    /// launch.
    ///
    /// A definition id is a content address, not a permission: lash does not check
    /// that the caller ran or holds the definition; authorizing creation from an id
    /// is the host's decision.
    pub fn definition(
        signature_claim: Option<super::super::ProcessSignature>,
        args: serde_json::Map<String, serde_json::Value>,
        launch: super::DeclaredDefinitionLaunch,
        originator: super::ProcessOriginator,
        lifetime: impl Into<LifetimeDecision>,
    ) -> Self {
        let (definition_id, launch) = launch.into_parts();
        Self::of_target(
            ProcessStartTarget::Definition {
                definition_id,
                signature_claim,
                args,
                launch,
            },
            originator,
            lifetime,
        )
    }

    /// Declare a session-turn start. It holds no environment: its child
    /// session runs under its own configuration (D5).
    pub fn session_turn(
        definition_key: impl Into<String>,
        create_request: crate::SessionCreateRequest,
        turn_input: crate::TurnInput,
        result: super::SessionTurnOutcome,
        originator: super::ProcessOriginator,
        lifetime: impl Into<LifetimeDecision>,
    ) -> Self {
        Self::of_target(
            ProcessStartTarget::Input(ProcessInput::SessionTurn {
                definition_key: definition_key.into(),
                create_request: Box::new(create_request),
                turn_input: Box::new(turn_input),
                result,
            }),
            originator,
            lifetime,
        )
    }

    /// What the start runs.
    pub fn input(&self) -> &ProcessStartTarget {
        &self.input
    }

    pub fn with_declared_identity(mut self, declared: super::DeclaredProcessIdentity) -> Self {
        self.identity = Some(declared);
        self
    }

    pub fn with_observers(
        mut self,
        observers: impl IntoIterator<Item = impl Into<SessionId>>,
    ) -> Self {
        self.observers = observers.into_iter().map(Into::into).collect();
        self
    }

    /// The only way a declaration becomes a request: every realization route
    /// passes `StartKeyDerivation::for_tool_intent(identity)` here, so every redrive of
    /// one declaration starts the same process by construction.
    pub(crate) fn into_request(self, start_key: crate::StartKey) -> ProcessStartRequest {
        ProcessStartRequest {
            start_key: Some(start_key),
            input: self.input,
            lifetime: self.lifetime,
            originator: self.originator,
            identity: self.identity,
            observers: self.observers,
            // A declared start runs under the scope of the intent that
            // declared it; realization supplies that parent.
            trace: StartTrace::default(),
        }
    }
}

/// Public host-facing request for starting a visible process handle.
///
/// A host keys it only with [`Self::with_host_start_key`]: the key is private,
/// so no host, tool or plugin code sets a key of a family lash derives for its
/// own start paths, and a host rail refuses one that arrives deserialized
/// ([`Self::keyed_in`]).
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProcessStartRequest {
    /// The start's idempotency key; `None` starts a new process every time.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    start_key: Option<crate::StartKey>,
    /// What the start runs: an input, or the definition one is resolved from.
    pub input: ProcessStartTarget,
    /// What ends the process. A host start is a root: `Detached`, or `Until`
    /// a session the host looked up.
    pub lifetime: LifetimeDecision,
    pub originator: super::ProcessOriginator,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub identity: Option<super::DeclaredProcessIdentity>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub observers: Vec<SessionId>,
    /// The start's telemetry, beside the request and never part of it: the
    /// parent the process is admitted under, and the caller context a
    /// host start's producer span takes. The registration that inserts the
    /// process retains the parent as its cause; a retry under the start's
    /// key reads the retained cause back. It is no part of what the key
    /// fences, and it is not serialized: it belongs to one call.
    #[serde(skip)]
    trace: StartTrace,
}

/// A process start's telemetry for one call ([`ProcessStartRequest`]).
#[derive(Clone, Debug, Default)]
struct StartTrace {
    /// What the process is admitted under; `None` admits it unobserved.
    parent: Option<lash_trace::TraceParent>,
    /// The caller choices a host start's producer span takes.
    call: lash_trace::TraceCall,
}

impl ProcessStartRequest {
    fn of_target(
        input: ProcessStartTarget,
        originator: super::ProcessOriginator,
        lifetime: impl Into<LifetimeDecision>,
    ) -> Self {
        Self {
            start_key: None,
            input,
            lifetime: lifetime.into(),
            originator,
            identity: None,
            observers: Vec::new(),
            trace: StartTrace::default(),
        }
    }

    /// A host's start of an engine input under `launch`: the environment
    /// the host published and the engine settings it states, sealed with the
    /// start's lifetime (M3). The request carries no process id: the
    /// registrar mints one. An idempotent host start adds a key with
    /// [`Self::with_host_start_key`].
    pub fn engine(
        kind: impl Into<String>,
        payload: serde_json::Value,
        launch: EngineLaunch,
        originator: super::ProcessOriginator,
        lifetime: impl Into<LifetimeDecision>,
    ) -> Self {
        Self::of_target(
            ProcessStartTarget::Input(ProcessInput::Engine {
                kind: kind.into(),
                payload,
                launch,
            }),
            originator,
            lifetime,
        )
    }

    /// A host's start of the definition `definition_id` names, under
    /// `launch`.
    ///
    /// A definition id is a content address, not a permission: lash does not check
    /// that the caller ran or holds the definition; authorizing creation from an id
    /// is the host's decision.
    pub fn definition(
        definition_id: super::super::ProcessDefinitionId,
        signature_claim: Option<super::super::ProcessSignature>,
        args: serde_json::Map<String, serde_json::Value>,
        launch: EngineLaunch,
        originator: super::ProcessOriginator,
        lifetime: impl Into<LifetimeDecision>,
    ) -> Self {
        Self::of_target(
            ProcessStartTarget::Definition {
                definition_id,
                signature_claim,
                args,
                launch,
            },
            originator,
            lifetime,
        )
    }

    /// A host's start of a session turn. It states no environment: its
    /// child session runs under its own configuration (D5).
    pub fn session_turn(
        definition_key: impl Into<String>,
        create_request: crate::SessionCreateRequest,
        turn_input: crate::TurnInput,
        result: super::SessionTurnOutcome,
        originator: super::ProcessOriginator,
        lifetime: impl Into<LifetimeDecision>,
    ) -> Self {
        Self::of_target(
            ProcessStartTarget::Input(ProcessInput::SessionTurn {
                definition_key: definition_key.into(),
                create_request: Box::new(create_request),
                turn_input: Box::new(turn_input),
                result,
            }),
            originator,
            lifetime,
        )
    }

    /// Open the start's producer span under `context`, the caller's trace
    /// context. An explicit context wins over the ambient one, which is
    /// then never consulted.
    pub fn trace_context(mut self, context: lash_trace::TraceCarrier) -> Self {
        self.trace.call.caller = lash_trace::TraceCaller::Explicit(Some(context));
        self
    }

    /// Open the start's producer span with no caller: detached from the
    /// ambient context, it is a trace root.
    pub fn trace_context_none(mut self) -> Self {
        self.trace.call.caller = lash_trace::TraceCaller::Explicit(None);
        self
    }

    /// How the start's producer span relates to its caller: its child (the
    /// default), or a new trace root that links it.
    pub fn trace_link_mode(mut self, mode: lash_trace::TraceLinkMode) -> Self {
        self.trace.call.link_mode = mode;
        self
    }

    /// The caller choices the start's producer span takes.
    pub fn trace_call(&self) -> &lash_trace::TraceCall {
        &self.trace.call
    }

    /// Admit the process under `parent`: the producer of the host call that
    /// starts it, or the scope of the work that declared it. `None` admits
    /// it unobserved.
    pub fn with_trace_parent(mut self, parent: Option<lash_trace::TraceParent>) -> Self {
        self.trace.parent = parent;
        self
    }

    /// The start's idempotency key, if it has one.
    pub fn start_key(&self) -> Option<&crate::StartKey> {
        self.start_key.as_ref()
    }

    /// Sets the start's idempotency key. Only lash's own start paths key a
    /// request with a key they derived; a host uses
    /// [`Self::with_host_start_key`].
    pub(crate) fn with_start_key(mut self, start_key: Option<crate::StartKey>) -> Self {
        self.start_key = start_key;
        self
    }

    /// Keys the start with a host-supplied key. Lash mixes nothing into it:
    /// the same bytes are one key across the store set, whoever presents them,
    /// and a start under a retained key returns that process only when it is
    /// the same start (ADR 0107).
    /// Reusing the key with a different request is refused with
    /// [`crate::PluginError::StartKeyConflict`].
    ///
    /// If a host loses the acknowledgement, it retries the same request with
    /// the same key to recover the process id. This deduplicates for as long
    /// as the process is retained, including after it ends. The host must
    /// record that id before pruning the process: Lash keeps no start receipt
    /// after pruning, and the same key can then start a new process.
    #[must_use]
    pub fn with_host_start_key(self, key: impl AsRef<[u8]>) -> Self {
        self.with_start_key(Some(crate::StartKey::for_host(key)))
    }

    /// A host start as the host rails realize it under `scope`: the host's
    /// own key makes the start idempotent while its process is retained, and
    /// a keyless start is always new, keyed by the scope and its ordinal
    /// among the run's keyless starts, so a durable handler's replay re-issues
    /// the same key (ADR 0107).
    ///
    /// # Errors
    ///
    /// `start_key_family_refused` for a request that carries a key of a
    /// family lash derives for its own start paths: a host mints only host
    /// keys, so a host rail never adopts a tool intent's process.
    pub fn keyed_in(self, scope: &crate::ActorContext) -> Result<Self, crate::PluginError> {
        match &self.start_key {
            Some(start_key) if start_key.is_host_supplied() => Ok(self),
            Some(_) => Err(crate::PluginError::Runtime(crate::RuntimeError::new(
                crate::RuntimeErrorCode::StartKeyFamilyRefused,
                "a host start carries only a host start key; this key is of a family lash \
                 derives for its own start paths",
            ))),
            None => {
                let start_key = scope.next_keyless_start_key();
                Ok(self.with_start_key(Some(start_key)))
            }
        }
    }

    /// A request never pins a definition reference: only the engine registry can, and only
    /// after resolving it against the engine's stored artifact.
    pub fn with_declared_identity(mut self, declared: super::DeclaredProcessIdentity) -> Self {
        self.identity = Some(declared);
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

    /// Extracts the registration outcome for store and durable-substrate implementors while
    /// persisting and coordinating durable process execution.
    pub fn into_registration(self) -> ProcessStartRegistration {
        let mut registration = ProcessStartRegistration::of_target(
            self.input,
            ProcessProvenance::new(self.originator),
            self.lifetime,
        )
        .with_start_key(self.start_key)
        .with_trace(lash_trace::TraceScopeOffer::caused_by(self.trace.parent));
        if let Some(identity) = self.identity {
            registration = registration.with_declared_identity(identity);
        }
        registration
    }
}
