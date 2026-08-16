#[derive(Clone, Debug, Error, PartialEq, Eq)]
pub enum LinkError {
    #[error("duplicate declaration `{name}`")]
    DuplicateDeclaration { name: String, span: Option<Span> },
    #[error("duplicate process parameter `{name}`")]
    DuplicateProcessParam { name: String, span: Option<Span> },
    #[error("duplicate process signal `{name}`")]
    DuplicateProcessSignal { name: String, span: Option<Span> },
    #[error("unknown process `{name}`")]
    UnknownProcess { name: String, span: Option<Span> },
    #[error("process `{process}` is missing argument `{arg}`")]
    MissingProcessArgument {
        process: String,
        arg: String,
        span: Option<Span>,
    },
    #[error("process `{process}` does not accept argument `{arg}`")]
    UnexpectedProcessArgument {
        process: String,
        arg: String,
        span: Option<Span>,
    },
    #[error("duplicate process argument `{arg}`")]
    DuplicateProcessArgument { arg: String, span: Option<Span> },
    #[error("unknown name `{name}`")]
    UnknownName { name: String, span: Option<Span> },
    #[error("unknown builtin `{name}`")]
    UnknownBuiltin { name: String, span: Option<Span> },
    #[error("unknown module `{path}`")]
    UnknownResource { path: String, span: Option<Span> },
    #[error("unknown type `{name}`")]
    UnknownType { name: String, span: Option<Span> },
    #[error("constructor `{path}` expects {expected}, got {actual}")]
    IncompatibleConstructorInput {
        path: String,
        expected: String,
        actual: String,
        span: Option<Span>,
    },
    #[error("operation `{operation}` expects {expected}, got {actual}")]
    IncompatibleOperationInput {
        operation: String,
        expected: String,
        actual: String,
        span: Option<Span>,
    },
    #[error("expected {expected}, got incompatible literal {actual}")]
    IncompatibleExpectedLiteral {
        expected: String,
        actual: String,
        span: Option<Span>,
    },
    #[error("process `{process}` return type is incompatible: expected {expected}, got {actual}")]
    IncompatibleProcessReturn {
        process: String,
        expected: String,
        actual: String,
        span: Option<Span>,
    },
    #[error("trigger registration requires {{ source, target, inputs, name? }}")]
    InvalidTriggerRegistration { span: Option<Span> },
    #[error(
        "trigger subscription_key must be a non-empty string literal and must not use the reserved `lash.internal/` prefix"
    )]
    InvalidTriggerSubscriptionKey { span: Option<Span> },
    #[error(
        "multiple trigger registrations derive the same default key for process `{process}` and source `{source_type}`; give each registration an explicit literal subscription_key"
    )]
    DuplicateDerivedTriggerSubscriptionKey {
        process: String,
        source_type: String,
        span: Option<Span>,
    },
    #[error(
        "trigger registration is too dynamic to derive a stable manifest key; add an explicit literal subscription_key"
    )]
    UnresolvedDerivedTriggerSubscriptionKey { span: Option<Span> },
    #[error("trigger registration `inputs` must be a literal record")]
    InvalidTriggerInputs { span: Option<Span> },
    #[error("trigger registration input `{input}` is duplicated")]
    DuplicateTriggerInput { input: String, span: Option<Span> },
    #[error("trigger target `{process}` input `{input}` is not mapped")]
    MissingTriggerInput {
        process: String,
        input: String,
        span: Option<Span>,
    },
    #[error("trigger target `{process}` has no input `{input}`")]
    UnknownTriggerInput {
        process: String,
        input: String,
        span: Option<Span>,
    },
    #[error("trigger registration `inputs` must map at least one param to `trigger.event`")]
    MissingTriggerEventInput { span: Option<Span> },
    #[error("`trigger.event` is only valid as a direct value inside trigger definition `inputs`")]
    TriggerEventOutsideInputs { span: Option<Span> },
    #[error(
        "`trigger.event` represents the whole event; projections such as `trigger.event.field` are not supported"
    )]
    TriggerEventProjection { span: Option<Span> },
    #[error("trigger listing requires {{ target }}")]
    InvalidTriggerList { span: Option<Span> },
    #[error("trigger source type `{source_ty}` is not registered as a TriggerSource")]
    UnknownTriggerEventType {
        source_ty: String,
        span: Option<Span>,
    },
    #[error("trigger target must be a process value, got {actual}")]
    InvalidTriggerTarget { actual: String, span: Option<Span> },
    #[error("trigger source emits {event}, but target input `{input_name}` expects {input}")]
    TriggerEventMismatch {
        event: String,
        input_name: String,
        input: String,
        span: Option<Span>,
    },
    #[error("receiver for operation `{operation}` is not a module authority")]
    UnresolvedReceiver {
        operation: String,
        suggestions: Vec<String>,
        span: Option<Span>,
    },
    #[error("resource type `{resource_type}` does not expose operation `{operation}`")]
    UnknownResourceOperation {
        resource_type: String,
        operation: String,
        suggestions: Vec<String>,
        span: Option<Span>,
    },
    #[error("module `{module_path}` does not expose operation `{operation}`; available identity-qualified paths: {}", suggestions.join(", "))]
    AmbiguousModuleOperation {
        module_path: String,
        operation: String,
        suggestions: Vec<String>,
        span: Option<Span>,
    },
    #[error("tools must be called through module paths, e.g. `{suggestion}`")]
    BareToolCall {
        name: String,
        suggestion: String,
        span: Option<Span>,
    },
    #[error("process `{process}` argument `{arg}` expects {expected}, got {actual}")]
    IncompatibleProcessArgument {
        process: Box<str>,
        arg: Box<str>,
        expected: Box<str>,
        actual: Box<str>,
        span: Option<Span>,
    },
    #[error("lashlang feature `{feature}` is disabled by this host")]
    FeatureDisabled {
        feature: &'static str,
        span: Option<Span>,
    },
    #[error("`{keyword}` can only be used inside a process body")]
    ProcessLifecycleOutsideProcess {
        keyword: &'static str,
        span: Option<Span>,
    },
    #[error("cannot access `{access}` on opaque host descriptor `{type_name}`")]
    OpaqueHostDescriptorAccess {
        type_name: String,
        access: String,
        span: Option<Span>,
    },
    #[error("object type has no field `{field}`")]
    UnknownObjectField { field: String, span: Option<Span> },
    #[error("operator `{operator}` does not accept {left} and {right}")]
    IncompatibleBinaryOperands {
        operator: &'static str,
        left: String,
        right: String,
        span: Option<Span>,
    },
    #[error("builtin `{builtin}` expects {expected}, got {actual}")]
    IncompatibleBuiltinOperands {
        builtin: String,
        expected: &'static str,
        actual: String,
        span: Option<Span>,
    },
    #[error("cannot iterate over {actual}; expected a list")]
    IncompatibleIterationTarget { actual: String, span: Option<Span> },
    #[error("failed to hash linked module: {message}")]
    ModuleHash { message: String },
}

