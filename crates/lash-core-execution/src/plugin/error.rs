use crate::ProcessId;
use crate::SessionId;

/// One refusal for a durable identity re-presented with different content.
///
/// The stores fence a re-submitted identity at the point it mutates: a process
/// registration fingerprint, a process-event replay key, a trigger occurrence
/// idempotency key. Matching content replays the first writer's result;
/// differing content cannot, because the identity is already bound. Every such
/// site returns this one error so the tool-intent front door can map all three
/// to a single typed host-facing refusal instead of matching prose (FIG-1489).
///
/// It is spelled as a [`RuntimeError`](crate::RuntimeError) carrying
/// [`RuntimeErrorCode::DurableIdentityConflict`](crate::RuntimeErrorCode::DurableIdentityConflict)
/// rather than a new `PluginError` variant, because `PluginError` is journaled
/// inside recorded process admissions: a new code string is data an
/// existing variant already carries, while a new variant would be a shape an
/// older build could not read.
pub fn durable_identity_conflict(message: impl Into<String>) -> PluginError {
    PluginError::Runtime(crate::RuntimeError::new(
        crate::RuntimeErrorCode::DurableIdentityConflict,
        message,
    ))
}

/// Whether `error` is the durable-identity refusal minted by
/// [`durable_identity_conflict`], however many conversions it has crossed.
pub fn is_durable_identity_conflict(error: &PluginError) -> bool {
    match error {
        PluginError::Runtime(error) => {
            error.code == crate::RuntimeErrorCode::DurableIdentityConflict
        }
        PluginError::RuntimeEffectController(error) => {
            error.code == crate::RuntimeErrorCode::DurableIdentityConflict
        }
        _ => false,
    }
}
/// The refusal a trigger store answers an ingest whose occurrence identity
/// retention has reclaimed (FIG-4513).
///
/// A reclaimed occurrence leaves a tombstone under its id. An ingest that
/// finds the tombstone is a redelivery of an emission that already ran, on a
/// host with no journal to answer it from: the store writes neither the
/// occurrence nor a delivery back, and refuses with this error. It carries
/// [`RuntimeErrorCode::TriggerOccurrenceReclaimed`](crate::RuntimeErrorCode::TriggerOccurrenceReclaimed)
/// for the reason [`durable_identity_conflict`] carries its code.
pub fn trigger_occurrence_reclaimed(occurrence_id: &str) -> PluginError {
    PluginError::Runtime(crate::RuntimeError::new(
        crate::RuntimeErrorCode::TriggerOccurrenceReclaimed,
        format!(
            "trigger occurrence `{occurrence_id}` was recorded and has since been reclaimed by retention"
        ),
    ))
}

/// Whether `error` is the refusal minted by [`trigger_occurrence_reclaimed`],
/// however many conversions it has crossed.
pub fn is_trigger_occurrence_reclaimed(error: &PluginError) -> bool {
    match error {
        PluginError::Runtime(error) => {
            error.code == crate::RuntimeErrorCode::TriggerOccurrenceReclaimed
        }
        PluginError::RuntimeEffectController(error) => {
            error.code == crate::RuntimeErrorCode::TriggerOccurrenceReclaimed
        }
        _ => false,
    }
}
// The plugin vocabulary owns both its live error and its durable intent cause.
// Adding a variant requires its code, failure class and lossless conversion here.
macro_rules! define_plugin_errors {
    ($live_derive:meta; $recorded_derive:meta; $( $(#[$($attr:tt)*])* $variant:ident $payload:tt
        => $from:pat => $recorded:tt => $convert:expr
        => $projection:pat => $code:expr => $class:expr; )*) => {
        #[ $live_derive ]
        #[serde(tag = "type", content = "message", rename_all = "snake_case")]
        #[non_exhaustive]
        pub enum PluginError {
            $( $(#[$($attr)*])* $variant $payload, )*
        }

        /// The typed command cause retained by a tool intent and carried to the host.
        #[ $recorded_derive ]
        #[serde(tag = "type", content = "message", rename_all = "snake_case", deny_unknown_fields)]
        pub enum ToolIntentCommandFailure {
            $( $(#[$($attr)*])* $variant $recorded, )*
        }

        impl From<&PluginError> for ToolIntentCommandFailure {
            fn from(error: &PluginError) -> Self {
                match error {
                    $( $from => $convert, )*
                }
            }
        }

        impl ToolIntentCommandFailure {
            pub fn code(&self) -> std::borrow::Cow<'_, str> { self.classification().0 }
            pub fn failure_class(&self) -> crate::ToolFailureClass { self.classification().1 }
            fn classification(&self) -> (std::borrow::Cow<'_, str>, crate::ToolFailureClass) {
                match self {
                    $( $projection => (($code).into(), $class), )*
                }
            }
        }
    };
}

/// The durable fields of a runtime command failure. Invocation-local retry
/// markers are not recorded command facts.
#[derive(
    Clone,
    Debug,
    PartialEq,
    serde::Serialize,
    serde::Deserialize,
    schemars::JsonSchema,
    thiserror::Error,
)]
#[error("{code}: {message}")]
#[serde(deny_unknown_fields)]
pub struct ToolIntentRuntimeFailure {
    #[schemars(with = "String")]
    pub code: crate::RuntimeErrorCode,
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cause: Option<crate::RuntimeErrorCause>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary: Option<Box<crate::RuntimeEffectReplayMismatchReport>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub turn_input_acceptance:
        Option<Box<lash_core_store::turn_input_vocabulary::TurnInputAcceptanceReceipt>>,
}

impl ToolIntentRuntimeFailure {
    fn failure_class(&self) -> crate::ToolFailureClass {
        if let Some(class) = self
            .cause
            .as_ref()
            .and_then(crate::RuntimeErrorCause::plugin_failure_class)
        {
            return if class == super::PluginFailureClass::Terminal {
                crate::ToolFailureClass::InvalidRequest
            } else {
                crate::ToolFailureClass::Unavailable
            };
        }
        if self.code.is_terminal()
            || self
                .cause
                .as_ref()
                .is_some_and(crate::RuntimeErrorCause::is_terminal)
        {
            crate::ToolFailureClass::InvalidRequest
        } else {
            crate::ToolFailureClass::Unavailable
        }
    }
}

define_plugin_errors! {
    derive(Debug, thiserror::Error, Clone, serde::Serialize, serde::Deserialize);
    derive(Debug, thiserror::Error, Clone, PartialEq, serde::Serialize, serde::Deserialize, schemars::JsonSchema);
    /// A provider refusal retains the host's classification and namespaced code.
    #[error("{message}")]
    ProviderFailure {
        kind: crate::ProviderFailureKind,
        code: Option<crate::FailureCode>,
        retryable: bool,
        terminal_reason: crate::LlmTerminalReason,
        message: String,
    }
        => PluginError::ProviderFailure { kind, code, retryable, terminal_reason, message }
        => {
        kind: crate::ProviderFailureKind,
        code: Option<crate::FailureCode>,
        retryable: bool,
        terminal_reason: crate::LlmTerminalReason,
        message: String,
    }
        => Self::ProviderFailure { kind: *kind, code: code.clone(), retryable: *retryable, terminal_reason: *terminal_reason, message: message.clone() }
        => Self::ProviderFailure { kind, code, retryable, .. }
        => code.as_ref().map(std::string::ToString::to_string).unwrap_or_else(|| kind.code().to_string())
        => if *retryable { crate::ToolFailureClass::Unavailable } else { crate::ToolFailureClass::InvalidRequest };
    #[error(transparent)]
    Operation(Box<super::PluginOperationFailure>)
        => PluginError::Operation(failure)
        => (Box<super::PluginOperationFailure>)
        => Self::Operation(failure.clone())
        => Self::Operation(failure)
        => failure.code.namespaced()
        => if failure.class == super::PluginFailureClass::Terminal { crate::ToolFailureClass::InvalidRequest } else { crate::ToolFailureClass::Unavailable };
    #[error("plugin runtime event hooks failed: {causes:?}")]
    HookFailures { causes: Vec<super::PluginHookFailure> }
        => PluginError::HookFailures { causes }
        => { causes: Vec<super::PluginHookFailure> }
        => Self::HookFailures { causes: causes.clone() }
        => Self::HookFailures { causes }
        => crate::RuntimeErrorCode::Plugin.as_str()
        => if causes.iter().all(|cause| cause.failure.class == super::PluginFailureClass::Terminal) { crate::ToolFailureClass::InvalidRequest } else { crate::ToolFailureClass::Unavailable };
    #[error(transparent)]
    TriggerOperation(Box<crate::TriggerOperationError>)
        => PluginError::TriggerOperation(source)
        => (Box<crate::TriggerOperationError>)
        => Self::TriggerOperation(source.clone())
        => Self::TriggerOperation(source)
        => source.code()
        => if source.is_terminal() { crate::ToolFailureClass::InvalidRequest } else { crate::ToolFailureClass::Unavailable };
/// A process cannot run without the behaviour its creation recorded.
    #[error("process engine `{engine_kind}` has no recorded configuration")]
    MissingRecordedProcessConfig { engine_kind: String }
        => PluginError::MissingRecordedProcessConfig { engine_kind }
        => { engine_kind: String }
        => Self::MissingRecordedProcessConfig { engine_kind: engine_kind.clone() }
        => Self::MissingRecordedProcessConfig { .. }
        => "missing_recorded_process_config"
        => crate::ToolFailureClass::InvalidRequest;
#[error("trigger registration requires an Engine target, received `{kind}`")]
    InvalidTriggerTarget { kind: String }
        => PluginError::InvalidTriggerTarget { kind }
        => { kind: String }
        => Self::InvalidTriggerTarget { kind: kind.clone() }
        => Self::InvalidTriggerTarget { .. }
        => "invalid_trigger_target"
        => crate::ToolFailureClass::InvalidRequest;
/// The process already accepted a different cancellation request.
    #[error(
        "process `{process_id}` already accepted cancellation {existing:?}; refused {requested:?}"
    )]
    ProcessCancelConflict {
        process_id: ProcessId,
        existing: Box<crate::CancelRequest>,
        requested: Box<crate::CancelRequest>,
    }
        => PluginError::ProcessCancelConflict { process_id, existing, requested }
        => {
        process_id: ProcessId,
        existing: Box<crate::CancelRequest>,
        requested: Box<crate::CancelRequest>,
    }
        => Self::ProcessCancelConflict { process_id: process_id.clone(), existing: existing.clone(), requested: requested.clone() }
        => Self::ProcessCancelConflict { .. }
        => "process_cancel_conflict"
        => crate::ToolFailureClass::InvalidRequest;
/// A new start named a closed scope: its starter has ended, or the scope
    /// its lifetime names has closed (FIG-3607 R11). The start is refused
    /// before an id is minted, so the refusal names the start by its key.
    #[error("cannot register process start {start_key:?}: scope `{parent}` has closed")]
    ParentEnded {
        start_key: Option<crate::StartKey>,
        parent: crate::ScopeId,
    }
        => PluginError::ParentEnded { start_key, parent }
        => {
        start_key: Option<crate::StartKey>,
        parent: crate::ScopeId,
    }
        => Self::ParentEnded { start_key: start_key.clone(), parent: parent.clone() }
        => Self::ParentEnded { .. }
        => "process_parent_ended"
        => crate::ToolFailureClass::InvalidRequest;
/// A host start key is bound to a retained process another start made
    /// (ADR 0107): the retry presented a different start. A host key is
    /// global, so the retained process may be another originator's; the
    /// refusal names the key and nothing of the process it is bound to.
    #[error("process start key `{start_key}` is bound to another start")]
    StartKeyConflict { start_key: crate::StartKey }
        => PluginError::StartKeyConflict { start_key }
        => { start_key: crate::StartKey }
        => Self::StartKeyConflict { start_key: start_key.clone() }
        => Self::StartKeyConflict { .. }
        => "process_start_key_conflict"
        => crate::ToolFailureClass::InvalidRequest;
/// A trigger delivery's start found no retained process under its key,
    /// and the delivery already bound to `process_id` (ADR 0107 §5,
    /// FIG-4369). The bound process was pruned, so its key finds nothing; the
    /// registrar read the binding in the transaction that checked the key and
    /// registered nothing.
    #[error(
        "trigger delivery `{occurrence_id}`/`{subscription_id}` is already bound to process `{process_id}`"
    )]
    TriggerDeliveryBound {
        occurrence_id: String,
        subscription_id: String,
        process_id: ProcessId,
    }
        => PluginError::TriggerDeliveryBound { occurrence_id, subscription_id, process_id }
        => {
        occurrence_id: String,
        subscription_id: String,
        process_id: ProcessId,
    }
        => Self::TriggerDeliveryBound { occurrence_id: occurrence_id.clone(), subscription_id: subscription_id.clone(), process_id: process_id.clone() }
        => Self::TriggerDeliveryBound { .. }
        => "trigger_delivery_bound"
        => crate::ToolFailureClass::InvalidRequest;
