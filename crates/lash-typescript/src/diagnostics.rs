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

    /// The accepted in-dialect idiom for a construct this code refuses.
    ///
    /// Lashlang has carried a repair channel since it shipped: `parse_hint` and
    /// `runtime_hint` match on the *error variant* and return the rewrite, and
    /// `format_source_diagnostic` renders it on its own `hint:` line. The
    /// TypeScript dialect regressed that — its rejections are a code plus one
    /// sentence, and where a rewrite exists at all it is glued onto the end of
    /// the refusal, where a model has to parse English to find it.
    ///
    /// This is the same seam, keyed the same way. A code that refuses a
    /// construct owes the reader the construct that replaces it; a code that
    /// reports a resource limit or a plain syntax error owes nothing, because
    /// there is no other way to write the program. `every_unsupported_code_
    /// names_the_accepted_idiom` holds the first half of that to account.
    ///
    /// Site-specific rewrites (which method, which constructor) do not belong
    /// here — they arrive with the diagnostic. See [`Diagnostic::new`].
    pub const fn accepted_idiom(self) -> Option<&'static str> {
        Some(match self {
            Self::ClassUnsupported => {
                "use a factory function returning a plain object; for coded errors use `Object.assign(new Error(message), { code })`"
            }
            Self::GeneratorUnsupported => {
                "build the whole list and return it, or drive the work with `for...of`"
            }
            Self::AsyncUnsupported => {
                "declare the function `async` where the dialect allows it, or move the awaited work to the caller"
            }
            Self::WithUnsupported => "name the object and read its properties explicitly",
            Self::EvalUnsupported => "write the code in the cell; there is no dynamic evaluation",
            Self::FunctionConstructorUnsupported => {
                "write a function declaration or arrow function in the cell"
            }
            Self::LabelUnsupported => {
                "extract the labeled region into a helper function and `return` from it"
            }
            Self::RegexFlagUnsupported => "use only the `g`, `i`, `m`, `s`, `u`, and `y` flags",
            Self::RegexIndicesFlagUnsupported => {
                "drop the `d` flag and use the match index and matched text"
            }
            Self::RegexUnicodeSetsFlagUnsupported => {
                "use `u` with ordinary Unicode character classes"
            }
            Self::AccessorUnsupported => {
                "expose a plain data property, or a function that computes the value"
            }
            Self::PrototypeMutationUnsupported => {
                "return a new object with the properties you want instead of reaching through the prototype"
            }
            Self::ThisUnsupported => {
                "pass the value in as a parameter; `globalThis.name` holds durable session state"
            }
            Self::NamespaceUnsupported => "declare the values at the top level of the cell",
            Self::DecoratorUnsupported => "call the wrapper function explicitly",
            Self::DynamicImportUnsupported => {
                "call the host tools already in scope; a cell imports nothing"
            }
            Self::JsxUnsupported => "build strings or plain objects instead of JSX elements",
            Self::ImportExportUnsupported => {
                "a cell is not a module: reference the bound variables and host tools already in scope"
            }
            Self::UsingUnsupported => "release the resource explicitly in a `finally` block",
            Self::NewUnsupported => {
                "only the Error family, `Map`, `Set`, `Date`, `RegExp`, `URL`, and `URLSearchParams` are constructible"
            }
            Self::ForUnsupported => {
                "write `for (let i = 0; i < end; i++)`, or iterate with `for...of`"
            }
            Self::ForOfUnsupported => "iterate a materialized array with plain `for...of`",
            Self::AwaitUnsupported => {
                "await tool calls, process handles, `sleep`, `waitSignal`, or `Promise.all`/`allSettled` — nothing else is awaitable"
            }
            Self::AwaitRequired => "add `await` — the call returns a promise",
            Self::YieldUnsupported => "collect the values into an array and return it",
            Self::TaggedTemplateUnsupported => {
                "call the function with an ordinary template literal argument"
            }
            Self::SuperUnsupported => "call the helper function directly",
            Self::MetaPropertyUnsupported => {
                "a cell has no module or construction context; read what you need from the bound variables"
            }
            Self::BigIntUnsupported => "use `number`, or carry the value as a decimal string",
            Self::PrivateNameUnsupported => {
                "keep the field in a plain object and do not export it from the factory"
            }
            Self::SequenceUnsupported => "put each expression on its own statement line",
            Self::InstanceOfUnsupported => {
                "check `err.name`, or use `Array.isArray(value)`; `instanceof` accepts only built-in constructors"
            }
            Self::DebuggerUnsupported => "use `console.log` to inspect values",
            Self::LoneSurrogateLiteralUnsupported => {
                "write the character as a complete code point escape"
            }
            Self::DeclareUnsupported => "declare the value with an initializer",
            Self::MutualRecursionUnsupported => {
                "restructure the functions so one calls the other, or drive the recursion with an explicit work list"
            }
            Self::MutableCaptureUnsupported => {
                "pass the value into the function as a parameter and return the new value"
            }
            Self::ProcessConfigFieldUnsupported => {
                "a `defineProcess` config accepts `name`, `signals`, and `run`"
            }
            Self::ProcessCaptureUnsupported => {
                "pass the value to the process through its `run` arguments"
            }
            Self::MethodUnsupported => {
                "use a method the dialect's standard-library contract lists for this receiver"
            }
            Self::UnsupportedStatement | Self::UnsupportedExpression => {
                "rewrite with the constructs the dialect prompt lists"
            }
            Self::RegexIteratorPosition => {
                "consume the iterator where it is produced: `[...expr]`, `for...of`, `Array.from`, `new Map`/`Set`, or `Object.fromEntries`"
            }
            Self::RegexPatternTooLong | Self::RegexNestingLimit => {
                "split the pattern into smaller expressions and match in steps"
            }
            Self::SourceNestingLimit => "name intermediate values instead of nesting expressions",
            Self::SourceTooLarge => "split the work across several cells",
            Self::ReservedIdentifier => "choose a different name",
            Self::ProcessDefinitionNotTopLevel => "bind `defineProcess` to one top-level `const`",
            Self::ProcessConfigLiteralRequired => {
                "pass `defineProcess` a literal object with static properties"
            }
            Self::ProcessNameLiteralRequired => "give `name` a string literal",
            Self::ProcessSignalsLiteralRequired => {
                "give `signals` a literal object with static properties"
            }
            Self::ProcessRunLiteralRequired => "give `run` a function literal",
            Self::ProcessTargetStaticRequired => {
                "name a top-level `defineProcess` binding directly"
            }
            _ => return None,
        })
    }

    /// What this code says about *whose* fault the failure is.
    ///
    /// A refusal means no version of this approach will be accepted and the fix
    /// is to write the other construct; a defect means the approach was fine
    /// and the code was not. Telling a model its program is broken when the
    /// runtime refused the construct sends it debugging something it cannot
    /// fix; telling it a construct is forbidden when it merely miscounted
    /// arguments sends it rewriting code that was already the right shape.
    ///
    /// Three codes cannot answer for themselves. `TS_METHOD_UNSUPPORTED` covers
    /// the whole determinism-refusal set — `Promise.then`, `Promise.race`,
    /// `crypto.randomUUID`, `localeCompare`, local-time `Date` readers, and the
    /// methods simply absent from the runtime surface — *and* ordinary arity
    /// mistakes like `[].map()` with no callback. `TS_EXPRESSION_UNSUPPORTED`
    /// and `TS_STATEMENT_UNSUPPORTED` are catch-alls that likewise carry both
    /// the `globalThis` addressing rules and plain malformed code. For those,
    /// only the emitting site knows, so [`Diagnostic::refusal`] and
    /// [`Diagnostic::defect`] make it say — and `every_ambiguous_code_site_
    /// classifies_itself` refuses to let a site stay silent.
    pub const fn classification(self) -> CodeClassification {
        match self {
            // Constructs the dialect does not have. Writing one again in any
            // form is refused again; the fix is the other construct.
            Self::ClassUnsupported
            | Self::GeneratorUnsupported
            | Self::AsyncUnsupported
            | Self::WithUnsupported
            | Self::EvalUnsupported
            | Self::FunctionConstructorUnsupported
            | Self::LabelUnsupported
            | Self::RegexFlagUnsupported
            | Self::RegexIndicesFlagUnsupported
            | Self::RegexUnicodeSetsFlagUnsupported
            | Self::AccessorUnsupported
            | Self::PrototypeMutationUnsupported
            | Self::ThisUnsupported
            | Self::NamespaceUnsupported
            | Self::DecoratorUnsupported
            | Self::DynamicImportUnsupported
            | Self::JsxUnsupported
            | Self::ImportExportUnsupported
            | Self::UsingUnsupported
            | Self::NewUnsupported
            | Self::ForUnsupported
            | Self::ForOfUnsupported
            | Self::AwaitUnsupported
            | Self::YieldUnsupported
            | Self::TaggedTemplateUnsupported
            | Self::SuperUnsupported
            | Self::MetaPropertyUnsupported
            | Self::BigIntUnsupported
            | Self::PrivateNameUnsupported
            | Self::SequenceUnsupported
            | Self::InstanceOfUnsupported
            | Self::DebuggerUnsupported
            | Self::LoneSurrogateLiteralUnsupported
            | Self::DeclareUnsupported
            | Self::MutualRecursionUnsupported
            | Self::MutableCaptureUnsupported
            | Self::ProcessConfigFieldUnsupported
            | Self::ProcessCaptureUnsupported
            // Rules about size, placement, and shape. No single construct to
            // name, but just as much a refusal: the runtime will not accept
            // this program however it is debugged.
            | Self::RegexIteratorPosition
            | Self::RegexPatternTooLong
            | Self::RegexNestingLimit
            | Self::SourceNestingLimit
            | Self::SourceTooLarge
            | Self::ReservedIdentifier
            | Self::ProcessDefinitionNotTopLevel
            | Self::ProcessConfigLiteralRequired
            | Self::ProcessNameLiteralRequired
            | Self::ProcessSignalsLiteralRequired
            | Self::ProcessRunLiteralRequired
            | Self::ProcessTargetStaticRequired => CodeClassification::AlwaysRefusal,

            // Both families, decided per site.
            Self::MethodUnsupported
            | Self::UnsupportedExpression
            | Self::UnsupportedStatement => CodeClassification::PerSite,

            // The program is wrong: a typo, a malformed literal, a rule of
            // ordinary JavaScript scoping, or a host resource failure.
            Self::SyntaxError
            | Self::RegexInvalid
            | Self::ParseResourcesUnavailable
            | Self::MissingInitializer
            | Self::DuplicateBinding
            | Self::TemporalDeadZone
            | Self::UnknownBinding
            | Self::AssignConst
            | Self::AwaitRequired
            | Self::ReturnOutsideFunction
            | Self::LoopControlOutsideLoop
            | Self::InvalidAst
            | Self::LinkError => CodeClassification::AlwaysDefect,
        }
    }

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

