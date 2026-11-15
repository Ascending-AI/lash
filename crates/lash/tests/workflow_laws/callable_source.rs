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
            declared_locals: written.declared_locals.clone(),
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
            Datum::Error(Box::new(lash_kernel_doc::ErrorDatum::new(
                "saved".into(),
                "typed value".into(),
                Datum::Absent,
            ))),
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

/// PRINT-STANDALONE (FIG-7660, FIG-7663): every published callable in the
/// capture corpus prints, ordinarily imports and retains its frozen behavior.
/// Cenote's R14 number-concatenation refusal is retained as an exact cell.
#[tokio::test]
async fn published_callable_capture_corpus_prints_and_roundtrips() {
    const CENOTE_SIGNUP_COUNTS: &str = r#"const created = await workflows.create({
  name: 'signup-counts-report',
  run: async (): Promise<unknown> => {
    const lines: Array<string> = [];
    for (let i = 1; i <= 3; i++) lines.push('Week ' + i + ':');
    return views.text(lines.join('\n'));
  }
});
console.log(created);
"#;
    let mut failures = Vec::new();
    for format in [SourceFormat::TypeScriptV1, SourceFormat::PythonV1] {
        let (core, pin, base, packages, request) = super::packages::capturing_views().await;
        let source = match format {
            SourceFormat::TypeScriptV1 => {
                r#"
export async function ships(): Promise<string> {
    let captured = "frozen `template` ${text}";
    const helper = () => captured;
    const digits = "3.1415926535897932384";
    const root = async (): Promise<unknown> => views.card("Pi", [views.heading(digits), views.text("20 digits")]);
    const greet = async (name: string): Promise<string> => helper() + name;
    await tools.capture({value: "frozen-helper-root", nested: {fact: root}});
    await tools.capture({value: "frozen-helper-greet", nested: {fact: greet}});
    captured = "later";
    return captured;
}
"#
            }
            SourceFormat::PythonV1 => {
                r#"
async def ships() -> str:
    captured = "frozen `template` ${text}"
    def helper(prefix: str = "") -> str:
        return prefix + captured
    digits = "3.1415926535897932384"
    async def root() -> object:
        return views.card("Pi", [views.heading(digits), views.text("20 digits")])
    async def greet(name: str) -> str:
        return helper("") + name
    await tools.capture({"value": "frozen-helper-root", "nested": {"fact": root}})
    await tools.capture({"value": "frozen-helper-greet", "nested": {"fact": greet}})
    captured = "later"
    return captured
"#
            }
        };
        // Generate the same capture shapes in each dialect, rather than a
        // separate host workflow law for every scalar and aggregate kind.
        let mut extra = String::new();
        let literal_cases = [
            ("value.absent()", "value.absent()"),
            ("null", "None"),
            ("true", "True"),
            (
                "integer.from_decimal(\"12345678901234567890\")",
                "12345678901234567890",
            ),
            ("3.25", "3.25"),
            ("7n", "7"),
            ("\"λ `template` ${text}\"", "\"λ `template` ${text}\""),
            ("bytes.from_hex(\"00ff\")", "bytes.from_hex(\"00ff\")"),
            ("[1, 2]", "[1, 2]"),
            ("tuple.of(1, \"two\")", "tuple.of(1, \"two\")"),
            ("set.of(1, 2)", "set.of(1, 2)"),
            (
                "map.from_entries([\"key\", 2])",
                "map.from_entries([\"key\", 2])",
            ),
            ("{key: [1, null]}", "{\"key\": [1, None]}"),
            (
                "record.new({key: [1, null]})",
                "record.new({\"key\": [1, None]})",
            ),
            (
                "timestamp.from_nanoseconds(integer.from_decimal(\"12345678901234567890\"))",
                "timestamp.from_nanoseconds(12345678901234567890)",
            ),
            (
                "error.new(\"saved\", \"message\", value.absent())",
                "error.new(\"saved\", \"message\", value.absent())",
            ),
        ];
        for (index, (ts, py)) in literal_cases.iter().enumerate() {
            match format {
                SourceFormat::TypeScriptV1 => extra.push_str(&format!(
                    "    const capture{index} = {ts};\n    const literal{index} = async (): Promise<unknown> => callable.invoke(views.text, value.same(capture{index}, capture{index}));\n    await tools.capture({{value: \"frozen-helper-literal{index}\", nested: {{fact: literal{index}}}}});\n"
                )),
                SourceFormat::PythonV1 => extra.push_str(&format!(
                    "    capture{index} = {py}\n    async def literal{index}() -> object:\n        return callable.invoke(views.text, value.same(capture{index}, capture{index}))\n    await tools.capture({{\"value\": \"frozen-helper-literal{index}\", \"nested\": {{\"fact\": literal{index}}}}})\n"
                )),
            }
        }
        if format == SourceFormat::TypeScriptV1 {
            extra.push_str(CENOTE_SIGNUP_COUNTS);
        }
        let source = source.replace(
            "    captured = \"later\"",
            &(extra + "    captured = \"later\""),
        );
        let imported = core
            .host_artifacts()
            .import_workflow(format.language(), &source, &request, None)
            .await
            .unwrap();
        let lash::workflow::WorkflowImport::Imported { document, .. } = imported else {
            panic!("{format:?}: {imported:?}");
        };
        let publication = publish_recorded(
            &core,
            &pin,
            &document,
            &Name::new("ships"),
            &base,
            None,
            (&packages, &request),
        )
        .await
        .unwrap();
        let run = start_with(
            &core,
            &publication.env_ref,
            &publication.definition,
            "freeze-helpers",
            Default::default(),
        )
        .await;
        assert_eq!(finished(&core, &run).await, serde_json::json!("later"));
        let origin = lash::package::CallableOrigin {
            publications: publication
                .environment
                .execution()
                .publications
                .iter()
                .map(lash::package::PublicationRecord::key)
                .collect(),
            authored: publication.environment.execution().authored.clone(),
        };
        let mut cases = vec![
            ("root".to_owned(), "", serde_json::Map::new()),
            (
                "greet".to_owned(),
                " Ada",
                [("name".into(), serde_json::json!(" Ada"))]
                    .into_iter()
                    .collect(),
            ),
        ];
        cases.extend(
            (0..literal_cases.len())
                .map(|index| (format!("literal{index}"), "", serde_json::Map::new())),
        );
        if format == SourceFormat::TypeScriptV1 {
            cases.push((
                "signup-counts-report".to_owned(),
                "",
                serde_json::Map::new(),
            ));
        }
        for (name, suffix, args) in cases {
            let marker = format!("frozen-helper-{name}");
            let shipped = {
                let calls = ECHO_CALLS
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                let captured = calls
                    .iter()
                    .rev()
                    .find(|args| {
                        args["value"] == marker
                            || (name == "signup-counts-report" && args["name"] == name)
                    })
                    .unwrap();
                if name == "signup-counts-report" {
                    captured["run"].clone()
                } else {
                    captured["nested"]["fact"].clone()
                }
            };
            let function = FunctionValue::from_transport_json(
                &shipped,
                &lash::workflow::document::FunctionRegistry::default(),
                Default::default(),
                lash_kernel_value::data_limits::KernelDataLimits::standard(),
            )
            .unwrap();
            core.host_artifacts()
                .inspect_callable(&function, &request, &origin)
                .await
                .unwrap();
            let definition = match core
                .host_artifacts()
                .publish_callable(&pin, &function, &request, &origin, None)
                .await
            {
                Ok(definition) => definition,
                Err(error) => {
                    failures.push(format!("{format:?} {name} publication: {error:?}"));
                    continue;
                }
            };
            let printed = core
                .host_artifacts()
                .print_workflow(&definition.id, None, format, Default::default())
                .await
                .unwrap_or_else(|error| panic!("{format:?} {name}: {error:?}"));
            let roundtrip =
                publish_source(&core, &pin, &printed.source, format, &request, "run").await;
            let original_env = publication.env_ref.clone();
            let roundtrip_env = definition_environment(&core, &pin, &roundtrip, &base).await;
            for (definition, env, kind) in [
                (&definition, &original_env, "original"),
                (&roundtrip, &roundtrip_env, "roundtrip"),
            ] {
                let run = start_with(
                    &core,
                    env,
                    definition,
                    &format!("{name}-{kind}"),
                    args.clone(),
                )
                .await;
                assert_eq!(
                    finished(&core, &run).await,
                    if name == "root" {
                        serde_json::json!({"type":"card", "title":"Pi", "children":[{"type":"heading", "text":"3.1415926535897932384"},{"type":"text", "value":"20 digits"}]})
                    } else if name == "greet" {
                        serde_json::json!(format!("frozen `template` ${{text}}{suffix}"))
                    } else if name == "signup-counts-report" {
                        serde_json::json!({"type":"text", "value":"Week 1:\nWeek 2:\nWeek 3:"})
                    } else {
                        serde_json::json!({"type":"text", "value":true})
                    },
                    "{format:?} {name} {kind}",
                );
            }
        }
        core.shutdown().await.unwrap();
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

/// The workflow `ships` of `source`, imported and run once; it hands each
/// callable of `names` to `tools.capture` as `collection-{name}`. Answers
/// the run's publication and each shipped callable with the origin its
/// publication takes.
async fn shipped_callables(
    core: &lash::LashCore,
    pin: &lash::process::HostArtifactPin,
    (base, packages, request): (
        &lash::process::ProcessEnv,
        &lash::package::RecordedPackages,
        &lash::vm::WorkflowEnvironmentRequest,
    ),
    format: SourceFormat,
    source: &str,
    names: &[&str],
) -> (
    super::support::Publication,
    lash::package::CallableOrigin,
    Vec<FunctionValue>,
) {
    let imported = core
        .host_artifacts()
        .import_workflow(format.language(), source, request, None)
        .await
        .unwrap();
    let lash::workflow::WorkflowImport::Imported { document, .. } = imported else {
        panic!("{imported:?}");
    };
    let publication = publish_recorded(
        core,
        pin,
        &document,
        &Name::new("ships"),
        base,
        None,
        (packages, request),
    )
    .await
    .unwrap();
    let run = start_with(
        core,
        &publication.env_ref,
        &publication.definition,
        &format!("ship-{}", names.join("-")),
        Default::default(),
    )
    .await;
    assert_eq!(finished(core, &run).await, serde_json::json!("shipped"));
    let origin = lash::package::CallableOrigin {
        publications: publication
            .environment
            .execution()
            .publications
            .iter()
            .map(lash::package::PublicationRecord::key)
            .collect(),
        authored: publication.environment.execution().authored.clone(),
    };
    let functions = names
        .iter()
        .map(|name| {
            let marker = format!("collection-{name}");
            let shipped = ECHO_CALLS
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .iter()
                .rev()
                .find(|args| args["value"] == marker)
                .unwrap()["nested"]["fact"]
                .clone();
            FunctionValue::from_transport_json(
                &shipped,
                &lash::workflow::document::FunctionRegistry::default(),
                Default::default(),
                lash_kernel_value::data_limits::KernelDataLimits::standard(),
            )
            .unwrap()
        })
        .collect();
    (publication, origin, functions)
}

/// Each shipped callable of `cases` publishes, prints in `format` and
/// re-imports; the original and the reimported callable both answer the
/// case's expected value. Answers each printed source by case name.
async fn printed_callables(
    (core, pin, base, request): (
        &lash::LashCore,
        &lash::process::HostArtifactPin,
        &lash::process::ProcessEnv,
        &lash::vm::WorkflowEnvironmentRequest,
    ),
    (publication, origin): (&super::support::Publication, &lash::package::CallableOrigin),
    format: SourceFormat,
    cases: &[(&str, serde_json::Value, serde_json::Value)],
    functions: &[FunctionValue],
) -> std::collections::BTreeMap<String, String> {
    let mut printed_sources = std::collections::BTreeMap::new();
    let mut refused = Vec::new();
    for ((name, args, expected), function) in cases.iter().zip(functions) {
        let definition = match core
            .host_artifacts()
            .publish_callable(pin, function, request, origin, None)
            .await
        {
            Ok(definition) => definition,
            Err(error) => {
                refused.push(format!("{format:?} {name} publication: {error:?}"));
                continue;
            }
        };
        let printed = core
            .host_artifacts()
            .print_workflow(&definition.id, None, format, Default::default())
            .await
            .unwrap();
        let roundtrip = publish_source(core, pin, &printed.source, format, request, "run").await;
        let roundtrip_env = definition_environment(core, pin, &roundtrip, base).await;
        for (definition, env, kind) in [
            (&definition, &publication.env_ref, "original"),
            (&roundtrip, &roundtrip_env, "roundtrip"),
        ] {
            assert_eq!(
                outcome(
                    core,
                    env,
                    definition,
                    &format!("{format:?}-{name}-{kind}"),
                    args
                )
                .await,
                Ok(expected.clone()),
                "{format:?} {name} {kind}\n{}",
                printed.source,
            );
        }
        printed_sources.insert((*name).to_owned(), printed.source);
    }
    assert!(refused.is_empty(), "{}", refused.join("\n"));
    printed_sources
}

/// What `definition` started under `env` with `args` ends in.
async fn outcome(
    core: &lash::LashCore,
    env: &lash::process::ProcessEnvRef,
    definition: &lash::process::ProcessDefinition,
    key: &str,
    args: &serde_json::Value,
) -> Result<serde_json::Value, (Option<String>, String)> {
    let run = start_with(
        core,
        env,
        definition,
        key,
        args.as_object().unwrap().clone(),
    )
    .await;
    let output = tokio::time::timeout(
        std::time::Duration::from_secs(120),
        core.processes().await_output(&run),
    )
    .await
    .expect("the process settles")
    .expect("the output reads");
    let lash_execution_contracts::process::ProcessAwaitOutput::Settled { output } = output else {
        panic!("the process settles with an output: {output:?}");
    };
    if output.is_success() {
        return Ok(output.value_for_projection());
    }
    let lash::tools::ToolCallOutcome::Failure(failure) = output.outcome else {
        panic!("the process fails: {output:?}");
    };
    let kind = match failure.cause.as_deref() {
        Some(lash::tools::ToolFailureCause::UncaughtError { error_kind, .. }) => error_kind.clone(),
        other => panic!("the failure's cause is the uncaught error: {other:?}"),
    };
    Err((kind, failure.message))
}

/// FIG-7697, FIG-7707, FIG-7710: a callable that reads a captured list by a
/// literal or computed index or its length, maps over it or calls another
/// of its rows, pushes onto it or adds into its element, calls a captured
/// map's or set's methods, indexes a captured list it reassigns, reads a
/// captured text's length or record's field, reads a `const` declared
/// after it, or is an anonymous arrow another arrow returns (FIG-7711),
/// publishes: its printed source
/// re-imports to the same kernel document (publication proves it), and
/// both answer alike. The printed operations keep their own checks: a list
/// read or length of an edited fill that is no list fails the list's kind
/// check rather than reading the text, and a method of a fill of another
/// kind fails as that method call of an ordinary binding does.
#[tokio::test]
async fn published_callables_capturing_collections_print_their_exact_operations() {
    let (core, pin, base, packages, request) = super::packages::capturing_views().await;
    let source = r#"
export async function ships(): Promise<string> {
    const xs = [7, 8];
    const m = new Map([["a", 1]]);
    const s = new Set([1]);
    let ys = [5];
    const t = "abc";
    const r = {a: 5, b: "z"};
    const listed = async (x: number): Promise<number> => x + xs[1];
    const indexed = async (x: number): Promise<number> => xs[x];
    const long = async (x: number): Promise<number> => x + xs.length;
    const scaled = async (x: number): Promise<unknown> => xs.map((v: number) => v + x);
    const found = async (x: number): Promise<number> => xs.indexOf(x);
    const measured = async (x: number): Promise<number> => x + t.length;
    const fielded = async (x: number): Promise<number> => x + r.a;
    const owned = async (key: string): Promise<boolean> => r.hasOwnProperty(key);
    const u = " ab ";
    const trimmed = async (x: number): Promise<unknown> => [x, u.trim()];
    const zs = [1];
    const pushed = async (x: number): Promise<number> => { zs.push(x); return zs.length + zs[1]; };
    const ws = [1, 2];
    const bumped = async (x: number): Promise<number> => { ws[1] += x; return ws[1]; };
    const later = async (x: number): Promise<unknown> => [x, k];
    const k = 4;
    const mapped = async (key: string): Promise<unknown> => { m.set(key, 2); return m.has(key) ? m.get(key) : 0; };
    const sets = async (x: number): Promise<boolean> => { s.add(x); return s.has(x); };
    const reassigned = async (x: number): Promise<number> => { ys = [x, x + 1]; return ys[1]; };
    const make = async (): Promise<unknown> => { const vs = [1, 2]; return async (x: number): Promise<unknown> => [x, vs[1]]; };
    await tools.capture({value: "collection-listed", nested: {fact: listed}});
    await tools.capture({value: "collection-mapped", nested: {fact: mapped}});
    await tools.capture({value: "collection-sets", nested: {fact: sets}});
    await tools.capture({value: "collection-reassigned", nested: {fact: reassigned}});
    await tools.capture({value: "collection-indexed", nested: {fact: indexed}});
    await tools.capture({value: "collection-long", nested: {fact: long}});
    await tools.capture({value: "collection-scaled", nested: {fact: scaled}});
    await tools.capture({value: "collection-found", nested: {fact: found}});
    await tools.capture({value: "collection-measured", nested: {fact: measured}});
    await tools.capture({value: "collection-fielded", nested: {fact: fielded}});
    await tools.capture({value: "collection-owned", nested: {fact: owned}});
    await tools.capture({value: "collection-trimmed", nested: {fact: trimmed}});
    await tools.capture({value: "collection-pushed", nested: {fact: pushed}});
    await tools.capture({value: "collection-bumped", nested: {fact: bumped}});
    await tools.capture({value: "collection-later", nested: {fact: later}});
    await tools.capture({value: "collection-returned", nested: {fact: await make()}});
    return "shipped";
}
"#;
    let cases = [
        ("listed", serde_json::json!({"x": 1}), serde_json::json!(9)),
        (
            "mapped",
            serde_json::json!({"key": "k"}),
            serde_json::json!(2),
        ),
        ("sets", serde_json::json!({"x": 2}), serde_json::json!(true)),
        (
            "reassigned",
            serde_json::json!({"x": 3}),
            serde_json::json!(4),
        ),
        ("indexed", serde_json::json!({"x": 0}), serde_json::json!(7)),
        ("long", serde_json::json!({"x": 1}), serde_json::json!(3)),
        (
            "scaled",
            serde_json::json!({"x": 1}),
            serde_json::json!([8, 9]),
        ),
        (
            "measured",
            serde_json::json!({"x": 1}),
            serde_json::json!(4),
        ),
        ("fielded", serde_json::json!({"x": 1}), serde_json::json!(6)),
        (
            "owned",
            serde_json::json!({"key": "a"}),
            serde_json::json!(true),
        ),
        (
            "trimmed",
            serde_json::json!({"x": 1}),
            serde_json::json!([1, "ab"]),
        ),
        ("found", serde_json::json!({"x": 8}), serde_json::json!(1)),
        ("pushed", serde_json::json!({"x": 5}), serde_json::json!(7)),
        ("bumped", serde_json::json!({"x": 3}), serde_json::json!(5)),
        (
            "later",
            serde_json::json!({"x": 1}),
            serde_json::json!([1, 4]),
        ),
        (
            "returned",
            serde_json::json!({"x": 1}),
            serde_json::json!([1, 2]),
        ),
    ];
    let names: Vec<_> = cases.iter().map(|(name, ..)| *name).collect();
    let (publication, origin, functions) = shipped_callables(
        &core,
        &pin,
        (&base, &packages, &request),
        SourceFormat::TypeScriptV1,
        source,
        &names,
    )
    .await;
    let printed_sources = printed_callables(
        (&core, &pin, &base, &request),
        (&publication, &origin),
        SourceFormat::TypeScriptV1,
        &cases,
        &functions,
    )
    .await;

    // Each edited fill replaces the one literal the printed source fills
    // the capture with.
    let edited = |name: &str, literal: &str, replacement: &str| {
        let source = &printed_sources[name];
        assert_eq!(source.matches(literal).count(), 1, "{source}");
        source.replace(literal, replacement)
    };
    let run_source = |source: String, key: &'static str, args: serde_json::Value| {
        let (core, pin, request, base) = (&core, &pin, &request, &base);
        async move {
            let definition = publish_source(
                core,
                pin,
                &source,
                SourceFormat::TypeScriptV1,
                request,
                "run",
            )
            .await;
            let env = definition_environment(core, pin, &definition, base).await;
            outcome(core, &env, &definition, key, &args).await
        }
    };
    // `xs[1]` of a text would read its character ("18"); the list read
    // takes the list's length first, whose kind check fails.
    assert_eq!(
        run_source(
            edited("listed", "[7, 8]", "\"78\""),
            "listed-text",
            serde_json::json!({"x": 1}),
        )
        .await,
        Err((
            Some("type_error".to_owned()),
            "collection has the wrong kind".to_owned()
        )),
    );
    // `xs.length` of a text would be its length (2); the list's length
    // takes the list's own read, whose kind check fails.
    assert_eq!(
        run_source(
            edited("long", "[7, 8]", "\"78\""),
            "long-text",
            serde_json::json!({"x": 1}),
        )
        .await,
        Err((
            Some("type_error".to_owned()),
            "collection has the wrong kind".to_owned()
        )),
    );
    // A map method of a list fails as the same method of an ordinary
    // binding nothing reassigns, a parameter.
    let ordinary = run_source(
        "export async function run(key: string, m: unknown): Promise<unknown> {\n  m.set(key, 2);\n  return m.has(key) ? m.get(key) : 0;\n}\n".into(),
        "mapped-ordinary",
        serde_json::json!({"key": "k", "m": [["a", 1]]}),
    )
    .await;
    assert!(ordinary.is_err(), "{ordinary:?}");
    assert_eq!(
        run_source(
            edited("mapped", "map.from_entries([\"a\", 1])", "[[\"a\", 1]]"),
            "mapped-list",
            serde_json::json!({"key": "k"}),
        )
        .await,
        ordinary,
    );
    // FIG-7710: a list's own row on a text fill runs the list row,
    // `Array.prototype.indexOf.call("78", 8)` (-1: no element is strictly
    // 8), not the text's `"78".indexOf(8)` (1).
    assert_eq!(
        run_source(
            edited("found", "[7, 8]", "\"78\""),
            "found-text",
            serde_json::json!({"x": 8}),
        )
        .await,
        Ok(serde_json::json!(-1)),
    );
    // FIG-7711: `[7, 8].hasOwnProperty("a")` would be false; the record's
    // method read tests the receiver's own fields first, whose kind check
    // fails on a list.
    assert_eq!(
        run_source(
            edited("owned", "{ a: 5, b: \"z\" }", "[7, 8]"),
            "owned-list",
            serde_json::json!({"key": "a"}),
        )
        .await,
        Err((
            Some("type_error".to_owned()),
            "collection has the wrong kind".to_owned()
        )),
    );
    // `[7, 8].trim()` would fail as a missing method's call; the text's
    // own row checks its receiver's length first, which fails.
    assert_eq!(
        run_source(
            edited("trimmed", "\" ab \"", "[7, 8]"),
            "trimmed-list",
            serde_json::json!({"x": 1}),
        )
        .await,
        Err((Some("type_error".to_owned()), "expected text".to_owned())),
    );
    // A push onto a text would dispatch the text's member; the
    // list's own push reads the list's length first, whose kind check
    // fails.
    assert_eq!(
        run_source(
            edited("pushed", "[1]", "\"ab\""),
            "pushed-text",
            serde_json::json!({"x": 5}),
        )
        .await,
        Err((
            Some("type_error".to_owned()),
            "collection has the wrong kind".to_owned()
        )),
    );
    // `ws[1] += x` of a text would read its character and write nothing;
    // the list's element read fails its kind check where it runs, and the
    // list's write fails at the bound it tests.
    assert_eq!(
        run_source(
            edited("bumped", "[1, 2]", "\"ab\""),
            "bumped-text",
            serde_json::json!({"x": 3}),
        )
        .await,
        Err((
            Some("type_error".to_owned()),
            "collection has the wrong kind".to_owned()
        )),
    );
    assert_eq!(
        run_source(
            "export async function run(x: number, xs: unknown): Promise<unknown> {\n  list.set(xs, 1, x);\n  return xs;\n}\n".into(),
            "set-text",
            serde_json::json!({"x": 3, "xs": "ab"}),
        )
        .await,
        Err((
            Some("type_error".to_owned()),
            "collection has the wrong kind".to_owned()
        )),
    );
    // The spelled check fails where the read of a binding still in its
    // temporal dead zone does.
    let uninitialized = |read: &str| {
        format!(
            "export async function run(x: number): Promise<unknown> {{\n  const f = async (): Promise<unknown> => [x, {read}];\n  const r = await f();\n  const k = 4;\n  return r;\n}}\n"
        )
    };
    let ordinary = run_source(
        uninitialized("k"),
        "later-ordinary",
        serde_json::json!({"x": 1}),
    )
    .await;
    assert!(ordinary.is_err(), "{ordinary:?}");
    assert_eq!(
        run_source(
            uninitialized("value.initialized(k)"),
            "later-spelled",
            serde_json::json!({"x": 1}),
        )
        .await,
        ordinary,
    );
    // The Python twins: `zs.append(x)` and `ws[1] += x` on a captured list.
    let python = r#"
async def ships() -> str:
    zs = [1]
    async def appended(x: int) -> int:
        zs.append(x)
        return len(zs) + zs[1]
    ws = [1, 2]
    async def added(x: int) -> int:
        ws[1] += x
        return ws[1]
    await tools.capture({"value": "collection-appended", "nested": {"fact": appended}})
    await tools.capture({"value": "collection-added", "nested": {"fact": added}})
    return "shipped"
"#;
    let cases = [
        (
            "appended",
            serde_json::json!({"x": 5}),
            serde_json::json!(7),
        ),
        ("added", serde_json::json!({"x": 3}), serde_json::json!(5)),
    ];
    let names: Vec<_> = cases.iter().map(|(name, ..)| *name).collect();
    let (publication, origin, functions) = shipped_callables(
        &core,
        &pin,
        (&base, &packages, &request),
        SourceFormat::PythonV1,
        python,
        &names,
    )
    .await;
    let printed_python = printed_callables(
        (&core, &pin, &base, &request),
        (&publication, &origin),
        SourceFormat::PythonV1,
        &cases,
        &functions,
    )
    .await;
    // A captured binding's receiver is untyped in Python: `ws[1] += x` of a
    // text reads its character with the generic item read, and the
    // in-place add fails where the original's does, before any store.
    let source = &printed_python["added"];
    assert_eq!(source.matches("[1, 2]").count(), 1, "{source}");
    let definition = publish_source(
        &core,
        &pin,
        &source.replace("[1, 2]", "\"ab\""),
        SourceFormat::PythonV1,
        &request,
        "run",
    )
    .await;
    let env = definition_environment(&core, &pin, &definition, &base).await;
    assert_eq!(
        outcome(
            &core,
            &env,
            &definition,
            "added-text",
            &serde_json::json!({"x": 3})
        )
        .await,
        Err((
            Some("TypeError".to_owned()),
            "unsupported operand type(s) for +: 'str' and 'int'".to_owned()
        )),
    );
    core.shutdown().await.unwrap();
}

