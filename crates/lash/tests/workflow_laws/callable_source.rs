//! K-CALLABLE source authoring laws over retained SQLite definitions.
use super::*;
use lash::dialect::SourceFormat;
use lash::workflow::document::{CaptureValue, Datum, FunctionValue, Node, WrittenSignature};
use lash::workflow::{WorkflowEditRefusal, WorkflowInspection};

const TS: &str = "const offset = 7; const f = (x: number): number => x + offset; const first = f; const second = f; const answer = async (x: number): Promise<number> => await tools.echo({value: first(x) + second(x)});";
const PY: &str = "offset = 7\ndef f(x: int) -> int:\n    return x + offset\nfirst = f\nsecond = f\nasync def answer(x: int) -> int:\n    return await tools.echo({\"value\": first(x) + second(x)})\n";

async fn saved(
    core: &lash::LashCore,
    pin: &lash::process::HostArtifactPin,
    format: SourceFormat,
) -> (
    lash::process::ProcessDefinition,
    lash::vm::WorkflowEnvironmentRequest,
    FunctionValue,
) {
    let (_, environment) = namespace(core, pin, &source_environment(false)).await;
    let release = lash_vm_releases::helper_release().unwrap();
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
            TS,
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
                SourceFormat::PythonV1 => None,
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
    let printed = artifacts
        .print_workflow(&base, None, SourceFormat::TypeScriptV1, Default::default())
        .await
        .unwrap();
    let edited_source = printed.source.replace("second(x)", "second(x) + 1");
    assert_ne!(edited_source, printed.source);
    let edit = artifacts
        .edited_from_read(
            &base,
            &edited_source,
            SourceFormat::TypeScriptV1,
            &environment,
            None,
        )
        .await
        .unwrap();
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
    let pin = lash::process::HostArtifactPin::mint();
    artifacts.release(pin.clone()).await.unwrap();
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
            "publish_workflow_edit",
            artifacts
                .publish_workflow_edit(&pin, &edit, &environment, None)
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

/// K-CALLABLE-ROUNDTRIP: reopen, duplicate code identities with distinct
/// environments, and Reference(0) survive unchanged source publication.
#[tokio::test]
async fn sqlite_reopen_callable_roundtrip_preserves_descriptor_and_frozen_graph() {
    for format in [SourceFormat::TypeScriptV1, SourceFormat::PythonV1] {
        let files = Files::new();
        let core = core_over(files.open().await, "callable-save").await;
        let pin = lash::process::HostArtifactPin::mint();
        let (definition, environment, function) = saved(&core, &pin, format).await;
        let captures = lash_vocabulary::tokens::encode_body(&function.captures);
        core.shutdown().await.unwrap();
        drop(core);
        let core = core_over(files.open().await, "callable-open").await;
        let base = inspect(&core, &definition).await;
        let printed = core
            .host_artifacts()
            .print_workflow(
                &base,
                None,
                format,
                lash::dialect::PrintOptions {
                    source_map: true,
                    ..Default::default()
                },
            )
            .await
            .unwrap_or_else(|error| panic!("{format:?}: {error:?}"));
        assert!(!printed.source.contains("offset ="));
        let map = printed.source_map.as_ref().unwrap();
        map.validate(base.document.document(), &printed.source)
            .unwrap();
        let edit = core
            .host_artifacts()
            .edited_from_read(&base, &printed.source, format, &environment, None)
            .await
            .unwrap();
        assert_eq!(edit.base_definition(), &definition.id);
        let published = core
            .host_artifacts()
            .publish_workflow_edit(&pin, &edit, &environment, None)
            .await
            .unwrap();
        assert_eq!(published, definition);
        let after = inspect(&core, &published).await;
        assert_eq!(after.document.callable(), Some(&function));
        assert_eq!(
            lash_vocabulary::tokens::encode_body(&after.document.callable().unwrap().captures),
            captures
        );
        let env = execution_environment(&core, &pin, &function).await;
        let run = start_with(
            &core,
            &env,
            &published,
            "reopened-callable",
            [("x".into(), serde_json::json!(3))].into_iter().collect(),
        )
        .await;
        assert_eq!(finished(&core, &run).await, serde_json::json!(22));
        core.shutdown().await.unwrap();
    }
}

/// K-CALLABLE-EDIT: replacement code preserves the frozen environment and
/// leaves the original definition usable after publication.
#[tokio::test]
async fn callable_edits_replace_code_and_preserve_capture_bytes() {
    for format in [SourceFormat::TypeScriptV1, SourceFormat::PythonV1] {
        let core = core().await;
        let pin = lash::process::HostArtifactPin::mint();
        let (definition, environment, original) = saved(&core, &pin, format).await;
        let base = inspect(&core, &definition).await;
        let printed = core
            .host_artifacts()
            .print_workflow(&base, None, format, Default::default())
            .await
            .unwrap();
        let edited = printed.source.replace("second(x)", "second(x) + 1");
        assert_ne!(edited, printed.source);
        let edit = core
            .host_artifacts()
            .edited_from_read(&base, &edited, format, &environment, None)
            .await
            .unwrap();
        let result = core
            .host_artifacts()
            .publish_workflow_edit(&pin, &edit, &environment, None)
            .await
            .unwrap();
        assert_ne!(result.id, definition.id);
        let after = inspect(&core, &result).await;
        let function = after.document.callable().unwrap();
        assert_ne!(function.callable.function, original.callable.function);
        assert_eq!(
            lash_vocabulary::tokens::encode_body(&function.captures),
            lash_vocabulary::tokens::encode_body(&original.captures)
        );
        assert_eq!(function.token, original.token);
        let packages = core.backend().package_store();
        let old_link = packages
            .link(&original.callable.link)
            .await
            .unwrap()
            .unwrap();
        let new_link = packages
            .link(&function.callable.link)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            new_link.link.fills, old_link.link.fills,
            "an edit keeps exact chosen import fills"
        );
        let refused = edited.replace("tools.echo", "tools.changed");
        assert!(matches!(
            core.host_artifacts()
                .edited_from_read(&base, &refused, format, &environment, None)
                .await
                .unwrap_err(),
            lash::EmbedError::WorkflowEdit(WorkflowEditRefusal::Callable(
                lash::workflow::document::CallableEditRefusal::ImportsChanged
            ))
        ));
        assert_eq!(
            inspect(&core, &definition).await.document.callable(),
            Some(&original)
        );
        let env = execution_environment(&core, &pin, &original).await;
        let args = [("x".into(), serde_json::json!(3))].into_iter().collect();
        let first = start_with(&core, &env, &definition, "original-callable", args).await;
        assert_eq!(finished(&core, &first).await, serde_json::json!(22));
        let edited_env = execution_environment(&core, &pin, function).await;
        let second = start_with(
            &core,
            &edited_env,
            &result,
            "edited-callable",
            [("x".into(), serde_json::json!(3))].into_iter().collect(),
        )
        .await;
        assert_eq!(finished(&core, &second).await, serde_json::json!(23));
        core.shutdown().await.unwrap();
    }
}

