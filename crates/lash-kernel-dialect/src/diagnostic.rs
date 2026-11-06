//! What a front end or a printer says when it will not go on.
pub use lash_kernel_doc::SourceOperandType;
use lash_kernel_doc::Span;
use std::fmt;

/// Whose fault a rejection is.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DiagnosticKind {
    /// The dialect or source admission policy refuses this construct.
    Refusal,
    /// The construct is supported, but this program uses it incorrectly.
    ProgramDefect,
}

/// A dialect-owned code, excluding codes that require structured evidence.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct DialectCode(String);

impl TryFrom<String> for DialectCode {
    type Error = &'static str;
    fn try_from(code: String) -> Result<Self, Self::Error> {
        match code.as_str() {
            "SIGNATURE_REQUIRED"
            | "ENTRY_ANNOTATION_REFUSED"
            | "NAMESPACE_SHADOWED_BY_LOCAL"
            | lash_kernel_doc::ImplicitTextConversion::CODE => {
                Err("structured diagnostic code requires its reason data")
            }
            _ if code.is_empty() => Err("diagnostic code is empty"),
            _ => Ok(Self(code)),
        }
    }
}
impl From<DialectCode> for String {
    fn from(code: DialectCode) -> Self {
        code.0
    }
}
impl DialectCode {
    /// A code declared by a dialect implementation.
    ///
    /// # Panics
    /// Panics for an empty code or one that requires structured reason data.
    #[expect(
        clippy::expect_used,
        reason = "dialect implementations declare their own non-structured code constants"
    )]
    pub fn from_static(code: &'static str) -> Self {
        code.to_owned()
            .try_into()
            .expect("a dialect-owned diagnostic code")
    }
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// The sole owner of a rejection's classification, code and source location.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(
    tag = "kind",
    content = "data",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum DiagnosticReason {
    SignatureRequired(Box<lash_kernel_doc::SignatureRequired>),
    EntryAnnotationRefused(Box<lash_kernel_value::EntryAnnotationRefusal>),
    NamespaceShadowing {
        shadowing: Box<lash_kernel_value::NamespaceShadowing>,
        span: Option<Span>,
    },
    ImplicitTextConversion(Box<lash_kernel_doc::ImplicitTextConversion>),
    Dialect {
        code: DialectCode,
        kind: DiagnosticKind,
        span: Option<Span>,
    },
}
impl DiagnosticReason {
    pub fn code(&self) -> &str {
        match self {
            Self::SignatureRequired(_) => "SIGNATURE_REQUIRED",
            Self::EntryAnnotationRefused(_) => "ENTRY_ANNOTATION_REFUSED",
            Self::NamespaceShadowing { .. } => "NAMESPACE_SHADOWED_BY_LOCAL",
            Self::ImplicitTextConversion(_) => lash_kernel_doc::ImplicitTextConversion::CODE,
            Self::Dialect { code, .. } => code.as_str(),
        }
    }
    pub fn kind(&self) -> DiagnosticKind {
        match self {
            Self::NamespaceShadowing { .. } => DiagnosticKind::ProgramDefect,
            Self::Dialect { kind, .. } => *kind,
            Self::SignatureRequired(_)
            | Self::EntryAnnotationRefused(_)
            | Self::ImplicitTextConversion(_) => DiagnosticKind::Refusal,
        }
    }
    pub fn span(&self) -> Option<Span> {
        match self {
            Self::SignatureRequired(fact) => fact.span,
            Self::EntryAnnotationRefused(fact) => fact.span,
            Self::ImplicitTextConversion(fact) => fact.span,
            Self::NamespaceShadowing { span, .. } | Self::Dialect { span, .. } => *span,
        }
    }
}

/// Non-blank diagnostic prose. JSON remains a string; decoding validates it.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct DiagnosticMessage(String);

impl TryFrom<String> for DiagnosticMessage {
    type Error = &'static str;