/// Whether a code answers the refusal-or-defect question by itself.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CodeClassification {
    /// Always the dialect refusing a construct.
    AlwaysRefusal,
    /// Always the program being wrong.
    AlwaysDefect,
    /// Both, depending on which site emitted it. The site must say.
    PerSite,
}

/// Whose fault a rejection is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DiagnosticKind {
    /// The dialect will not run this construct, however it is debugged.
    Refusal,
    /// The construct is allowed; this instance of it is wrong.
    ProgramDefect,
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
    /// What the dialect refused. The refusal only.
    pub message: String,
    pub span: Option<SourceSpan>,
    /// Whose fault this is. Read by the RLM feedback layer to choose between
    /// "the runtime refused this" and "the defect is in the program".
    pub kind: DiagnosticKind,
    /// What to write instead: the structured repair channel.
    ///
    /// A rejection a model cannot act on costs a whole turn. The dialect has
    /// always known the accepted idiom — it was just written into the middle of
    /// `message`, where reading it means parsing English. This is the same
    /// content on a channel a consumer can find: the message holds the refusal,
    /// these hold the rewrite, and no text appears in both.
    pub suggestions: Vec<String>,
}

impl Diagnostic {
    /// Builds a diagnostic, splitting any repair text the message carries into
    /// [`Diagnostic::suggestions`].
    ///
    /// The crate's rejections were authored as `"Unsupported: <refusal>.
    /// <rewrite>"` — one string, two jobs. Splitting here rather than at 34 call
    /// sites keeps the convention in one place and makes the migration total:
    /// there is no way to author a repair-carrying message that *keeps* its
    /// repair text in `message`. Codes whose messages carry no rewrite fall back
    /// to [`DiagnosticCode::accepted_idiom`], so the reject-helper rejections
    /// — the ones that said only "X are not in the TypeScript dialect" — gain
    /// one too.
    pub(crate) fn new(
        code: DiagnosticCode,
        message: impl Into<String>,
        span: Option<SourceSpan>,
    ) -> Self {
        Self::classified(code, message, span, kind_from_code(code))
    }

