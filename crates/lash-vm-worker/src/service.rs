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
            helpers,
            reads,
        } => match embedding.library_for(helpers) {
            Err(_) => Response::HelperReleaseNotHeld { release: helpers },
            Ok(library) => match embedding.lower(
                &dialect,
                &source,
                &lash_kernel_dialect::Environment {
                    library,
                    effects: &effects,
                    tool_roots: &tool_roots,
                    controls: &controls,
                    bindings: &bindings,
                    functions: &functions,
                },
            ) {
                None => Response::UnknownDialect { dialect },
                Some(Ok(lowered)) => Response::Lowered {
                    reads: reached(&lowered.document, library, reads),
                    document: encoded(lowered.document.to_json())?,
                    annotations: encoded(lowered.annotations.to_json())?,
                    function_values: embedding
                        .dialects
                        .get(&dialect)
                        .and_then(|package| package.function_values.clone()),
                },
                Some(Err(diagnostic)) => Response::DialectRefused(refusal(diagnostic)),
            },
        },
        Request::Print {
            format,
            document,
            options,
        } => {
            let dialect = format.dialect();
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
                .get(dialect)
                .and_then(|package| package.printer.as_ref())
            {
                None => Response::UnknownDialect {
                    dialect: dialect.into(),
                },
                Some(printer) => match printer.print(&document, options) {
                    Ok(printed) => Response::Printed(printed),
                    Err(error) => Response::DialectRefused(refusal(error.into())),
                },
            }
        }
        Request::CompileGenerated {
            format,
            source,
            limits,
        } => match embedding.dialects.get(format.dialect()) {
            None => Response::UnknownDialect {
                dialect: format.dialect().into(),
            },
            Some(package) => match (package.compile_generated)(&source, limits) {
                Ok(document) => Response::Generated {
                    document: encoded(document.to_json())?,
                },
                Err(error) => Response::DialectRefused(refusal(error)),
            },
        },
    };
    rmp_serde::to_vec_named(&response)
        .map(EncodedPayload)
        .map_err(|error| PoolError::payload(PayloadKind::ServiceResponse, error))
}

/// Those of `reads` the cell `document`'s `main` calls or spawns; none
/// when its linked effects cannot be certified, so every effect it performs
/// stays recorded.
fn reached(
    document: &Document,
    library: &dyn lash_kernel_doc::FunctionCatalog,
    mut reads: std::collections::BTreeSet<lash_kernel_doc::EffectName>,
) -> std::collections::BTreeSet<lash_kernel_doc::EffectName> {
    if reads.is_empty() {
        return reads;
    }
    let linked = lash_kernel_check::derive_view(document, library)
        .ok()
        .and_then(|graph| lash_kernel_check::main_effects(document, &graph, library).ok());
    match linked {
        Some(linked) => {
            reads.retain(|effect| linked.contains_key(effect));
            reads
        }
        None => std::collections::BTreeSet::new(),
    }
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

#[cfg(all(test, feature = "dialect-typescript", feature = "dialect-python"))]
mod tests {
    use super::*;
    use lash_kernel_dialect::{GeneratedSourceLimits, PrintOptions, SourceFormat};

    std::thread_local! {
        static COMPILER_CALLS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    }

    fn counted_compiler(_: &str, _: GeneratedSourceLimits) -> Result<Document, Diagnostic> {
        COMPILER_CALLS.set(COMPILER_CALLS.get() + 1);
        Ok(Document::new(
            lash_kernel_doc::NumberPolicy::Float,
            vec![lash_kernel_doc::Stmt::Finish {
                value: lash_kernel_doc::Expr::Literal(lash_kernel_doc::Literal::Null),
            }],
        ))
    }

    /// Printing invokes only the installed printer. Even mapped printing
    /// makes zero generated-compiler calls; compilation is separately callable.
    #[test]
    fn printing_never_invokes_the_generated_compiler() {
        let mut embedding =
            crate::embedding::tests::every_dialect(&Default::default()).expect("embedding");
        for package in embedding.dialects.values_mut() {
            package.compile_generated = counted_compiler;
        }
        COMPILER_CALLS.set(0);
        let document = lash_kernel_doc::parse_document(
            "kernel 1\nnumbers float\nmain { let greeting = \"λ\" finish greeting }",
        )
        .expect("test document");
        for format in [SourceFormat::TypeScriptV1, SourceFormat::PythonV1] {
            for source_map in [false, true] {
                let request = Request::Print {
                    format,
                    document: document.to_json().expect("json").into_bytes(),
                    options: PrintOptions {
                        source_map,
                        ..Default::default()
                    },
                };
                let encoded = EncodedPayload(rmp_serde::to_vec_named(&request).expect("request"));
                let response = perform(&embedding, &encoded).expect("print response");
                let Response::Printed(printed) =
                    rmp_serde::from_slice(&response.0).expect("response")
                else {
                    panic!("printing must succeed");
                };
                assert_eq!(COMPILER_CALLS.get(), 0, "printing must not compile");
                assert_eq!(printed.source_map.is_some(), source_map);
                assert_eq!(
                    serde_json::to_value(&printed)
                        .expect("wire fields")
                        .get("source_map")
                        .is_some(),
                    source_map,
                    "maps cross only when requested"
                );
                let decoded = match format {
                    SourceFormat::TypeScriptV1 => lash_dialect_typescript::compile_generated(
                        &printed.source,
                        Default::default(),
                    )
                    .expect("TS decode"),
                    SourceFormat::PythonV1 => {
                        lash_dialect_python::compile_generated(&printed.source, Default::default())
                            .expect("Python decode")
                    }
                };
                assert_eq!(
                    document.canonical_bytes().expect("canonical"),
                    decoded.canonical_bytes().expect("canonical")
                );
                let cell = Request::Lower {
                    dialect: format.dialect().into(),
                    source: printed.source,
                    effects: Default::default(),
                    tool_roots: Default::default(),
                    controls: Default::default(),
                    bindings: Default::default(),
                    functions: Default::default(),
                    helpers: embedding.helper_release(),
                    reads: Default::default(),
                };
                let response = perform(
                    &embedding,
                    &EncodedPayload(rmp_serde::to_vec_named(&cell).expect("cell request")),
                )
                .expect("cell response");
                assert!(
                    matches!(
                        rmp_serde::from_slice::<Response>(&response.0).expect("cell response"),
                        Response::DialectRefused(_)
                    ),
                    "a worker cell cannot use the generated marker to import"
                );
                assert_eq!(COMPILER_CALLS.get(), 0);
            }
            let encoded = EncodedPayload(
                rmp_serde::to_vec_named(&Request::CompileGenerated {
                    format,
                    source: String::new(),
                    limits: Default::default(),
                })
                .expect("request"),
            );
            let response = perform(&embedding, &encoded).expect("compile response");
            assert!(matches!(
                rmp_serde::from_slice::<Response>(&response.0).expect("response"),
                Response::Generated { .. }
            ));
            assert_eq!(COMPILER_CALLS.replace(0), 1);
        }
    }
}
