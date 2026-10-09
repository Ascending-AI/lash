//! The dialect's typed rejections.
//!
//! A [`Code`] is the stable name of one rejection; a caller matches on it
//! and never on the message. A refusal says the dialect does not have a
//! construct however it is written; a program defect says this use of a
//! supported construct is wrong.

use lash_kernel_dialect::{Diagnostic, DiagnosticKind, Span};
use ruff_text_size::TextRange;

macro_rules! codes {
    ($($variant:ident => $text:literal, $kind:ident;)*) => {
        /// Every rejection the Python front end makes.
        #[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
        pub enum Code {
            $($variant,)*
        }

        impl Code {
            /// Every code, in declaration order.
            pub const ALL: &'static [Code] = &[$(Code::$variant,)*];

            /// The text a diagnostic carries in `code`.
            pub fn as_str(self) -> &'static str {
                match self {
                    $(Code::$variant => $text,)*
                }
            }

            pub fn kind(self) -> DiagnosticKind {
                match self {
                    $(Code::$variant => DiagnosticKind::$kind,)*
                }
            }
        }
    };
}

codes! {
    Syntax => "PY_SYNTAX", ProgramDefect;
    SourceTooLarge => "PY_SOURCE_TOO_LARGE", Refusal;
    TooDeep => "PY_TOO_DEEP", Refusal;
    InvalidDocument => "PY_INVALID_DOCUMENT", Refusal;
    LibraryMissing => "PY_LIBRARY_MISSING", Refusal;
    UnknownName => "PY_UNKNOWN_NAME", ProgramDefect;
    Arguments => "PY_ARGUMENTS", ProgramDefect;
    AwaitOutsideAsync => "PY_AWAIT_OUTSIDE_ASYNC", ProgramDefect;
    ReturnOutsideFunction => "PY_RETURN_OUTSIDE_FUNCTION", ProgramDefect;
    CoroutineNotAwaited => "PY_COROUTINE_NOT_AWAITED", Refusal;
    ClassUnsupported => "PY_CLASS_UNSUPPORTED", Refusal;
    GeneratorUnsupported => "PY_GENERATOR_UNSUPPORTED", Refusal;
    WithUnsupported => "PY_WITH_UNSUPPORTED", Refusal;
    MatchUnsupported => "PY_MATCH_UNSUPPORTED", Refusal;
    DecoratorUnsupported => "PY_DECORATOR_UNSUPPORTED", Refusal;
    ImportUnsupported => "PY_IMPORT_UNSUPPORTED", Refusal;
    RegexUnsupported => "PY_REGEX_UNSUPPORTED", Refusal;
    StarUnsupported => "PY_STAR_UNSUPPORTED", Refusal;
    KeywordCallDynamic => "PY_KEYWORD_CALL_DYNAMIC", Refusal;
    AttributeUnsupported => "PY_ATTRIBUTE_UNSUPPORTED", Refusal;
    MethodUnsupported => "PY_METHOD_UNSUPPORTED", Refusal;
    BuiltinAsValue => "PY_BUILTIN_AS_VALUE", Refusal;
    BuiltinUnsupported => "PY_BUILTIN_UNSUPPORTED", Refusal;
    OperatorUnsupported => "PY_OPERATOR_UNSUPPORTED", Refusal;
    FormatSpecUnsupported => "PY_FORMAT_SPEC_UNSUPPORTED", Refusal;
    AsyncUnsupported => "PY_ASYNC_UNSUPPORTED", Refusal;
    DeleteUnsupported => "PY_DELETE_UNSUPPORTED", Refusal;
    TargetUnsupported => "PY_TARGET_UNSUPPORTED", Refusal;
    ExceptionClass => "PY_EXCEPTION_CLASS", Refusal;
    GlobalShadowed => "PY_GLOBAL_SHADOWED", Refusal;
    LiteralUnsupported => "PY_LITERAL_UNSUPPORTED", Refusal;
    SyntaxUnsupported => "PY_SYNTAX_UNSUPPORTED", Refusal;
}

/// A span of the submitted source.
pub(crate) fn span(range: TextRange) -> Span {
    Span {
        start: range.start().to_usize(),
        end: range.end().to_usize(),
    }
}

pub(crate) fn diagnostic(code: Code, message: impl Into<String>, range: TextRange) -> Diagnostic {
    Diagnostic {
        code: code.as_str().to_string(),
        message: message.into(),
        span: Some(span(range)),
        kind: code.kind(),
        repairs: Vec::new(),
    }
}

/// A rejection with what to write instead.
pub(crate) fn with_repair(
    code: Code,
    message: impl Into<String>,
    range: TextRange,
    repair: impl Into<String>,
) -> Diagnostic {
    let mut diagnostic = diagnostic(code, message, range);
    diagnostic.repairs.push(repair.into());
    diagnostic
}

/// A rejection that names no place in the source.
pub(crate) fn unplaced(code: Code, message: impl Into<String>) -> Diagnostic {
    Diagnostic {
        code: code.as_str().to_string(),
        message: message.into(),
        span: None,
        kind: code.kind(),
        repairs: Vec::new(),
    }
}