    /// The dialect refusing a construct, from a site whose code cannot say so
    /// on its own. See [`DiagnosticCode::classification`].
    pub(crate) fn refusal(
        code: DiagnosticCode,
        message: impl Into<String>,
        span: Option<SourceSpan>,
    ) -> Self {
        debug_assert!(
            code.classification() != CodeClassification::AlwaysDefect,
            "{} never refuses a construct",
            code.as_str()
        );
        Self::classified(code, message, span, DiagnosticKind::Refusal)
    }

    /// The program being wrong, from a site whose code cannot say so on its
    /// own. See [`DiagnosticCode::classification`].
    pub(crate) fn defect(
        code: DiagnosticCode,
        message: impl Into<String>,
        span: Option<SourceSpan>,
    ) -> Self {
        debug_assert!(
            code.classification() != CodeClassification::AlwaysRefusal,
            "{} always refuses a construct",
            code.as_str()
        );
        Self::classified(code, message, span, DiagnosticKind::ProgramDefect)
    }

    /// Replaces the repair text, for a site whose rewrite is more specific than
    /// anything the code-level idiom table can say.
    pub(crate) fn with_hint(mut self, repair: impl Into<String>) -> Self {
        self.suggestions = vec![repair.into()];
        self
    }

    /// Whether the dialect refused the construct, as opposed to reporting a
    /// defect in an allowed one.
    pub fn is_dialect_refusal(&self) -> bool {
        self.kind == DiagnosticKind::Refusal
    }

