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
    RegexInvalid,
    RegexPatternTooLong,
    RegexNestingLimit,
    RegexFlagUnsupported,
    RegexIndicesFlagUnsupported,
    RegexUnicodeSetsFlagUnsupported,
    RegexIteratorPosition,
    AccessorUnsupported,
    PrototypeMutationUnsupported,
    ThisUnsupported,
    NamespaceUnsupported,
    DecoratorUnsupported,
    DynamicImportUnsupported,
    JsxUnsupported,
    ImportExportUnsupported,
    UsingUnsupported,
    NewUnsupported,
    ForUnsupported,
    ForOfUnsupported,
    AwaitUnsupported,
    AwaitRequired,
    YieldUnsupported,
    TaggedTemplateUnsupported,
    SuperUnsupported,
    MetaPropertyUnsupported,
    BigIntUnsupported,
    PrivateNameUnsupported,
    SequenceUnsupported,
    InstanceOfUnsupported,
    DebuggerUnsupported,
    LoneSurrogateLiteralUnsupported,
    SourceNestingLimit,
    SourceTooLarge,
    ParseResourcesUnavailable,
    DeclareUnsupported,
    MissingInitializer,
    ReservedIdentifier,
    MutualRecursionUnsupported,
    DuplicateBinding,
    TemporalDeadZone,
    UnknownBinding,
    AssignConst,
    MutableCaptureUnsupported,
    ProcessDefinitionNotTopLevel,
    ProcessConfigLiteralRequired,
    ProcessConfigFieldUnsupported,
    ProcessNameLiteralRequired,
    ProcessSignalsLiteralRequired,
    ProcessRunLiteralRequired,
    ProcessCaptureUnsupported,
    ProcessTargetStaticRequired,
    MethodUnsupported,
    ReturnOutsideFunction,
    LoopControlOutsideLoop,
    UnsupportedStatement,
    UnsupportedExpression,
    InvalidAst,
    LinkError,
}

