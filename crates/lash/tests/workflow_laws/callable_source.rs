//! K-CALLABLE source authoring laws over retained SQLite definitions.
use super::*;
use lash::dialect::SourceFormat;
use lash::workflow::WorkflowInspection;
use lash::workflow::document::{CaptureValue, Datum, FunctionValue, Node, WrittenSignature};

const TS: &str = "const offset = 7; const f = (x: number): number => x + offset; const first = f; const second = f; const answer = async (x: number): Promise<number> => await tools.echo({value: first(x) + second(x)});";
const PY: &str = "offset = 7\ndef f(x: int) -> int:\n    return x + offset\nfirst = f\nsecond = f\nasync def answer(x: int) -> int:\n    return await tools.echo({\"value\": first(x) + second(x)})\n";

fn token_datum(expression: &Expr, callable: &Name) -> Datum {
    match expression {
        Expr::Literal(Literal::Null) => Datum::Null,
        Expr::Literal(Literal::Bool(value)) => Datum::Bool(*value),
        Expr::Literal(Literal::Int(value)) => Datum::Int(value.clone()),
        Expr::Literal(Literal::Text(value)) => Datum::Text(value.clone()),
        Expr::Tuple(parts) => Datum::Tuple(
            parts
                .iter()
                .map(|part| token_datum(part, callable))
                .collect(),
        ),
        Expr::Variable(name) if name == callable => Datum::Null,
        _ => panic!("literal callable token data: {expression:?}"),
    }
}

async fn saved(
    core: &lash::LashCore,
    pin: &lash::process::HostArtifactPin,
    format: SourceFormat,
) -> (
    lash::process::ProcessDefinition,
    lash::vm::WorkflowEnvironmentRequest,
    FunctionValue,
) {
    saved_source(core, pin, format, TS).await
}