    fn classified(
        code: DiagnosticCode,
        message: impl Into<String>,
        span: Option<SourceSpan>,
        kind: DiagnosticKind,
    ) -> Self {
        let message = message.into();
        let (message, suggestion) = split_inline_repair(message);
        let suggestions = suggestion
            .or_else(|| code.accepted_idiom().map(str::to_string))
            .into_iter()
            .collect();
        Self {
            code,
            message,
            span,
            kind,
            suggestions,
        }
    }

    /// Builds a diagnostic whose refusal and rewrite are authored separately.
    ///
    /// For rejections that never adopted the `"Unsupported: "` convention and
    /// whose repair text therefore has no sentence break to split on. Passing
    /// the two halves is always preferable to writing one string and hoping the
    /// splitter finds the seam.
    pub(crate) fn with_repair(
        code: DiagnosticCode,
        message: impl Into<String>,
        repair: impl Into<String>,
        span: Option<SourceSpan>,
    ) -> Self {
        Self {
            code,
            message: message.into(),
            span,
            kind: kind_from_code(code),
            suggestions: vec![repair.into()],
        }
    }
}

/// The kind a code answers with on its own.
///
/// `PerSite` codes must not reach here — [`Diagnostic::refusal`] and
/// [`Diagnostic::defect`] are their only constructors, and a source-walking
/// test enforces it — so their arm is unreachable in practice rather than a
/// silent default that would quietly file one family under the other.
fn kind_from_code(code: DiagnosticCode) -> DiagnosticKind {
    match code.classification() {
        CodeClassification::AlwaysRefusal => DiagnosticKind::Refusal,
        CodeClassification::AlwaysDefect | CodeClassification::PerSite => {
            DiagnosticKind::ProgramDefect
        }
    }
}