/// A trigger delivery's start found no retained process under its key,
    /// and no delivery row: retention removed the delivery once its bound
    /// process was pruned (FIG-4369). The registrar registered nothing.
    #[error(
        "trigger delivery `{occurrence_id}`/`{subscription_id}` is no longer reserved: its process was pruned and the delivery retired"
    )]
    TriggerDeliveryRetired {
        occurrence_id: String,
        subscription_id: String,
    }
        => PluginError::TriggerDeliveryRetired { occurrence_id, subscription_id }
        => {
        occurrence_id: String,
        subscription_id: String,
    }
        => Self::TriggerDeliveryRetired { occurrence_id: occurrence_id.clone(), subscription_id: subscription_id.clone() }
        => Self::TriggerDeliveryRetired { .. }
        => "trigger_delivery_retired"
        => crate::ToolFailureClass::InvalidRequest;
/// Discovery must itself be an inline member of the tool catalogue.
    #[error("discovery operation `{operation}` must be an inline catalogue member")]
    InvalidToolDiscovery { operation: String }
        => PluginError::InvalidToolDiscovery { operation }
        => { operation: String }
        => Self::InvalidToolDiscovery { operation: operation.clone() }
        => Self::InvalidToolDiscovery { .. }
        => "invalid_tool_discovery"
        => crate::ToolFailureClass::InvalidRequest;
/// A protocol's per-call batch maximum exceeds its hard ceiling.
    #[error("batch maximum {requested} exceeds the ceiling of {ceiling} members")]
    InvalidBatchMaximum { requested: usize, ceiling: usize }
        => PluginError::InvalidBatchMaximum { requested, ceiling }
        => { requested: usize, ceiling: usize }
        => Self::InvalidBatchMaximum { requested: *requested, ceiling: *ceiling }
        => Self::InvalidBatchMaximum { .. }
        => "invalid_batch_maximum"
        => crate::ToolFailureClass::InvalidRequest;
    #[error("{source}")]
    UnusableSchema { source: Box<crate::SchemaAdmissionError> }
        => PluginError::UnusableSchema { source }
        => { source: Box<crate::SchemaAdmissionError> }
        => Self::UnusableSchema { source: source.clone() }
        => Self::UnusableSchema { .. }
        => "unusable_schema"
        => crate::ToolFailureClass::InvalidRequest;
    #[error("unusable tool schema: {source}")]
    UnusableToolSchema { source: Box<crate::ToolCatalogBuildError> }
        => PluginError::UnusableToolSchema { source }
        => { source: Box<crate::ToolCatalogBuildError> }
        => Self::UnusableToolSchema { source: source.clone() }
        => Self::UnusableToolSchema { .. }
        => "unusable_tool_schema"
        => crate::ToolFailureClass::InvalidRequest;
    #[error("invalid {context}: {source}")]
    ValueMismatch { context: String, source: Box<crate::ValueMismatch> }
        => PluginError::ValueMismatch { context, source }
        => { context: String, source: Box<crate::ValueMismatch> }
        => Self::ValueMismatch { context: context.clone(), source: source.clone() }
        => Self::ValueMismatch { .. }
        => "value_mismatch"
        => crate::ToolFailureClass::InvalidRequest;
/// An effective resident catalog member could not supply its immutable definition.
    #[error("resident tool `{name}` ({tool_id}) has no contract")]
    ResidentToolContractUnavailable {
        tool_id: crate::ToolId,
        name: String,
    }
        => PluginError::ResidentToolContractUnavailable { tool_id, name }
        => {
        tool_id: crate::ToolId,
        name: String,
    }
        => Self::ResidentToolContractUnavailable { tool_id: tool_id.clone(), name: name.clone() }
        => Self::ResidentToolContractUnavailable { .. }
        => "resident_tool_contract_unavailable"
        => crate::ToolFailureClass::Internal;