async fn saved_source(
    core: &lash::LashCore,
    pin: &lash::process::HostArtifactPin,
    format: SourceFormat,
    typescript: &str,
) -> (
    lash::process::ProcessDefinition,
    lash::vm::WorkflowEnvironmentRequest,
    FunctionValue,
) {
    let (_, environment) = namespace(core, pin, &source_environment(false)).await;
    let release = lash_kernel_releases::helper_release().unwrap();
    let roots = release.resolver_roots();
    let library =
        lash::dialect::NamedLibrary::resolving(&release.functions, |id| roots.contains(id))
            .unwrap();
    let checked = core
        .host_artifacts()
        .workflow_environment(&environment)
        .await
        .unwrap()
        .unwrap();
    let bound = lash::dialect::Environment::new(&library, checked.namespaces());
    let lowered = match format {
        SourceFormat::TypeScriptV1 => lash::typescript::lower(
            typescript,
            &bound,
            lash::dialect::Mode::Cell(lash::dialect::CellEnvironment::first()),
        )
        .unwrap(),
        SourceFormat::PythonV1 => lash::python::lower(
            PY,
            &bound,
            lash::dialect::Mode::Cell(lash::dialect::CellEnvironment::first()),
        )
        .unwrap(),
    };
    let start = core
        .host_artifacts()
        .publish_workflow(
            pin,
            &lowered.document.clone().into(),
            &lash::package::StartSelector::Main,
            &environment,
            None,
            None,
        )
        .await
        .unwrap();
    let make = |name: &str| {
        let (site, written) = lowered
            .written
            .iter()
            .find(|(_, written)| {
                written
                    .signature_origin
                    .as_ref()
                    .is_some_and(|origin| origin.name.as_deref() == Some(name))
            })
            .unwrap();
        let Node::Expr(Expr::Closure(closure)) = lowered.document.node(site).unwrap() else {
            panic!("written closure");
        };
        let presentation = closure.presentation.as_ref().unwrap();
        FunctionValue {
            written_signature: Some(WrittenSignature {
                language: written.language,
                signature: written.signature.clone(),
            }),
            signature_origin: written.signature_origin.clone(),
            implicit_text_conversions: written.implicit_text_conversions.clone(),
            callable: lash::package::CallableId {
                link: start.link,
                function: closure.function,
            },
            captures: Default::default(),
            signature: presentation.signature.clone(),
            convention: presentation.convention.clone(),
            captions: lowered.captions.clone(),
            representation: presentation.representation.clone(),
            creation: Some(site.clone()),
            token: match format {
                SourceFormat::TypeScriptV1 => Some(vec![
                    Datum::Text("ts.function".into()),
                    Datum::Text(String::new()),
                    Datum::Text(name.into()),
                    Datum::Float(lash_kernel_doc::Float::new(1.0)),
                    Datum::Null,
                    Datum::Record(Vec::new()),
                ]),
                SourceFormat::PythonV1 => {
                    // Retain the lowered metadata, replacing only the bound callable.
                    let mut token_site = site.clone();
                    token_site.path.pop().unwrap();
                    let Node::Expr(Expr::Let { name, body, .. }) =
                        lowered.document.node(&token_site).unwrap()
                    else {
                        panic!("lowered callable binding");
                    };
                    let Datum::Tuple(token) = token_datum(body, name) else {
                        panic!("lowered callable token");
                    };
                    Some(token)
                }
            },
        }
    };
    let data = |n: i64| {
        CaptureValue::Data(match format {
            SourceFormat::TypeScriptV1 => Datum::Float(lash_kernel_doc::Float::new(n as f64)),
            SourceFormat::PythonV1 => Datum::Int(n.into()),
        })
    };
    let mut first = make("f");
    first.captures.insert(Name::new("offset"), data(7_i64));
    let mut second = make("f");
    second.captures.insert(Name::new("offset"), data(9_i64));
    let mut function = make("answer");
    function.captures = [
        (Name::new("first"), CaptureValue::Function(Box::new(first))),
        (
            Name::new("second"),
            CaptureValue::Function(Box::new(second)),
        ),
        (Name::new("recursive_surplus"), CaptureValue::Reference(0)),
    ]
    .into();
    // PRINT-STANDALONE retains exact non-JSON kinds in surplus captures too.
    function.captures.insert(
        Name::new("typed_surplus"),
        CaptureValue::Data(Datum::Tuple(vec![
            Datum::Error(Box::new(lash_kernel_doc::ErrorDatum {
                kind: "saved".into(),
                message: "typed value".into(),
                data: Datum::Absent,
            })),
            Datum::Timestamp(lash_kernel_doc::Timestamp {
                nanoseconds: lash_kernel_doc::Integer::parse("123456789012345678901234567890")
                    .unwrap(),
            }),
            Datum::Bytes(lash_kernel_doc::Bytes::parse_hex("00ff").unwrap()),
            Datum::Map(vec![(
                Datum::Int(7.into()),
                Datum::Set(vec![Datum::Int(2.into()), Datum::Int(1.into())]),
            )]),
            Datum::Record(vec![("absent".into(), Datum::Absent)]),
        ])),
    );
    let origin = lash::package::CallableOrigin {
        authored: [start.link].into(),
        publications: environment
            .snapshot
            .advertisements
            .iter()
            .map(|ad| ad.publication.key())
            .collect(),
    };
    let definition = core
        .host_artifacts()
        .publish_callable(pin, &function, &environment, &origin, None)
        .await
        .unwrap();
    (definition, environment, function)
}
async fn execution_environment(
    core: &lash::LashCore,
    pin: &lash::process::HostArtifactPin,
    function: &FunctionValue,
) -> lash::process::ProcessEnvRef {
    let base = source_environment(false);
    let (packages, _) = namespace(core, pin, &base).await;
    let environment = core
        .host_artifacts()
        .link_process_env(
            &base,
            &lash::process::LinkTarget::Linked(lash::package::LinkedStart {
                link: function.callable.link,
                selector: lash::package::StartSelector::Main,
            }),
            &packages,
            &Default::default(),
        )
        .await
        .unwrap();
    core.host_artifacts()
        .publish_process_env(pin, &environment)
        .await
        .unwrap()
}

async fn inspect(
    core: &lash::LashCore,
    definition: &lash::process::ProcessDefinition,
) -> WorkflowInspection {
    let WorkflowRead::Inspected(base) = core
        .host_artifacts()
        .definition_graph(&definition.id)
        .await
        .unwrap()
    else {
        panic!("saved workflow document");
    };
    *base
}

async fn linked_draft(
    core: &lash::LashCore,
    environment: &lash::vm::WorkflowEnvironmentRequest,
    format: SourceFormat,
) -> lash::workflow::document::DocumentBytes {
    use lash::workflow::document::SourceLanguage;
    let (language, source) = match format {
        SourceFormat::TypeScriptV1 => (
            SourceLanguage::TypeScript,
            "export function answer(x: number): number { return x + 1; }",
        ),
        SourceFormat::PythonV1 => (
            SourceLanguage::Python,
            "def answer(x: int) -> int:\n    return x + 1\n",
        ),
    };
    let imported = core
        .host_artifacts()
        .import_workflow(language, source, environment, None)
        .await
        .unwrap();
    let lash::workflow::WorkflowImport::Imported { document, .. } = imported else {
        panic!("authored linked workflow: {imported:?}");
    };
    document
}