impl DiagnosticCode {
    /// Every diagnostic code the dialect can emit.
    ///
    /// The prompt the model reads names codes in prose, and prose cannot be
    /// type-checked: FIG-1306 shipped `TS_FOR_OF_ITERATOR_UNSUPPORTED`, a code
    /// that never existed, into the production prompt, a test that pinned it,
    /// and a runbook gate that could therefore never fire. This list is what
    /// lets a test walk the rendered prompt and reject a name the dialect
    /// cannot produce. `all_codes_are_listed` keeps it complete.
    pub const ALL: &'static [Self] = &[
        Self::SyntaxError,
        Self::ClassUnsupported,
        Self::GeneratorUnsupported,
        Self::AsyncUnsupported,
        Self::WithUnsupported,
        Self::EvalUnsupported,
        Self::FunctionConstructorUnsupported,
        Self::LabelUnsupported,
        Self::RegexInvalid,
        Self::RegexPatternTooLong,
        Self::RegexNestingLimit,
        Self::RegexFlagUnsupported,
        Self::RegexIndicesFlagUnsupported,
        Self::RegexUnicodeSetsFlagUnsupported,
        Self::RegexIteratorPosition,
        Self::AccessorUnsupported,
        Self::PrototypeMutationUnsupported,
        Self::ThisUnsupported,
        Self::NamespaceUnsupported,
        Self::DecoratorUnsupported,
        Self::DynamicImportUnsupported,
        Self::JsxUnsupported,
        Self::ImportExportUnsupported,
        Self::UsingUnsupported,
        Self::NewUnsupported,
        Self::ForUnsupported,
        Self::ForOfUnsupported,
        Self::AwaitUnsupported,
        Self::AwaitRequired,
        Self::YieldUnsupported,
        Self::TaggedTemplateUnsupported,
        Self::SuperUnsupported,
        Self::MetaPropertyUnsupported,
        Self::BigIntUnsupported,
        Self::PrivateNameUnsupported,
        Self::SequenceUnsupported,
        Self::InstanceOfUnsupported,
        Self::DebuggerUnsupported,
        Self::LoneSurrogateLiteralUnsupported,
        Self::SourceNestingLimit,
        Self::SourceTooLarge,
        Self::ParseResourcesUnavailable,
        Self::DeclareUnsupported,
        Self::MissingInitializer,
        Self::ReservedIdentifier,
        Self::MutualRecursionUnsupported,
        Self::DuplicateBinding,
        Self::TemporalDeadZone,
        Self::UnknownBinding,
        Self::AssignConst,
        Self::MutableCaptureUnsupported,
        Self::ProcessDefinitionNotTopLevel,
        Self::ProcessConfigLiteralRequired,
        Self::ProcessConfigFieldUnsupported,
        Self::ProcessNameLiteralRequired,
        Self::ProcessSignalsLiteralRequired,
        Self::ProcessRunLiteralRequired,
        Self::ProcessCaptureUnsupported,
        Self::ProcessTargetStaticRequired,
        Self::MethodUnsupported,
        Self::ReturnOutsideFunction,
        Self::LoopControlOutsideLoop,
        Self::UnsupportedStatement,
        Self::UnsupportedExpression,
        Self::InvalidAst,
        Self::LinkError,
    ];

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
            Self::RegexInvalid => "TS_REGEX_INVALID",
            Self::RegexPatternTooLong => "TS_REGEX_PATTERN_TOO_LONG",
            Self::RegexNestingLimit => "TS_REGEX_PATTERN_NESTING_LIMIT",
            Self::RegexFlagUnsupported => "TS_REGEX_FLAG_UNSUPPORTED",
            Self::RegexIndicesFlagUnsupported => "TS_REGEX_INDICES_FLAG_UNSUPPORTED",
            Self::RegexUnicodeSetsFlagUnsupported => "TS_REGEX_UNICODE_SETS_FLAG_UNSUPPORTED",
            Self::RegexIteratorPosition => "TS_REGEX_ITERATOR_POSITION",
            Self::AccessorUnsupported => "TS_ACCESSOR_UNSUPPORTED",
            Self::PrototypeMutationUnsupported => "TS_PROTOTYPE_MUTATION_UNSUPPORTED",
            Self::ThisUnsupported => "TS_THIS_UNSUPPORTED",
            Self::NamespaceUnsupported => "TS_NAMESPACE_UNSUPPORTED",
            Self::DecoratorUnsupported => "TS_DECORATOR_UNSUPPORTED",
            Self::DynamicImportUnsupported => "TS_DYNAMIC_IMPORT_UNSUPPORTED",
            Self::JsxUnsupported => "TS_JSX_UNSUPPORTED",
            Self::ImportExportUnsupported => "TS_IMPORT_EXPORT_UNSUPPORTED",
            Self::UsingUnsupported => "TS_USING_UNSUPPORTED",
            Self::NewUnsupported => "TS_NEW_UNSUPPORTED",
            Self::ForUnsupported => "TS_FOR_UNSUPPORTED",
            Self::ForOfUnsupported => "TS_FOR_OF_UNSUPPORTED",
            Self::AwaitUnsupported => "TS_AWAIT_UNSUPPORTED",
            Self::AwaitRequired => "TS_AWAIT_REQUIRED",
            Self::YieldUnsupported => "TS_YIELD_UNSUPPORTED",
            Self::TaggedTemplateUnsupported => "TS_TAGGED_TEMPLATE_UNSUPPORTED",
            Self::SuperUnsupported => "TS_SUPER_UNSUPPORTED",
            Self::MetaPropertyUnsupported => "TS_META_PROPERTY_UNSUPPORTED",
            Self::BigIntUnsupported => "TS_BIGINT_UNSUPPORTED",
            Self::PrivateNameUnsupported => "TS_PRIVATE_NAME_UNSUPPORTED",
            Self::SequenceUnsupported => "TS_SEQUENCE_UNSUPPORTED",
            Self::InstanceOfUnsupported => "TS_INSTANCEOF_UNSUPPORTED",
            Self::DebuggerUnsupported => "TS_DEBUGGER_UNSUPPORTED",
            Self::LoneSurrogateLiteralUnsupported => "TS_LONE_SURROGATE_LITERAL_UNSUPPORTED",
            Self::SourceNestingLimit => "TS_SOURCE_NESTING_LIMIT",
            Self::SourceTooLarge => "TS_SOURCE_TOO_LARGE",
            Self::ParseResourcesUnavailable => "TS_PARSE_RESOURCES_UNAVAILABLE",
            Self::DeclareUnsupported => "TS_DECLARE_UNSUPPORTED",
            Self::MissingInitializer => "TS_MISSING_INITIALIZER",
            Self::ReservedIdentifier => "TS_RESERVED_IDENTIFIER",
            Self::MutualRecursionUnsupported => "TS_MUTUAL_RECURSION_UNSUPPORTED",
            Self::DuplicateBinding => "TS_DUPLICATE_BINDING",
            Self::TemporalDeadZone => "TS_TEMPORAL_DEAD_ZONE",
            Self::UnknownBinding => "TS_UNKNOWN_BINDING",
            Self::AssignConst => "TS_ASSIGN_CONST",
            Self::MutableCaptureUnsupported => "TS_MUTABLE_CAPTURE_UNSUPPORTED",
            Self::ProcessDefinitionNotTopLevel => "TS_PROCESS_DEFINITION_NOT_TOP_LEVEL",
            Self::ProcessConfigLiteralRequired => "TS_PROCESS_CONFIG_LITERAL_REQUIRED",
            Self::ProcessConfigFieldUnsupported => "TS_PROCESS_CONFIG_FIELD_UNSUPPORTED",
            Self::ProcessNameLiteralRequired => "TS_PROCESS_NAME_LITERAL_REQUIRED",
            Self::ProcessSignalsLiteralRequired => "TS_PROCESS_SIGNALS_LITERAL_REQUIRED",
            Self::ProcessRunLiteralRequired => "TS_PROCESS_RUN_LITERAL_REQUIRED",
            Self::ProcessCaptureUnsupported => "TS_PROCESS_CAPTURE_UNSUPPORTED",
            Self::ProcessTargetStaticRequired => "TS_PROCESS_TARGET_STATIC_REQUIRED",
            Self::MethodUnsupported => "TS_METHOD_UNSUPPORTED",
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