#[error("resident catalog repeats tool id `{tool_id}`")]
    ResidentToolDuplicateId { tool_id: crate::ToolId }
        => PluginError::ResidentToolDuplicateId { tool_id }
        => { tool_id: crate::ToolId }
        => Self::ResidentToolDuplicateId { tool_id: tool_id.clone() }
        => Self::ResidentToolDuplicateId { .. }
        => "resident_tool_duplicate_id"
        => crate::ToolFailureClass::Internal;
#[error("resident catalog repeats tool name `{name}`")]
    ResidentToolDuplicateName { name: String }
        => PluginError::ResidentToolDuplicateName { name }
        => { name: String }
        => Self::ResidentToolDuplicateName { name: name.clone() }
        => Self::ResidentToolDuplicateName { .. }
        => "resident_tool_duplicate_name"
        => crate::ToolFailureClass::Internal;
/// A catalog member's declared execution is refused against the runtime's
    /// execution budgets: an inline-only tool declared above the ceiling.
    #[error("{source}")]
    ToolRegistrationRefused { source: Box<crate::RegistrationRefused> }
        => PluginError::ToolRegistrationRefused { source }
        => { source: Box<crate::RegistrationRefused> }
        => Self::ToolRegistrationRefused { source: source.clone() }
        => Self::ToolRegistrationRefused { .. }
        => "tool_registration_refused"
        => crate::ToolFailureClass::InvalidRequest;
/// An effective resident catalog member has no executable route in the pinned registry.
    #[error("resident tool `{name}` ({tool_id}) has no pinned execution route: {reason}")]
    ResidentToolRouteUnavailable {
        tool_id: crate::ToolId,
        name: String,
        reason: String,
    }
        => PluginError::ResidentToolRouteUnavailable { tool_id, name, reason }
        => {
        tool_id: crate::ToolId,
        name: String,
        reason: String,
    }
        => Self::ResidentToolRouteUnavailable { tool_id: tool_id.clone(), name: name.clone(), reason: reason.clone() }
        => Self::ResidentToolRouteUnavailable { .. }
        => "resident_tool_route_unavailable"
        => crate::ToolFailureClass::Internal;
/// A fresh session create named an id the catalog already holds. A
    /// create never adopts an existing session (FIG-4112); replaying a
    /// recorded identity is a process run's `SessionTurn` initialisation,
    /// not a create.
    #[error("session `{session_id}` already exists")]
    SessionAlreadyExists { session_id: crate::SessionId }
        => PluginError::SessionAlreadyExists { session_id }
        => { session_id: crate::SessionId }
        => Self::SessionAlreadyExists { session_id: session_id.clone() }
        => Self::SessionAlreadyExists { .. }
        => "session_already_exists"
        => crate::ToolFailureClass::InvalidRequest;
/// A lane-less head write found an owner in the store transaction.
    /// Submit host head writes as boundary session commands, or retry once
    /// the owner releases the head. The refused write changes nothing.
    #[error(
        "session `{session_id}`'s head is owned by {owner}; a head write outside the shift is refused"
    )]
    SessionHeadOwned {
        session_id: SessionId,
        owner: crate::store::SessionHeadOwner,
    }
        => PluginError::SessionHeadOwned { session_id, owner }
        => {
        session_id: SessionId,
        owner: crate::store::SessionHeadOwner,
    }
        => Self::SessionHeadOwned { session_id: session_id.clone(), owner: owner.clone() }
        => Self::SessionHeadOwned { .. }
        => "session_head_owned"
        => crate::ToolFailureClass::Unavailable;
#[error("plugin registration error: {0}")]
    Registration (String)
        => PluginError::Registration(source)
        => (String)
        => Self::Registration(source.clone())
        => Self::Registration(_)
        => "registration"
        => crate::ToolFailureClass::Internal;
/// A plugin's config registration cannot stand on the deployment's plugin set.
#[error("config registration is invalid: {0}")]
    ConfigRegistration (super::ConfigRegistrationError)
        => PluginError::ConfigRegistration(source)
        => (super::ConfigRegistrationError)
        => Self::ConfigRegistration(source.clone())
        => Self::ConfigRegistration(_)
        => "config_registration"
        => crate::ToolFailureClass::Internal;
#[error("plugin invoke error: {0}")]
    Invoke (String)
        => PluginError::Invoke(source)
        => (String)
        => Self::Invoke(source.clone())
        => Self::Invoke(_)
        => "invoke"
        => crate::ToolFailureClass::Execution;
#[error("plugin session error: {0}")]
    Session (String)
        => PluginError::Session(source)
        => (String)
        => Self::Session(source.clone())
        => Self::Session(_)
        => "session"
        => crate::ToolFailureClass::Unavailable;