fn host_captions(base: &WorkflowInspection, title: &str) -> lash::workflow::document::Captions {
    use lash::workflow::document::{Caption, CaptionEntry, Captions, Site};
    let document = base.document.document();
    let unit = base.document.callable().map_or_else(
        || function_unit(document, "answer"),
        |function| Unit::Library(function.callable.function),
    );
    let mut entries = vec![CaptionEntry::site(
        Site::new(unit.clone(), vec![]),
        Caption::new(title, None).unwrap(),
    )];
    // The saved callable is an expression-bodied arrow; its body has no
    // source statement to caption. The linked entry has a return statement.
    if base.document.callable().is_none() {
        entries.push(CaptionEntry::site(
            Site::new(unit, vec![0]),
            Caption::new("Host step", Some("Step details".into())).unwrap(),
        ));
    }
    if base.document.callable().is_some() {
        entries.push(CaptionEntry::site(
            Site::new(Unit::Main, [0]),
            Caption::new("Original initializer", None).unwrap(),
        ));
    }
    Captions::new(document, entries).unwrap()
}

async fn caption_base(
    core: &lash::LashCore,
    pin: &lash::process::HostArtifactPin,
    format: SourceFormat,
    callable: bool,
) -> (WorkflowInspection, lash::vm::WorkflowEnvironmentRequest) {
    if callable {
        let (definition, environment, _) = saved(core, pin, format).await;
        (inspect(core, &definition).await, environment)
    } else {
        let (_, environment) = namespace(core, pin, &source_environment(false)).await;
        let document = linked_draft(core, &environment, format).await;
        let published = publish(
            core,
            pin,
            &document,
            &Name::new("answer"),
            &source_environment(false),
            None,
        )
        .await
        .unwrap();
        (inspect(core, &published.definition).await, environment)
    }
}

/// FIG-7189: callable overrides retain check_captions' typed identity and
/// byte-budget refusals before the frozen value is sent to a worker.
#[tokio::test]
async fn callable_caption_overrides_validate_identity_and_limits() {
    let core = core().await;
    let pin = lash::process::HostArtifactPin::mint();
    let format = SourceFormat::TypeScriptV1;
    let (base, _) = caption_base(&core, &pin, format, true).await;
    let wrong = lash::workflow::document::Captions::empty(
        Document::new(lash::workflow::document::NumberPolicy::Float, Vec::new()).identity(),
    );
    let captions = host_captions(&base, "Host title");
    for (captions, options) in [
        (&wrong, lash::dialect::PrintOptions::default()),
        (
            &captions,
            lash::dialect::PrintOptions {
                limits: lash::dialect::GeneratedSourceLimits {
                    max_bytes: 1,
                    ..Default::default()
                },
                ..Default::default()
            },
        ),
    ] {
        let expected =
            lash::dialect::check_captions(base.document.document(), captions, options.limits)
                .unwrap_err();
        let error = core
            .host_artifacts()
            .print_workflow(&base.definition.id, Some(captions), format, options)
            .await
            .unwrap_err();
        let lash::EmbedError::WorkflowSource(lash::vm::SourceServiceError::Print(actual)) = error
        else {
            panic!("caption refusal: {error:?}");
        };
        assert_eq!(actual, expected);
    }
    core.shutdown().await.unwrap();
}

/// FIG-7475: a saved arrow's authored tool import stays separate from the
/// clock helper's operation declaration. Publication, materialization and source republication preserve
/// both operations and the returned value.
#[tokio::test]
async fn saved_arrow_with_clock_and_tool_materializes_and_prints() {
    let core = core().await;
    let pin = lash::process::HostArtifactPin::mint();
    let source = "const offset = 7; const f = (x: number): number => x + offset; const first = f; const second = f; const answer = async (x: number): Promise<number> => { const now = Date.now(); return await tools.echo({value: first(x) + second(x)}); };";
    let format = SourceFormat::TypeScriptV1;
    let (definition, environment, function) = saved_source(&core, &pin, format, source).await;
    let opened = core
        .host_artifacts()
        .open_workflow(&definition.id, None)
        .await
        .unwrap();
    let document = opened.draft.document();
    let release = lash::vm::helper_release().unwrap();
    let helper_imports =
        lash_kernel_doc::helper_imports(document, release.helpers.as_ref()).unwrap();
    assert!(
        helper_imports
            .values()
            .any(|import| import.name.to_string() == "typescript.clock")
    );
    assert!(
        document
            .imports
            .values()
            .any(|import| import.name.to_string() == "tools.echo")
    );
    let printed = core
        .host_artifacts()
        .print_workflow(&definition.id, None, format, Default::default())
        .await
        .unwrap();
    let roundtrip = publish_source(&core, &pin, &printed.source, format, &environment, "run").await;
    let original_env = execution_environment(&core, &pin, &function).await;
    let roundtrip_env =
        definition_environment(&core, &pin, &roundtrip, &source_environment(false)).await;
    for (definition, environment, key) in [
        (&definition, &original_env, "clock-tool-original"),
        (&roundtrip, &roundtrip_env, "clock-tool-roundtrip"),
    ] {
        let run = start_with(
            &core,
            environment,
            definition,
            key,
            [("x".into(), serde_json::json!(3))].into_iter().collect(),
        )
        .await;
        assert_eq!(finished(&core, &run).await, serde_json::json!(22));
    }
    core.shutdown().await.unwrap();
}

