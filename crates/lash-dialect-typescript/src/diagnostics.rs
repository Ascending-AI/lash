use std::fmt;

pub use lash_kernel_dialect::{DiagnosticKind, Span as SourceSpan};

/// Stable names for every TypeScript dialect rejection.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum DiagnosticCode {
    SyntaxError,
    ClassUnsupported,
    GeneratorUnsupported,
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
    ArgumentsUnsupported,
    NamespaceUnsupported,
    DecoratorUnsupported,
    DynamicImportUnsupported,
    JsxUnsupported,
    ImportExportUnsupported,
    UsingUnsupported,
    NewUnsupported,
    ForUnsupported,
    ForOfUnsupported,
    AwaitRequired,
    UnawaitedTool,
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
    ShadowsBuiltin,
    ControlCallPlacement,
    MutualRecursionUnsupported,
    DuplicateBinding,
    DuplicateNodeLabel,
    TemporalDeadZone,
    UnknownBinding,
    AssignConst,
    SavedFunctionUnusable,
    NonLiftableCapture,
    ProcessParamTypeUnsupported,
    ProcessReturnTypeUnsupported,
    MethodUnsupported,
    DateImmutable,
    DeleteNonReferenceUnsupported,
    FunctionRedeclarationUnsupported,
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
        Self::ArgumentsUnsupported,
        Self::NamespaceUnsupported,
        Self::DecoratorUnsupported,
        Self::DynamicImportUnsupported,
        Self::JsxUnsupported,
        Self::ImportExportUnsupported,
        Self::UsingUnsupported,
        Self::NewUnsupported,
        Self::ForUnsupported,
        Self::ForOfUnsupported,
        Self::AwaitRequired,
        Self::UnawaitedTool,
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
        Self::ShadowsBuiltin,
        Self::ControlCallPlacement,
        Self::MutualRecursionUnsupported,
        Self::DuplicateBinding,
        Self::DuplicateNodeLabel,
        Self::TemporalDeadZone,
        Self::UnknownBinding,
        Self::AssignConst,
        Self::SavedFunctionUnusable,
        Self::NonLiftableCapture,
        Self::ProcessParamTypeUnsupported,
        Self::ProcessReturnTypeUnsupported,
        Self::MethodUnsupported,
        Self::DateImmutable,
        Self::DeleteNonReferenceUnsupported,
        Self::FunctionRedeclarationUnsupported,
        Self::ReturnOutsideFunction,
        Self::LoopControlOutsideLoop,
        Self::UnsupportedStatement,
        Self::UnsupportedExpression,
        Self::InvalidAst,
        Self::LinkError,
    ];

    /// The accepted in-dialect idiom for a construct this code refuses.
    ///
    /// Lash VM has carried a repair channel since it shipped: `parse_hint` and
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
            Self::ArgumentsUnsupported => {
                "declare an explicit `...rest` parameter and use it instead"
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
                "only the Error family, `Array`, `Map`, `Set`, `Date`, `RegExp`, `URL`, and `URLSearchParams` are constructible"
            }
            Self::ForUnsupported => {
                "run the update in the body before a `continue` that leaves a `try` with a `finally`, and drop it from the loop head"
            }
            Self::ForOfUnsupported => "iterate a materialized array with plain `for...of`",
            Self::AwaitRequired => "add `await` — the call returns a promise",
            Self::UnawaitedTool => {
                "await the tool call, or collect its handle into `Promise.all` or `Promise.allSettled` and await the aggregate"
            }
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
            Self::SavedFunctionUnusable => {
                "define the function again in this cell, using only what this session offers"
            }
            Self::NonLiftableCapture => "pass the value to the process through its `run` arguments",
            Self::ProcessParamTypeUnsupported => {
                "declare the parameter with a durable type: a primitive, an array, an object literal, a union of string literals, or a host data type"
            }
            Self::ProcessReturnTypeUnsupported => {
                "declare the return with a durable type, optionally wrapped in Promise"
            }
            Self::MethodUnsupported => {
                "use a method the dialect's standard-library contract lists for this receiver"
            }
            Self::DateImmutable => "build a new date instead: `new Date(d.getTime() + n)`",
            Self::DeleteNonReferenceUnsupported => {
                "evaluate the operand as its own statement; `delete` removes a property, `delete object.member`"
            }
            Self::FunctionRedeclarationUnsupported => {
                "give each declaration its own name; hold a value that changes in a `let`"
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
            Self::ShadowsBuiltin => {
                "rename the binding and its references, preserving the built-in name"
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
    /// the whole determinism-refusal set — `Promise.then`, `Promise.resolve`,
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
            | Self::ArgumentsUnsupported
            | Self::NamespaceUnsupported
            | Self::DecoratorUnsupported
            | Self::DynamicImportUnsupported
            | Self::JsxUnsupported
            | Self::ImportExportUnsupported
            | Self::UsingUnsupported
            | Self::NewUnsupported
            | Self::ForUnsupported
            | Self::ForOfUnsupported
            | Self::UnawaitedTool
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
            | Self::SavedFunctionUnusable
            | Self::NonLiftableCapture
            | Self::DateImmutable
            // Stricter than ECMA-262 exactly where `tsc --strict` rejects
            // the program (ADR 0064, FIG-3651).
            | Self::DeleteNonReferenceUnsupported
            | Self::FunctionRedeclarationUnsupported
            // Rules about size, placement, and shape. No single construct to
            // name, but just as much a refusal: the runtime will not accept
            // this program however it is debugged.
            | Self::RegexIteratorPosition
            | Self::RegexPatternTooLong
            | Self::RegexNestingLimit
            | Self::SourceNestingLimit
            | Self::SourceTooLarge
            | Self::ReservedIdentifier
            | Self::ShadowsBuiltin
            | Self::ProcessParamTypeUnsupported
            | Self::ProcessReturnTypeUnsupported => CodeClassification::AlwaysRefusal,

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
            | Self::DuplicateNodeLabel
            | Self::TemporalDeadZone
            | Self::UnknownBinding
            | Self::AssignConst
            | Self::AwaitRequired
            | Self::ReturnOutsideFunction
            | Self::LoopControlOutsideLoop
            | Self::InvalidAst
            | Self::ControlCallPlacement
            | Self::LinkError => CodeClassification::AlwaysDefect,
        }
    }

    /// The stable machine-readable diagnostic name.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::SyntaxError => "TS_SYNTAX_ERROR",
            Self::ClassUnsupported => "TS_CLASS_UNSUPPORTED",
            Self::GeneratorUnsupported => "TS_GENERATOR_UNSUPPORTED",
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
            Self::ArgumentsUnsupported => "TS_ARGUMENTS_UNSUPPORTED",
            Self::NamespaceUnsupported => "TS_NAMESPACE_UNSUPPORTED",
            Self::DecoratorUnsupported => "TS_DECORATOR_UNSUPPORTED",
            Self::DynamicImportUnsupported => "TS_DYNAMIC_IMPORT_UNSUPPORTED",
            Self::JsxUnsupported => "TS_JSX_UNSUPPORTED",
            Self::ImportExportUnsupported => "TS_IMPORT_EXPORT_UNSUPPORTED",
            Self::UsingUnsupported => "TS_USING_UNSUPPORTED",
            Self::NewUnsupported => "TS_NEW_UNSUPPORTED",
            Self::ForUnsupported => "TS_FOR_UNSUPPORTED",
            Self::ForOfUnsupported => "TS_FOR_OF_UNSUPPORTED",
            Self::AwaitRequired => "TS_AWAIT_REQUIRED",
            Self::UnawaitedTool => "TS_UNAWAITED_TOOL",
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
            Self::ShadowsBuiltin => "TS_SHADOWS_BUILTIN",
            Self::ControlCallPlacement => "TS_CONTROL_CALL_PLACEMENT",
            Self::MutualRecursionUnsupported => "TS_MUTUAL_RECURSION_UNSUPPORTED",
            Self::DuplicateBinding => "TS_DUPLICATE_BINDING",
            Self::DuplicateNodeLabel => "TS_DUPLICATE_NODE_LABEL",
            Self::TemporalDeadZone => "TS_TEMPORAL_DEAD_ZONE",
            Self::UnknownBinding => "TS_UNKNOWN_BINDING",
            Self::AssignConst => "TS_ASSIGN_CONST",
            Self::SavedFunctionUnusable => "TS_SAVED_FUNCTION_UNUSABLE",
            Self::NonLiftableCapture => "TS_NON_LIFTABLE_CAPTURE",
            Self::ProcessParamTypeUnsupported => "TS_PROCESS_PARAM_TYPE_UNSUPPORTED",
            Self::ProcessReturnTypeUnsupported => "TS_PROCESS_RETURN_TYPE_UNSUPPORTED",
            Self::MethodUnsupported => "TS_METHOD_UNSUPPORTED",
            Self::DateImmutable => "TS_DATE_IMMUTABLE",
            Self::DeleteNonReferenceUnsupported => "TS_DELETE_NON_REFERENCE_UNSUPPORTED",
            Self::FunctionRedeclarationUnsupported => "TS_FUNCTION_REDECLARATION_UNSUPPORTED",
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
    /// `hint:` line, exactly as Lash VM renders its own hint channel.
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}: {}", self.code.as_str(), self.message)?;
        for suggestion in &self.suggestions {
            write!(formatter, "\nhint: {suggestion}")?;
        }
        Ok(())
    }
}