/// An atomic plugin-state refusal, retained across journal transport.
    #[error("plugin state: {0}")]
    State (#[source] super::PluginStateError)
        => PluginError::State(source)
        => (#[source] super::PluginStateError)
        => Self::State(source.clone())
        => Self::State(source)
        => "state"
        => match source {
            super::PluginStateError::InvalidKey { .. } | super::PluginStateError::ValueTooLarge { .. } | super::PluginStateError::StoreTooLarge { .. } => crate::ToolFailureClass::InvalidRequest,
            super::PluginStateError::Unrecorded { .. } | super::PluginStateError::EffectOwnerMismatch | super::PluginStateError::Frontier { .. } | super::PluginStateError::Encode { .. } | super::PluginStateError::Decode { .. } => crate::ToolFailureClass::Internal,
            super::PluginStateError::PublicationFenced { .. } => crate::ToolFailureClass::Unavailable,
        };
/// A factory's [`super::PluginDeclaration`] failed the composition's owner
    /// and format checks (FIG-4732): a session cannot materialize under a
    /// plugin set that misstates itself, before any plugin callback runs.
    #[error(transparent)]
    Declaration (#[from] super::PluginDeclarationError)
        => PluginError::Declaration(source)
        => (#[from] super::PluginDeclarationError)
        => Self::Declaration(source.clone())
        => Self::Declaration(_)
        => "declaration"
        => crate::ToolFailureClass::Internal;
    #[error(transparent)]
    Format (#[from] super::FormatRefusal)
        => PluginError::Format(source)
        => (#[from] super::FormatRefusal)
        => Self::Format(source.clone())
        => Self::Format(_)
        => crate::RuntimeErrorCode::Plugin.as_str()
        => crate::ToolFailureClass::InvalidRequest;
/// A store compatibility refusal, preserved through plugin-facing ports.
    #[error(transparent)]
    StoreRefusal (#[from] crate::store::StoreRefusal)
        => PluginError::StoreRefusal(source)
        => (#[from] crate::store::StoreRefusal)
        => Self::StoreRefusal(source.clone())
        => Self::StoreRefusal(source)
        => source.code().as_str().to_string()
        => crate::ToolFailureClass::Internal;
/// The storage substrate faulted under a plugin-facing port: the identical
    /// operation may succeed when it is made again, and nothing about the
    /// fault is the operation's answer.
    #[error("{fault}")]
    StoreUnavailable { fault: crate::store::StoreFault }
        => PluginError::StoreUnavailable { fault }
        => { fault: crate::store::StoreFault }
        => Self::StoreUnavailable { fault: fault.clone() }
        => Self::StoreUnavailable { fault }
        => fault.code().as_str().to_string()
        => crate::ToolFailureClass::Unavailable;
/// A captured plugin init payload exceeded the durable-request bound.
    #[error("captured session init payload is {bytes} bytes, exceeding the {limit}-byte bound")]
    SessionInitTooLarge { bytes: usize, limit: usize }
        => PluginError::SessionInitTooLarge { bytes, limit }
        => { bytes: usize, limit: usize }
        => Self::SessionInitTooLarge { bytes: *bytes, limit: *limit }
        => Self::SessionInitTooLarge { .. }
        => "session_init_too_large"
        => crate::ToolFailureClass::InvalidRequest;
/// An existing plugin session cannot be reconstructed because a required
    /// protocol-owned field is absent from its durable record.
    #[error("recorded session config for plugin `{plugin_id}` is missing required field `{field}`")]
    MissingRecordedSessionConfig { plugin_id: String, field: String }
        => PluginError::MissingRecordedSessionConfig { plugin_id, field }
        => { plugin_id: String, field: String }
        => Self::MissingRecordedSessionConfig { plugin_id: plugin_id.clone(), field: field.clone() }
        => Self::MissingRecordedSessionConfig { .. }
        => "missing_recorded_session_config"
        => crate::ToolFailureClass::InvalidRequest;
/// A host attempted to substitute a durably pinned protocol selection.
    #[error(
        "recorded session config for plugin `{plugin_id}` pins `{field}` to {recorded}, refusing {requested}"
    )]
    RecordedSessionConfigConflict {
        plugin_id: String,
        field: String,
        recorded: String,
        requested: String,
    }
        => PluginError::RecordedSessionConfigConflict { plugin_id, field, recorded, requested }
        => {
        plugin_id: String,
        field: String,
        recorded: String,
        requested: String,
    }
        => Self::RecordedSessionConfigConflict { plugin_id: plugin_id.clone(), field: field.clone(), recorded: recorded.clone(), requested: requested.clone() }
        => Self::RecordedSessionConfigConflict { .. }
        => "recorded_session_config_conflict"
        => crate::ToolFailureClass::InvalidRequest;
#[error(transparent)]
    Runtime (crate::RuntimeError)
        => PluginError::Runtime(source)
        => (ToolIntentRuntimeFailure)
        => Self::Runtime(ToolIntentRuntimeFailure { code: source.code.clone(), message: source.message.clone(), cause: source.cause.clone(), summary: source.summary.clone(), turn_input_acceptance: source.turn_input_acceptance.clone() })
        => Self::Runtime(source)
        => source.code.as_str()
        => source.failure_class();
/// A turn-scoped plugin write presented a lapsed or superseded borrowed
    /// session-execution guard.
    #[error("session execution lease for `{session_id}` was lost before plugin commit")]
    SessionExecutionLeaseLost { session_id: SessionId }
        => PluginError::SessionExecutionLeaseLost { session_id }
        => { session_id: SessionId }
        => Self::SessionExecutionLeaseLost { session_id: session_id.clone() }
        => Self::SessionExecutionLeaseLost { .. }
        => "session_execution_lease_lost"
        => crate::ToolFailureClass::Unavailable;
/// A session append operation id was reused for different semantic request content.
    #[error(
        "append operation `{operation_key}` for session `{session_id}` was reused with different request content"
    )]
    AppendOperationIdentityConflict {
        /// Session whose append operation identity conflicted.
        session_id: SessionId,
        /// Canonical durable operation key that was reused incorrectly.
        operation_key: String,
    }
        => PluginError::AppendOperationIdentityConflict { session_id, operation_key }
        => {
        /// Session whose append operation identity conflicted.
        session_id: SessionId,
        /// Canonical durable operation key that was reused incorrectly.
        operation_key: String,
    }
        => Self::AppendOperationIdentityConflict { session_id: session_id.clone(), operation_key: operation_key.clone() }
        => Self::AppendOperationIdentityConflict { .. }
        => "append_operation_identity_conflict"
        => crate::ToolFailureClass::InvalidRequest;
/// Durable append receipt metadata contradicts the retry's requested-node
    /// count. This is store corruption, not a caller-recoverable conflict.
    #[error(
        "append receipt `{operation_key}` for session `{session_id}` has contradictory requested-node counts (stored {stored}, attempted {attempted})"
    )]
    AppendReceiptRequestedNodeCountCorrupt {
        /// Session whose append receipt is corrupt.
        session_id: SessionId,
        /// Canonical durable operation key of the corrupt receipt.
        operation_key: String,
        stored: u64,
        /// Count carried by the retry.
        attempted: u64,
    }
        => PluginError::AppendReceiptRequestedNodeCountCorrupt { session_id, operation_key, stored, attempted }
        => {
        /// Session whose append receipt is corrupt.
        session_id: SessionId,
        /// Canonical durable operation key of the corrupt receipt.
        operation_key: String,
        stored: u64,
        /// Count carried by the retry.
        attempted: u64,
    }
        => Self::AppendReceiptRequestedNodeCountCorrupt { session_id: session_id.clone(), operation_key: operation_key.clone(), stored: *stored, attempted: *attempted }
        => Self::AppendReceiptRequestedNodeCountCorrupt { .. }
        => "append_receipt_requested_node_count_corrupt"
        => crate::ToolFailureClass::Internal;
/// A durable plugin-owned record contained a value outside its declared
    /// representation. Retrying cannot repair the stored bytes.
    #[error("stored {record_kind} data is corrupt: {message}")]
    StoredDataCorrupt {
        /// Stable name of the durable record whose payload was unreadable.
        record_kind: String,
        /// Backend diagnostic describing the malformed field or payload.
        message: String,
    }
        => PluginError::StoredDataCorrupt { record_kind, message }
        => {
        /// Stable name of the durable record whose payload was unreadable.
        record_kind: String,
        /// Backend diagnostic describing the malformed field or payload.
        message: String,
    }
        => Self::StoredDataCorrupt { record_kind: record_kind.clone(), message: message.clone() }
        => Self::StoredDataCorrupt { .. }
        => "stored_data_corrupt"
        => crate::ToolFailureClass::Internal;
/// A backend-owned authoritative clock produced a value before the Unix
    /// epoch, outside the runtime clock contract.
    #[error("{clock} returned a pre-Unix-epoch millisecond value: {epoch_ms}")]
    ClockBeforeUnixEpoch { clock: String, epoch_ms: i64 }
        => PluginError::ClockBeforeUnixEpoch { clock, epoch_ms }
        => { clock: String, epoch_ms: i64 }
        => Self::ClockBeforeUnixEpoch { clock: clock.clone(), epoch_ms: *epoch_ms }
        => Self::ClockBeforeUnixEpoch { .. }
        => "clock_before_unix_epoch"
        => crate::ToolFailureClass::Internal;
#[error("process handle `{process_id}` is not live or visible in this session")]
    ProcessNotVisible { process_id: ProcessId }
        => PluginError::ProcessNotVisible { process_id }
        => { process_id: ProcessId }
        => Self::ProcessNotVisible { process_id: process_id.clone() }
        => Self::ProcessNotVisible { .. }
        => "process_not_visible"
        => crate::ToolFailureClass::InvalidRequest;
/// A session-only operation ran under a process runtime. A process has
    /// no session and no agent frame of its own, and is never handed its
    /// originator's session as a stand-in.
    #[error("`{operation}` needs a session runtime, but process `{process_id}` owns this one")]
    NotASessionRuntime {
        operation: String,
        process_id: ProcessId,
    }
        => PluginError::NotASessionRuntime { operation, process_id }
        => {
        operation: String,
        process_id: ProcessId,
    }
        => Self::NotASessionRuntime { operation: operation.clone(), process_id: process_id.clone() }
        => Self::NotASessionRuntime { .. }
        => "not_a_session_runtime"
        => crate::ToolFailureClass::InvalidRequest;
/// An external or host completion named a stored attachment that has no
    /// upload evidence: its source was ended and swept, so the output would
    /// reference bytes nothing can read. Nothing is recorded.
    #[error("process output attachment `{digest}` is no longer available")]
    ProcessOutputAttachmentUnavailable { digest: crate::AttachmentId }
        => PluginError::ProcessOutputAttachmentUnavailable { digest }
        => { digest: crate::AttachmentId }
        => Self::ProcessOutputAttachmentUnavailable { digest: digest.clone() }
        => Self::ProcessOutputAttachmentUnavailable { .. }
        => "process_output_attachment_unavailable"
        => crate::ToolFailureClass::InvalidRequest;
/// An operation referenced a process id that the registry never knew.
    #[error("unknown process `{process_id}`")]
    ProcessUnknown { process_id: ProcessId }
        => PluginError::ProcessUnknown { process_id }
        => { process_id: ProcessId }
        => Self::ProcessUnknown { process_id: process_id.clone() }
        => Self::ProcessUnknown { .. }
        => "process_unknown"
        => crate::ToolFailureClass::InvalidRequest;
/// A Process Change Feed cursor predates deletion history removed by
    /// Tombstone Compaction. The consumer must perform a full relist before
    /// resuming from the reported horizon.
    #[error(
        "process change cursor {requested_cursor:?} is below tombstone-compaction horizon {tombstone_compaction_horizon:?}; a full relist is required"
    )]
    ProcessChangeCursorPruned {
        requested_cursor: crate::ProcessChangeCursor,
        tombstone_compaction_horizon: crate::ProcessChangeCursor,
    }
        => PluginError::ProcessChangeCursorPruned { requested_cursor, tombstone_compaction_horizon }
        => {
        requested_cursor: crate::ProcessChangeCursor,
        tombstone_compaction_horizon: crate::ProcessChangeCursor,
    }
        => Self::ProcessChangeCursorPruned { requested_cursor: *requested_cursor, tombstone_compaction_horizon: *tombstone_compaction_horizon }
        => Self::ProcessChangeCursorPruned { .. }
        => "process_change_cursor_pruned"
        => crate::ToolFailureClass::Internal;
    /// The cursor predates retained subscription deletion evidence. Resync
    /// through the atomic subscription snapshot before continuing.
    #[error(
        "trigger subscription change cursor {requested_cursor:?} is below tombstone-compaction horizon {tombstone_compaction_horizon:?}; a full relist is required"
    )]
    TriggerSubscriptionChangeCursorPruned {
        requested_cursor: crate::TriggerSubscriptionChangeCursor,
        tombstone_compaction_horizon: crate::TriggerSubscriptionChangeCursor,
    }
        => PluginError::TriggerSubscriptionChangeCursorPruned { requested_cursor, tombstone_compaction_horizon }
        => {
        requested_cursor: crate::TriggerSubscriptionChangeCursor,
        tombstone_compaction_horizon: crate::TriggerSubscriptionChangeCursor,
    }
        => Self::TriggerSubscriptionChangeCursorPruned { requested_cursor: *requested_cursor, tombstone_compaction_horizon: *tombstone_compaction_horizon }
        => Self::TriggerSubscriptionChangeCursorPruned { .. }
        => "trigger_subscription_change_cursor_pruned"
        => crate::ToolFailureClass::Internal;