/// MATERIALIZE-GRAPH/DRAFT-INDEPENDENT: both private dialect ABIs survive owned-document publication and cold reopen.
#[tokio::test]
async fn materialized_callable_adapters_and_environments_survive_sqlite_reopen() {
    for format in [SourceFormat::TypeScriptV1, SourceFormat::PythonV1] {
        let files = Files::new();
        let core = core_over(files.open().await, "materialize-save").await;
        let pin = lash::process::HostArtifactPin::mint();
        let (a, _, function) = saved(&core, &pin, format).await;
        let opened = core
            .host_artifacts()
            .open_workflow(&a.id, None)
            .await
            .unwrap();
        let b = publish(
            &core,
            &pin,
            &opened.draft.into_document().into(),
            &Name::new("run"),
            &source_environment(false),
            None,
        )
        .await
        .unwrap();
        let env_a = execution_environment(&core, &pin, &function).await;
        core.shutdown().await.unwrap();
        drop(core);
        let core = core_over(files.open().await, "materialize-reopen").await;
        let reopened = core
            .host_artifacts()
            .open_workflow(&b.definition.id, None)
            .await
            .unwrap();
        assert_eq!(
            reopened.selector,
            lash::package::StartSelector::Export(Name::new("run"))
        );
        assert_eq!(reopened.draft.identity(), b.document);
        for (definition, env, key) in [
            (&a, &env_a, "materialize-a"),
            (&b.definition, &b.env_ref, "materialize-b"),
        ] {
            let run = start_with(
                &core,
                env,
                definition,
                key,
                [("x".into(), serde_json::json!(3))].into_iter().collect(),
            )
            .await;
            assert_eq!(
                finished(&core, &run).await,
                serde_json::json!(22),
                "{format:?}"
            );
        }
        core.shutdown().await.unwrap();
    }
}

/// K-CALLABLE-ROUNDTRIP: reopen, duplicate code identities with distinct
/// environments, and Reference(0) survive unchanged source publication.
#[tokio::test]
async fn print_standalone_reopens_both_dialects_without_a_source_definition() {
    for format in [SourceFormat::TypeScriptV1, SourceFormat::PythonV1] {
        let files = Files::new();
        let core = core_over(files.open().await, "callable-save").await;
        let pin = lash::process::HostArtifactPin::mint();
        let (definition, _, function) = saved(&core, &pin, format).await;
        core.shutdown().await.unwrap();
        drop(core);
        let core = core_over(files.open().await, "callable-open").await;
        let printed = core
            .host_artifacts()
            .print_workflow(
                &definition.id,
                None,
                format,
                lash::dialect::PrintOptions {
                    source_map: true,
                    ..Default::default()
                },
            )
            .await
            .unwrap_or_else(|error| panic!("{format:?}: {error:?}"));
        assert!(
            printed.source.contains("= 7"),
            "PRINT-STANDALONE needs ordinary captured declarations: {}",
            printed.source
        );
        let fresh = super::core().await;
        let pin_b = lash::process::HostArtifactPin::mint();
        let (_, environment_b) = namespace(&fresh, &pin_b, &source_environment(false)).await;
        assert!(matches!(
            fresh
                .host_artifacts()
                .definition_graph(&definition.id)
                .await
                .unwrap(),
            WorkflowRead::Unavailable(_)
        ));
        let published = publish_source(
            &fresh,
            &pin_b,
            &printed.source,
            format,
            &environment_b,
            "run",
        )
        .await;
        assert_eq!(published.signature, definition.signature);
        let after = inspect(&fresh, &published).await;
        assert!(after.document.callable().is_none());
        let env_a = execution_environment(&core, &pin, &function).await;
        let env_b =
            definition_environment(&fresh, &pin_b, &published, &source_environment(false)).await;
        for (host, env, workflow, key) in [
            (&core, &env_a, &definition, "standalone-a"),
            (&fresh, &env_b, &published, "standalone-b"),
        ] {
            let run = start_with(
                host,
                env,
                workflow,
                key,
                [("x".into(), serde_json::json!(3))].into_iter().collect(),
            )
            .await;
            assert_eq!(finished(host, &run).await, serde_json::json!(22));
        }
        fresh.shutdown().await.unwrap();
        core.shutdown().await.unwrap();
    }
}

