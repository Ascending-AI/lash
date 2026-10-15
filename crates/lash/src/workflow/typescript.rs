//! Whole source import: lowering followed by admission, without publication.
use lash_kernel_dialect::Environment;
use lash_kernel_doc::Annotations;

use super::{WorkflowAdmissionRefusal, WorkflowDocument, WorkflowDocumentEntry};

pub use lash_kernel_dialect::{
    Diagnostic as WorkflowSourceDiagnostic, DiagnosticKind as WorkflowSourceDiagnosticKind,
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
    /// The registered worker does not install the requested dialect.
    DialectUnavailable { dialect: String },
    /// The worker registry cannot resolve one library definition per name.
    LibraryUnavailable(lash_kernel_dialect::LibraryError),
}

pub(crate) async fn import(
    workers: &lash_vm_client::service::Service,
    source: &str,
    dialect: &Environment<'_>,
    checker: &lash_kernel_check::Environment<'_>,
    limits: lash_dialect_typescript::SourceLimits,
) -> crate::Result<WorkflowImport> {
    let lowered = match workers
        .import_typescript_workflow(source, dialect, limits.max_bytes(), limits.max_nesting())
        .await
    {
        Ok(lowered) => lowered,
        Err(error) => return source_refusal(error),
    };
    Ok(admit(lowered.document, lowered.annotations, checker))
}

pub(crate) fn source_refusal(
    error: lash_vm_client::service::SourceServiceError,
) -> crate::Result<WorkflowImport> {
    match error {
        lash_vm_client::service::SourceServiceError::Dialect(refusal) => {
            Ok(WorkflowImport::Refused(vec![WorkflowSourceDiagnostic {
                code: refusal.code,
                message: refusal.message,
                span: refusal
                    .span
                    .map(|(start, end)| lash_kernel_doc::Span { start, end }),
                kind: if refusal.unsupported {
                    WorkflowSourceDiagnosticKind::Refusal
                } else {
                    WorkflowSourceDiagnosticKind::ProgramDefect
                },
                repairs: refusal.repairs,
            }]))
        }
        lash_vm_client::service::SourceServiceError::UnknownDialect { dialect } => Ok(
            WorkflowImport::Unsupported(WorkflowImportUnsupported::DialectUnavailable { dialect }),
        ),
        error => Err(crate::EmbedError::WorkflowSource(error)),
    }
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