/// A read of one process's events starts below the prefix its host released
    /// (`release_process_events`). The events at or below the horizon keep
    /// their sequence and replay identity but no longer carry their payload;
    /// the reader resumes after the reported horizon.
    #[error(
        "process `{process_id}` events through sequence {released_through} were released by the host"
    )]
    ProcessEventsReleased {
        process_id: ProcessId,
        /// The highest released sequence: reads resume strictly after it.
        released_through: u64,
    }
        => PluginError::ProcessEventsReleased { process_id, released_through }
        => {
        process_id: ProcessId,
        /// The highest released sequence: reads resume strictly after it.
        released_through: u64,
    }
        => Self::ProcessEventsReleased { process_id: process_id.clone(), released_through: *released_through }
        => Self::ProcessEventsReleased { .. }
        => "process_events_released"
        => crate::ToolFailureClass::InvalidRequest;
#[error(transparent)]
    RuntimeEffectController (#[from] crate::RuntimeEffectControllerError)
        => PluginError::RuntimeEffectController(source)
        => (ToolIntentRuntimeFailure)
        => Self::RuntimeEffectController(ToolIntentRuntimeFailure { code: source.code.clone(), message: source.message.clone(), cause: source.cause.clone(), summary: source.summary.clone(), turn_input_acceptance: None })
        => Self::RuntimeEffectController(source)
        => source.code.as_str()
        => source.failure_class();
#[error("process execution authority for `{process_id}` is missing or superseded")]
    ProcessExecutionSuperseded { process_id: ProcessId }
        => PluginError::ProcessExecutionSuperseded { process_id }
        => { process_id: ProcessId }
        => Self::ProcessExecutionSuperseded { process_id: process_id.clone() }
        => Self::ProcessExecutionSuperseded { .. }
        => "process_execution_superseded"
        => crate::ToolFailureClass::Unavailable;
#[error("monotonic counter `{counter}` cannot advance past {current}")]
    MonotonicCounterOverflow { counter: String, current: u64 }
        => PluginError::MonotonicCounterOverflow { counter, current }
        => { counter: String, current: u64 }
        => Self::MonotonicCounterOverflow { counter: counter.clone(), current: *current }
        => Self::MonotonicCounterOverflow { .. }
        => "monotonic_counter_overflow"
        => crate::ToolFailureClass::Internal;
#[error(
        "process outcome is no longer retained (terminal state `{terminal_label}`, pruned at {pruned_at_ms}ms)"
    )]
    ProcessNoLongerRetained {
        terminal_label: crate::RetiredProcessStatus,
        pruned_at_ms: u64,
    }
        => PluginError::ProcessNoLongerRetained { terminal_label, pruned_at_ms }
        => {
        terminal_label: crate::RetiredProcessStatus,
        pruned_at_ms: u64,
    }
        => Self::ProcessNoLongerRetained { terminal_label: *terminal_label, pruned_at_ms: *pruned_at_ms }
        => Self::ProcessNoLongerRetained { .. }
        => "process_no_longer_retained"
        => crate::ToolFailureClass::InvalidRequest;
/// A recovery would end a process that a later segment already carries
    /// (FIG-3820).
    #[error("process `{process_id}` is carried by its segment {segment_ordinal}")]
    ProcessHandedOver {
        process_id: ProcessId,
        segment_ordinal: u64,
    }
        => PluginError::ProcessHandedOver { process_id, segment_ordinal }
        => {
        process_id: ProcessId,
        segment_ordinal: u64,
    }
        => Self::ProcessHandedOver { process_id: process_id.clone(), segment_ordinal: *segment_ordinal }
        => Self::ProcessHandedOver { .. }
        => "process_handed_over"
        => crate::ToolFailureClass::InvalidRequest;
#[error("process `{process_id}` is already terminal in state `{status:?}`")]
    ProcessAlreadyTerminal {
        process_id: ProcessId,
        status: crate::ProcessStatus,
    }
        => PluginError::ProcessAlreadyTerminal { process_id, status }
        => {
        process_id: ProcessId,
        status: crate::ProcessStatus,
    }
        => Self::ProcessAlreadyTerminal { process_id: process_id.clone(), status: *status }
        => Self::ProcessAlreadyTerminal { .. }
        => "process_already_terminal"
        => crate::ToolFailureClass::InvalidRequest;
#[error(
        "terminal process status `{declared_status:?}` contradicts outcome status `{outcome_status:?}`"
    )]
    ProcessTerminalOutcomeMismatch {
        declared_status: crate::ProcessStatus,
        outcome_status: Option<crate::ProcessStatus>,
    }
        => PluginError::ProcessTerminalOutcomeMismatch { declared_status, outcome_status }
        => {
        declared_status: crate::ProcessStatus,
        outcome_status: Option<crate::ProcessStatus>,
    }
        => Self::ProcessTerminalOutcomeMismatch { declared_status: *declared_status, outcome_status: *outcome_status }
        => Self::ProcessTerminalOutcomeMismatch { .. }
        => "process_terminal_outcome_mismatch"
        => crate::ToolFailureClass::InvalidRequest;
#[error("process event type `{event_type}` is reserved for its dedicated registry mutation")]
    ReservedProcessEvent { event_type: String }
        => PluginError::ReservedProcessEvent { event_type }
        => { event_type: String }
        => Self::ReservedProcessEvent { event_type: event_type.clone() }
        => Self::ReservedProcessEvent { .. }
        => "reserved_process_event"
        => crate::ToolFailureClass::InvalidRequest;
/// A stored wake-delivery row is keyed by an id other than the one its wake computes.
    #[error("wake delivery row `{delivery_id}` holds a wake whose identity is `{wake_id}`")]
    WakeDeliveryIdentityMismatch { delivery_id: String, wake_id: String }
        => PluginError::WakeDeliveryIdentityMismatch { delivery_id, wake_id }
        => { delivery_id: String, wake_id: String }
        => Self::WakeDeliveryIdentityMismatch { delivery_id: delivery_id.clone(), wake_id: wake_id.clone() }
        => Self::WakeDeliveryIdentityMismatch { .. }
        => "wake_delivery_identity_mismatch"
        => crate::ToolFailureClass::InvalidRequest;
#[error(
        "process wake delivery format version {found} is incompatible with version {expected}; drain in-flight sessions on the old build before deploying this build, or recreate development/test stores"
    )]
    ProcessWakeDeliveryFormatVersionMismatch { expected: u32, found: u32 }
        => PluginError::ProcessWakeDeliveryFormatVersionMismatch { expected, found }
        => { expected: u32, found: u32 }
        => Self::ProcessWakeDeliveryFormatVersionMismatch { expected: *expected, found: *found }
        => Self::ProcessWakeDeliveryFormatVersionMismatch { .. }
        => "process_wake_delivery_format_version_mismatch"
        => crate::ToolFailureClass::InvalidRequest;