async fn publish_source(
    core: &lash::LashCore,
    pin: &lash::process::HostArtifactPin,
    source: &str,
    format: SourceFormat,
    environment: &lash::vm::WorkflowEnvironmentRequest,
    entry: &str,
) -> lash::process::ProcessDefinition {
    let imported = core
        .host_artifacts()
        .import_workflow(format.language(), source, environment, None)
        .await
        .unwrap();
    let lash::workflow::WorkflowImport::Imported {
        document,
        source_map,
        ..
    } = imported
    else {
        panic!("ordinary import: {imported:?}\n{source}");
    };
    let start = core
        .host_artifacts()
        .publish_workflow(
            pin,
            &document,
            &lash::package::StartSelector::Export(Name::new(entry)),
            environment,
            Some((&source_map, source)),
            None,
        )
        .await
        .unwrap();
    let draft = lash::vm::definition_draft(&lash::process::KernelProcessDefinition {
        code: lash::vm::KernelInlineRequest::Linked { start },
    })
    .unwrap();
    core.host_artifacts()
        .publish_definition(pin, &draft)
        .await
        .unwrap()
}

/// CAPTION-STANDALONE: both dialects spell mapped host captions and import independently.
#[tokio::test]
async fn caption_standalone_preserves_supported_sidecars_and_reports_losses() {
    let core = core().await;
    let pin = lash::process::HostArtifactPin::mint();
    for format in [SourceFormat::TypeScriptV1, SourceFormat::PythonV1] {
        for callable in [false, true] {
            let (base, environment) = caption_base(&core, &pin, format, callable).await;
            let captions = host_captions(&base, "Host title");
            let printed = core
                .host_artifacts()
                .print_workflow(
                    &base.definition.id,
                    Some(&captions),
                    format,
                    Default::default(),
                )
                .await
                .unwrap();
            let imported = core
                .host_artifacts()
                .import_workflow(format.language(), &printed.source, &environment, None)
                .await
                .unwrap();
            let lash::workflow::WorkflowImport::Imported {
                captions: after, ..
            } = imported
            else {
                panic!("{imported:?}");
            };
            assert!(
                after
                    .entries()
                    .iter()
                    .any(|e| e.caption.title() == "Host title"),
                "{format:?} callable={callable}: {printed:?}"
            );
            if callable {
                assert!(
                    printed
                        .dropped
                        .contains(&lash::workflow::document::CaptionTarget::Site(
                            lash::workflow::document::Site::new(Unit::Main, [0]),
                        ))
                );
                assert!(
                    !after
                        .entries()
                        .iter()
                        .any(|entry| entry.caption.title() == "Original initializer")
                );
            } else {
                assert!(
                    after
                        .entries()
                        .iter()
                        .any(|entry| entry.caption.title() == "Host step")
                );
            }
            publish_source(
                &core,
                &pin,
                &printed.source,
                format,
                &environment,
                if callable { "run" } else { "answer" },
            )
            .await;
        }
    }
    core.shutdown().await.unwrap();
}