/// K-CALLABLE-CAPTURE-SET: exact lexical free-name differences are typed.
#[tokio::test]
async fn callable_edits_refuse_added_removed_and_renamed_captures() {
    for format in [SourceFormat::TypeScriptV1, SourceFormat::PythonV1] {
        let core = core().await;
        let pin = lash::process::HostArtifactPin::mint();
        let (definition, environment, _) = saved(&core, &pin, format).await;
        let base = inspect(&core, &definition).await;
        let printed = core
            .host_artifacts()
            .print_workflow(&base, None, format, Default::default())
            .await
            .unwrap();
        for (edited, added, removed) in [
            (
                printed.source.replace("second(x)", "second(x) + surprise"),
                vec!["surprise"],
                vec![],
            ),
            (
                printed.source.replace("second(x)", "first(x)"),
                vec![],
                vec!["second"],
            ),
            (
                printed.source.replace(
                    "second(x)",
                    if format == SourceFormat::TypeScriptV1 {
                        "second(x) + (() : number => surprise)()"
                    } else {
                        "second(x) + (lambda: surprise)()"
                    },
                ),
                vec!["surprise"],
                vec![],
            ),
            (
                printed.source.replace(
                    "first(x)",
                    if format == SourceFormat::TypeScriptV1 {
                        "((first: number): number => first)(x)"
                    } else {
                        "(lambda first: first)(x)"
                    },
                ),
                vec![],
                vec!["first"],
            ),
            (
                printed.source.replace("first(x)", "third(x)"),
                vec!["third"],
                vec!["first"],
            ),
        ] {
            let error = core
                .host_artifacts()
                .edited_from_read(&base, &edited, format, &environment, None)
                .await
                .unwrap_err();
            let lash::EmbedError::WorkflowEdit(WorkflowEditRefusal::Callable(
                lash::workflow::document::CallableEditRefusal::CaptureSetChanged {
                    added: actual_added,
                    removed: actual_removed,
                },
            )) = error
            else {
                panic!("typed capture refusal: {error:?}");
            };
            assert_eq!(actual_added, added.into_iter().map(Name::new).collect());
            assert_eq!(actual_removed, removed.into_iter().map(Name::new).collect());
        }
        assert!(matches!(
            core.host_artifacts()
                .edited_from_read(&base, "", format, &environment, None)
                .await
                .unwrap_err(),
            lash::EmbedError::WorkflowEdit(WorkflowEditRefusal::Callable(
                lash::workflow::document::CallableEditRefusal::EntryMissing
            ))
        ));
        let changed_shape = if format == SourceFormat::TypeScriptV1 {
            printed
                .source
                .replace("(x: number)", "(x: number, y: number)")
        } else {
            printed.source.replace("(x: int)", "(x: int, y: int)")
        };
        assert_ne!(changed_shape, printed.source);
        assert!(matches!(
            core.host_artifacts()
                .edited_from_read(&base, &changed_shape, format, &environment, None)
                .await
                .unwrap_err(),
            lash::EmbedError::WorkflowEdit(WorkflowEditRefusal::Callable(
                lash::workflow::document::CallableEditRefusal::EntryShapeChanged
            ))
        ));
        core.shutdown().await.unwrap();
    }
}