/// A process-registry continuation was passed to a backend other than the
    /// backend that issued it.
    #[error("process registry cursor belongs to backend `{actual}`, not `{expected}`")]
    ProcessRegistryCursorBackendMismatch { expected: String, actual: String }
        => PluginError::ProcessRegistryCursorBackendMismatch { expected, actual }
        => { expected: String, actual: String }
        => Self::ProcessRegistryCursorBackendMismatch { expected: expected.clone(), actual: actual.clone() }
        => Self::ProcessRegistryCursorBackendMismatch { .. }
        => "process_registry_cursor_backend_mismatch"
        => crate::ToolFailureClass::InvalidRequest;
}

impl<R> From<crate::MaintenanceFailure<R>> for PluginError {
    fn from(error: crate::MaintenanceFailure<R>) -> Self {
        match error.stop {
            crate::MaintenanceStop::Failed(error) => Self::from(error),
            crate::MaintenanceStop::Refused(refusal) => Self::Session(refusal.to_string()),
        }
    }
}

impl From<crate::AttachmentStoreError> for PluginError {
    fn from(error: crate::AttachmentStoreError) -> Self {
        crate::RuntimeEffectControllerError::output_retention_failed(&error).into()
    }
}

/// Blank text offered as an identity is the store's refusal of it.
impl From<lash_sansio::BlankIdentity> for PluginError {
    fn from(error: lash_sansio::BlankIdentity) -> Self {
        crate::StoreError::from(error).into()
    }
}

impl From<crate::StoreError> for PluginError {
    /// A store error at a plugin-facing port, with its class
    /// ([`StoreError::runtime_code`](crate::StoreError::runtime_code)) kept:
    /// a fault of the substrate is [`Self::StoreUnavailable`], a refusal with
    /// a typed twin here is that twin, and every other refusal travels as
    /// the runtime error the store classifies it as.
    fn from(error: crate::StoreError) -> Self {
        if let Some(fault) = crate::store::StoreFault::of_store_error(&error) {
            return Self::StoreUnavailable { fault };
        }
        if let Some(refusal) = crate::store::StoreRefusal::of_store_error(&error) {
            return Self::StoreRefusal(refusal);
        }
        match error {
            crate::StoreError::SessionHeadOwned { session_id, owner } => {
                Self::SessionHeadOwned { session_id, owner }
            }
            // Bytes the store cannot decode stay undecodable: typed and
            // terminal, never a failure a redrive repeats.
            crate::StoreError::StoredDataCorrupt {
                record_kind,
                message,
            } => Self::StoredDataCorrupt {
                record_kind: record_kind.to_string(),
                message,
            },
            crate::StoreError::AppendOperationIdentityConflict {
                session_id,
                operation_key,
            } => Self::AppendOperationIdentityConflict {
                session_id,
                operation_key,
            },
            crate::StoreError::AppendReceiptRequestedNodeCountCorrupt {
                session_id,
                operation_key,
                stored,
                attempted,
            } => Self::AppendReceiptRequestedNodeCountCorrupt {
                session_id,
                operation_key,
                stored,
                attempted,
            },
            crate::StoreError::MonotonicCounterOverflow { counter, current } => {
                Self::MonotonicCounterOverflow {
                    counter: counter.to_string(),
                    current,
                }
            }
            crate::StoreError::SessionExecutionLeaseExpired { session_id } => {
                Self::SessionExecutionLeaseLost { session_id }
            }
            error => Self::Runtime(error.runtime_error()),
        }
    }
}

impl From<crate::store::StoreFault> for PluginError {
    fn from(fault: crate::store::StoreFault) -> Self {
        Self::StoreUnavailable { fault }
    }
}

impl PluginError {
    /// A failure of the infrastructure behind a plugin service that names no
    /// typed cause: the attempt's, never the operation's answer. It travels
    /// under the live code a session-seam failure settles as, so a redrive
    /// repairs it and nothing records it.
    pub fn attempt_fault(message: impl Into<String>) -> Self {
        Self::Runtime(crate::RuntimeError::new(
            crate::RuntimeErrorCode::PluginSessionManager,
            message,
        ))
    }

    /// A store error met while doing `context`, classified as
    /// [`From<StoreError>`](Self::from) classifies it. A typed variant keeps
    /// its fields; a refusal carried as a runtime error names `context`.
    pub fn of_store_error(context: impl std::fmt::Display, error: crate::StoreError) -> Self {
        match Self::from(error) {
            Self::Runtime(mut error) => {
                error.message = format!("{context}: {}", error.message);
                Self::Runtime(error)
            }
            typed => typed,
        }
    }
}

/// The decided posture of a [`PluginError`]: the one classification its
/// retry projections, its turn failure and its effect-controller error read.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PluginErrorClass {
    /// Retrying the identical plugin operation is explicitly safe.
    Retryable,
    /// A fact about this attempt: the identical call is refused again, and a
    /// redrive under fresh authority is not.
    Redrivable,
    /// Retrying cannot succeed without changing input, durable state,
    /// configuration, or wiring.
    Terminal,
}

impl PluginError {
    /// The decided posture of this error.
    ///
    /// This is the single classification site: the match is exhaustive, so a
    /// new variant does not compile until it is deliberately classified.
    pub fn class(&self) -> PluginErrorClass {
        use PluginErrorClass::{Redrivable, Retryable, Terminal};
        match self {
            Self::Operation(failure) => plugin_error_class(failure.class),
            Self::HookFailures { causes } => plugin_error_class(
                causes
                    .iter()
                    .map(|cause| cause.failure.class)
                    .min_by_key(|class| class.precedence())
                    .unwrap_or(super::PluginFailureClass::Terminal),
            ),
            // the substrate faulted, or the owning shift releases the head
            // at its boundary.
            Self::StoreUnavailable { .. } | Self::SessionHeadOwned { .. } => Retryable,
            Self::Runtime(error) => {
                if error.is_retryable() {
                    Retryable
                } else if error.is_terminal() {
                    Terminal
                } else {
                    Redrivable
                }
            }
            Self::RuntimeEffectController(error) => {
                if error.is_terminal() {
                    Terminal
                } else if error.clone().into_runtime_error().is_retryable() {
                    Retryable
                } else {
                    Redrivable
                }
            }
            // a trigger operation the store did not carry out names no cause a
            // retry of the identical request is known to clear.
            Self::TriggerOperation(error) => {
                if error.is_terminal() {
                    Terminal
                } else {
                    Redrivable
                }
            }
            // a successor holds the lane or the process; this execution's
            // authority is gone and a redrive under a new one proceeds.
            Self::SessionExecutionLeaseLost { .. } | Self::ProcessExecutionSuperseded { .. } => {
                Redrivable
            }
            // a deliberate refusal over the operation's inputs, the durable
            // state it met, or the deployment's wiring. A store that did not
            // answer is `StoreUnavailable`, and infrastructure that did not
            // is an attempt fault ([`Self::attempt_fault`]): neither is a
            // session, registration or invoke error.
            Self::ProviderFailure { .. }
            | Self::UnusableSchema { .. }
            | Self::UnusableToolSchema { .. }
            | Self::ValueMismatch { .. }
            | Self::Session(_)
            | Self::Registration(_)
            | Self::ConfigRegistration(_)
            | Self::Invoke(_)
            | Self::State(_)
            | Self::Declaration(_)
            | Self::Format(_)
            | Self::StoreRefusal(_)
            | Self::StoredDataCorrupt { .. }
            | Self::MissingRecordedProcessConfig { .. }
            | Self::InvalidTriggerTarget { .. }
            | Self::ProcessCancelConflict { .. }
            | Self::ParentEnded { .. }
            | Self::StartKeyConflict { .. }
            | Self::TriggerDeliveryBound { .. }
            | Self::TriggerDeliveryRetired { .. }
            | Self::InvalidToolDiscovery { .. }
            | Self::InvalidBatchMaximum { .. }
            | Self::ResidentToolContractUnavailable { .. }
            | Self::ResidentToolDuplicateId { .. }
            | Self::ResidentToolDuplicateName { .. }
            | Self::ToolRegistrationRefused { .. }
            | Self::ResidentToolRouteUnavailable { .. }
            | Self::SessionAlreadyExists { .. }
            | Self::SessionInitTooLarge { .. }
            | Self::MissingRecordedSessionConfig { .. }
            | Self::RecordedSessionConfigConflict { .. }
            | Self::AppendOperationIdentityConflict { .. }
            | Self::AppendReceiptRequestedNodeCountCorrupt { .. }
            | Self::ClockBeforeUnixEpoch { .. }
            | Self::ProcessNotVisible { .. }
            | Self::NotASessionRuntime { .. }
            | Self::ProcessOutputAttachmentUnavailable { .. }
            | Self::ProcessUnknown { .. }
            | Self::ProcessChangeCursorPruned { .. }
            | Self::TriggerSubscriptionChangeCursorPruned { .. }
            | Self::ProcessEventsReleased { .. }
            | Self::MonotonicCounterOverflow { .. }
            | Self::ProcessNoLongerRetained { .. }
            | Self::ProcessHandedOver { .. }
            | Self::ProcessAlreadyTerminal { .. }
            | Self::ProcessTerminalOutcomeMismatch { .. }
            | Self::ReservedProcessEvent { .. }
            | Self::WakeDeliveryIdentityMismatch { .. }
            | Self::ProcessWakeDeliveryFormatVersionMismatch { .. }
            | Self::ProcessRegistryCursorBackendMismatch { .. } => Terminal,
        }
    }

