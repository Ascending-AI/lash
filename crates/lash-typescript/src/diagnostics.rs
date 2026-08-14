use std::fmt;

/// Stable names for every TypeScript dialect rejection.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum DiagnosticCode {
    SyntaxError,
    ClassUnsupported,
    GeneratorUnsupported,
    AsyncUnsupported,
    WithUnsupported,
    EvalUnsupported,
    FunctionConstructorUnsupported,
    LabelUnsupported,
    RegExpUnsupported,
    AccessorUnsupported,
    PrototypeMutationUnsupported,
    ThisUnsupported,
    EnumUnsupported,
    NamespaceUnsupported,
    DecoratorUnsupported,
    DynamicImportUnsupported,
    JsxUnsupported,
    ImportExportUnsupported,
    VarUnsupported,
    UsingUnsupported,
    DestructuringUnsupported,
    SpreadUnsupported,
    OptionalChainingUnsupported,
    NewUnsupported,
    UpdateUnsupported,
    SwitchUnsupported,
    DoWhileUnsupported,
    ForUnsupported,
    ForInUnsupported,
    ForOfUnsupported,
    DeleteUnsupported,
    AwaitUnsupported,
    YieldUnsupported,
    TaggedTemplateUnsupported,
    ComputedPropertyUnsupported,
    ObjectMethodUnsupported,
    SuperUnsupported,
    MetaPropertyUnsupported,
    BigIntUnsupported,
    PrivateNameUnsupported,
    AssignmentOperatorUnsupported,
    SequenceUnsupported,
    BitwiseUnsupported,
    ExponentiationUnsupported,
    InOperatorUnsupported,
    InstanceOfUnsupported,
    DebuggerUnsupported,
    EmptyCatchBindingUnsupported,
    LoneSurrogateLiteralUnsupported,
    SourceNestingLimit,
    ParameterDefaultUnsupported,
    ParameterRestUnsupported,
    DeclareUnsupported,
    MissingInitializer,
    ReservedIdentifier,
    DuplicateBinding,
    TemporalDeadZone,
    UnknownBinding,
    AssignConst,
    MutableCaptureUnsupported,
    ReturnOutsideFunction,
    LoopControlOutsideLoop,
    UnsupportedStatement,
    UnsupportedExpression,
    InvalidAst,
    LinkError,
}