/// FIG-7191: every host publication reports an ended pin through the typed
/// artifact accessor, including document and package publication wrappers.
#[tokio::test]
async fn released_pin_publications_keep_the_typed_artifact_refusal() {
    let core = core().await;
    let artifacts = core.host_artifacts();
    let retained = lash::process::HostArtifactPin::mint();
    let (definition, environment, function) =
        saved(&core, &retained, SourceFormat::TypeScriptV1).await;
    let base = inspect(&core, &definition).await;
    let document = base.document.document().clone().into();
    let start = lash::package::LinkedStart {
        link: function.callable.link,
        selector: lash::package::StartSelector::Main,
    };
    let draft = lash::vm::definition_draft(&lash::process::KernelProcessDefinition {
        code: lash::vm::KernelInlineRequest::Linked { start },
    })
    .unwrap();
    let origin = lash::package::CallableOrigin {
        authored: [function.callable.link].into(),
        publications: environment
            .snapshot
            .advertisements
            .iter()
            .map(|accepted| accepted.publication.key())
            .collect(),
    };
    let (repeated, sql) = lash::tracing::sql::collect(
        "namespace-repeat",
        "sqlite",
        artifacts.process_namespace(
            &retained,
            &source_environment(false),
            environment.context.clone(),
        ),
    )
    .await;
    repeated.expect("an unchanged namespace remains admissible");
    assert_eq!(
        sql.rows_affected, 0,
        "FIG-7427: unchanged namespace changed package records: {sql:?}"
    );
    assert_eq!(
        sql.shapes
            .iter()
            .filter(|(shape, _)| matches!(
                shape.split_whitespace().next(),
                Some("insert" | "update" | "delete" | "replace")
            ))
            .map(|(_, work)| work.statements)
            .sum::<u64>(),
        0,
        "FIG-7427: unchanged namespace submitted package-store writes: {sql:?}"
    );
    let pin = lash::process::HostArtifactPin::mint();
    artifacts.release(pin.clone()).await.unwrap();
    let store = core.backend().package_store();
    let empty = store.prepare_publications(&[]).await.unwrap();
    let claim = lash::persistence::ReferrerClaim::unguarded(
        lash::persistence::ArtifactReferrer::HostPin(pin.clone()),
    )
    .unwrap();
    let error = store
        .acquire_prepared_publications(&claim, &empty)
        .await
        .expect_err("an empty prepared closure still checks the released owner");
    assert!(
        matches!(
            error,
            lash_durable::DurableError::Package(
                lash_durable::packages::PackageRefusal::ReferrerEnded(_)
            )
        ),
        "{error:?}"
    );
    let errors = [
        (
            "publish_workflow",
            artifacts
                .publish_workflow(
                    &pin,
                    &document,
                    &lash::package::StartSelector::Main,
                    &environment,
                    None,
                    None,
                )
                .await
                .unwrap_err(),
        ),
        (
            "publish_callable",
            artifacts
                .publish_callable(&pin, &function, &environment, &origin, None)
                .await
                .unwrap_err(),
        ),
        (
            "publish_definition",
            artifacts
                .publish_definition(&pin, &draft)
                .await
                .unwrap_err(),
        ),
        (
            "process_namespace",
            artifacts
                .process_namespace(
                    &pin,
                    &source_environment(false),
                    environment.context.clone(),
                )
                .await
                .unwrap_err(),
        ),
        (
            "publish_process_env",
            artifacts
                .publish_process_env(&pin, &source_environment(false))
                .await
                .unwrap_err(),
        ),
    ];
    for (entry, error) in errors {
        assert_eq!(
            error.artifact_refusal(),
            Some(lash::process::HostArtifactRefusal::PinReleased),
            "{entry}: {error:?}",
        );
    }
    core.shutdown().await.unwrap();
}

/// VALUE-PRINT-REFUSAL: unspellable bound tokens refuse, while inspection remains available.
#[tokio::test]
async fn value_print_refusal_retains_the_typed_token_site() {
    let core = core().await;
    let pin = lash::process::HostArtifactPin::mint();
    let (_, environment, original) = saved(&core, &pin, SourceFormat::TypeScriptV1).await;
    let mut unprintable = original.clone();
    unprintable.token.as_mut().unwrap()[1] = Datum::Text("bound-this".into());
    let origin = lash::package::CallableOrigin {
        authored: [original.callable.link].into(),
        publications: environment
            .snapshot
            .advertisements
            .iter()
            .map(|ad| ad.publication.key())
            .collect(),
    };
    let opaque = core
        .host_artifacts()
        .publish_callable(&pin, &unprintable, &environment, &origin, None)
        .await
        .unwrap();
    let graph = inspect(&core, &opaque).await;
    assert!(!lash_kernel_doc::print_document(graph.document.document()).is_empty());
    assert!(matches!(
        core.host_artifacts()
            .print_workflow(
                &graph.definition.id,
                None,
                SourceFormat::TypeScriptV1,
                Default::default()
            )
            .await
            .unwrap_err(),
        lash::EmbedError::WorkflowSource(lash_vm_client::service::SourceServiceError::Print(
            lash::dialect::PrintError::Unprintable { .. }
        ))
    ));
    core.shutdown().await.unwrap();
}