/// K-CALLABLE-PRESENTATION: caption edits retain captures and the complete
/// public signature even though the body uses a private packed adapter.
#[tokio::test]
async fn callable_caption_edits_preserve_the_public_presentation() {
    let core = core().await;
    let pin = lash::process::HostArtifactPin::mint();
    let (definition, environment, original) = saved(&core, &pin, SourceFormat::TypeScriptV1).await;
    let base = inspect(&core, &definition).await;
    let printed = core
        .host_artifacts()
        .print_workflow(&base, None, SourceFormat::TypeScriptV1, Default::default())
        .await
        .unwrap();
    let source = format!("/** New title */\n{}", printed.source);
    let edit = core
        .host_artifacts()
        .edited_from_read(
            &base,
            &source,
            SourceFormat::TypeScriptV1,
            &environment,
            None,
        )
        .await
        .unwrap();
    let published = core
        .host_artifacts()
        .publish_workflow_edit(&pin, &edit, &environment, None)
        .await
        .unwrap();
    assert_ne!(published.id, definition.id);
    let after = inspect(&core, &published).await;
    let function = after.document.callable().unwrap();
    assert_eq!(function.callable, original.callable);
    assert_eq!(function.signature, original.signature);
    assert_eq!(function.captures, original.captures);
    assert_eq!(function.written_signature, original.written_signature);
    assert!(
        function
            .captions
            .entries()
            .iter()
            .any(|entry| entry.caption.title() == "New title")
    );
    let missing = printed.source.replace("(x: number)", "(x)");
    assert_ne!(missing, printed.source);
    let error = core
        .host_artifacts()
        .edited_from_read(
            &base,
            &missing,
            SourceFormat::TypeScriptV1,
            &environment,
            None,
        )
        .await
        .unwrap_err();
    assert!(
        matches!(&error, lash::EmbedError::WorkflowSource(lash_vm_client::service::SourceServiceError::Dialect(refused)) if refused.fact.is_some()),
        "{error:?}"
    );
    let implicit = printed
        .source
        .replace("second(x)", "second(x) + `${{value: x}}`.length");
    let error = core
        .host_artifacts()
        .edited_from_read(
            &base,
            &implicit,
            SourceFormat::TypeScriptV1,
            &environment,
            None,
        )
        .await
        .unwrap_err();
    assert!(
        matches!(&error, lash::EmbedError::WorkflowSource(lash_vm_client::service::SourceServiceError::Dialect(refused)) if refused.code == lash_kernel_doc::ImplicitTextConversion::CODE),
        "{error:?}"
    );
    let allow = Some(implicit_text_limits(
        lash::dialect::workflow::ImplicitTextConversionPolicy::Allow,
    ));
    let permitted = core
        .host_artifacts()
        .edited_from_read(
            &base,
            &implicit,
            SourceFormat::TypeScriptV1,
            &environment,
            allow,
        )
        .await
        .unwrap();
    let error = core
        .host_artifacts()
        .publish_workflow_edit(&pin, &permitted, &environment, None)
        .await
        .unwrap_err();
    assert!(
        matches!(&error, lash::EmbedError::WorkflowSource(lash_vm_client::service::SourceServiceError::Dialect(refused)) if refused.code == lash_kernel_doc::ImplicitTextConversion::CODE),
        "{error:?}"
    );
    let captioned = inspect(&core, &published).await;
    let printed_captioned = core
        .host_artifacts()
        .print_workflow(
            &captioned,
            None,
            SourceFormat::TypeScriptV1,
            Default::default(),
        )
        .await
        .unwrap();
    let removed_caption = printed_captioned.source.replace("/** New title */\n", "");
    assert_ne!(removed_caption, printed_captioned.source);
    let removed = core
        .host_artifacts()
        .edited_from_read(
            &captioned,
            &removed_caption,
            SourceFormat::TypeScriptV1,
            &environment,
            None,
        )
        .await
        .unwrap();
    assert!(!removed.dropped_captions().is_empty());
    core.shutdown().await.unwrap();
}

