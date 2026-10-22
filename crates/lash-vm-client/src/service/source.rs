//! Read/edit integrator seam: these source operations intentionally exchange
//! typed documents; host retention uses `DocumentBytes`.
//!
//! Host-owned source operations, isolated in the service's worker pool.
use lash_kernel_dialect::{GeneratedSourceLimits, PrintOptions, PrintedSource, SourceFormat};
use lash_kernel_doc::{Captions, Document, IdentifiedDocument};

use lash_protocol_types::engine_fault::{PayloadKind, SequenceFault};

use super::{DialectRefusal, Request, Response, Service, runtime_ops::ServiceRuntimeOps as _};
use crate::PoolError;

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
    #[error("undeclared package members: {members:?}")]
    UndeclaredMembers { members: Vec<String> },
}

impl Service {
    /// Print exact generated source, optionally with disposable final-text
    /// spans. This performs no compilation, comparison, admission or storage.
    /// Whole-workflow limits are independent of the ordinary cell byte cap.
    ///
    /// `captions`, of `document`, are spelled where the dialect can; the rest
    /// are listed in [`PrintedSource::dropped`]. Captions of another
    /// document, or naming a site that is not captionable, are refused here.
    pub async fn print(
        &self,
        document: &Document,
        captions: Option<&Captions>,
        format: SourceFormat,
        options: PrintOptions,
    ) -> Result<PrintedSource, SourceServiceError> {
        // Defend recursive serialization of a host-constructed document.
        lash_kernel_dialect::check_document_depth(document, options.limits)?;
        if let Some(captions) = captions {
            lash_kernel_dialect::check_captions(document, captions, options.limits)?;
        }
        let response = self
            .request_accounted(Request::Print {
                format,
                document: document.canonical_bytes(),
                captions: captions.cloned(),
                options,
            })
            .await?;
        match response {
            Response::Printed(printed) => Ok(printed),
            other => Err(refused(other)),
        }
    }

    /// Lower a whole authored workflow module in the dialect of `language`,
    /// against exactly `declared_packages` in the surface `packages` (the
    /// worker's own set, or a stored one sent whole), under the host's
    /// source limits or, without them, the dialect's
    /// standard policy. Exported source is read back against its base
    /// document with [`Self::read_exported`] instead.
    pub async fn import_workflow(
        &self,
        language: lash_kernel_doc::SourceLanguage,
        source: &str,
        namespaces: &lash_kernel_dialect::Namespaces,
        limits: Option<lash_kernel_dialect::workflow::WorkflowSourceLimits>,
    ) -> Result<(Document, lash_kernel_doc::SourceMap, Captions), SourceServiceError> {
        let response = self
            .request_accounted(Request::ImportWorkflow {
                language,
                source: source.to_owned(),
                namespaces: namespaces.clone(),
                limits,
            })
            .await?;
        match response {
            Response::Lowered {
                document,
                source_map,
                captions,
                ..
            } => {
                let document = IdentifiedDocument::decode_standard(&document)
                    .map_err(|error| PoolError::payload(PayloadKind::ServiceResponse, error))?;
                // A module import answers the map and the captions of the
                // source it read.
                let (map, captions) = source_map.zip(captions).ok_or_else(|| {
                    PoolError::payload(
                        PayloadKind::ServiceResponse,
                        "a workflow import answered without its source map or captions",
                    )
                })?;
                map.validate_identified(&document, source)
                    .map_err(|error| PoolError::payload(PayloadKind::ServiceResponse, error))?;
                captions
                    .validate_identified(&document)
                    .map_err(|error| PoolError::payload(PayloadKind::ServiceResponse, error))?;
                Ok((document.into_document(), map, captions))
            }
            other => Err(refused(other)),
        }
    }

    /// Read exported source back against `base`, the document it was printed
    /// from, without resolving or admitting its dependencies, with the
    /// captions the source spells. The source carries only code; the
    /// environment is the base document's. A host may
    /// then compare `Document::canonical_bytes()`, the canonical token
    /// encoding identities are taken over (`K-ID-003`), and separately admit
    /// the document against its execution environment.
    pub async fn read_exported(
        &self,
        source: &str,
        base: &Document,
        format: SourceFormat,
        limits: GeneratedSourceLimits,
    ) -> Result<(Document, Captions), SourceServiceError> {
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
                operand_type: diagnostic.operand_type,
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
            Response::Read { document, captions } => {
                let document = IdentifiedDocument::decode_standard(&document)
                    .map_err(|error| PoolError::payload(PayloadKind::ServiceResponse, error))?;
                captions
                    .validate_identified(&document)
                    .map_err(|error| PoolError::payload(PayloadKind::ServiceResponse, error))?;
                Ok((document.into_document(), captions))
            }
            other => Err(refused(other)),
        }
    }
}

fn refused(response: Response) -> SourceServiceError {
    match response {
        Response::Unprintable { site } => {
            SourceServiceError::Print(lash_kernel_dialect::PrintError::Unprintable { site })
        }
        Response::DialectRefused(refusal) => SourceServiceError::Dialect(refusal),
        Response::UndeclaredMembers { members } => {
            SourceServiceError::UndeclaredMembers { members }
        }
        Response::UnknownDialect { dialect } => SourceServiceError::UnknownDialect { dialect },
        _ => PoolError::breach(SequenceFault::UnexpectedServiceResponse).into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// FIG-7109: source-service errors retain the worker's typed code site.
    #[test]
    fn unprintable_worker_sites_reach_the_source_service_error() {
        let site = lash_vocabulary::sites::Site::new(lash_vocabulary::sites::Unit::Main, vec![2]);
        let wire = rmp_serde::to_vec_named(&Response::Unprintable { site: site.clone() }).unwrap();
        let response = rmp_serde::from_slice(&wire).unwrap();
        let error = refused(response);
        assert!(
            matches!(error,
            SourceServiceError::Print(lash_kernel_dialect::PrintError::Unprintable { site: actual })
                if actual == site),
            "host must retain the typed site"
        );
    }
}