impl DiagnosticCode {
    /// The stable machine-readable diagnostic name.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::SyntaxError => "TS_SYNTAX_ERROR",
            Self::ClassUnsupported => "TS_CLASS_UNSUPPORTED",
            Self::GeneratorUnsupported => "TS_GENERATOR_UNSUPPORTED",
            Self::AsyncUnsupported => "TS_ASYNC_UNSUPPORTED",
            Self::WithUnsupported => "TS_WITH_UNSUPPORTED",
            Self::EvalUnsupported => "TS_EVAL_UNSUPPORTED",
            Self::FunctionConstructorUnsupported => "TS_FUNCTION_CONSTRUCTOR_UNSUPPORTED",
            Self::LabelUnsupported => "TS_LABEL_UNSUPPORTED",
            Self::RegExpUnsupported => "TS_REGEXP_UNSUPPORTED",
            Self::AccessorUnsupported => "TS_ACCESSOR_UNSUPPORTED",
            Self::PrototypeMutationUnsupported => "TS_PROTOTYPE_MUTATION_UNSUPPORTED",
            Self::ThisUnsupported => "TS_THIS_UNSUPPORTED",
            Self::EnumUnsupported => "TS_ENUM_UNSUPPORTED",
            Self::NamespaceUnsupported => "TS_NAMESPACE_UNSUPPORTED",
            Self::DecoratorUnsupported => "TS_DECORATOR_UNSUPPORTED",
            Self::DynamicImportUnsupported => "TS_DYNAMIC_IMPORT_UNSUPPORTED",
            Self::JsxUnsupported => "TS_JSX_UNSUPPORTED",
            Self::ImportExportUnsupported => "TS_IMPORT_EXPORT_UNSUPPORTED",
            Self::VarUnsupported => "TS_VAR_UNSUPPORTED",
            Self::UsingUnsupported => "TS_USING_UNSUPPORTED",
            Self::DestructuringUnsupported => "TS_DESTRUCTURING_UNSUPPORTED",
            Self::SpreadUnsupported => "TS_SPREAD_UNSUPPORTED",
            Self::OptionalChainingUnsupported => "TS_OPTIONAL_CHAINING_UNSUPPORTED",
            Self::NewUnsupported => "TS_NEW_UNSUPPORTED",
            Self::UpdateUnsupported => "TS_UPDATE_UNSUPPORTED",
            Self::SwitchUnsupported => "TS_SWITCH_UNSUPPORTED",
            Self::DoWhileUnsupported => "TS_DO_WHILE_UNSUPPORTED",
            Self::ForUnsupported => "TS_FOR_UNSUPPORTED",
            Self::ForInUnsupported => "TS_FOR_IN_UNSUPPORTED",
            Self::ForOfUnsupported => "TS_FOR_OF_UNSUPPORTED",
            Self::DeleteUnsupported => "TS_DELETE_UNSUPPORTED",
            Self::AwaitUnsupported => "TS_AWAIT_UNSUPPORTED",
            Self::YieldUnsupported => "TS_YIELD_UNSUPPORTED",
            Self::TaggedTemplateUnsupported => "TS_TAGGED_TEMPLATE_UNSUPPORTED",
            Self::ComputedPropertyUnsupported => "TS_COMPUTED_PROPERTY_UNSUPPORTED",
            Self::ObjectMethodUnsupported => "TS_OBJECT_METHOD_UNSUPPORTED",
            Self::SuperUnsupported => "TS_SUPER_UNSUPPORTED",
            Self::MetaPropertyUnsupported => "TS_META_PROPERTY_UNSUPPORTED",
            Self::BigIntUnsupported => "TS_BIGINT_UNSUPPORTED",
            Self::PrivateNameUnsupported => "TS_PRIVATE_NAME_UNSUPPORTED",
            Self::AssignmentOperatorUnsupported => "TS_ASSIGNMENT_OPERATOR_UNSUPPORTED",
            Self::SequenceUnsupported => "TS_SEQUENCE_UNSUPPORTED",
            Self::BitwiseUnsupported => "TS_BITWISE_UNSUPPORTED",
            Self::ExponentiationUnsupported => "TS_EXPONENTIATION_UNSUPPORTED",
            Self::InOperatorUnsupported => "TS_IN_OPERATOR_UNSUPPORTED",
            Self::InstanceOfUnsupported => "TS_INSTANCEOF_UNSUPPORTED",
            Self::DebuggerUnsupported => "TS_DEBUGGER_UNSUPPORTED",
            Self::EmptyCatchBindingUnsupported => "TS_EMPTY_CATCH_BINDING_UNSUPPORTED",
            Self::LoneSurrogateLiteralUnsupported => "TS_LONE_SURROGATE_LITERAL_UNSUPPORTED",
            Self::SourceNestingLimit => "TS_SOURCE_NESTING_LIMIT",
            Self::ParameterDefaultUnsupported => "TS_PARAMETER_DEFAULT_UNSUPPORTED",
            Self::ParameterRestUnsupported => "TS_PARAMETER_REST_UNSUPPORTED",
            Self::DeclareUnsupported => "TS_DECLARE_UNSUPPORTED",
            Self::MissingInitializer => "TS_MISSING_INITIALIZER",
            Self::ReservedIdentifier => "TS_RESERVED_IDENTIFIER",
            Self::DuplicateBinding => "TS_DUPLICATE_BINDING",
            Self::TemporalDeadZone => "TS_TEMPORAL_DEAD_ZONE",
            Self::UnknownBinding => "TS_UNKNOWN_BINDING",
            Self::AssignConst => "TS_ASSIGN_CONST",
            Self::MutableCaptureUnsupported => "TS_MUTABLE_CAPTURE_UNSUPPORTED",
            Self::ReturnOutsideFunction => "TS_RETURN_OUTSIDE_FUNCTION",
            Self::LoopControlOutsideLoop => "TS_LOOP_CONTROL_OUTSIDE_LOOP",
            Self::UnsupportedStatement => "TS_STATEMENT_UNSUPPORTED",
            Self::UnsupportedExpression => "TS_EXPRESSION_UNSUPPORTED",
            Self::InvalidAst => "TS_INVALID_SHARED_AST",
            Self::LinkError => "TS_LINK_ERROR",
        }
    }
}

/// Byte offsets in the submitted TypeScript source.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SourceSpan {
    pub start: usize,
    pub end: usize,
}

/// A named parse or link-time TypeScript dialect rejection.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Diagnostic {
    pub code: DiagnosticCode,
    pub message: String,
    pub span: Option<SourceSpan>,
}

impl Diagnostic {
    pub(crate) fn new(
        code: DiagnosticCode,
        message: impl Into<String>,
        span: Option<SourceSpan>,
    ) -> Self {
        Self {
            code,
            message: message.into(),
            span,
        }
    }
}

impl fmt::Display for Diagnostic {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}: {}", self.code.as_str(), self.message)
    }
}

impl std::error::Error for Diagnostic {}