/// K-CALLABLE-SOURCE-COVERAGE: authenticating the supplied inspection fails
/// with BaseMismatch before a different environment can be printed or read.
#[tokio::test]
async fn callable_source_operations_refuse_a_mismatched_inspection() {
    let core = core().await;
    let pin = lash::process::HostArtifactPin::mint();
    let (definition, environment, original) = saved(&core, &pin, SourceFormat::TypeScriptV1).await;
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
            .print_workflow(&graph, None, SourceFormat::TypeScriptV1, Default::default())
            .await
            .unwrap_err(),
        lash::EmbedError::WorkflowSource(lash_vm_client::service::SourceServiceError::Print(
            lash::dialect::PrintError::Unprintable { .. }
        ))
    ));
    let mut base = inspect(&core, &definition).await;
    let mut function = base.document.callable().unwrap().clone();
    function
        .captures
        .insert(Name::new("extra"), CaptureValue::Reference(0));
    let documents = base.document.document().clone();
    let release = lash_vm_releases::helper_release().unwrap();
    base.document =
        lash::workflow::WorkflowDocument::captured(documents, function, &release.helpers).unwrap();
    let error = core
        .host_artifacts()
        .print_workflow(&base, None, SourceFormat::TypeScriptV1, Default::default())
        .await
        .unwrap_err();
    assert!(
        matches!(
            error,
            lash::EmbedError::WorkflowEdit(WorkflowEditRefusal::BaseMismatch { .. })
        ),
        "{error:?}"
    );
    core.shutdown().await.unwrap();
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

