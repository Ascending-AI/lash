//! The pure work a parent asks of a worker: lowering source to a kernel
//! document and printing a document as source. Both read guest-controlled
//! input, so both run here, in the worker's crash domain.
use lash_kernel_dialect::workflow::WorkflowSourceLimits;
use lash_kernel_dialect::{Diagnostic, DiagnosticKind, Mode};
use lash_kernel_doc::{Document, SourceLanguage};

use lash_protocol_types::engine_fault::{Detail, PayloadKind, RunInput, RunRefusal};
use lash_vm_client::PoolError;
use lash_vm_client::service::{DialectRefusal, Request, Response};
use lash_vm_protocol::EncodedPayload;

use crate::embedding::Embedding;

pub(crate) fn perform(
    embedding: &Embedding,
    request: &EncodedPayload,
) -> Result<EncodedPayload, PoolError> {
    let request: Request = rmp_serde::from_slice(&request.0)
        .map_err(|error| PoolError::payload(PayloadKind::ServiceRequest, error))?;
    let response = match request {
        Request::PrintCallable {
            base,
            format,
            options,
        } => {
            match embedding
                .dialects
                .get(format.dialect())
                .and_then(|package| package.callable_source.as_ref())
            {
                None => Response::UnknownDialect {
                    dialect: format.dialect().into(),
                },
                Some(codec) => match codec.print(&base, embedding.library(), options) {
                    Ok(printed) => Response::Printed(printed),
                    Err(lash_kernel_dialect::PrintError::Unprintable { site }) => {
                        Response::Unprintable { site }
                    }
                    Err(error) => Response::DialectRefused(refusal(error.into())),
                },
            }
        }
        Request::ReadCallable {
            source,
            base,
            format,
            limits,
            policy,
        } => {
            match embedding
                .dialects
                .get(format.dialect())
                .and_then(|package| package.callable_source.as_ref())
            {
                None => Response::UnknownDialect {
                    dialect: format.dialect().into(),
                },
                Some(codec) => {
                    match codec.read(&source, &base, embedding.library(), limits, policy) {
                        Ok(read) => Response::CallableRead(Box::new(read)),
                        Err(lash_kernel_dialect::CallableReadError::Source(error)) => {
                            Response::DialectRefused(refusal(error))
                        }
                        Err(lash_kernel_dialect::CallableReadError::Edit(error)) => {
                            Response::CallableEditRefused(error)
                        }
                    }
                }
            }
        }

        Request::ToolCalls {
            dialect,
            source,
            bindings,
            functions,
            namespaces,
        } => match embedding.dialects.get(&dialect) {
            None => Response::UnknownDialect { dialect },
            Some(package) => match package.front_end.tool_calls(
                &source,
                &lash_kernel_dialect::Environment::new(embedding.library(), &namespaces),
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
        },
        Request::Lower {
            dialect,
            source,
            namespaces,
            bindings,
            functions,
        } => {
            match embedding.lower(
                &dialect,
                &source,
                &lash_kernel_dialect::Environment::new(embedding.library(), &namespaces),
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
            }
        }
        Request::ImportWorkflow {
            language,
            source,
            namespaces,
            limits,
        } => {
            let environment =
                lash_kernel_dialect::Environment::new(embedding.library(), &namespaces);
            match import_module(embedding, language, &source, &environment, limits) {
                None => Response::UnknownDialect {
                    dialect: dialect_name(language).into(),
                },
                Some(Err(diagnostic)) => Response::DialectRefused(refusal(diagnostic)),
                Some(Ok(lowered)) => Response::Lowered {
                    document: lowered.document.body().to_vec(),
                    source_map: lowered.source_map,
                    written: lowered.written,
                    captions: Some(lowered.captions),
                    function_values: None,
                },
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
                Some(printer) => {
                    match printer.print(&document, captions.as_ref(), embedding.library(), options)
                    {
                        Ok(printed) => Response::Printed(printed),
                        Err(lash_kernel_dialect::PrintError::Unprintable { site }) => {
                            Response::Unprintable { site }
                        }
                        Err(error) => Response::DialectRefused(refusal(error.into())),
                    }
                }
            }
        }
        Request::Read {
            format,
            source,
            base,
            limits,
            source_limits,
        } => match embedding.dialects.get(format.dialect()) {
            None => Response::UnknownDialect {
                dialect: format.dialect().into(),
            },
            Some(package) => match (package.read)(
                &source,
                &decoded_document(base)?,
                embedding.library(),
                limits,
                source_limits,
            ) {
                Ok((document, captions)) => Response::Read {
                    document: document.canonical_bytes(),
                    captions,
                },
                Err(error) => Response::DialectRefused(refusal(error)),
            },
        },
    };
    respond(&response)
}

fn respond(response: &Response) -> Result<EncodedPayload, PoolError> {
    rmp_serde::to_vec_named(response)
        .map(EncodedPayload)
        .map_err(|error| PoolError::payload(PayloadKind::ServiceResponse, error))
}

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
                .map(|source| source.with_implicit_text_conversion(limits.implicit_text_conversion))
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

fn decoded_document(body: Vec<u8>) -> Result<Document, PoolError> {
    lash_vocabulary::tokens::decode_body(&body).map_err(|error| {
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
        operand_type: diagnostic.operand_type,

        fact: diagnostic.fact,
    }
}

#[cfg(all(test, feature = "dialect-typescript", feature = "dialect-python"))]
mod tests {
    use super::*;
    use lash_kernel_dialect::{GeneratedSourceLimits, PrintOptions, SourceFormat};

    /// FIG-7142: discovery reports namespace collisions as facts, not hints.
    #[test]
    fn discovery_reports_a_namespace_shadowed_by_a_cell_local() {
        use lash_vocabulary::{Name, PackagePath};
        use std::collections::{BTreeMap, BTreeSet};
        let embedding = crate::embedding::tests::every_dialect(&Default::default()).unwrap();
        for dialect in ["typescript", "python"] {
            let namespaces = lash_kernel_dialect::Namespaces::new(
                BTreeMap::new(),
                BTreeSet::from([PackagePath::parse("jev").unwrap()]),
                &|_| false,
            )
            .unwrap();
            let bindings = BTreeSet::from([Name::new("jev")]);
            let send = |request| {
                let response = perform(
                    &embedding,
                    &EncodedPayload(rmp_serde::to_vec_named(&request).unwrap()),
                )
                .unwrap();
                rmp_serde::from_slice::<Response>(&response.0).unwrap()
            };
            for bound in [false, true] {
                let response = send(Request::ToolCalls {
                    dialect: dialect.into(),
                    source: "".into(),
                    bindings: if bound {
                        bindings.clone()
                    } else {
                        BTreeSet::new()
                    },
                    functions: BTreeMap::new(),
                    namespaces: namespaces.clone(),
                });
                let json = serde_json::to_value(response).unwrap();
                assert_eq!(
                    json["tool_calls"]["namespace_collisions"],
                    if bound {
                        serde_json::json!([
                            {"namespace": "jev", "binding_name": "jev"}
                        ])
                    } else {
                        serde_json::json!([])
                    }
                );
            }
            let response = send(Request::Lower {
                dialect: dialect.into(),
                source: if dialect == "typescript" {
                    "await jev.decide({});"
                } else {
                    "await jev.decide({})\n"
                }
                .into(),
                bindings,
                functions: BTreeMap::new(),
                namespaces,
            });
            let Response::DialectRefused(refusal) = response else {
                panic!("compile-time refusal");
            };
            assert_eq!(refusal.code, "NAMESPACE_SHADOWED_BY_LOCAL");
            assert_eq!(refusal.span.unwrap().0, 6);
            assert!(!refusal.unsupported);
            assert!(refusal.repairs.is_empty());
            let Some(lash_kernel_dialect::DiagnosticFact::NamespaceShadowing(facts)) =
                refusal.fact.as_deref()
            else {
                panic!("typed namespace facts");
            };
            assert_eq!(facts.binding_name, "jev");
            assert_eq!(facts.binding_origin, None);
            assert_eq!(facts.namespace, "jev");
            assert_eq!(facts.call_path, "jev.decide");
        }
    }

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
            namespaces: Default::default(),
            bindings: Default::default(),
            functions,
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

        let name = lash_vocabulary::names::Name::new("advance");
        let function = lash_vm_client::service::LowerFunction {
            name: name.clone(),
            document: identified.clone(),
            link: lash_vocabulary::LinkId::from_bytes([1; 32]),
            callable: lash_vocabulary::CallableId {
                link: lash_vocabulary::LinkId::from_bytes([1; 32]),
                function: *identified
                    .functions
                    .keys()
                    .next()
                    .expect("authored function"),
            },
            environment: lash_kernel_doc::heap::EntityId {
                family: lash_kernel_doc::heap::FamilyId(0),
                ordinal: 0,
            },
            references: Default::default(),
            written: None,
            token: None,
            held: None,
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

    /// FIG-7109: the worker preserves an unprintable callable as a typed site.
    #[test]
    fn unprintable_callable_sites_cross_the_worker_wire() {
        use lash_vocabulary::{
            Name,
            sites::{Site, Unit},
        };
        let embedding =
            crate::embedding::tests::every_dialect(&Default::default()).expect("embedding");
        let namespaces = Default::default();
        let environment = lash_kernel_dialect::Environment::new(embedding.library(), &namespaces);
        let lowered = lash_dialect_typescript::lower(
            "export function hidden(): number { return 1; }",
            &environment,
            Mode::Module,
        )
        .expect("authored module");
        let mut document = lowered.document.document().clone();
        let lash_kernel_doc::Export::Function(export) = document
            .exports
            .get_mut(&Name::new("hidden"))
            .expect("export")
        else {
            panic!("function export");
        };
        export.convention = lash_kernel_doc::StartConvention::Direct;
        let expected = Site::new(Unit::Library(export.function), Vec::new());
        let request = Request::Print {
            format: SourceFormat::TypeScriptV1,
            document: document.canonical_bytes(),
            captions: None,
            options: Default::default(),
        };
        let encoded = EncodedPayload(rmp_serde::to_vec_named(&request).expect("request"));
        let response = perform(&embedding, &encoded).expect("print response");
        let response: Response = rmp_serde::from_slice(&response.0).expect("response");
        assert!(
            matches!(response, Response::Unprintable { site } if site == expected),
            "the callable site must survive IPC without parsing diagnostic prose"
        );
    }

    /// FIG-7145: typed entry annotation refusals survive the worker boundary,
    /// with source coordinates intact and no prose needed to recover the fact.
    #[test]
    fn signature_required_facts_cross_the_worker_wire() {
        let required = lash_kernel_doc::SignatureRequired {
            function: lash_kernel_doc::FunctionLocation {
                name: Some("handle".into()),
                site: None,
            },
            position: lash_kernel_doc::SignaturePosition::Parameter {
                index: 0,
                name: lash_vocabulary::Name::new("event"),
            },
            span: Some(lash_kernel_doc::Span { start: 17, end: 22 }),
        };
        let mut unknown = serde_json::to_value(&required).expect("fact JSON");
        unknown["unexpected"] = true.into();
        assert!(serde_json::from_value::<lash_kernel_doc::SignatureRequired>(unknown).is_err());
        let mut wrong_code = serde_json::to_value(&required).expect("fact JSON");
        wrong_code["code"] = "OTHER".into();
        assert!(serde_json::from_value::<lash_kernel_doc::SignatureRequired>(wrong_code).is_err());
        let response = Response::DialectRefused(refusal(required.clone().into()));
        let wire = rmp_serde::to_vec_named(&response).expect("refusal wire");
        let Response::DialectRefused(decoded) =
            rmp_serde::from_slice(&wire).expect("typed refusal")
        else {
            panic!("a dialect refusal");
        };
        assert_eq!(decoded.code, "SIGNATURE_REQUIRED");
        assert_eq!(decoded.span, Some((17, 22)));
        assert_eq!(
            decoded.fact,
            Some(Box::new(
                lash_kernel_dialect::DiagnosticFact::SignatureRequired(required)
            ))
        );
        assert!(decoded.message.is_empty());
        assert!(decoded.repairs.is_empty());
    }

    std::thread_local! {
        static READER_CALLS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    }

    fn counted_reader(
        _: &str,
        _: &Document,
        _: &dyn lash_kernel_dialect::Library,
        _: GeneratedSourceLimits,
        _: Option<WorkflowSourceLimits>,
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
        let namespaces = Default::default();
        let environment = lash_kernel_dialect::Environment::new(embedding.library(), &namespaces);
        for (format, source) in [
            (SourceFormat::TypeScriptV1, "const greeting = \"λ\";"),
            (SourceFormat::PythonV1, "greeting = \"λ\""),
        ] {
            // Readable printing accepts documents the authored dialect can
            // reconstruct exactly; bare kernel `finish` has no such TS form.
            let lowered = embedding
                .lower(
                    format.dialect(),
                    source,
                    &environment,
                    Mode::Cell(lash_kernel_dialect::CellEnvironment::first()),
                )
                .expect("installed dialect")
                .expect("authored fixture");
            let document = lowered.document.document();
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
                        document,
                        embedding.library(),
                        Default::default(),
                        None,
                    )
                    .expect("TS read"),
                    SourceFormat::PythonV1 => lash_dialect_python::read(
                        &printed.source,
                        document,
                        embedding.library(),
                        Default::default(),
                        None,
                    )
                    .expect("Python read"),
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
                    source_limits: None,
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
