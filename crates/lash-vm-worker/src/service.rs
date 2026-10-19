//! The pure work a parent asks of a worker: lowering source to a kernel
//! document and printing a document as source. Both read guest-controlled
//! input, so both run here, in the worker's crash domain.
use lash_kernel_dialect::workflow::WorkflowSourceLimits;
use lash_kernel_dialect::{Diagnostic, DiagnosticKind, Mode};
use lash_kernel_doc::{Document, SourceLanguage};
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
        Request::ToolCalls {
            dialect,
            source,
            bindings,
            functions,
            effects,
            controls,
            helpers,
        } => {
            let library = match embedding.library_for(helpers) {
                Ok(library) => library,
                Err(_) => {
                    return rmp_serde::to_vec_named(&Response::HelperReleaseNotHeld {
                        release: helpers,
                    })
                    .map(EncodedPayload)
                    .map_err(|error| PoolError::payload(PayloadKind::ServiceResponse, error));
                }
            };
            match embedding.dialects.get(&dialect) {
                None => Response::UnknownDialect { dialect },
                Some(package) => match package.front_end.tool_calls(
                    &source,
                    &lash_kernel_dialect::Environment {
                        library,
                        effects: &effects,
                        tool_roots: &Default::default(),
                        controls: &controls,
                    },
                    lash_kernel_dialect::CellEnvironment {
                        bindings: &bindings,
                        functions: &functions
                            .into_iter()
                            .map(|(name, function)| (name, function.into_saved()))
                            .collect(),
                    },
                ) {
                    Ok(calls) => Response::ToolCalls(calls),
                    Err(diagnostic) => Response::DialectRefused(refusal(diagnostic)),
                },
            }
        }
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
                },
                Mode::Cell(lash_kernel_dialect::CellEnvironment {
                    bindings: &bindings,
                    functions: &functions
                        .into_iter()
                        .map(|(name, function)| (name, function.into_saved()))
                        .collect(),
                }),
            ) {
                None => Response::UnknownDialect { dialect },
                Some(Ok(lowered)) => Response::Lowered {
                    reads: reached(&lowered.document, library, reads),
                    document: lowered.document.body().to_vec(),
                    source_map: lowered.source_map,
                    written: lowered.written,
                    // Function captions travel in the written table.
                    captions: None,
                    function_values: embedding
                        .dialects
                        .get(&dialect)
                        .and_then(|package| package.function_values.clone()),
                },
                Some(Err(diagnostic)) => Response::DialectRefused(refusal(diagnostic)),
            },
        },
        Request::ImportWorkflow {
            language,
            source,
            effects,
            tool_roots,
            controls,
            limits,
        } => {
            let environment = lash_kernel_dialect::Environment {
                library: embedding.library(),
                effects: &effects,
                tool_roots: &tool_roots,
                controls: &controls,
            };
            match import_module(embedding, language, &source, &environment, limits) {
                None => Response::UnknownDialect {
                    dialect: dialect_name(language).into(),
                },
                Some(Ok(lowered)) => Response::Lowered {
                    document: lowered.document.body().to_vec(),
                    source_map: lowered.source_map,
                    written: lowered.written,
                    captions: Some(lowered.captions),
                    function_values: None,
                    reads: Default::default(),
                },
                Some(Err(diagnostic)) => Response::DialectRefused(refusal(diagnostic)),
            }
        }
        Request::Print {
            format,
            document,
            captions,
            options,
        } => {
            let dialect = format.dialect();
            let document = decoded_document(document)?;
            match embedding
                .dialects
                .get(dialect)
                .and_then(|package| package.printer.as_ref())
            {
                None => Response::UnknownDialect {
                    dialect: dialect.into(),
                },
                Some(printer) => match printer.print(&document, captions.as_ref(), options) {
                    Ok(printed) => Response::Printed(printed),
                    Err(error) => Response::DialectRefused(refusal(error.into())),
                },
            }
        }
        Request::Read {
            format,
            source,
            base,
            limits,
        } => match embedding.dialects.get(format.dialect()) {
            None => Response::UnknownDialect {
                dialect: format.dialect().into(),
            },
            Some(package) => match (package.read)(&source, &decoded_document(base)?, limits) {
                Ok((document, captions)) => Response::Read {
                    document: document.canonical_bytes(),
                    captions,
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
/// The installed dialect's name for source in `language`.
fn dialect_name(language: SourceLanguage) -> &'static str {
    match language {
        SourceLanguage::TypeScript => lash_vm_releases::TYPESCRIPT,
        SourceLanguage::Python => lash_vm_releases::PYTHON,
    }
}

/// Lowers a workflow module in `language` under `limits`, or the dialect's
/// standard policy; `None` when the worker does not install that dialect.
fn import_module(
    embedding: &Embedding,
    language: SourceLanguage,
    source: &str,
    environment: &lash_kernel_dialect::Environment<'_>,
    limits: Option<WorkflowSourceLimits>,
) -> Option<Result<lash_kernel_dialect::Lowered, Diagnostic>> {
    if !embedding.installs(dialect_name(language)) {
        return None;
    }
    match language {
        SourceLanguage::TypeScript => typescript_module(source, environment, limits),
        SourceLanguage::Python => python_module(source, environment, limits),
    }
}

#[cfg(feature = "dialect-typescript")]
fn typescript_module(
    source: &str,
    environment: &lash_kernel_dialect::Environment<'_>,
    limits: Option<WorkflowSourceLimits>,
) -> Option<Result<lash_kernel_dialect::Lowered, Diagnostic>> {
    let limits = match limits {
        None => Ok(lash_dialect_typescript::SourceLimits::standard()),
        Some(limits) => {
            lash_dialect_typescript::SourceLimits::new(limits.max_bytes, limits.max_nesting)
        }
    };
    Some(
        limits
            .and_then(|limits| {
                lash_dialect_typescript::lower_with_limits(
                    source,
                    environment,
                    Mode::Module,
                    limits,
                )
            })
            .map_err(Into::into),
    )
}

#[cfg(not(feature = "dialect-typescript"))]
fn typescript_module(
    _: &str,
    _: &lash_kernel_dialect::Environment<'_>,
    _: Option<WorkflowSourceLimits>,
) -> Option<Result<lash_kernel_dialect::Lowered, Diagnostic>> {
    None
}

#[cfg(feature = "dialect-python")]
fn python_module(
    source: &str,
    environment: &lash_kernel_dialect::Environment<'_>,
    limits: Option<WorkflowSourceLimits>,
) -> Option<Result<lash_kernel_dialect::Lowered, Diagnostic>> {
    let limits = match limits {
        None => Ok(lash_dialect_python::SourceLimits::standard()),
        Some(limits) => {
            lash_dialect_python::SourceLimits::new(limits.max_bytes, limits.max_nesting)
        }
    };
    Some(limits.and_then(|limits| {
        lash_dialect_python::lower_with_limits(source, environment, Mode::Module, limits)
    }))
}

#[cfg(not(feature = "dialect-python"))]
fn python_module(
    _: &str,
    _: &lash_kernel_dialect::Environment<'_>,
    _: Option<WorkflowSourceLimits>,
) -> Option<Result<lash_kernel_dialect::Lowered, Diagnostic>> {
    None
}

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

/// A document a request carries as its token body.
fn decoded_document(body: Vec<u8>) -> Result<Document, PoolError> {
    lash_kernel_doc::graph_tokens::decode_body(&body).map_err(|error| {
        PoolError::refused(RunRefusal::Undecodable {
            input: RunInput::Document,
            detail: Detail::new(error),
        })
    })
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

    /// FIG-6257: a document crosses the worker IPC as its canonical token
    /// body, never as JSON: `Lowered` answers the body its source map's
    /// identity is the digest of, and a saved function in `Lower` carries
    /// its document's body verbatim.
    #[test]
    fn documents_cross_the_worker_ipc_as_token_bodies() {
        let embedding =
            crate::embedding::tests::every_dialect(&Default::default()).expect("embedding");
        let lower = |functions| Request::Lower {
            dialect: "typescript".to_owned(),
            source: "function advance(value) { return value + 2; }".to_owned(),
            effects: Default::default(),
            tool_roots: Default::default(),
            controls: Default::default(),
            bindings: Default::default(),
            functions,
            helpers: lash_vm_library::HELPER_RELEASE,
            reads: Default::default(),
        };
        let encoded =
            EncodedPayload(rmp_serde::to_vec_named(&lower(Default::default())).expect("request"));
        let response = perform(&embedding, &encoded).expect("lowered");
        let Response::Lowered {
            document,
            source_map,
            ..
        } = rmp_serde::from_slice(&response.0).expect("response")
        else {
            panic!("the cell lowers");
        };
        assert!(
            serde_json::from_slice::<serde_json::Value>(&document).is_err(),
            "the document is not JSON"
        );
        let identified =
            lash_kernel_doc::IdentifiedDocument::decode_standard(&document).expect("a token body");
        assert_eq!(identified.body(), &document[..]);
        assert_eq!(identified.id(), identified.document().identity());
        assert_eq!(source_map.expect("a source map").document, identified.id());

        let name = lash_kernel_doc::Name::new("advance");
        let function = lash_vm_client::service::LowerFunction {
            name: name.clone(),
            document: identified.clone(),
            helper_owners: Default::default(),
            references: Default::default(),
            written: None,
            token: None,
        };
        let wire =
            rmp_serde::to_vec_named(&lower([(name.clone(), function)].into())).expect("request");
        assert!(
            wire.windows(document.len())
                .any(|bytes| bytes == &document[..]),
            "the saved function's document crosses as its token body"
        );
        let Request::Lower { functions, .. } = rmp_serde::from_slice(&wire).expect("request")
        else {
            panic!("a lowering request");
        };
        assert_eq!(functions[&name].document.id(), identified.id());
    }

    std::thread_local! {
        static READER_CALLS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    }

    fn counted_reader(
        _: &str,
        _: &Document,
        _: GeneratedSourceLimits,
    ) -> Result<(Document, lash_kernel_doc::Captions), Diagnostic> {
        READER_CALLS.set(READER_CALLS.get() + 1);
        let document = Document::new(
            lash_kernel_doc::NumberPolicy::Float,
            vec![lash_kernel_doc::Stmt::Finish {
                value: lash_kernel_doc::Expr::Literal(lash_kernel_doc::Literal::Null),
            }],
        );
        let captions = lash_kernel_doc::Captions::empty(document.identity());
        Ok((document, captions))
    }

    /// Printing invokes only the installed printer. Even mapped printing
    /// makes zero reader calls; reading is separately callable.
    #[test]
    fn printing_never_invokes_the_reader() {
        let mut embedding =
            crate::embedding::tests::every_dialect(&Default::default()).expect("embedding");
        for package in embedding.dialects.values_mut() {
            package.read = counted_reader;
        }
        READER_CALLS.set(0);
        let document = lash_kernel_doc::parse_document(
            "kernel 1\nnumbers float\nmain { let greeting = \"λ\" finish greeting }",
        )
        .expect("test document");
        for format in [SourceFormat::TypeScriptV1, SourceFormat::PythonV1] {
            for source_map in [false, true] {
                let request = Request::Print {
                    format,
                    document: document.canonical_bytes(),
                    captions: None,
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
                assert_eq!(READER_CALLS.get(), 0, "printing must not read");
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
                    SourceFormat::TypeScriptV1 => lash_dialect_typescript::read(
                        &printed.source,
                        &document,
                        Default::default(),
                    )
                    .expect("TS read"),
                    SourceFormat::PythonV1 => {
                        lash_dialect_python::read(&printed.source, &document, Default::default())
                            .expect("Python read")
                    }
                };
                assert_eq!(document.canonical_bytes(), decoded.0.canonical_bytes());
                assert_eq!(READER_CALLS.get(), 0);
            }
            let encoded = EncodedPayload(
                rmp_serde::to_vec_named(&Request::Read {
                    format,
                    source: String::new(),
                    base: document.canonical_bytes(),
                    limits: Default::default(),
                })
                .expect("request"),
            );
            let response = perform(&embedding, &encoded).expect("read response");
            assert!(matches!(
                rmp_serde::from_slice::<Response>(&response.0).expect("response"),
                Response::Read { .. }
            ));
            assert_eq!(READER_CALLS.replace(0), 1);
        }
    }
}