/// Splits `"Unsupported: <refusal>. <rewrite>"` into its two halves.
///
/// Only the authored `Unsupported: ` convention is split, and only at the first
/// sentence break: the leading clause names the construct, everything after it
/// is the repair. A message without that shape is returned whole — guessing at
/// sentence boundaries in arbitrary prose would move refusals into the hint
/// line, which is worse than leaving them where they are.
fn split_inline_repair(message: String) -> (String, Option<String>) {
    const PREFIX: &str = "Unsupported: ";
    if !message.starts_with(PREFIX) {
        return (message, None);
    }
    let Some(break_at) = message[PREFIX.len()..]
        .find(". ")
        .map(|offset| PREFIX.len() + offset)
    else {
        return (message, None);
    };
    let repair = message[break_at + ". ".len()..].trim().to_string();
    if repair.is_empty() {
        return (message, None);
    }
    (message[..break_at].to_string(), Some(repair))
}

impl fmt::Display for Diagnostic {
    /// The model-facing one-liner: code, refusal, then each rewrite on its own
    /// `hint:` line, exactly as Lashlang renders its own hint channel.
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}: {}", self.code.as_str(), self.message)?;
        for suggestion in &self.suggestions {
            write!(formatter, "\nhint: {suggestion}")?;
        }
        Ok(())
    }
}

impl std::error::Error for Diagnostic {}

/// Renders a diagnostic against the source the model actually submitted.
///
/// [`Diagnostic`]'s `Display` has no source to consult, so it can only name the
/// code and the refusal — which is what the RLM executor sent, discarding the
/// span every diagnostic already carried. A model that is told *what* is wrong
/// but not *where* has to re-derive the location from a program it wrote a turn
/// ago; with more than one candidate site in the cell, it guesses.
///
/// The offsets are byte offsets into that same submitted source — the parser is
/// handed the cell verbatim, with no wrapper, prelude, or preamble in front of
/// it — so line 1 here is the model's line 1 and needs no remapping. The layout
/// is Lashlang's `format_source_diagnostic`, deliberately: a session reads both
/// dialects' failures with the same eyes.
pub fn format_diagnostic(source: &str, diagnostic: &Diagnostic) -> String {
    let mut rendered = format!(
        "{}: {}",
        diagnostic.code.as_str(),
        diagnostic.message.trim_end()
    );
    if let Some(span) = diagnostic.span {
        let start = span.start.min(source.len());
        let (line, column, line_end, source_line) = line_column_snippet(source, start);
        let caret_pad = " ".repeat(column.saturating_sub(1));
        let underline_len = if start < line_end {
            let underline_end = span.end.max(start.saturating_add(1)).min(line_end);
            source[start..underline_end].chars().count().max(1)
        } else {
            1
        };
        let underline = format!("^{}", "~".repeat(underline_len.saturating_sub(1)));
        rendered.push_str(&format!(
            "\n--> line {line}, column {column}\n{source_line}\n{caret_pad}{underline}"
        ));
    }
    for suggestion in &diagnostic.suggestions {
        rendered.push_str("\nhint: ");
        rendered.push_str(suggestion);
    }
    rendered
}

/// The 1-based line and column of `offset`, the end of its line, and the line
/// itself. A byte offset that lands mid-character is clamped to the character
/// boundary before it rather than panicking on a slice.
fn line_column_snippet(source: &str, offset: usize) -> (usize, usize, usize, String) {
    let offset = offset.min(source.len());
    let mut line = 1usize;
    let mut line_start = 0usize;
    for (index, character) in source.char_indices() {
        if index >= offset {
            break;
        }
        if character == '\n' {
            line += 1;
            line_start = index + character.len_utf8();
        }
    }
    let line_end = source[line_start..]
        .find('\n')
        .map_or(source.len(), |offset| line_start + offset);
    let column = source[line_start..offset.max(line_start)].chars().count() + 1;
    (
        line,
        column,
        line_end,
        source[line_start..line_end].to_string(),
    )
}

#[cfg(test)]
mod tests {
    use super::{CodeClassification, DiagnosticCode};