/// FIG-7697 graph: each of two instances of one callable, bound to distinct
/// values, captures a list holding a callable that calls it back. Each
/// publishes, prints and re-imports to a callable that recurses through
/// its own list to its own value, while the published callable still runs
/// under its original defining Link. A capture past the function-value
/// bound is refused before anything is published.
#[tokio::test]
async fn published_callables_capturing_a_collection_of_themselves_keep_their_graph() {
    let (core, pin, base, packages, request) = super::packages::capturing_views().await;
    let source = r#"
export async function ships(): Promise<string> {
    const make = (base: number) => {
        const loop = [null];
        const seen = new Set([base]);
        const answer = async (n: number): Promise<number> => {
            if (n <= 0) {
                return seen.has(base) ? base : -1;
            }
            const next = loop[0];
            return await next(n - 1);
        };
        const again = async (n: number): Promise<number> => await answer(n);
        loop[0] = again;
        return answer;
    };
    const ten = make(10);
    const twenty = make(20);
    await tools.capture({value: "collection-ten", nested: {fact: ten}});
    await tools.capture({value: "collection-twenty", nested: {fact: twenty}});
    return "shipped";
}
"#;
    let (publication, origin, functions) = shipped_callables(
        &core,
        &pin,
        (&base, &packages, &request),
        SourceFormat::TypeScriptV1,
        source,
        &["ten", "twenty"],
    )
    .await;
    assert_eq!(functions[0].callable, functions[1].callable);
    for (function, expected) in functions.iter().zip([10, 20]) {
        let definition = core
            .host_artifacts()
            .publish_callable(&pin, function, &request, &origin, None)
            .await
            .unwrap_or_else(|error| panic!("{expected}: {error:?}"));
        let printed = core
            .host_artifacts()
            .print_workflow(
                &definition.id,
                None,
                SourceFormat::TypeScriptV1,
                Default::default(),
            )
            .await
            .unwrap();
        let roundtrip = publish_source(
            &core,
            &pin,
            &printed.source,
            SourceFormat::TypeScriptV1,
            &request,
            "run",
        )
        .await;
        let roundtrip_env = definition_environment(&core, &pin, &roundtrip, &base).await;
        // The published callable runs under the environment its own
        // defining Link was published in.
        for (definition, env, kind) in [
            (&definition, &publication.env_ref, "original"),
            (&roundtrip, &roundtrip_env, "roundtrip"),
        ] {
            assert_eq!(
                outcome(
                    &core,
                    env,
                    definition,
                    &format!("{expected}-{kind}"),
                    &serde_json::json!({"n": 2}),
                )
                .await,
                Ok(serde_json::json!(expected)),
                "{expected} {kind}\n{}",
                printed.source,
            );
        }
    }

    let ceiling = lash::workflow::document::FunctionValueLimits::CEILING;
    let mut large = functions[0].clone();
    large.captures.insert(
        Name::new("surplus"),
        CaptureValue::Data(Datum::Text("x".repeat(usize::try_from(ceiling).unwrap()))),
    );
    let refused = core
        .host_artifacts()
        .publish_callable(&pin, &large, &request, &origin, None)
        .await
        .unwrap_err();
    assert!(format!("{refused:?}").contains("TooLarge"), "{refused:?}");
    core.shutdown().await.unwrap();
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
async fn unprintable_callable_refuses_publication_without_retaining_definition() {
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
    let (unpublished, _) = core
        .host_artifacts()
        .inspect_callable(&unprintable, &environment, &origin)
        .await
        .unwrap();
    assert!(matches!(
        core.host_artifacts()
            .publish_callable(&pin, &unprintable, &environment, &origin, None)
            .await
            .unwrap_err(),
        lash::EmbedError::WorkflowSource(lash_vm_client::service::SourceServiceError::Print(
            lash::dialect::PrintError::Unprintable { .. }
        ))
    ));
    assert!(matches!(
        core.host_artifacts()
            .definition_graph(&unpublished.id)
            .await
            .unwrap(),
        WorkflowRead::Unavailable(_)
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

#[path = "callable_source/cenote_r14.rs"]
mod cenote_r14;
