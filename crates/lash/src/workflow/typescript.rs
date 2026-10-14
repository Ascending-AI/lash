//! Whole source import: lowering followed by admission, without publication.
use lash_kernel_dialect::Environment;
use lash_kernel_doc::Annotations;

use super::{WorkflowAdmissionRefusal, WorkflowDocument, WorkflowDocumentEntry};

pub use lash_kernel_dialect::{
    Diagnostic as WorkflowSourceDiagnostic, DiagnosticKind as WorkflowSourceDiagnosticKind,
    Span as WorkflowSourceSpan,
};

/// The outcome of importing host-authored TypeScript against a workflow environment.
#[derive(Clone, Debug, PartialEq)]
#[non_exhaustive]
pub enum WorkflowImport {
    /// The source lowered and its document was admitted. Nothing was published.
    /// The reference selects `main`; all process entries remain in the document.
    Imported {
        document: WorkflowDocument,
        annotations: Annotations,
    },
    /// The front end refused the source, with its original byte spans.
    Refused(Vec<WorkflowSourceDiagnostic>),
    /// The lowered document cannot run against the supplied environment.
    Admission(WorkflowAdmissionRefusal),
    /// This core cannot provide the requested source operation.
    Unsupported(WorkflowImportUnsupported),
}

/// Why whole TypeScript workflow import is unavailable.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum WorkflowImportUnsupported {
    /// No kernel workflow document engine is installed on this core.
    EngineUnavailable,
    /// The worker registry cannot resolve one library definition per name.
    LibraryUnavailable(lash_kernel_dialect::LibraryError),
}

pub(crate) fn import(
    source: &str,
    dialect: &Environment<'_>,
    checker: &lash_kernel_check::Environment<'_>,
    limits: lash_dialect_typescript::SourceLimits,
) -> WorkflowImport {
    let lowered = match lash_dialect_typescript::lower_workflow_with_limits(source, dialect, limits)
    {
        Ok(lowered) => lowered,
        Err(diagnostic) => return WorkflowImport::Refused(vec![diagnostic.into()]),
    };
    admit(lowered.document, lowered.annotations, checker)
}

pub(crate) fn admit(
    document: lash_kernel_doc::Document,
    annotations: Annotations,
    checker: &lash_kernel_check::Environment<'_>,
) -> WorkflowImport {
    let document = match WorkflowDocument::admit(document, WorkflowDocumentEntry::Main, checker) {
        Ok(document) => document,
        Err(refusal) => return WorkflowImport::Admission(refusal),
    };
    WorkflowImport::Imported {
        document,
        annotations,
    }
}

#[cfg(test)]
mod tests;
