//! What a front end or a printer says when it will not go on.

use std::fmt;

use lash_kernel_doc::Span;

/// Whose fault a rejection is.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum DiagnosticKind {
    /// The dialect or the host's source admission policy refuses this construct.
    #[default]
    Refusal,
    /// The construct is supported; this instance of it is wrong.
    ProgramDefect,
}

/// A typed rejection of a source text or of a document.
///
/// `code` is the dialect's stable name for the rejection; a caller matches
/// on it, never on the message. `repairs` say what to write instead, apart
/// from the message, so a consumer finds the rewrite without parsing prose.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Diagnostic {
    pub code: String,
    pub message: String,
    pub span: Option<Span>,
    pub kind: DiagnosticKind,
    pub repairs: Vec<String>,
    /// Static operand facts for an implicit text conversion; no repair prose.
    pub operand_type: Option<Box<SourceOperandType>>,
    /// Structured source refusal; presentation belongs to the host.
    pub fact: Option<Box<DiagnosticFact>>,
}

impl Diagnostic {
    /// A lexical binding wins over the namespace, so this call is unreachable.
    pub fn namespace_shadowed(
        shadowing: lash_vocabulary::NamespaceShadowing,
        span: Option<Span>,
    ) -> Self {
        Self {
            code: "NAMESPACE_SHADOWED_BY_LOCAL".into(),
            message: format!(
                "`{}` resolves to local `{}`, which shadows namespace `{}`",
                shadowing.call_path, shadowing.binding_name, shadowing.namespace
            ),
            span,
            kind: DiagnosticKind::ProgramDefect,
            repairs: Vec::new(),
            operand_type: None,
            fact: Some(Box::new(DiagnosticFact::NamespaceShadowing(shadowing))),
        }
    }
}

impl fmt::Display for Diagnostic {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.code, self.message)?;
        for repair in &self.repairs {
            write!(f, " ({repair})")?;
        }
        Ok(())
    }
}

impl std::error::Error for Diagnostic {}

/// What source analysis knows about an operand, independently of runtime values.
/// Functions retain only their known result; objects and arrays distinguish proven
/// representation from annotated fields or elements.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum SourceOperandType {
    Any,
    Undefined,
    Null,
    Boolean,
    Number,
    Text,
    List(Box<Self>),
    Record(std::collections::BTreeMap<String, Self>),
    Object,
    Array,
    Map,
    Set,
    Function { result: Box<Self> },
    Union(Vec<Self>),
    Package { package: String, name: String },
}

/// Typed facts emitted by a front end before executable admission.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
#[non_exhaustive]
pub enum DiagnosticFact {
    SignatureRequired(lash_kernel_doc::SignatureRequired),
    NamespaceShadowing(lash_vocabulary::NamespaceShadowing),
}

impl From<lash_kernel_doc::SignatureRequired> for Diagnostic {
    fn from(required: lash_kernel_doc::SignatureRequired) -> Self {
        Self {
            code: "SIGNATURE_REQUIRED".to_owned(),
            span: required.span,
            fact: Some(Box::new(DiagnosticFact::SignatureRequired(required))),
            ..Self::default()
        }
    }
}