    /// Settles a plugin hook's failure by its class (FIG-3575).
    ///
    /// A live fault the hook ran into aborts the turn under the code the
    /// effect controller carries it by, so a redrive repairs it. A terminal
    /// error that carries a runtime code of its own (a store refusal, corrupt
    /// stored data, a carried runtime error a turn does not record) keeps it.
    /// Every other terminal error is a deliberate refusal over the turn's
    /// inputs or durable state: an outcome spelled as `refusal`, recorded or
    /// settled once instead of retried.
    pub fn into_turn_failure(self, refusal: crate::RuntimeErrorCode) -> crate::RuntimeError {
        match self {
            error @ (Self::Operation(_) | Self::HookFailures { .. }) => {
                crate::RuntimeEffectControllerError::from(error).into_runtime_error()
            }
            Self::UnusableSchema { source } => {
                crate::RuntimeError::new(refusal, source.to_string())
                    .with_cause(crate::RuntimeErrorCause::SchemaRefused { source })
            }
            Self::UnusableToolSchema { source } => {
                crate::RuntimeError::new(refusal, source.to_string())
                    .with_cause(crate::RuntimeErrorCause::ToolSchemaRefused { source })
            }
            Self::ValueMismatch { context, source } => {
                crate::RuntimeError::new(refusal, format!("invalid {context}: {source}"))
                    .with_cause(crate::RuntimeErrorCause::ValueMismatch {
                        context: context.into_boxed_str(),
                        source,
                    })
            }
            Self::Runtime(error) if keeps_its_code(&error) => error,
            Self::RuntimeEffectController(error)
                if error.turn_failure_cause().aborts_invocation()
                    || keeps_its_code(&error.clone().into_runtime_error()) =>
            {
                error.into_runtime_error()
            }
            refused @ (Self::Runtime(_) | Self::RuntimeEffectController(_)) => {
                crate::RuntimeError::new(refusal, refused.to_string())
            }
            error => match error.class() {
                PluginErrorClass::Retryable | PluginErrorClass::Redrivable => {
                    crate::RuntimeEffectControllerError::from(error).into_runtime_error()
                }
                PluginErrorClass::Terminal => {
                    let carried = crate::RuntimeEffectControllerError::from(error.clone())
                        .into_runtime_error();
                    if keeps_its_code(&carried) {
                        carried
                    } else {
                        crate::RuntimeError::new(refusal, error.to_string())
                    }
                }
            },
        }
    }

    /// The park this failure puts a process in, when it is a refusal that
    /// parks ([`ParkReason::of_error`](crate::store::ParkReason::of_error)):
    /// the body refused to replay its journal with nothing dispatched.
    pub fn park_reason(&self) -> Option<crate::store::ParkReason> {
        match self {
            Self::Runtime(error) => crate::store::ParkReason::of_error(error),
            Self::RuntimeEffectController(error) => {
                crate::store::ParkReason::of_error(&error.clone().into_runtime_error())
            }
            error @ (Self::Operation(_) | Self::HookFailures { .. }) => {
                crate::store::ParkReason::of_error(
                    &crate::RuntimeEffectControllerError::from(error.clone()).into_runtime_error(),
                )
            }
            _ => None,
        }
    }

    /// The text an engine fails a retried attempt with: this error's display
    /// and, when it carries a typed attempt fault, that fault's record
    /// ([`RuntimeEffectControllerError::attempt_failure_text`](crate::RuntimeEffectControllerError::attempt_failure_text)),
    /// so the park of the engine's exhausted retries keeps the fault typed.
    #[must_use]
    pub fn attempt_failure_text(&self) -> String {
        match self {
            Self::Runtime(error) => error.attempt_failure_text(),
            Self::RuntimeEffectController(error) => error.attempt_failure_text(),
            error => error.to_string(),
        }
    }

    /// Whether retrying the identical plugin operation is explicitly safe.
    pub fn is_retryable(&self) -> bool {
        self.class() == PluginErrorClass::Retryable
    }

    /// Whether retrying the identical plugin operation cannot succeed without
    /// changing durable state, configuration, or wiring.
    pub fn is_terminal(&self) -> bool {
        self.class() == PluginErrorClass::Terminal
    }
}

/// Whether a failed turn carries `error` under its own code instead of the
/// failing hook's: it aborts the invocation, or its code names a fact of the
/// store or the deployment that a host reads.
fn keeps_its_code(error: &crate::RuntimeError) -> bool {
    error.turn_failure_cause().aborts_invocation()
        || matches!(
            error.cause.as_ref(),
            Some(
                crate::RuntimeErrorCause::ProviderFailure { .. }
                    | crate::RuntimeErrorCause::ModuleArtifactRefused { .. }
                    | crate::RuntimeErrorCause::PluginFormat { .. }
                    | crate::RuntimeErrorCause::SchemaRefused { .. }
                    | crate::RuntimeErrorCause::ToolSchemaRefused { .. }
                    | crate::RuntimeErrorCause::ValueMismatch { .. }
            )
        )
        || error.is_session_retirement()
        || error.store_refusal().is_some()
        || matches!(
            error.code,
            crate::RuntimeErrorCode::RuntimeStoreCorrupt
                | crate::RuntimeErrorCode::StoreRefused
                | crate::RuntimeErrorCode::RecordedTerminationUnavailable
                | crate::RuntimeErrorCode::MissingRecordedProcessConfig
        )
}

fn plugin_error_class(class: super::PluginFailureClass) -> PluginErrorClass {
    match class {
        super::PluginFailureClass::Retryable => PluginErrorClass::Retryable,
        super::PluginFailureClass::Terminal => PluginErrorClass::Terminal,
        super::PluginFailureClass::Redrivable | super::PluginFailureClass::Parked => {
            PluginErrorClass::Redrivable
        }
    }
}

pub(crate) fn runtime_operation_failure(
    error: crate::RuntimeError,
) -> super::PluginOperationFailure {
    error.into()
}

impl From<PluginError> for super::PluginOperationFailure {
    fn from(error: PluginError) -> Self {
        if let PluginError::Operation(failure) = error {
            return *failure;
        }
        let class = error.class();
        let payload = match serde_json::to_value(&error) {
            Ok(payload) => payload,
            Err(error) => {
                return runtime_operation_failure(crate::RuntimeError::new(
                    crate::RuntimeErrorCode::RecordEncodingFailed,
                    error.to_string(),
                ));
            }
        };
        let mut failure = runtime_operation_failure(
            crate::RuntimeEffectControllerError::from(error).into_runtime_error(),
        );
        if failure.class != super::PluginFailureClass::Parked {
            failure.class = match class {
                PluginErrorClass::Retryable => super::PluginFailureClass::Retryable,
                PluginErrorClass::Redrivable => super::PluginFailureClass::Redrivable,
                PluginErrorClass::Terminal => super::PluginFailureClass::Terminal,
            };
        }
        failure.error_type = "lash.plugin".into();
        failure.payload = payload;
        failure
    }
}