    /// Every module that constructs diagnostics, read as source.
    pub(super) fn emitting_sources() -> Vec<(&'static str, &'static str)> {
        vec![
            ("adapter/mod.rs", include_str!("adapter/mod.rs")),
            ("adapter/enums.rs", include_str!("adapter/enums.rs")),
            ("adapter/nesting.rs", include_str!("adapter/nesting.rs")),
            (
                "adapter/prototype_chain.rs",
                include_str!("adapter/prototype_chain.rs"),
            ),
            ("lower/mod.rs", include_str!("lower/mod.rs")),
            ("lower/binding.rs", include_str!("lower/binding.rs")),
            ("lower/calls.rs", include_str!("lower/calls.rs")),
            ("lower/constructs.rs", include_str!("lower/constructs.rs")),
            ("lower/loops.rs", include_str!("lower/loops.rs")),
            ("lower/regex.rs", include_str!("lower/regex.rs")),
            ("lower/array_map.rs", include_str!("lower/array_map.rs")),
            (
                "lower/array_callbacks.rs",
                include_str!("lower/array_callbacks.rs"),
            ),
            ("lower/await_expr.rs", include_str!("lower/await_expr.rs")),
            (
                "lower/json_replacer.rs",
                include_str!("lower/json_replacer.rs"),
            ),
            ("lower/stdlib.rs", include_str!("lower/stdlib.rs")),
            ("lower/graph.rs", include_str!("lower/graph.rs")),
            (
                "adapter/rejections.rs",
                include_str!("adapter/rejections.rs"),
            ),
            ("regex.rs", include_str!("regex.rs")),
            ("signatures.rs", include_str!("signatures.rs")),
            ("diagnostics.rs", include_str!("diagnostics.rs")),
            ("lib.rs", include_str!("lib.rs")),
        ]
    }

    /// The walker below reads a hand-written file list, and a hand-written list
    /// beside a growing crate is the same drift shape `all_codes_are_listed`
    /// exists to catch. A module the crate gains and this list lacks would make
    /// the totality check quietly partial — the exact failure mode that let two
    /// earlier versions of the classification ship. So the list is checked
    /// against the crate's own `mod` declarations.
    #[test]
    fn the_source_walker_reads_every_module() {
        let listed = emitting_sources()
            .into_iter()
            .map(|(path, _)| path.to_string())
            .collect::<std::collections::BTreeSet<_>>();

        let mut missing = Vec::new();
        for (parent, source) in [
            ("", include_str!("lib.rs")),
            ("adapter/", include_str!("adapter/mod.rs")),
            ("lower/", include_str!("lower/mod.rs")),
        ] {
            for line in source.lines().map(str::trim) {
                let Some(rest) = line.strip_prefix("mod ").and_then(|r| r.strip_suffix(';')) else {
                    continue;
                };
                let candidates = [
                    format!("{parent}{rest}.rs"),
                    format!("{rest}/mod.rs"),
                    format!("{parent}{rest}/mod.rs"),
                ];
                if !candidates.iter().any(|path| listed.contains(path)) {
                    missing.push(candidates[0].clone());
                }
            }
        }
        assert!(
            missing.is_empty(),
            "these modules can emit diagnostics the walker never reads: {missing:?}"
        );
    }

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

    /// The repair channel is only worth having if a model can rely on it, and
    /// the codes it can rely on are the ones that refuse a construct: those all
    /// have an in-dialect replacement, by definition of being a *dialect*
    /// restriction rather than a resource limit or a typo.
    #[test]
    fn every_unsupported_code_names_the_accepted_idiom() {
        let silent = DiagnosticCode::ALL
            .iter()
            .filter(|code| code.as_str().ends_with("_UNSUPPORTED"))
            .filter(|code| code.accepted_idiom().is_none())
            .map(|code| code.as_str())
            .collect::<Vec<_>>();
        assert!(
            silent.is_empty(),
            "these refuse a construct without naming its replacement: {silent:?}"
        );
    }

    /// `[POLICY]` feedback tells the model to rewrite the cell "in the form
    /// named above". A refusal with no named form makes that instruction a lie,
    /// and a lie in the repair loop costs a whole turn.
    #[test]
    fn a_refusal_always_has_a_form_to_name() {
        for code in DiagnosticCode::ALL {
            if code.classification() == CodeClassification::AlwaysRefusal {
                assert!(
                    code.accepted_idiom().is_some(),
                    "{} refuses without naming a replacement",
                    code.as_str()
                );
            }
        }
    }