#[cfg(test)]
mod tests {
    use super::DiagnosticCode;

    /// `ALL` is only useful if it is complete, and a hand-maintained list beside
    /// the enum is exactly the drift shape that put a nonexistent code in the
    /// production prompt. This reads the enum's own declaration and requires
    /// every variant to appear.
    #[test]
    fn all_codes_are_listed() {
        let source = include_str!("diagnostics.rs");
        let declaration = source
            .split("pub enum DiagnosticCode {")
            .nth(1)
            .and_then(|rest| rest.split('}').next())
            .expect("the enum declares its variants");
        let declared = declaration
            .lines()
            .map(str::trim)
            .filter_map(|line| line.strip_suffix(','))
            .filter(|line| {
                line.chars().next().is_some_and(char::is_uppercase)
                    && line.chars().all(char::is_alphanumeric)
            })
            .collect::<Vec<_>>();
        assert!(
            declared.len() > 50,
            "the declaration parse found only {declared:?}"
        );

        let listed = DiagnosticCode::ALL
            .iter()
            .map(|code| format!("{code:?}"))
            .collect::<std::collections::BTreeSet<_>>();
        let missing = declared
            .iter()
            .filter(|variant| !listed.contains(**variant))
            .collect::<Vec<_>>();
        assert!(
            missing.is_empty(),
            "DiagnosticCode::ALL is missing {missing:?}"
        );
        assert_eq!(
            listed.len(),
            DiagnosticCode::ALL.len(),
            "DiagnosticCode::ALL repeats a variant"
        );
    }

    /// Every listed code must render a distinct `TS_`-prefixed token, since the
    /// prompt walker matches on exactly that shape.
    #[test]
    fn every_code_renders_a_distinct_ts_token() {
        let mut seen = std::collections::BTreeSet::new();
        for code in DiagnosticCode::ALL {
            let token = code.as_str();
            assert!(
                token.starts_with("TS_"),
                "{code:?} renders `{token}`, which the prompt walker cannot recognise"
            );
            assert!(seen.insert(token), "`{token}` is rendered by two variants");
        }
    }
}