/// FIG-7189: unpublished drafts print and ordinarily import their host captions.
#[tokio::test]
async fn unpublished_linked_drafts_roundtrip_host_captions() {
    let core = core().await;
    let pin = lash::process::HostArtifactPin::mint();
    let (_, environment) = namespace(&core, &pin, &source_environment(false)).await;
    let service = lash::vm::WorkerService::default();
    for format in [SourceFormat::TypeScriptV1, SourceFormat::PythonV1] {
        let draft = linked_draft(&core, &environment, format).await;
        let document = draft.to_document().unwrap();
        let captions = lash::workflow::document::Captions::new(
            &document,
            vec![lash::workflow::document::CaptionEntry::site(
                lash::workflow::document::Site::new(function_unit(&document, "answer"), vec![]),
                lash::workflow::document::Caption::new("Draft title", None).unwrap(),
            )],
        )
        .unwrap();
        let printed = service
            .print(&document, Some(&captions), format, Default::default())
            .await
            .unwrap();
        let (read, _, read_captions) = service
            .import_workflow(
                format.language(),
                &printed.source,
                core.host_artifacts()
                    .workflow_environment(&environment)
                    .await
                    .unwrap()
                    .unwrap()
                    .namespaces(),
                None,
            )
            .await
            .unwrap();
        assert_eq!(read.canonical_bytes(), document.canonical_bytes());
        assert_eq!(read_captions, captions);
    }
    core.shutdown().await.unwrap();
}

async fn definition_environment(
    core: &lash::LashCore,
    pin: &lash::process::HostArtifactPin,
    definition: &lash::process::ProcessDefinition,
    base: &lash::process::ProcessEnv,
) -> lash::process::ProcessEnvRef {
    let (packages, _) = namespace(core, pin, base).await;
    let environment = core
        .host_artifacts()
        .link_process_env(
            base,
            &lash::process::LinkTarget::Definition(definition.id.clone()),
            &packages,
            &Default::default(),
        )
        .await
        .unwrap();
    core.host_artifacts()
        .publish_process_env(pin, &environment)
        .await
        .unwrap()
}

/// FRESH-SOURCE-POLICY: printing permissively admitted code cannot authorize strict publication.
#[tokio::test]
async fn fresh_source_policy_checks_printed_text_again_at_publication() {
    use lash::dialect::workflow::ImplicitTextConversionPolicy as Policy;
    let core = core().await;
    let pin = lash::process::HostArtifactPin::mint();
    let (_, environment) = namespace(&core, &pin, &source_environment(false)).await;
    let source =
        "export function run(event: { identity_id: string }): string { return 'answer ' + event; }";
    let imported = core
        .host_artifacts()
        .import_workflow(
            SourceFormat::TypeScriptV1.language(),
            source,
            &environment,
            Some(implicit_text_limits(Policy::Allow)),
        )
        .await
        .unwrap();
    let lash::workflow::WorkflowImport::Imported { document, .. } = imported else {
        panic!("{imported:?}");
    };
    let service = lash::vm::WorkerService::default();
    let printed = service
        .print(
            &document.to_document().unwrap(),
            None,
            SourceFormat::TypeScriptV1,
            Default::default(),
        )
        .await
        .unwrap();
    let checked = core
        .host_artifacts()
        .workflow_environment(&environment)
        .await
        .unwrap()
        .unwrap();
    let (document, map, _) = service
        .import_workflow(
            SourceFormat::TypeScriptV1.language(),
            &printed.source,
            checked.namespaces(),
            Some(implicit_text_limits(Policy::Allow)),
        )
        .await
        .unwrap();
    let error = core
        .host_artifacts()
        .publish_workflow(
            &pin,
            &document.into(),
            &lash::package::StartSelector::Export(Name::new("run")),
            &environment,
            Some((&map, &printed.source)),
            Some(implicit_text_limits(Policy::Strict)),
        )
        .await
        .unwrap_err();
    let diagnostic = implicit_text_diagnostic(error);
    assert_eq!(diagnostic.code(), "TS_IMPLICIT_TEXT_CONVERSION");
    core.shutdown().await.unwrap();
}

const FRESH_ECHO: lash::process::ProcessConfigKey<()> =
    lash::process::ProcessConfigKey::new("print-fresh-echo", lash::plugins::FormatVersion::ONE);
struct FreshEcho;
#[lash::async_trait]
impl lash::tools::StaticToolExecute for FreshEcho {
    async fn execute(&self, call: lash::tools::ToolCall<'_>) -> lash::tools::ToolAttemptOutcome {
        lash::tools::ToolOutcome::ok(serde_json::json!(
            call.args["value"].as_f64().unwrap() + 100.0
        ))
        .into()
    }
}