    /// The three per-site codes must never be answered by a table.
    ///
    /// This is the check that would have caught both earlier attempts. Deriving
    /// the answer from `accepted_idiom().is_some()` filed every arity mistake
    /// under "refused"; excluding the codes wholesale filed the entire
    /// determinism-refusal set — `Promise.then`, `crypto.randomUUID`,
    /// `localeCompare`, the local-time `Date` readers — under "your program is
    /// wrong", which sends a model debugging something the runtime will never
    /// run. Neither a default nor an exclusion is available now: the site says,
    /// or the crate does not compile past this test.
    ///
    /// Read from the crate's own sources rather than exercised through inputs,
    /// because the property is about *every* site, including ones no fixture
    /// reaches.
    #[test]
    fn every_ambiguous_code_site_classifies_itself() {
        let per_site = DiagnosticCode::ALL
            .iter()
            .filter(|code| code.classification() == CodeClassification::PerSite)
            .map(|code| format!("{code:?}"))
            .collect::<std::collections::BTreeSet<_>>();
        assert_eq!(
            per_site.len(),
            3,
            "the walker below is written for exactly these: {per_site:?}"
        );

        // The constructors and helpers that state a family. Anything else
        // building a per-site code is a site that never chose.
        const CLASSIFYING: &[&str] = &[
            "Diagnostic::refusal",
            "Diagnostic::defect",
            "reject_refusal",
            "reject_defect",
        ];

        let mut unclassified = Vec::new();
        let mut classified = 0usize;
        for (path, source) in emitting_sources() {
            // Production code only. A test fixture naming a code emits nothing
            // to a model, and this file's own fixtures would otherwise trip it.
            let source = source.split("#[cfg(test)]").next().unwrap_or_default();
            for (offset, _) in source.match_indices("DiagnosticCode::") {
                let name = source[offset + "DiagnosticCode::".len()..]
                    .split(|ch: char| !ch.is_alphanumeric())
                    .next()
                    .unwrap_or_default();
                if !per_site.contains(name) {
                    continue;
                }
                // The callee whose argument list this code opens: step back over
                // whitespace to the `(`, then read the identifier before it.
                // Reading the nearest preceding `Diagnostic::` instead would
                // walk past helper calls and land on an unrelated constructor.
                let head = source[..offset].trim_end();
                let Some(head) = head.strip_suffix('(') else {
                    unclassified.push(format!(
                        "{path}: `{name}` is not the first argument of a call"
                    ));
                    continue;
                };
                let callee = head
                    .rsplit(|ch: char| !(ch.is_alphanumeric() || ch == '_' || ch == ':'))
                    .next()
                    .unwrap_or_default();
                if CLASSIFYING.contains(&callee) {
                    classified += 1;
                } else {
                    unclassified.push(format!("{path}: `{name}` built by `{callee}`"));
                }
            }
        }

        assert!(
            unclassified.is_empty(),
            "a per-site code must say which family it is: {unclassified:#?}"
        );
        assert!(
            classified > 40,
            "the walker found only {classified} sites, so it is not reading the crate"
        );
    }

    /// Both families of `TS_METHOD_UNSUPPORTED`, through real programs.
    ///
    /// The determinism refusals are the half an exclusion silently broke: the
    /// runtime will never run `Promise.then` or `localeCompare` however they are
    /// written, and telling a model the defect is in its program sends it
    /// debugging instead of rewriting.
    #[test]
    fn one_code_carries_both_families() {
        for (source, label) in [
            (
                "finish(Promise.resolve(1).then((v) => v));",
                "Promise chaining",
            ),
            ("finish(Promise.race([]));", "Promise.race"),
            ("finish('a'.localeCompare('b'));", "localeCompare"),
            ("finish((1).toLocaleString());", "toLocaleString"),
            ("finish('e'.normalize());", "String.normalize"),
            ("finish(crypto.randomUUID());", "crypto.randomUUID"),
            ("finish(structuredClone({}));", "structuredClone"),
            ("finish(new Date(0).getHours());", "local-time Date reader"),
            ("finish(btoa('x'));", "base64 globals"),
            (
                "finish(JSON.parse('{}', (k, v) => v));",
                "JSON.parse reviver",
            ),
        ] {
            let error = crate::validate(source).expect_err(label);
            assert!(
                error.is_dialect_refusal(),
                "{label}: the runtime will never run this, so it must read as a refusal ({})",
                error.code.as_str()
            );
            assert!(
                !error.suggestions.is_empty(),
                "{label}: a refusal owes the reader a replacement"
            );
        }

        for (source, label) in [
            ("finish([1].map());", "Array.map arity"),
            (
                "finish(JSON.stringify(1, null, 2, 3));",
                "JSON.stringify arity",
            ),
            ("finish(Array.from());", "Array.from arity"),
            ("finish('a'.replace());", "regex-method arity"),
        ] {
            let error = crate::validate(source).expect_err(label);
            assert!(
                !error.is_dialect_refusal(),
                "{label}: the method exists and the call is wrong ({})",
                error.code.as_str()
            );
        }
    }