    fn try_from(message: String) -> Result<Self, Self::Error> {
        if message.trim().is_empty() {
            Err("diagnostic message is blank")
        } else {
            Ok(Self(message))
        }
    }
}
impl From<DiagnosticMessage> for String {
    fn from(message: DiagnosticMessage) -> Self {
        message.0
    }
}
impl DiagnosticMessage {
    /// Prose authored by a diagnostic producer, rather than untrusted input.
    /// Use `TryFrom<String>` when decoding host-supplied text.
    ///
    /// # Panics
    /// Panics if the producer supplies empty or whitespace-only prose.
    #[expect(
        clippy::expect_used,
        reason = "blank authored diagnostic prose is a producer bug"
    )]
    pub fn authored(message: impl Into<String>) -> Self {
        Self::try_from(message.into()).expect("diagnostics must explain the rejection")
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}
impl fmt::Display for DiagnosticMessage {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A typed rejection with host-facing prose and repair suggestions.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Diagnostic {
    pub reason: DiagnosticReason,
    pub message: DiagnosticMessage,
    pub repairs: Vec<String>,
}
impl Diagnostic {
    pub fn new(reason: DiagnosticReason, message: DiagnosticMessage) -> Self {
        Self {
            reason,
            message,
            repairs: Vec::new(),
        }
    }
    pub fn dialect(
        code: DialectCode,
        kind: DiagnosticKind,
        span: Option<Span>,
        message: DiagnosticMessage,
    ) -> Self {
        Self::new(DiagnosticReason::Dialect { code, kind, span }, message)
    }
    pub fn code(&self) -> &str {
        self.reason.code()
    }
    pub fn kind(&self) -> DiagnosticKind {
        self.reason.kind()
    }
    pub fn span(&self) -> Option<Span> {
        self.reason.span()
    }
    pub fn namespace_shadowed(
        shadowing: lash_kernel_value::NamespaceShadowing,
        span: Option<Span>,
    ) -> Self {
        let message = format!(
            "`{}` resolves to local `{}`, which shadows namespace `{}`",
            shadowing.call_path, shadowing.binding_name, shadowing.namespace
        );
        Self::new(
            DiagnosticReason::NamespaceShadowing {
                shadowing: Box::new(shadowing),
                span,
            },
            DiagnosticMessage::authored(message),
        )
    }
}
impl fmt::Display for Diagnostic {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.code(), self.message)?;
        for repair in &self.repairs {
            write!(f, " ({repair})")?;
        }
        Ok(())
    }
}
impl std::error::Error for Diagnostic {}
impl From<lash_kernel_doc::SignatureRequired> for Diagnostic {
    fn from(required: lash_kernel_doc::SignatureRequired) -> Self {
        signature_diagnostic(
            lash_kernel_value::EntrySignatureRefusal::Required(Box::new(required)),
            None,
        )
    }
}

impl Diagnostic {
    /// Explain an entry refusal in the submitted source's language.
    pub fn entry_signature(
        refusal: lash_kernel_value::EntrySignatureRefusal,
        language: lash_kernel_doc::SourceLanguage,
    ) -> Self {
        signature_diagnostic(refusal, Some(language))
    }
}

fn signature_diagnostic(
    refusal: lash_kernel_value::EntrySignatureRefusal,
    language: Option<lash_kernel_doc::SourceLanguage>,
) -> Diagnostic {
    use lash_kernel_value::{AnnotationResolutionReason, EntrySignatureRefusal, SignaturePosition};
    let (function, position, problem) = match &refusal {
        EntrySignatureRefusal::Required(required) => (
            &required.function,
            &required.position,
            "requires an explicit annotation".to_owned(),
        ),
        EntrySignatureRefusal::Annotation(refused) => {
            let problem = match &refused.reason {
                AnnotationResolutionReason::Unresolved { name } => {
                    format!("has an unresolved annotation `{name}`")
                }
                AnnotationResolutionReason::Cycle { name } => {
                    format!("has a cyclic annotation `{name}`")
                }
                AnnotationResolutionReason::Unsupported { construct } => {
                    format!("uses an unsupported annotation ({construct})")
                }
                AnnotationResolutionReason::Limit => {
                    "has an annotation exceeding the resolution limit".to_owned()
                }
            };
            (&refused.function, &refused.position, problem)
        }
    };
    let name = function.name.as_deref().unwrap_or("entry");
    let (position, ts_params, py_params) = match position {
        SignaturePosition::Parameter { name, .. } => (
            format!("parameter `{name}`"),
            format!("{name}: unknown"),
            format!("{name}: object"),
        ),
        SignaturePosition::Result => ("result".into(), String::new(), String::new()),
    };
    let ts = format!("{name}({ts_params}): Promise<unknown>");
    let py = format!("def {name}({py_params}) -> object:");
    let accepted = match language {
        Some(lash_kernel_doc::SourceLanguage::TypeScript) => format!("`{ts}`"),
        Some(lash_kernel_doc::SourceLanguage::Python) => format!("`{py}`"),
        None => format!("TypeScript `{ts}` or Python `{py}`"),
    };
    let message = DiagnosticMessage::authored(format!(
        "Entry `{name}` {position} {problem}; accepted signature, for example: {accepted}"
    ));
    let reason = match refusal {
        EntrySignatureRefusal::Required(required) => DiagnosticReason::SignatureRequired(required),
        EntrySignatureRefusal::Annotation(refused) => {
            DiagnosticReason::EntryAnnotationRefused(refused)
        }
    };
    Diagnostic::new(reason, message)
}