impl LinkError {
    pub fn span(&self) -> Option<Span> {
        match self {
            Self::DuplicateDeclaration { span, .. }
            | Self::DuplicateProcessParam { span, .. }
            | Self::DuplicateProcessSignal { span, .. }
            | Self::UnknownProcess { span, .. }
            | Self::MissingProcessArgument { span, .. }
            | Self::UnexpectedProcessArgument { span, .. }
            | Self::DuplicateProcessArgument { span, .. }
            | Self::UnknownName { span, .. }
            | Self::UnknownBuiltin { span, .. }
            | Self::UnknownResource { span, .. }
            | Self::UnknownType { span, .. }
            | Self::IncompatibleConstructorInput { span, .. }
            | Self::IncompatibleOperationInput { span, .. }
            | Self::IncompatibleExpectedLiteral { span, .. }
            | Self::IncompatibleProcessReturn { span, .. }
            | Self::InvalidTriggerRegistration { span }
            | Self::InvalidTriggerSubscriptionKey { span }
            | Self::DuplicateDerivedTriggerSubscriptionKey { span, .. }
            | Self::UnresolvedDerivedTriggerSubscriptionKey { span }
            | Self::InvalidTriggerInputs { span }
            | Self::DuplicateTriggerInput { span, .. }
            | Self::MissingTriggerInput { span, .. }
            | Self::UnknownTriggerInput { span, .. }
            | Self::MissingTriggerEventInput { span }
            | Self::TriggerEventOutsideInputs { span }
            | Self::TriggerEventProjection { span }
            | Self::InvalidTriggerList { span }
            | Self::UnknownTriggerEventType { span, .. }
            | Self::InvalidTriggerTarget { span, .. }
            | Self::TriggerEventMismatch { span, .. }
            | Self::UnresolvedReceiver { span, .. }
            | Self::UnknownResourceOperation { span, .. }
            | Self::AmbiguousModuleOperation { span, .. }
            | Self::BareToolCall { span, .. }
            | Self::IncompatibleProcessArgument { span, .. }
            | Self::FeatureDisabled { span, .. }
            | Self::ProcessLifecycleOutsideProcess { span, .. }
            | Self::OpaqueHostDescriptorAccess { span, .. }
            | Self::UnknownObjectField { span, .. }
            | Self::IncompatibleBinaryOperands { span, .. }
            | Self::IncompatibleBuiltinOperands { span, .. }
            | Self::IncompatibleIterationTarget { span, .. } => *span,
            Self::ModuleHash { .. } => None,
        }
    }
}
