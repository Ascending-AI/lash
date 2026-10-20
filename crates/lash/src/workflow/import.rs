//! Whole workflow import in any installed dialect: lowering under the
//! shared workflow contract, then admission, without publication.
use lash_kernel_dialect::workflow::WorkflowSourceLimits;
use lash_kernel_doc::{Captions, DocumentBytes, SourceLanguage, SourceMap};

use super::{WorkflowAdmissionRefusal, WorkflowDocument, WorkflowDocumentEntry};

pub use lash_kernel_dialect::{
    Diagnostic as WorkflowSourceDiagnostic, DiagnosticKind as WorkflowSourceDiagnosticKind,
};

/// The outcome of importing a host-authored workflow module against a
/// workflow environment.
#[derive(Clone, Debug, PartialEq)]
#[non_exhaustive]
pub enum WorkflowImport {
    /// The source lowered and its document was admitted. Nothing was
    /// published. The opaque bytes retain every entry; reading a tree is
    /// explicit through `DocumentBytes::to_document`. `source_map` maps the
    /// document to `source`, the text it was lowered from; publish both with
    /// the document to keep them. `captions` are those the source spelled:
    /// they replace whatever the host held for this workflow.
    Imported {
        packages: lash_execution_contracts::CellPackages,
        document: DocumentBytes,
        source_map: SourceMap,
        source: String,
        captions: Captions,
    },
    /// Exported source read back against the document it was printed from
    /// ([`HostArtifacts::import_exported_workflow`](crate::process::HostArtifacts::import_exported_workflow))
    /// and admitted. Nothing was published. Reading back makes no map:
    /// printing the document answers one with its text. `captions` are those
    /// the source spelled, which replace the host's.
    Read {
        document: DocumentBytes,
        captions: Captions,
    },
    /// The front end refused the source, with its original byte spans.
    Refused(Vec<WorkflowSourceDiagnostic>),
    /// The lowered document cannot run against the supplied environment.
    Admission(WorkflowAdmissionRefusal),
    /// This core cannot provide the requested source operation.
    Unsupported(WorkflowImportUnsupported),
}

/// Why whole workflow import is unavailable.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum WorkflowImportUnsupported {
    /// No kernel workflow document engine is installed on this core.
    EngineUnavailable,
    /// The registered worker does not install the requested dialect.
    DialectUnavailable { dialect: String },
}

pub(crate) async fn import(
    workers: &lash_vm_client::service::Service,
    language: SourceLanguage,
    source: &str,
    packages: lash_execution_contracts::CellPackages,
    operations: &lash_kernel_dialect::Operations,
    checker: &lash_kernel_check::Environment<'_>,
    limits: Option<WorkflowSourceLimits>,
) -> crate::Result<WorkflowImport> {
    let (document, source_map, captions) = match workers
        .import_workflow(
            language,
            source,
            operations,
            lash_vm_client::service::SurfaceRef::of(packages.digest(), packages.stored_wire()),
            limits,
        )
        .await
    {
        Ok(imported) => imported,
        Err(error) => return source_refusal(error),
    };
    let bytes = DocumentBytes::from(document.clone());
    let _admitted = match WorkflowDocument::admit(document, WorkflowDocumentEntry::Main, checker) {
        Ok(document) => document,
        Err(refusal) => return Ok(WorkflowImport::Admission(refusal)),
    };
    Ok(WorkflowImport::Imported {
        packages,
        document: bytes,
        source_map,
        source: source.to_owned(),
        captions,
    })
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

/// Admits exported source read back against its base.
pub(crate) fn admit_read(
    document: lash_kernel_doc::Document,
    captions: Captions,
    checker: &lash_kernel_check::Environment<'_>,
) -> WorkflowImport {
    let bytes = DocumentBytes::from(document.clone());
    let _admitted = match WorkflowDocument::admit(document, WorkflowDocumentEntry::Main, checker) {
        Ok(document) => document,
        Err(refusal) => return WorkflowImport::Admission(refusal),
    };
    WorkflowImport::Read {
        document: bytes,
        captions,
    }
}
