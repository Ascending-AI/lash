//! Host-owned source operations, isolated in the service's worker pool.
use lash_kernel_dialect::{GeneratedSourceLimits, PrintOptions, PrintedSource, SourceFormat};
use lash_kernel_doc::Document;

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
    Encode(#[from] lash_kernel_doc::EncodeError),
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
                document: document.to_json()?.into_bytes(),
                options,
            })
            .await?;
        match response {
            Response::Printed(printed) => Ok(printed),
            other => Err(refused(other)),
        }
    }

    /// Lower a whole authored TypeScript workflow under the host's source limits.
    /// Generated workflows use [`Self::compile_generated`] independently.
    pub async fn import_typescript_workflow(
        &self,
        source: &str,
        environment: &lash_kernel_dialect::Environment<'_>,
        max_bytes: usize,
        max_nesting: usize,
    ) -> Result<lash_kernel_dialect::Lowered, SourceServiceError> {
        let response = self
            .request_accounted(Request::ImportTypeScriptWorkflow {
                source: source.to_owned(),
                effects: environment.effects.clone(),
                tool_roots: environment.tool_roots.clone(),
                controls: environment.controls.clone(),
                max_bytes,
                max_nesting,
            })
            .await?;
        match response {
            Response::Lowered {
                document,
                annotations,
                ..
            } => {
                let document = std::str::from_utf8(&document)
                    .map_err(|error| PoolError::payload(PayloadKind::ServiceResponse, error))?;
                let annotations = std::str::from_utf8(&annotations)
                    .map_err(|error| PoolError::payload(PayloadKind::ServiceResponse, error))?;
                Ok(lash_kernel_dialect::Lowered {
                    document: Document::from_json(document)
                        .map_err(|error| PoolError::payload(PayloadKind::ServiceResponse, error))?,
                    annotations: lash_kernel_doc::Annotations::from_json(annotations)
                        .map_err(|error| PoolError::payload(PayloadKind::ServiceResponse, error))?,
                })
            }
            other => Err(refused(other)),
        }
    }

    /// Decode exact generated source without resolving or admitting its
    /// dependencies. A host may then compare `Document::canonical_bytes()`
    /// and separately admit the document against its execution environment.
    pub async fn compile_generated(
        &self,
        source: &str,
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
            .request_accounted(Request::CompileGenerated {
                format,
                source: source.to_owned(),
                limits,
            })
            .await?;
        match response {
            Response::Generated { document } => {
                let text = std::str::from_utf8(&document)
                    .map_err(|error| PoolError::payload(PayloadKind::ServiceResponse, error))?;
                Document::from_json(text)
                    .map_err(|error| PoolError::payload(PayloadKind::ServiceResponse, error).into())
            }
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