    /// The catch-all expression code carries both families too, and its
    /// `globalThis` half is the one an exclusion breaks.
    #[test]
    fn the_expression_catch_all_carries_both_families() {
        for (source, label) in [
            ("finish(globalThis);", "bare globalThis"),
            (
                "globalThis['x'] = 1; finish(1);",
                "computed globalThis access",
            ),
        ] {
            let error = crate::validate(source).expect_err(label);
            assert!(
                error.is_dialect_refusal(),
                "{label}: a globalThis addressing rule is a refusal ({})",
                error.code.as_str()
            );
        }

        for (source, label) in [
            ("finish(parseInt());", "parseInt arity"),
            ("const enum E { a = 1 / 0 }", "const enum finite value"),
            (
                "function f(...rest, last) {}",
                "rest parameters must be last",
            ),
        ] {
            let error = crate::validate(source).expect_err(label);
            assert!(
                !error.is_dialect_refusal(),
                "{label}: ordinary mistake ({})",
                error.code.as_str()
            );
        }
    }

    /// Codes that answer for themselves still answer correctly end to end.
    #[test]
    fn a_wrong_program_is_not_a_dialect_refusal() {
        for (source, label) in [
            ("const x = 1; x = 2;", "assignment to a const"),
            ("finish(taks);", "misspelled name"),
            ("const y: int = ;", "syntax error"),
        ] {
            let error = crate::validate(source).expect_err(label);
            assert!(
                !error.is_dialect_refusal(),
                "{label}: {} reports a wrong program",
                error.code.as_str()
            );
        }

        for (source, label) in [
            ("class A {}", "classes"),
            ("function* g() {}", "generators"),
            ("label: while (true) { break; }", "labels"),
            ("const r = /x/v;", "the unicode-sets flag"),
            ("const n = 1n;", "bigint literals"),
        ] {
            let error = crate::validate(source).expect_err(label);
            assert!(
                error.is_dialect_refusal(),
                "{label}: {} refuses a construct",
                error.code.as_str()
            );
        }
    }

    /// The channel exists to *move* repair text, not to copy it. A suggestion
    /// repeated inside the message is the pre-FIG-1411 shape wearing a new
    /// field, and it costs the reader the same second parse.
    #[test]
    fn a_suggestion_is_never_repeated_in_the_message() {
        for (source, label) in [
            ("class A {}", "class"),
            ("const r = /x/v;", "regex flag"),
            ("label: while (true) { break; }", "label"),
            ("const x = arguments;", "arguments"),
        ] {
            let error = crate::validate(source).expect_err(label);
            assert!(!error.suggestions.is_empty(), "{label}: {error:?}");
            for suggestion in &error.suggestions {
                assert!(
                    !error.message.contains(suggestion.as_str()),
                    "{label}: `{suggestion}` is in both the message and the hint"
                );
            }
        }
    }

    /// The authored `"Unsupported: <refusal>. <rewrite>"` convention is the
    /// whole migration, so the split has to hold at its edges as well as its
    /// middle.
    #[test]
    fn only_the_unsupported_convention_is_split() {
        assert_eq!(
            super::split_inline_repair("Unsupported: classes. Use functions.".to_string()),
            (
                "Unsupported: classes".to_string(),
                Some("Use functions.".to_string())
            )
        );
        assert_eq!(
            super::split_inline_repair("Unsupported: classes.".to_string()),
            ("Unsupported: classes.".to_string(), None),
            "a refusal with no rewrite keeps its trailing period"
        );
        assert_eq!(
            super::split_inline_repair("classes. Use functions.".to_string()),
            ("classes. Use functions.".to_string(), None),
            "prose outside the convention is never guessed at"
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