#[cfg(test)]
mod classification_tests {
    #[test]
    fn schema_causes_retain_typed_fields_through_recorded_intent_and_runtime() {
        let admission = crate::JsonSchema::admit(serde_json::Value::Null)
            .expect_err("null cannot be admitted as a schema");
        let catalog = crate::ToolDefinition::raw(
            "bad",
            "bad",
            "bad",
            serde_json::Value::Null,
            serde_json::json!({}),
        )
        .expect_err("a tool cannot publish an unusable schema");
        let mismatch = crate::ValueMismatch {
            instance_path: "/count".into(),
            message: "integer required".into(),
        };
        for (plugin, source, expected_runtime) in [
            (
                super::PluginError::UnusableSchema {
                    source: Box::new(admission.clone()),
                },
                serde_json::to_value(&admission).expect("encode admission cause"),
                crate::RuntimeErrorCause::SchemaRefused {
                    source: Box::new(admission),
                },
            ),
            (
                super::PluginError::UnusableToolSchema {
                    source: Box::new(catalog.clone()),
                },
                serde_json::to_value(&catalog).expect("encode catalog cause"),
                crate::RuntimeErrorCause::ToolSchemaRefused {
                    source: Box::new(catalog),
                },
            ),
            (
                super::PluginError::ValueMismatch {
                    context: "payload".into(),
                    source: Box::new(mismatch.clone()),
                },
                serde_json::to_value(&mismatch).expect("encode value mismatch"),
                crate::RuntimeErrorCause::ValueMismatch {
                    context: "payload".into(),
                    source: Box::new(mismatch),
                },
            ),
        ] {
            let command = super::ToolIntentCommandFailure::from(&plugin);
            assert_eq!(
                command.failure_class(),
                crate::ToolFailureClass::InvalidRequest
            );
            let recorded = serde_json::to_value(&command).expect("record command refusal");
            assert_eq!(recorded["message"]["source"], source);
            let replayed: super::ToolIntentCommandFailure =
                serde_json::from_value(recorded).expect("replay command refusal");
            assert_eq!(replayed, command);
            let controller = crate::RuntimeEffectControllerError::from(plugin.clone());
            for carried in [
                plugin,
                super::PluginError::RuntimeEffectController(controller.clone()),
                super::PluginError::Runtime(controller.into_runtime_error()),
            ] {
                assert_eq!(carried.class(), super::PluginErrorClass::Terminal);
                let runtime = carried.into_turn_failure(crate::RuntimeErrorCode::Plugin);
                assert!(runtime.is_terminal());
                assert!(!runtime.is_retryable());
                assert_eq!(
                    serde_json::to_value(runtime.cause).expect("encode runtime cause"),
                    serde_json::to_value(Some(&expected_runtime)).expect("encode expected cause"),
                );
            }
        }
    }

    #[test]
    fn format_refusal_retains_fields_through_intent_and_runtime_boundaries() {
        let refusal = super::super::FormatRefusal {
            plugin: "unreadable-plugin".into(),
            namespace: super::super::FormatNamespace::Config,
            stored: crate::FormatVersion::new(7).expect("nonzero format"),
            readable: crate::FormatVersion::ONE,
        };
        let expected = serde_json::json!({
            "plugin": "unreadable-plugin", "namespace": "config", "stored": 7, "readable": 1,
        });
        let plugin = super::PluginError::from(refusal);
        let controller = crate::RuntimeEffectControllerError::from(plugin.clone());
        for error in [
            plugin,
            super::PluginError::RuntimeEffectController(controller.clone()),
            super::PluginError::Runtime(controller.into_runtime_error()),
        ] {
            let cause = super::ToolIntentCommandFailure::from(&error);
            assert_eq!(
                cause.failure_class(),
                crate::ToolFailureClass::InvalidRequest
            );
            assert_eq!(cause.code(), crate::RuntimeErrorCode::Plugin.as_str());
            let encoded = serde_json::to_value(&cause).expect("record command cause");
            let fields = if encoded["type"] == "format" {
                &encoded["message"]
            } else {
                &encoded["message"]["cause"]["refusal"]
            };
            assert_eq!(fields, &expected);
            let replayed: super::ToolIntentCommandFailure =
                serde_json::from_value(encoded).expect("replay command cause");
            assert_eq!(replayed, cause);
        }
    }

    use super::*;

    #[test]
    fn head_ownership_survives_plugin_journaling_and_error_conversions() {
        let session_id = SessionId::from("busy-session");
        for owner in [
            crate::store::SessionHeadOwner::Run {
                run: crate::TurnId::from("bound-run"),
            },
            crate::store::SessionHeadOwner::FollowOn {
                follow_on: crate::TurnId::from("owed-follow-on"),
            },
            crate::store::SessionHeadOwner::CommandLane { enqueue_seq: 7 },
        ] {
            let plugin = PluginError::from(crate::StoreError::SessionHeadOwned {
                session_id: session_id.clone(),
                owner: owner.clone(),
            });
            let encoded = serde_json::to_vec(&plugin).expect("encode plugin journal");
            let plugin: PluginError =
                serde_json::from_slice(&encoded).expect("replay plugin journal");
            assert!(matches!(
                &plugin,
                PluginError::SessionHeadOwned { session_id: found_session, owner: found_owner }
                    if *found_session == session_id && *found_owner == owner
            ));
            assert!(plugin.is_retryable());
            assert!(!plugin.is_terminal());
            let controller = crate::RuntimeEffectControllerError::from(plugin.clone());
            assert_eq!(controller.code, crate::RuntimeErrorCode::SessionHeadOwned);
            let runtime = plugin.into_turn_failure(crate::RuntimeErrorCode::Plugin);
            assert_eq!(runtime.code, crate::RuntimeErrorCode::SessionHeadOwned);
            assert!(runtime.is_retryable());
            assert!(!runtime.is_terminal());
        }
    }

    #[test]
    fn store_refusals_survive_plugin_journaling_and_turn_failure_mapping() {
        use crate::compat::{CompatRefusal, VersionRange};
        use crate::store::StoreRefusal;
        for refusal in [
            StoreRefusal::WriterFenced {
                recorded: 2,
                writable: VersionRange::exactly(1),
            },
            StoreRefusal::Incompatible {
                refusal: CompatRefusal::Unstamped {
                    component: "postgres".into(),
                    writing_release: None,
                },
            },
        ] {
            let plugin = PluginError::from(refusal.clone().into_store_error());
            let encoded = serde_json::to_vec(&plugin).expect("encode plugin journal");
            let plugin: PluginError =
                serde_json::from_slice(&encoded).expect("replay plugin journal");
            assert!(matches!(&plugin, PluginError::StoreRefusal(found) if *found == refusal));
            assert!(plugin.is_terminal());
            assert!(!plugin.is_retryable());
            let controller = crate::RuntimeEffectControllerError::from(plugin.clone());
            assert_eq!(controller.code, refusal.code());
            for plugin in [
                plugin,
                PluginError::RuntimeEffectController(controller.clone()),
                PluginError::Runtime(controller.clone().into_runtime_error()),
            ] {
                let runtime = plugin.into_turn_failure(crate::RuntimeErrorCode::Plugin);
                assert_eq!(runtime.code, refusal.code());
                assert_eq!(runtime.cause, controller.cause);
                assert!(runtime.is_terminal());
            }
            let artifact = crate::ArtifactStoreError::from(refusal.clone().into_store_error());
            assert!(
                matches!(PluginError::from(artifact), PluginError::StoreRefusal(found) if found == refusal)
            );
        }
    }

    #[test]
    fn stored_corruption_keeps_its_fields_across_plugin_and_runtime_boundaries() {
        let expected = serde_json::json!({
            "kind": "stored_data_corrupt", "record_kind": "TurnCancelRequest", "message": "invalid scope",
        });
        let plugin = PluginError::from(crate::StoreError::StoredDataCorrupt {
            record_kind: "TurnCancelRequest",
            message: "invalid scope".into(),
        });
        assert!(plugin.is_terminal());
        let plugin: PluginError =
            serde_json::from_slice(&serde_json::to_vec(&plugin).unwrap()).unwrap();
        let controller = crate::RuntimeEffectControllerError::from(plugin.clone());
        for plugin in [
            plugin,
            PluginError::RuntimeEffectController(controller.clone()),
            PluginError::Runtime(controller.clone().into_runtime_error()),
        ] {
            let runtime = plugin.into_turn_failure(crate::RuntimeErrorCode::Plugin);
            assert_eq!(runtime.code, crate::RuntimeErrorCode::RuntimeStoreCorrupt);
            assert_eq!(serde_json::to_value(&runtime.cause).unwrap(), expected);
            assert!(runtime.is_terminal());
            assert!(!runtime.is_retryable());
        }
    }
}
