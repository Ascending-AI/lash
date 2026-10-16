//! Host-owned source operations, isolated in the service's worker pool.
use lash_kernel_dialect::{GeneratedSourceLimits, PrintOptions, PrintedSource, SourceFormat};
use lash_kernel_doc::Document;
use lash_kernel_doc::graph_tokens::decode_body;

use super::{DialectRefusal, Request, Response, Service, runtime_ops::ServiceRuntimeOps as _};
use crate::PoolError;
use lash_vm_protocol::{PayloadKind, SequenceFault};

/// A pure source operation's typed refusal or worker failure. Decoding a
/// document does not admit it; admission remains a separate host operation.
#[derive(Debug, thiserror::Error)]
pub enum SourceServiceError {
    #[error(transparent)]
    Worker(#[from] PoolError),
    #[error(transparent)]
    Print(#[from] lash_kernel_dialect::PrintError),
    #[error("generated source refused: {0:?}")]
    Dialect(DialectRefusal),
    #[error("the worker has no `{dialect}` dialect")]
    UnknownDialect { dialect: String },
}

impl Service {
    /// Print exact generated source, optionally with disposable final-text
    /// spans. This performs no compilation, comparison, admission or storage.
    /// Whole-workflow limits are independent of the ordinary cell byte cap.
    pub async fn print(
        &self,
        document: &Document,
        format: SourceFormat,
        options: PrintOptions,
    ) -> Result<PrintedSource, SourceServiceError> {
        // Defend recursive serialization of a host-constructed document.
        lash_kernel_dialect::check_document_depth(document, options.limits)?;
        let response = self
            .request_accounted(Request::Print {
                format,
                document: document.canonical_bytes(),
                options,
            })
            .await?;
        match response {
            Response::Printed(printed) => Ok(printed),
            other => Err(refused(other)),
        }
    }

    /// Lower a whole authored workflow module in the dialect of `language`,
    /// under the host's source limits or, without them, the dialect's
    /// standard policy. Exported source is read back against its base
    /// document with [`Self::read_exported`] instead.
    pub async fn import_workflow(
        &self,
        language: lash_kernel_doc::SourceLanguage,
        source: &str,
        environment: &lash_kernel_dialect::Environment<'_>,
        limits: Option<lash_kernel_dialect::workflow::WorkflowSourceLimits>,
    ) -> Result<(Document, lash_kernel_doc::SourceMap), SourceServiceError> {
        let response = self
            .request_accounted(Request::ImportWorkflow {
                language,
                source: source.to_owned(),
                effects: environment.effects.clone(),
                tool_roots: environment.tool_roots.clone(),
                controls: environment.controls.clone(),
                limits,
            })
            .await?;
        match response {
            Response::Lowered {
                document,
                source_map,
                ..
            } => {
                let document = decode_body::<Document>(&document)
                    .map_err(|error| PoolError::payload(PayloadKind::ServiceResponse, error))?;
                // A module import answers the map of the source it read.
                let map = source_map.ok_or_else(|| {
                    PoolError::payload(
                        PayloadKind::ServiceResponse,
                        "a workflow import answered without its source map",
                    )
                })?;
                map.validate(&document, source)
                    .map_err(|error| PoolError::payload(PayloadKind::ServiceResponse, error))?;
                Ok((document, map))
            }
            other => Err(refused(other)),
        }
    }

    /// Read exported source back against `base`, the document it was printed
    /// from, without resolving or admitting its dependencies. The source
    /// carries only code; the environment is the base document's. A host may
    /// then compare `Document::canonical_bytes()`, the canonical token
    /// encoding identities are taken over (`K-ID-003`), and separately admit
    /// the document against its execution environment.
    pub async fn read_exported(
        &self,
        source: &str,
        base: &Document,
        format: SourceFormat,
        limits: GeneratedSourceLimits,
    ) -> Result<Document, SourceServiceError> {
        if source.len() > limits.max_bytes {
            let diagnostic: lash_kernel_dialect::Diagnostic =
                lash_kernel_dialect::PrintError::Bytes {
                    limit: limits.max_bytes,
                }
                .into();
            return Err(SourceServiceError::Dialect(DialectRefusal {
                code: diagnostic.code,
                message: diagnostic.message,
                span: None,
                unsupported: true,
                repairs: diagnostic.repairs,
            }));
        }
        let response = self
            .request_accounted(Request::Read {
                format,
                source: source.to_owned(),
                base: base.canonical_bytes(),
                limits,
            })
            .await?;
        match response {
            Response::Read { document } => decode_body::<Document>(&document)
                .map_err(|error| PoolError::payload(PayloadKind::ServiceResponse, error).into()),
            other => Err(refused(other)),
        }
    }
}

fn refused(response: Response) -> SourceServiceError {
    match response {
        Response::DialectRefused(refusal) => SourceServiceError::Dialect(refusal),
        Response::UnknownDialect { dialect } => SourceServiceError::UnknownDialect { dialect },
        _ => PoolError::breach(SequenceFault::UnexpectedServiceResponse).into(),
    }
}