/// FIG-7189: unchanged source saves preserve the host's authoritative sidecar
/// on both representations, without mutating the authenticated print base.
#[tokio::test]
async fn host_captions_survive_saved_workflow_source_edits() {
    for format in [SourceFormat::TypeScriptV1, SourceFormat::PythonV1] {
        for callable in [false, true] {
            let core = core().await;
            let pin = lash::process::HostArtifactPin::mint();
            let (base, environment) = caption_base(&core, &pin, format, callable).await;
            let captions = host_captions(&base, "Host title");
            let printed = core
                .host_artifacts()
                .print_workflow(&base, Some(&captions), format, Default::default())
                .await
                .unwrap();
            assert!(printed.source.contains("Host title"), "{}", printed.source);
            if !callable {
                assert!(printed.source.contains("Host step"), "{}", printed.source);
            }
            let edit = core
                .host_artifacts()
                .edited_from_read(&base, &printed.source, format, &environment, None)
                .await
                .unwrap();
            assert_eq!(edit.captions(), &captions);
            assert!(edit.dropped_captions().is_empty());
            let published = core
                .host_artifacts()
                .publish_workflow_edit(&pin, &edit, &environment, None)
                .await
                .unwrap();
            assert_eq!(edit.captions(), &captions);
            let after = inspect(&core, &published).await;
            if callable {
                assert_eq!(&after.document.callable().unwrap().captions, &captions);
                assert_eq!(
                    inspect(&core, &base.definition).await.document.callable(),
                    base.document.callable()
                );
                // An explicit empty override replaces stored captions only for this print.
                let empty =
                    lash::workflow::document::Captions::empty(after.document.document().identity());
                let cleared = core
                    .host_artifacts()
                    .print_workflow(&after, Some(&empty), format, Default::default())
                    .await
                    .unwrap();
                assert!(!cleared.source.contains("Host title"));
                let frozen = core
                    .host_artifacts()
                    .print_workflow(&after, None, format, Default::default())
                    .await
                    .unwrap();
                assert!(frozen.source.contains("Host title"));
            } else {
                assert_eq!(published, base.definition);
            }
            core.shutdown().await.unwrap();
        }
    }
}

/// FIG-7189: a source-only caption change never invalidates the saved base.
#[tokio::test]
async fn caption_only_source_edits_do_not_mismatch_the_base() {
    for format in [SourceFormat::TypeScriptV1, SourceFormat::PythonV1] {
        for callable in [false, true] {
            let core = core().await;
            let pin = lash::process::HostArtifactPin::mint();
            let (base, environment) = caption_base(&core, &pin, format, callable).await;
            let captions = host_captions(&base, "Host title");
            let printed = core
                .host_artifacts()
                .print_workflow(&base, Some(&captions), format, Default::default())
                .await
                .unwrap();
            let source = printed.source.replace("Host title", "Edited title");
            assert_ne!(source, printed.source);
            let edit = core
                .host_artifacts()
                .edited_from_read(&base, &source, format, &environment, None)
                .await
                .unwrap();
            assert_eq!(edit.base_definition(), &base.definition.id);
            assert_eq!(edit.captions(), &host_captions(&base, "Edited title"));
            let published = core
                .host_artifacts()
                .publish_workflow_edit(&pin, &edit, &environment, None)
                .await
                .unwrap();
            if !callable {
                assert_eq!(published, base.definition);
            }
            core.shutdown().await.unwrap();
        }
    }
}

/// FIG-7189: unpublished linked drafts need no authenticated base to print
/// and read their host captions through the exported worker source service.
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
        let (read, read_captions) = service
            .read_exported(&printed.source, &document, format, Default::default(), None)
            .await
            .unwrap();
        assert_eq!(read.canonical_bytes(), document.canonical_bytes());
        assert_eq!(read_captions, captions);
    }
    core.shutdown().await.unwrap();
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
            .print_workflow(&base, Some(captions), format, options)
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