impl std::error::Error for Diagnostic {}

impl From<Diagnostic> for lash_kernel_dialect::Diagnostic {
    fn from(diagnostic: Diagnostic) -> Self {
        Self {
            code: diagnostic.code.as_str().to_string(),
            message: diagnostic.message,
            span: diagnostic.span,
            kind: diagnostic.kind,
            repairs: diagnostic.suggestions,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{CodeClassification, DiagnosticCode};

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

    /// Policy feedback tells the model to rewrite the cell "in the form
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

    /// FIG-5765: a repair preserves the operands the model actually wrote.
    #[test]
    fn repairs_name_the_rejected_operands() {
        let error =
            crate::validate("const renderInvoice = (text) => text; renderInvoice`invoice`;")
                .expect_err("tagged templates are refused");
        assert_eq!(error.code, DiagnosticCode::TaggedTemplateUnsupported);
        assert!(error.is_dialect_refusal());
        assert!(
            error
                .suggestions
                .iter()
                .any(|repair| repair.contains("renderInvoice(`invoice`)")),
            "the repair must name the user's function and argument: {error}"
        );
        for (source, names) in [
            (
                // Locale formatting is refused on literal receivers.
                "const invoice = 1; [invoice].toLocaleString();",
                vec!["[invoice].toLocaleString"],
            ),
            (
                "const invoice = { total: 1 }; invoice.__proto__;",
                vec!["invoice", "__proto__"],
            ),
            (
                "const invoice = { total: 1 }; const baseInvoice = { paid: true }; invoice.__proto__ = baseInvoice;",
                vec!["invoice", "__proto__", "baseInvoice"],
            ),
            (
                "const invoice = { total: 1 }; const CustomInvoice = () => ({}); invoice instanceof CustomInvoice;",
                vec!["invoice", "CustomInvoice"],
            ),
            (
                "const CustomInvoice = () => ({}); new CustomInvoice();",
                vec!["CustomInvoice", "`Array`"],
            ),
            ("Math.extra();", vec!["Math.extra"]),
            ("Math;", vec!["Math"]),
            (
                "const loadInvoice = () => 1; delete loadInvoice();",
                vec!["loadInvoice()"],
            ),
        ] {
            let error = crate::validate(source).expect_err("the construct is refused");
            assert!(error.is_dialect_refusal(), "{error}");
            for name in names {
                assert!(
                    error.suggestions.iter().any(|repair| repair.contains(name)),
                    "the repair must preserve {name}: {error}"
                );
            }
        }
    }

    /// FIG-5765: every advertised repair idiom is accepted by the dialect.
    /// Prose-only idioms have a complete cell witness; concrete site repairs
    /// are checked against the actual suggestion before lowering that form.
    #[test]
    fn every_accepted_repair_idiom_lowers() {
        use DiagnosticCode::*;
        let examples = [
            (
                ClassUnsupported,
                "function make(message, code) { return Object.assign(new Error(message), { code }); } make('oops', 'bad');",
            ),
            (
                GeneratorUnsupported,
                "function collect() { const values = []; for (const n of [1, 2]) values.push(n); return values; } collect();",
            ),
            (
                WithUnsupported,
                "const invoice = { total: 1 }; invoice.total;",
            ),
            (EvalUnsupported, "const answer = 1 + 2; answer;"),
            (
                FunctionConstructorUnsupported,
                "function add(n) { return n + 1; } const inc = n => n + 1; inc(add(1));",
            ),
            (
                LabelUnsupported,
                "function region() { while (true) { return 1; } } region();",
            ),
            (RegexFlagUnsupported, "const r = /x/gimsuy; r.exec('x');"),
            (
                RegexIndicesFlagUnsupported,
                "const match = /x/.exec('x'); if (match) { match.index; match[0]; }",
            ),
            (
                RegexUnicodeSetsFlagUnsupported,
                "const r = /[a-z]/u; r.test('x');",
            ),
            (
                AccessorUnsupported,
                "const o = { value: 1, compute: () => 1 }; o.value; o.compute();",
            ),
            (
                PrototypeMutationUnsupported,
                "const invoice = { total: 1 }; Object.assign({}, invoice, { paid: true });",
            ),
            (
                ThisUnsupported,
                "function read(value) { return value; } const invoice = read(1); globalThis.invoice;",
            ),
            (
                ArgumentsUnsupported,
                "function read(...rest) { return rest; } read(1, 2);",
            ),
            (NamespaceUnsupported, "const invoice = 1; invoice;"),
            (
                DecoratorUnsupported,
                "const wrap = value => value; const invoice = wrap(1); invoice;",
            ),
            (DynamicImportUnsupported, "await echo(1);"),
            (
                JsxUnsupported,
                "const element = { tag: 'p', text: 'invoice' }; const html = `<p>${element.text}</p>`; html;",
            ),
            (ImportExportUnsupported, "await echo(1);"),
            (
                UsingUnsupported,
                "const release = () => 1; try { 1; } finally { release(); }",
            ),
            (
                NewUnsupported,
                "new Array(3); new Map(); new Set(); new Date(0); new RegExp('x'); new URL('https://example.com'); new URLSearchParams('a=b'); new Error('x'); new TypeError('x'); new RangeError('x'); new SyntaxError('x'); new ReferenceError('x'); new URIError('x'); new EvalError('x'); new AggregateError([], 'x');",
            ),
            (
                ForUnsupported,
                "let i = 0; for (; i < 2;) { try { i++; continue; } finally { console.log(i); } }",
            ),
            (
                ForOfUnsupported,
                "const values = [1, 2]; for (const value of values) console.log(value);",
            ),
            (AwaitRequired, "await echo(1);"),
            (
                UnawaitedTool,
                "await echo(1); await Promise.all([echo(2)]); await Promise.allSettled([echo(3)]);",
            ),
            (
                YieldUnsupported,
                "function collect() { return [1, 2]; } collect();",
            ),
            (
                TaggedTemplateUnsupported,
                "const renderInvoice = text => text; renderInvoice(`invoice`);",
            ),
            (
                SuperUnsupported,
                "const helper = value => value; helper(1);",
            ),
            (
                MetaPropertyUnsupported,
                "const context = { name: 'invoice' }; context.name;",
            ),
            (
                BigIntUnsupported,
                "const n = 12; const decimal = '12345678901234567890'; n; decimal;",
            ),
            (
                PrivateNameUnsupported,
                "function make() { const field = { value: 1 }; return () => field.value; } make()();",
            ),
            (SequenceUnsupported, "console.log(1); console.log(2);"),
            (
                InstanceOfUnsupported,
                "const err = new Error('x'); err.name; Array.isArray([]); [] instanceof Array;",
            ),
            (DebuggerUnsupported, "console.log({ total: 1 });"),
            (
                LoneSurrogateLiteralUnsupported,
                r"const character = '\u{1F639}'; character;",
            ),
            (DeclareUnsupported, "const invoice = 1; invoice;"),
            (
                MutualRecursionUnsupported,
                "function second(n) { return n; } function first(n) { return second(n); } first(1); const work = [1]; while (work.length) work.pop();",
            ),
            (
                SavedFunctionUnusable,
                "function read(value) { return value; } const invoice = read(1); invoice;",
            ),
            (
                NonLiftableCapture,
                "const job = async (value: number): Promise<number> => { return value; }; job;",
            ),
            (
                ProcessParamTypeUnsupported,
                "const job = async (n: number, items: number[], row: { total: number }, status: 'open' | 'paid'): Promise<number> => { return n; }; job;",
            ),
            (
                ProcessReturnTypeUnsupported,
                "const job = async (n: number): Promise<number> => { return n; }; job;",
            ),
            (
                MethodUnsupported,
                "const invoice = [1]; invoice.map(n => n + 1);",
            ),
            (
                DateImmutable,
                "const d = new Date(0); const n = 1; new Date(d.getTime() + n);",
            ),
            (
                DeleteNonReferenceUnsupported,
                "const object = { member: 1 }; delete object.member;",
            ),
            (
                FunctionRedeclarationUnsupported,
                "function first() { return 1; } function second() { return 2; } let value = first(); value = second();",
            ),
            (
                UnsupportedStatement,
                "const invoice = 1; console.log(invoice);",
            ),
            (UnsupportedExpression, "const invoice = 1; invoice + 2;"),
            (
                RegexIteratorPosition,
                "[...'x'.matchAll(/x/g)]; for (const match of 'x'.matchAll(/x/g)) match[0]; Array.from('x'.matchAll(/x/g)); new Map([[1, 2]]); new Set('x'.matchAll(/x/g)); Object.fromEntries([[1, 2]]);",
            ),
            (
                RegexPatternTooLong,
                "const first = /x/; const second = /y/; first.test('x'); second.test('y');",
            ),
            (
                RegexNestingLimit,
                "const first = /x/; const second = /y/; first.test('x'); second.test('y');",
            ),
            (
                SourceNestingLimit,
                "const subtotal = 1 + 2; const total = subtotal + 3; total;",
            ),
            (SourceTooLarge, "const invoice = 1;"),
            (ReservedIdentifier, "const invoice = 1; invoice;"),
            (ShadowsBuiltin, "const URL_ = 1; URL_;"),
        ];
        for code in DiagnosticCode::ALL {
            if code.accepted_idiom().is_some() {
                assert!(
                    examples.iter().any(|(sample_code, _)| sample_code == code),
                    "{} advertises an idiom with no lowering witness",
                    code.as_str()
                );
            }
        }
        for (code, source) in examples {
            crate::tests::lower_with_effects(source).unwrap_or_else(|error| {
                panic!(
                    "{} suggests an unaccepted idiom: {error}\n{source}",
                    code.as_str()
                )
            });
        }

        let sites = [
            (
                "const invoice = { name: 'Invoice' }; const CustomInvoice = () => ({});",
                "invoice instanceof CustomInvoice;",
                InstanceOfUnsupported,
                "(invoice).name",
            ),
            (
                "const renderInvoice = text => text;",
                "renderInvoice`invoice`;",
                TaggedTemplateUnsupported,
                "renderInvoice(`invoice`)",
            ),
            (
                "const renderInvoice = text => text;",
                r"renderInvoice`\8`;",
                TaggedTemplateUnsupported,
                r#"renderInvoice("`\\8`")"#,
            ),
            (
                "const renderInvoice = text => text; const invoice = { total: 1 };",
                "renderInvoice`total ${invoice.total}`;",
                TaggedTemplateUnsupported,
                "renderInvoice(`total ${invoice.total}`)",
            ),
            (
                "let invoice = { total: 1 }; const baseInvoice = { paid: true };",
                "invoice.__proto__ = baseInvoice;",
                PrototypeMutationUnsupported,
                "Object.assign({}, invoice, baseInvoice)",
            ),
            (
                "let invoice = { total: 1 }; const baseInvoice = { paid: true };",
                "invoice['__proto__'] = baseInvoice;",
                PrototypeMutationUnsupported,
                "Object.assign({}, invoice, baseInvoice)",
            ),
            (
                "const paid = true;",
                "Array.prototype.paid = paid;",
                PrototypeMutationUnsupported,
                "({ paid: paid })",
            ),
            (
                "const paid = true; const field = 'paid';",
                "Array.prototype[field] = paid;",
                PrototypeMutationUnsupported,
                "({ [field]: paid })",
            ),
            (
                "const invoice = { total: 1 }; const CustomInvoice = () => ({});",
                "invoice instanceof CustomInvoice;",
                InstanceOfUnsupported,
                "Array.isArray(invoice)",
            ),
            (
                "const invoices = [{ total: 1 }]; const CustomInvoice = () => ({});",
                "invoices[0] instanceof CustomInvoice;",
                InstanceOfUnsupported,
                "Array.isArray(invoices[0])",
            ),
            (
                "const loadInvoice = () => 1;",
                "delete loadInvoice();",
                DeleteNonReferenceUnsupported,
                "loadInvoice();",
            ),
        ];
        for (prelude, refused, code, form) in sites {
            let error = crate::validate(&format!("{prelude} {refused}"))
                .expect_err("the source construct is refused");
            assert_eq!(error.code, code);
            assert!(error.is_dialect_refusal(), "{error}");
            assert!(
                error.suggestions.iter().any(|repair| repair.contains(form)),
                "the offered form must be the one we lower: {error}\nexpected {form}"
            );
            crate::validate(&format!("{prelude} {form};"))
                .unwrap_or_else(|error| panic!("a repair must lower: {form}: {error}"));
        }
        let constructors = NewUnsupported
            .accepted_idiom()
            .expect("constructors offer a repair");
        assert!(
            constructors.contains("`Array`"),
            "Array is constructible: {constructors}"
        );
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
