//! The pure work a parent asks of a worker: lowering source to a kernel
//! document and printing a document as source. Both read guest-controlled
//! input, so both run here, in the worker's crash domain.
use lash_kernel_dialect::{Diagnostic, DiagnosticKind};
use lash_kernel_doc::Document;
use lash_vm_client::PoolError;
use lash_vm_client::service::{DialectRefusal, Request, Response};
use lash_vm_protocol::{Detail, EncodedPayload, PayloadKind, RunInput, RunRefusal};

use crate::embedding::Embedding;

pub(crate) fn perform(
    embedding: &Embedding,
    request: &EncodedPayload,
) -> Result<EncodedPayload, PoolError> {
    let request: Request = rmp_serde::from_slice(&request.0)
        .map_err(|error| PoolError::payload(PayloadKind::ServiceRequest, error))?;
    let response = match request {
        Request::Lower {
            dialect,
            source,
            effects,
            tool_roots,
            controls,
            bindings,
            functions,
        } => match embedding.lower(
            &dialect,
            &source,
            &lash_kernel_dialect::Environment {
                library: embedding.library(),
                effects: &effects,
                tool_roots: &tool_roots,
                controls: &controls,
                bindings: &bindings,
                functions: &functions,
            },
        ) {
            None => Response::UnknownDialect { dialect },
            Some(Ok(lowered)) => Response::Lowered {
                document: encoded(lowered.document.to_json())?,
                annotations: encoded(lowered.annotations.to_json())?,
            },
            Some(Err(diagnostic)) => Response::DialectRefused(refusal(diagnostic)),
        },
        Request::Print { dialect, document } => {
            let document = std::str::from_utf8(&document)
                .map_err(|error| error.to_string())
                .and_then(|text| Document::from_json(text).map_err(|error| error.to_string()))
                .map_err(|error| {
                    PoolError::refused(RunRefusal::Undecodable {
                        input: RunInput::Document,
                        detail: Detail::new(error),
                    })
                })?;
            match embedding
                .dialects
                .get(&dialect)
                .and_then(|package| package.printer.as_ref())
            {
                None => Response::UnknownDialect { dialect },
                Some(printer) => match printer.print(&document, &embedding.library) {
                    Ok(source) => Response::Printed { source },
                    Err(diagnostic) => Response::DialectRefused(refusal(diagnostic)),
                },
            }
        }
    };
    rmp_serde::to_vec_named(&response)
        .map(EncodedPayload)
        .map_err(|error| PoolError::payload(PayloadKind::ServiceResponse, error))
}

fn encoded(json: Result<String, impl std::fmt::Display>) -> Result<Vec<u8>, PoolError> {
    json.map(String::into_bytes)
        .map_err(|error| PoolError::payload(PayloadKind::ServiceResponse, error))
}

fn refusal(diagnostic: Diagnostic) -> DialectRefusal {
    DialectRefusal {
        code: diagnostic.code,
        message: diagnostic.message,
        span: diagnostic.span.map(|span| (span.start, span.end)),
        unsupported: matches!(diagnostic.kind, DiagnosticKind::Refusal),
        repairs: diagnostic.repairs,
    }
}
