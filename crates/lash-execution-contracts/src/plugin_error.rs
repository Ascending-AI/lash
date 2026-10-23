use lash_vocabulary::identity::ProcessId;
use lash_vocabulary::identity::SessionId;

/// One refusal for a durable identity re-presented with different content.
///
/// The stores fence a re-submitted identity at the point it mutates: a process
/// registration fingerprint or a process-event replay key. Matching content
/// replays the first writer's result; differing content cannot, because the
/// identity is already bound. Every such site returns this one error so the
/// tool-intent front door can map both to a single typed host-facing refusal instead of matching prose (FIG-1489).
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
            return if class == lash_protocol_types::PluginFailureClass::Terminal {
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
    /// A package port refusal or infrastructure failure retains its typed cause.
    #[error("{0}")]
    Package (Box<crate::process::linking::EnginePortError>)
        => PluginError::Package(source)
        => (Box<crate::process::linking::EnginePortError>)
        => Self::Package(source.clone())
        => Self::Package(source)
        => "package"
        => source.failure_class();
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
    Operation(Box<lash_protocol_types::PluginOperationFailure>)
        => PluginError::Operation(failure)
        => (Box<lash_protocol_types::PluginOperationFailure>)
        => Self::Operation(failure.clone())
        => Self::Operation(failure)
        => failure.code.namespaced()
        => if failure.class == lash_protocol_types::PluginFailureClass::Terminal { crate::ToolFailureClass::InvalidRequest } else { crate::ToolFailureClass::Unavailable };
    #[error("plugin runtime event hooks failed: {causes:?}")]
    HookFailures { causes: Vec<lash_protocol_types::PluginHookFailure> }
        => PluginError::HookFailures { causes }
        => { causes: Vec<lash_protocol_types::PluginHookFailure> }
        => Self::HookFailures { causes: causes.clone() }
        => Self::HookFailures { causes }
        => crate::RuntimeErrorCode::Plugin.as_str()
        => if causes.iter().all(|cause| cause.failure.class == lash_protocol_types::PluginFailureClass::Terminal) { crate::ToolFailureClass::InvalidRequest } else { crate::ToolFailureClass::Unavailable };
/// A process cannot run without the behaviour its creation recorded.
    #[error("process engine `{engine_kind}` has no recorded configuration")]
    MissingRecordedProcessConfig { engine_kind: String }
        => PluginError::MissingRecordedProcessConfig { engine_kind }
        => { engine_kind: String }
        => Self::MissingRecordedProcessConfig { engine_kind: engine_kind.clone() }
        => Self::MissingRecordedProcessConfig { .. }
        => "missing_recorded_process_config"
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
/// A process environment was refused at admission, or a start named one
    /// that is not published (FIG-6034). A retry meets it again.
    #[error("{refusal}")]
    ProcessEnvRefused { refusal: lash_core_store::process_env::ProcessEnvRefusal }
        => PluginError::ProcessEnvRefused { refusal }
        => { refusal: lash_core_store::process_env::ProcessEnvRefusal }
        => Self::ProcessEnvRefused { refusal: refusal.clone() }
        => Self::ProcessEnvRefused { .. }
        => "process_env_refused"
        => crate::ToolFailureClass::InvalidRequest;
/// A host's successor request for a process was refused (FIG-6121, D2):
    /// nothing was recorded, and the process goes on as written.
    #[error("process `{process}` is not carried onto its successor: {refusal}")]
    ProcessSuccessorRefused {
        process: ProcessId,
        refusal: Box<crate::ProcessSuccessorRefusal>,
    }
        => PluginError::ProcessSuccessorRefused { process, refusal }
        => {
        process: ProcessId,
        refusal: Box<crate::ProcessSuccessorRefusal>,
    }
        => Self::ProcessSuccessorRefused { process: process.clone(), refusal: refusal.clone() }
        => Self::ProcessSuccessorRefused { .. }
        => "process_successor_refused"
        => crate::ToolFailureClass::InvalidRequest;
/// An installed package's namespace is closed to deferred resolution.
    #[error("package path `{path}` cannot be resolved dynamically; use a public member offered by its surface (no such member is offered by a deferred provider)")]
    PackageNamespace { path: String }
        => PluginError::PackageNamespace { path }
        => { path: String }
        => Self::PackageNamespace { path: path.clone() }
        => Self::PackageNamespace { .. }
        => "package_namespace"
        => crate::ToolFailureClass::InvalidRequest;
/// OP-INTERNAL: document code cannot resolve a package-private operation.
    #[error("internal operation `{operation}` may only be performed by its package bodies")]
    InternalOperation { operation: String }
        => PluginError::InternalOperation { operation }
        => { operation: String }
        => Self::InternalOperation { operation: operation.clone() }
        => Self::InternalOperation { .. }
        => "internal_operation"
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
/// An execution mode's tool presenter refused: the host's presentation
    /// config at a session's creation, or a section it could not present.
    /// `presenter` is the installed presenter id; `code` is its stable, dotted
    /// cause kind. `message` carries human-readable details.
    #[error("plugin `{plugin_id}`'s tool presenter `{presenter}` refused: {message}")]
    ToolPresentationRefused { plugin_id: String, presenter: String, code: String, message: String }
        => PluginError::ToolPresentationRefused { plugin_id, presenter, code, message }
        => { plugin_id: String, presenter: String, code: String, message: String }
        => Self::ToolPresentationRefused { plugin_id: plugin_id.clone(), presenter: presenter.clone(), code: code.clone(), message: message.clone() }
        => Self::ToolPresentationRefused { .. }
        => "tool_presentation_refused"
        => crate::ToolFailureClass::InvalidRequest;
    #[error("{source}")]
    UnusableSchema { source: Box<lash_vocabulary::json_schema::SchemaAdmissionError> }
        => PluginError::UnusableSchema { source }
        => { source: Box<lash_vocabulary::json_schema::SchemaAdmissionError> }
        => Self::UnusableSchema { source: source.clone() }
        => Self::UnusableSchema { .. }
        => "unusable_schema"
        => crate::ToolFailureClass::InvalidRequest;
    #[error("unusable tool schema: {source}")]
    UnusableToolSchema { source: Box<lash_vocabulary::ToolCatalogBuildError> }
        => PluginError::UnusableToolSchema { source }
        => { source: Box<lash_vocabulary::ToolCatalogBuildError> }
        => Self::UnusableToolSchema { source: source.clone() }
        => Self::UnusableToolSchema { .. }
        => "unusable_tool_schema"
        => crate::ToolFailureClass::InvalidRequest;
    #[error("invalid {context}: {source}")]
    ValueMismatch { context: String, source: Box<lash_vocabulary::json_schema::ValueMismatch> }
        => PluginError::ValueMismatch { context, source }
        => { context: String, source: Box<lash_vocabulary::json_schema::ValueMismatch> }
        => Self::ValueMismatch { context: context.clone(), source: source.clone() }
        => Self::ValueMismatch { .. }
        => "value_mismatch"
        => crate::ToolFailureClass::InvalidRequest;
/// An effective resident catalog member could not supply its immutable definition.
    #[error("resident tool `{operation}` ({tool_id}) has no contract")]
    ResidentToolContractUnavailable {
        tool_id: lash_vocabulary::tool_contract::ToolId,
        operation: String,
    }
        => PluginError::ResidentToolContractUnavailable { tool_id, operation }
        => {
        tool_id: lash_vocabulary::tool_contract::ToolId,
        operation: String,
    }
        => Self::ResidentToolContractUnavailable { tool_id: tool_id.clone(), operation: operation.clone() }
        => Self::ResidentToolContractUnavailable { .. }
        => "resident_tool_contract_unavailable"
        => crate::ToolFailureClass::Internal;
#[error("resident catalog repeats tool id `{tool_id}`")]
    ResidentToolDuplicateId { tool_id: lash_vocabulary::tool_contract::ToolId }
        => PluginError::ResidentToolDuplicateId { tool_id }
        => { tool_id: lash_vocabulary::tool_contract::ToolId }
        => Self::ResidentToolDuplicateId { tool_id: tool_id.clone() }
        => Self::ResidentToolDuplicateId { .. }
        => "resident_tool_duplicate_id"
        => crate::ToolFailureClass::Internal;
#[error("resident tool `{tool_id}` names no operation: {message}")]
    ResidentToolInvalidOperation { tool_id: lash_vocabulary::tool_contract::ToolId, message: String }
        => PluginError::ResidentToolInvalidOperation { tool_id, message }
        => { tool_id: lash_vocabulary::tool_contract::ToolId, message: String }
        => Self::ResidentToolInvalidOperation { tool_id: tool_id.clone(), message: message.clone() }
        => Self::ResidentToolInvalidOperation { .. }
        => "resident_tool_invalid_operation"
        => crate::ToolFailureClass::Internal;
#[error("resident tools `{first_tool}` and `{second_tool}` are both the operation `{operation}`")]
    ResidentToolDuplicateOperation { operation: String, first_tool: lash_vocabulary::tool_contract::ToolId, second_tool: lash_vocabulary::tool_contract::ToolId }
        => PluginError::ResidentToolDuplicateOperation { operation, first_tool, second_tool }
        => { operation: String, first_tool: lash_vocabulary::tool_contract::ToolId, second_tool: lash_vocabulary::tool_contract::ToolId }
        => Self::ResidentToolDuplicateOperation { operation: operation.clone(), first_tool: first_tool.clone(), second_tool: second_tool.clone() }
        => Self::ResidentToolDuplicateOperation { .. }
        => "resident_tool_duplicate_operation"
        => crate::ToolFailureClass::Internal;
/// A catalog member's declared execution is refused against the runtime's
    /// execution budgets: an inline-only tool declared above the ceiling.
    #[error("{source}")]
    ToolRegistrationRefused { source: Box<lash_vocabulary::tool_contract::RegistrationRefused> }
        => PluginError::ToolRegistrationRefused { source }
        => { source: Box<lash_vocabulary::tool_contract::RegistrationRefused> }
        => Self::ToolRegistrationRefused { source: source.clone() }
        => Self::ToolRegistrationRefused { .. }
        => "tool_registration_refused"
        => crate::ToolFailureClass::InvalidRequest;
/// The tools a run's providers advertise under its config do not build
    /// into one surface: two share a model-facing name, or a tool id. The
    /// host reconfigures one provider.
    #[error("{refusal}")]
    ToolSurfaceRefused { refusal: Box<crate::SurfaceRefusal> }
        => PluginError::ToolSurfaceRefused { refusal }
        => { refusal: Box<crate::SurfaceRefusal> }
        => Self::ToolSurfaceRefused { refusal: refusal.clone() }
        => Self::ToolSurfaceRefused { .. }
        => "tool_surface_refused"
        => crate::ToolFailureClass::InvalidRequest;
/// A fresh session create named an id the catalog already holds. A
    /// create never adopts an existing session (FIG-4112); replaying a
    /// recorded identity is a process run's `SessionTurn` initialisation,
    /// not a create.
    #[error("session `{session_id}` already exists")]
    SessionAlreadyExists { session_id: lash_vocabulary::identity::SessionId }
        => PluginError::SessionAlreadyExists { session_id }
        => { session_id: lash_vocabulary::identity::SessionId }
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
#[error("plugin preflight refused: {0}")]
    Preflight (super::PreflightIssue)
        => PluginError::Preflight(source)
        => (super::PreflightIssue)
        => Self::Preflight(source.clone())
        => Self::Preflight(_)
        => "preflight"
        => crate::ToolFailureClass::Unavailable;
#[error("provider `{owner}` cannot decode its configuration: {message}")]
    ProviderConfig { owner: String, message: String }
        => PluginError::ProviderConfig { owner, message }
        => { owner: String, message: String }
        => Self::ProviderConfig { owner: owner.clone(), message: message.clone() }
        => Self::ProviderConfig { .. }
        => "provider_config"
        => crate::ToolFailureClass::InvalidRequest;
#[error("provider `{provider}` belongs to another registry; expected {expected:?}")]
    ProviderRegistryMismatch { expected: super::RegistryKind, provider: super::ToolProviderHandle }
        => PluginError::ProviderRegistryMismatch { expected, provider }
        => { expected: lash_vocabulary::claims::RegistryKind, provider: lash_vocabulary::providers::ToolProviderHandle }
        => Self::ProviderRegistryMismatch { expected: *expected, provider: provider.clone() }
        => Self::ProviderRegistryMismatch { .. }
        => "provider_registry_mismatch"
        => crate::ToolFailureClass::InvalidRequest;
/// A plugin's config registration cannot stand on the deployment's plugin set.
#[error("config registration is invalid: {0}")]
    ConfigRegistration (crate::config_error::ConfigRegistrationError)
        => PluginError::ConfigRegistration(source)
        => (crate::config_error::ConfigRegistrationError)
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
    State (#[source] lash_core_store::plugin_state::PluginStateError)
        => PluginError::State(source)
        => (#[source] lash_core_store::plugin_state::PluginStateError)
        => Self::State(source.clone())
        => Self::State(source)
        => "state"
        => match source {
            lash_core_store::plugin_state::PluginStateError::InvalidKey { .. } | lash_core_store::plugin_state::PluginStateError::ValueTooLarge { .. } | lash_core_store::plugin_state::PluginStateError::StoreTooLarge { .. } => crate::ToolFailureClass::InvalidRequest,
            lash_core_store::plugin_state::PluginStateError::Unrecorded { .. } | lash_core_store::plugin_state::PluginStateError::EffectOwnerMismatch | lash_core_store::plugin_state::PluginStateError::Frontier { .. } | lash_core_store::plugin_state::PluginStateError::Encode { .. } | lash_core_store::plugin_state::PluginStateError::Decode { .. } => crate::ToolFailureClass::Internal,
            lash_core_store::plugin_state::PluginStateError::PublicationFenced { .. } => crate::ToolFailureClass::Unavailable,
        };
/// A factory's [`super::PluginDeclaration`] failed the composition's owner
    /// and format checks (FIG-4732): a session cannot materialize under a
    /// plugin set that misstates itself, before any plugin callback runs.
    #[error(transparent)]
    Declaration (#[from] crate::declaration_error::PluginDeclarationError)
        => PluginError::Declaration(source)
        => (#[from] crate::declaration_error::PluginDeclarationError)
        => Self::Declaration(source.clone())
        => Self::Declaration(_)
        => "declaration"
        => crate::ToolFailureClass::Internal;
    #[error(transparent)]
    Format (#[from] lash_core_store::plugin_state::FormatRefusal)
        => PluginError::Format(source)
        => (#[from] lash_core_store::plugin_state::FormatRefusal)
        => Self::Format(source.clone())
        => Self::Format(_)
        => crate::RuntimeErrorCode::Plugin.as_str()
        => crate::ToolFailureClass::InvalidRequest;
/// A typed linked-content or execution-use refusal from the store.
    #[error(transparent)]
    PackageStoreRefused (#[from] lash_durable::packages::PackageRefusal)
        => PluginError::PackageStoreRefused(source)
        => (#[from] lash_durable::packages::PackageRefusal)
        => Self::PackageStoreRefused(source.clone())
        => Self::PackageStoreRefused(_)
        => "package_store_refused"
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
    /// A roster continuation was reused with a different selection.
    #[error("process roster continuation requires its original filter")]
    ProcessRosterFilterMismatch {}
        => PluginError::ProcessRosterFilterMismatch {}
        => {}
        => Self::ProcessRosterFilterMismatch {}
        => Self::ProcessRosterFilterMismatch {}
        => "process_roster_filter_mismatch"
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
#[error("process event type `{event_type}` names no lifecycle kind")]
    UnknownProcessEventKind { event_type: String }
        => PluginError::UnknownProcessEventKind { event_type }
        => { event_type: String }
        => Self::UnknownProcessEventKind { event_type: event_type.clone() }
        => Self::UnknownProcessEventKind { .. }
        => "unknown_process_event_kind"
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
impl From<lash_vocabulary::identity::BlankIdentity> for PluginError {
    fn from(error: lash_vocabulary::identity::BlankIdentity) -> Self {
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
            Self::Package(source) => match source.failure_class() {
                crate::ToolFailureClass::Unavailable => Retryable,
                _ => Terminal,
            },
            Self::Operation(failure) => plugin_error_class(failure.class),
            Self::HookFailures { causes } => plugin_error_class(
                causes
                    .iter()
                    .map(|cause| cause.failure.class)
                    .min_by_key(|class| class.precedence())
                    .unwrap_or(lash_protocol_types::PluginFailureClass::Terminal),
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
            Self::PackageStoreRefused(_) => Terminal,
            Self::StoreRefusal(refusal) => {
                if refusal.is_terminal() {
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
            Self::PackageNamespace { .. }
            | Self::InternalOperation { .. }
            | Self::ProviderFailure { .. }
            | Self::UnusableSchema { .. }
            | Self::UnusableToolSchema { .. }
            | Self::ValueMismatch { .. }
            | Self::Session(_)
            | Self::Registration(_)
            | Self::Preflight(_)
            | Self::ProviderConfig { .. }
            | Self::ProviderRegistryMismatch { .. }
            | Self::ConfigRegistration(_)
            | Self::Invoke(_)
            | Self::State(_)
            | Self::Declaration(_)
            | Self::Format(_)
            | Self::StoredDataCorrupt { .. }
            | Self::MissingRecordedProcessConfig { .. }
            | Self::ProcessCancelConflict { .. }
            | Self::ParentEnded { .. }
            | Self::StartKeyConflict { .. }
            | Self::ProcessEnvRefused { .. }
            | Self::InvalidToolDiscovery { .. }
            | Self::InvalidBatchMaximum { .. }
            | Self::ToolPresentationRefused { .. }
            | Self::ResidentToolContractUnavailable { .. }
            | Self::ResidentToolDuplicateId { .. }
            | Self::ResidentToolDuplicateOperation { .. }
            | Self::ResidentToolInvalidOperation { .. }
            | Self::ToolRegistrationRefused { .. }
            | Self::ToolSurfaceRefused { .. }
            | Self::SessionAlreadyExists { .. }
            | Self::MissingRecordedSessionConfig { .. }
            | Self::RecordedSessionConfigConflict { .. }
            | Self::AppendOperationIdentityConflict { .. }
            | Self::AppendReceiptRequestedNodeCountCorrupt { .. }
            | Self::ClockBeforeUnixEpoch { .. }
            | Self::ProcessNotVisible { .. }
            | Self::NotASessionRuntime { .. }
            | Self::ProcessOutputAttachmentUnavailable { .. }
            | Self::ProcessUnknown { .. }
            | Self::ProcessRosterFilterMismatch {}
            | Self::ProcessChangeCursorPruned { .. }
            | Self::ProcessEventsReleased { .. }
            | Self::MonotonicCounterOverflow { .. }
            | Self::ProcessNoLongerRetained { .. }
            | Self::ProcessHandedOver { .. }
            | Self::ProcessAlreadyTerminal { .. }
            | Self::ProcessTerminalOutcomeMismatch { .. }
            | Self::UnknownProcessEventKind { .. }
            | Self::ProcessRegistryCursorBackendMismatch { .. }
            | Self::ProcessSuccessorRefused { .. } => Terminal,
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
            error @ Self::ProcessSuccessorRefused { .. } => {
                // The controller delegates this refusal here. Build its
                // terminal cause directly rather than converting back to
                // the controller, and retain the execution-owned fields in
                // the existing plugin envelope at the store/host boundary.
                let payload = match serde_json::to_value(&error) {
                    Ok(payload) => payload,
                    Err(source) => {
                        return crate::RuntimeError::new(
                            crate::RuntimeErrorCode::RecordEncodingFailed,
                            source.to_string(),
                        );
                    }
                };
                let message = error.to_string();
                let failure = lash_protocol_types::PluginOperationFailure {
                    error_type: "lash.plugin".into(),
                    error_version: std::num::NonZeroU32::MIN,
                    payload,
                    class: lash_protocol_types::PluginFailureClass::Terminal,
                    code: crate::FailureCode::from(&refusal),
                    message: message.clone(),
                    origin: None,
                };
                crate::RuntimeError::new(refusal, message).with_cause(
                    crate::RuntimeErrorCause::PluginOperation {
                        failure: Box::new(failure),
                    },
                )
            }
            Self::Runtime(error)
                if matches!(
                    error.cause,
                    Some(crate::RuntimeErrorCause::ControlPhaseExpired { .. })
                ) =>
            {
                let mut refused = crate::RuntimeError::new(refusal, error.message);
                refused.cause = error.cause;
                refused
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
                crate::RuntimeErrorCause::ControlPhaseExpired { .. }
                    | crate::RuntimeErrorCause::ProviderFailure { .. }
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

fn plugin_error_class(class: lash_protocol_types::PluginFailureClass) -> PluginErrorClass {
    match class {
        lash_protocol_types::PluginFailureClass::Retryable => PluginErrorClass::Retryable,
        lash_protocol_types::PluginFailureClass::Terminal => PluginErrorClass::Terminal,
        lash_protocol_types::PluginFailureClass::Redrivable
        | lash_protocol_types::PluginFailureClass::Parked => PluginErrorClass::Redrivable,
    }
}

pub fn runtime_operation_failure(
    error: crate::RuntimeError,
) -> lash_protocol_types::PluginOperationFailure {
    error.into()
}

impl From<PluginError> for lash_protocol_types::PluginOperationFailure {
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
        // These direct refusals are wrapped in a PluginOperation cause by
        // the controller. Build their envelope without reentering that conversion.
        let runtime = match &error {
            PluginError::Package(_)
            | PluginError::PackageNamespace { .. }
            | PluginError::InternalOperation { .. }
            | PluginError::Preflight(_)
            | PluginError::ProviderConfig { .. }
            | PluginError::ProviderRegistryMismatch { .. } => {
                crate::RuntimeError::new(crate::RuntimeErrorCode::Plugin, error.to_string())
            }
            _ => crate::RuntimeEffectControllerError::from(error).into_runtime_error(),
        };
        let mut failure = runtime_operation_failure(runtime);
        if failure.class != lash_protocol_types::PluginFailureClass::Parked {
            failure.class = match class {
                PluginErrorClass::Retryable => lash_protocol_types::PluginFailureClass::Retryable,
                PluginErrorClass::Redrivable => lash_protocol_types::PluginFailureClass::Redrivable,
                PluginErrorClass::Terminal => lash_protocol_types::PluginFailureClass::Terminal,
            };
        }
        failure.error_type = "lash.plugin".into();
        failure.payload = payload;
        failure
    }
}

impl From<crate::config_error::ConfigRegistrationError> for PluginError {
    fn from(error: crate::config_error::ConfigRegistrationError) -> Self {
        Self::ConfigRegistration(error)
    }
}

#[cfg(test)]
mod classification_tests {
    #[test]
    fn schema_causes_retain_typed_fields_through_recorded_intent_and_runtime() {
        let admission = lash_vocabulary::json_schema::JsonSchema::admit(serde_json::Value::Null)
            .expect_err("null cannot be admitted as a schema");
        let catalog = lash_vocabulary::tool_contract::ToolDefinition::raw(
            "bad",
            "tools.bad",
            "bad",
            serde_json::Value::Null,
            serde_json::json!({}),
        )
        .expect_err("a tool cannot publish an unusable schema");
        let mismatch = lash_vocabulary::json_schema::ValueMismatch {
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
        let refusal = lash_core_store::plugin_state::FormatRefusal {
            plugin: "unreadable-plugin".into(),
            namespace: crate::FormatNamespace::Config,
            stored: lash_vocabulary::providers::FormatVersion::new(7).expect("nonzero format"),
            readable: lash_vocabulary::providers::FormatVersion::ONE,
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
                run: lash_vocabulary::identity::TurnId::from("bound-run"),
            },
            crate::store::SessionHeadOwner::CommandLane { enqueue_seq: 7 },
            crate::store::SessionHeadOwner::Actor { epoch: 3 },
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

pub fn registry_error(error: lash_durable::DurableError) -> crate::PluginError {
    use lash_durable::{StoreFailure, StoreFailureKind};
    match error {
        lash_durable::DurableError::Package(refusal) => {
            crate::PluginError::PackageStoreRefused(refusal)
        }
        lash_durable::DurableError::StoreRefused(refusal) => {
            crate::PluginError::StoreRefusal(refusal)
        }
        lash_durable::DurableError::Store(StoreFailure {
            kind: StoreFailureKind::Contended,
            ..
        }) => crate::PluginError::StoreUnavailable {
            fault: crate::store::StoreFault::Contended,
        },
        lash_durable::DurableError::Store(StoreFailure {
            kind: StoreFailureKind::Unavailable,
            message,
        }) => crate::PluginError::StoreUnavailable {
            fault: crate::store::StoreFault::Backend { message },
        },
        error => crate::PluginError::Session(error.to_string()),
    }
}

#[cfg(test)]
mod error_class_tests;
