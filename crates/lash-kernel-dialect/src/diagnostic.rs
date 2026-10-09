//! What a front end or a printer says when it will not go on.

use std::fmt;

/// Byte offsets in the submitted source.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Span {
    pub start: usize,
    pub end: usize,
}

/// Whose fault a rejection is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DiagnosticKind {
    /// The dialect does not support this construct, however it is written.
    Refusal,
    /// The construct is supported; this instance of it is wrong.
    ProgramDefect,
}

/// A typed rejection of a source text or of a document.
///
/// `code` is the dialect's stable name for the rejection; a caller matches
/// on it, never on the message. `repairs` say what to write instead, apart
/// from the message, so a consumer finds the rewrite without parsing prose.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Diagnostic {
    pub code: String,
    pub message: String,
    pub span: Option<Span>,
    pub kind: DiagnosticKind,
    pub repairs: Vec<String>,
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