/// PRINT-ENVIRONMENT: source requirements are fulfilled by B's selected
/// host, with typed missing and conflicting requirement refusals.
#[tokio::test]
async fn print_environment_selects_fresh_dependencies_and_refuses_conflicts() {
    let stores = Arc::new(lash_sqlite_store::SqliteStoreSet::memory().await.unwrap());
    let provider = Arc::new(lash::tools::StaticToolProvider::new(
        echo_definitions(),
        FreshEcho,
    ));
    let factory = lash::plugins::StaticPluginFactory::new(
        lash::plugins::PluginDeclaration::initial(FRESH_ECHO.component()),
        lash::plugins::PluginSpec::new()
            .with_tool_provider(lash::tool_provider_key!("fresh"), provider.clone())
            .with_package_provider(
                lash::tool_provider_key!("fresh"),
                Arc::new(lash::package::ToolPackageProvider::new(
                    FRESH_ECHO.component(),
                    lash::tool_provider_key!("fresh"),
                    [lash::package::Segment::new("tools").unwrap()].into(),
                    provider,
                )),
            ),
    )
    .as_component(FRESH_ECHO.format())
    .unwrap();
    let core = builder_with_process_tools(stores, false, EchoComponent::Installed)
        .await
        .process_component(Arc::new(factory), FRESH_ECHO)
        .build(node_identity("print-environment"))
        .unwrap();
    let pin = lash::process::HostArtifactPin::mint();
    let (a, _, original) = saved(&core, &pin, SourceFormat::TypeScriptV1).await;
    let printed = core
        .host_artifacts()
        .print_workflow(&a.id, None, SourceFormat::TypeScriptV1, Default::default())
        .await
        .unwrap();
    let fresh = lash::process::ProcessEnv::builder(lash::MaxToolCalls::new(1024))
        .component(FRESH_ECHO, &())
        .component(lash::vm::KERNEL_SERVICES_CONFIG, &())
        .build()
        .unwrap();
    let (_, request) = namespace(&core, &pin, &fresh).await;
    let b = publish_source(
        &core,
        &pin,
        &printed.source,
        SourceFormat::TypeScriptV1,
        &request,
        "run",
    )
    .await;
    let env_a = execution_environment(&core, &pin, &original).await;
    let env_b = definition_environment(&core, &pin, &b, &fresh).await;
    for (definition, env, key, expected) in [
        (&a, &env_a, "print-env-a", 22),
        (&b, &env_b, "print-env-b", 122),
    ] {
        let run = start_with(
            &core,
            env,
            definition,
            key,
            [("x".into(), serde_json::json!(3))].into_iter().collect(),
        )
        .await;
        assert_eq!(
            finished(&core, &run).await.as_f64(),
            Some(f64::from(expected))
        );
    }
    let missing = lash::process::ProcessEnv::builder(lash::MaxToolCalls::new(1024))
        .component(lash::vm::KERNEL_SERVICES_CONFIG, &())
        .build()
        .unwrap();
    let (_, missing) = namespace(&core, &pin, &missing).await;
    let refused = core
        .host_artifacts()
        .import_workflow(
            SourceFormat::TypeScriptV1.language(),
            &printed.source,
            &missing,
            None,
        )
        .await
        .unwrap();
    let lash::workflow::WorkflowImport::Refused(diagnostics) = refused else {
        panic!("{refused:?}");
    };
    assert!(!diagnostics.is_empty());
    assert!(
        diagnostics
            .iter()
            .any(|d| d.code().contains("PACKAGE") || d.code().contains("UNKNOWN"))
    );
    let opened = core
        .host_artifacts()
        .open_workflow(&a.id, None)
        .await
        .unwrap();
    let mut document = opened.draft.into_document();
    let mut conflict = document
        .imports
        .values()
        .find(|i| i.name.to_string() == "tools.echo")
        .unwrap()
        .clone();
    let lash::package::ImportKind::Operation { signature } = &mut conflict.kind else {
        panic!("operation");
    };
    signature.result = lash::schema::Type::Text;
    document.imports.insert(
        lash::package::ImportSlot(document.imports.len() as u32),
        conflict,
    );
    let error = lash::vm::WorkerService::default()
        .print(
            &document,
            None,
            SourceFormat::TypeScriptV1,
            Default::default(),
        )
        .await
        .unwrap_err();
    assert!(matches!(
        error,
        lash::vm::SourceServiceError::Print(lash::dialect::PrintError::Namespace(
            lash::dialect::NamespaceRefusal::ImportNameConflict { .. }
        ))
    ));
    core.shutdown().await.unwrap();
}
