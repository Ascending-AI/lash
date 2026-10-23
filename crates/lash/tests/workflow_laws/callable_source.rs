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
            &lash::package::LinkedStart {
                link: function.callable.link,
                selector: lash::package::StartSelector::Main,
            },
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
            .print_workflow(&base, format, Default::default())
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
            .print_workflow(&base, format, Default::default())
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
        .print_workflow(&base, SourceFormat::TypeScriptV1, Default::default())
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
        .print_workflow(&captioned, SourceFormat::TypeScriptV1, Default::default())
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
            .print_workflow(&graph, SourceFormat::TypeScriptV1, Default::default())
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
        .print_workflow(&base, SourceFormat::TypeScriptV1, Default::default())
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
