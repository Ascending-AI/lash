use super::*;

use lashlang::testing::ast_builders as b;

/// A storage-backed test backend for paths that do not execute engine effects.
pub(crate) async fn sqlite_recording_backend() -> lash_core::Backend {
    lash_conformance::recording_backend_over(sqlite_memory_store_set().await)
}

/// A fresh memory store set's Lashlang artifact store: a storage port a test
/// reaches without an engine.
pub(crate) async fn memory_artifact_store() -> LashlangArtifacts {
    use lash_core_execution::StoreSet;
    LashlangArtifacts::new(sqlite_memory_store_set().await.module_artifacts())
}

thread_local! {
    /// The store sets the running test opened, held as its backends are.
    static HELD_STORE_SETS: std::cell::RefCell<Vec<Arc<lash_sqlite_store::SqliteStoreSet>>> =
        const { std::cell::RefCell::new(Vec::new()) };
}

/// A fresh SQLite memory store set, storage only (no engine), held for the
/// rest of the running test: the twin of `sqlite_recording_backend` for a test that
/// reaches only store ports.
pub(crate) async fn sqlite_memory_store_set() -> Arc<lash_sqlite_store::SqliteStoreSet> {
    let stores = Arc::new(
        lash_sqlite_store::SqliteStoreSet::memory()
            .await
            .expect("open a SQLite memory store set"),
    );
    HELD_STORE_SETS.with(|held| held.borrow_mut().push(Arc::clone(&stores)));
    stores
}

/// The session policy the harness's execution env declares: the worker
/// builds its runtime over it, so it must carry model metadata.
/// A fresh host pin's claim: what a test publishes fixtures under
/// (ADR 0113 §2.6).
pub(crate) fn host_claim() -> lash_core::ReferrerClaim {
    lash_core::ReferrerClaim::unguarded(lash_core::ArtifactReferrer::HostPin(
        lash_core::HostArtifactPin::mint(),
    ))
    .expect("a host pin is unguarded")
}

#[path = "lib_tests/aggregate_child.rs"]
mod aggregate_child;
mod pre_cutover_refusal;
mod second_front_end;

/// `process <name>(<params>) -> <return_ty> { finish <body> }` as a one-process
/// module. ADR 0096 retired the Lashlang front-end, so the fixtures that used
/// to be written as source state their AST instead; the source each one stood
/// for is kept as a comment at the call site.
pub(crate) fn process_module(
    name: &str,
    params: Vec<lashlang::ProcessParam>,
    return_ty: lashlang::TypeExpr,
    body: lashlang::Expr,
) -> lashlang::Program {
    b::module(
        vec![b::process_returning(
            name,
            params,
            return_ty,
            b::finish(body),
        )],
        Vec::new(),
    )
}

/// The labelled workflow witness, whose Lashlang source is spelled out at the
/// call site: labelled statements, an if/else, a `for`, a map and a
/// `while`.
fn labeled_workflow_program() -> lashlang::Program {
    b::program(vec![
        b::labelled(
            b::label("Seed value", None),
            b::assign("value", b::num(1.0)),
        ),
        b::if_else(
            b::bool_lit(true),
            b::block(vec![b::labelled(
                b::label("Selected print", None),
                b::print(b::var("value")),
            )]),
            b::block(vec![b::labelled(
                b::label("Skipped print", None),
                b::print(b::num(0.0)),
            )]),
        ),
        b::for_in(
            "item",
            b::list(vec![b::num(1.0), b::num(2.0)]),
            b::block(vec![b::labelled(
                b::label("For print", None),
                b::print(b::var("item")),
            )]),
        ),
        b::assign(
            "measured",
            b::map(
                b::list(vec![b::num(1.0), b::num(2.0)]),
                "item",
                b::builtin("len", vec![b::list(vec![b::var("item")])]),
            ),
        ),
        b::assign("count", b::num(0.0)),
        b::while_loop(
            b::binary(
                b::var("count"),
                lashlang::CoercingBinaryOp::Less,
                b::num(1.0),
            ),
            b::block(vec![
                b::labelled(b::label("Loop print", None), b::print(b::var("count"))),
                b::assign(
                    "count",
                    b::binary(
                        b::var("count"),
                        lashlang::CoercingBinaryOp::Add,
                        b::num(1.0),
                    ),
                ),
            ]),
        ),
        b::labelled(b::label("Finish value", None), b::finish(b::var("value"))),
    ])
}

/// `process scan(root: str) -> str { finish root }`
fn scan_module() -> lashlang::Program {
    process_module(
        "scan",
        vec![b::param("root", lashlang::TypeExpr::Str)],
        lashlang::TypeExpr::Str,
        b::var("root"),
    )
}

/// `process handler(<first>: <first_ty>, <second>: str) -> bool { finish true }`
fn handler_module(first: &str, first_ty: lashlang::TypeExpr, second: &str) -> lashlang::Program {
    process_module(
        "handler",
        vec![
            b::param(first, first_ty),
            b::param(second, lashlang::TypeExpr::Str),
        ],
        lashlang::TypeExpr::Bool,
        b::bool_lit(true),
    )
}

#[tokio::test(flavor = "current_thread")]
async fn foreground_trace_skeleton_is_derived_from_the_workflow_graph() {
    let source = r#"
        @label(title: "Seed value")
        value = 1
        if true {
          @label(title: "Selected print")
          print value
        } else {
          @label(title: "Skipped print")
          print 0
        }
        for item in [1, 2] {
          @label(title: "For print")
          print item
        }
        measured = [len([item]) for item in [1, 2]]
        count = 0
        while count < 1 {
          @label(title: "Loop print")
          print count
          count = count + 1
        }
        @label(title: "Finish value")
        finish value
    "#;
    let environment = LashlangHostEnvironment::new(
        lashlang::LashlangHostCatalog::new(),
        LashlangAbilities::all(),
    )
    .with_language_features(lashlang::LashlangLanguageFeatures::default().with_label_annotations());
    let program = labeled_workflow_program();
    let output = lashlang::compile_module(lashlang::ModuleCompileRequest {
        source,
        program: program.clone(),
        environment: &environment,
    })
    .expect("labeled workflow compiles");
    // The projection is language-neutral and lives beside the IR (ADR 0100
    // R8): this witness is direct IR — `@label` and a list comprehension have
    // no TypeScript form — and projects with no dialect in the graph.
    let graph = lashlang::workflow_graph_from_program(&program, &lashlang::NoStatementText);
    let trace_graph =
        lashlang::workflow_graph_from_artifact(&output.artifact, &lashlang::NoStatementText);
    let trace_map = trace_lashlang_main_map(&lashlang::workflow_graph_from_artifact(
        &output.artifact,
        &lashlang::NoStatementText,
    ));
    assert_eq!(
        Some(output.artifact.source_identity()),
        trace_graph.source_identity,
        "the trace integration must retain the projector's source identity"
    );

    let container_kinds = graph
        .nodes()
        .filter_map(|node| match &node.kind {
            lashlang::WorkflowNodeKind::Container(lashlang::WorkflowContainer::If { .. }) => {
                Some("if")
            }
            lashlang::WorkflowNodeKind::Container(lashlang::WorkflowContainer::For { .. }) => {
                Some("for")
            }
            lashlang::WorkflowNodeKind::Container(lashlang::WorkflowContainer::While {
                ..
            }) => Some("while"),
            _ => None,
        })
        .collect::<std::collections::BTreeSet<_>>();
    assert_eq!(
        container_kinds,
        std::collections::BTreeSet::from(["for", "if", "while"]),
        "the equality probe must cover every workflow container kind"
    );

    let expected_nodes = graph
        .nodes()
        .filter(|node| !node.execution_sites.is_empty())
        .map(|node| node.id.to_string())
        .collect::<std::collections::BTreeSet<_>>();
    let actual_nodes = trace_map
        .nodes
        .iter()
        .map(|node| node.id.clone())
        .collect::<std::collections::BTreeSet<_>>();

    assert!(!expected_nodes.is_empty());
    assert_eq!(actual_nodes, expected_nodes);
    assert!(
        trace_map
            .nodes
            .iter()
            .any(|node| node.label == "Selected print")
    );
    assert!(
        trace_map
            .nodes
            .iter()
            .any(|node| node.label == "Loop print")
    );
}

#[tokio::test(flavor = "current_thread")]
async fn process_trace_map_is_obtainable_without_an_execution_started_event() {
    let environment = LashlangHostEnvironment::new(
        lashlang::LashlangHostCatalog::new(),
        LashlangAbilities::default(),
    );
    let output = lashlang::compile_module(lashlang::ModuleCompileRequest {
        source: r#"process scan(root: str) -> str { finish root }"#,
        program: scan_module(),
        environment: &environment,
    })
    .expect("process module compiles");
    let store = crate::lib_tests::memory_artifact_store().await;
    store
        .publish_module_artifact(&crate::lib_tests::host_claim(), &output.artifact)
        .await
        .expect("artifact publishes");
    let input = LashlangProcessInput {
        module_ref: output.module_ref.clone(),
        process_ref: output
            .artifact
            .process_ref("scan")
            .expect("scan export")
            .clone(),
        host_requirements_ref: output.host_requirements_ref.clone(),
        process_name: "scan".to_string(),
        args: serde_json::Map::new(),
    };

    let direct = trace_lashlang_process_map(
        &lashlang::workflow_graph_from_artifact(&output.artifact, &lashlang::NoStatementText),
        "scan",
    )
    .expect("direct map");
    let snapshot = trace_lashlang_process_map_snapshot(
        &lash_vm_client::service::Service::default(),
        &store,
        &input,
    )
    .await
    .expect("stored map snapshot");
    assert_eq!(snapshot, direct);
    assert!(!snapshot.nodes.is_empty());

    let mut missing_process = input.clone();
    missing_process.process_name = "missing".to_string();
    assert!(matches!(
        trace_lashlang_process_map_snapshot(&lash_vm_client::service::Service::default(),&store, &missing_process).await,
        Err(TraceLanguageExecutionMapError::ProcessMissing { process_name, .. })
            if process_name == "missing"
    ));

    let missing_hash = lashlang::ContentHash::new("missing-trace-map-artifact");
    let mut missing_artifact = input;
    missing_artifact.module_ref = lashlang::ModuleRef::new(&missing_hash);
    assert!(matches!(
        trace_lashlang_process_map_snapshot(
            &lash_vm_client::service::Service::default(),
            &store,
            &missing_artifact
        )
        .await,
        Err(TraceLanguageExecutionMapError::ArtifactMissing(_))
    ));
}

#[test]
fn missing_tool_binding_is_not_fabricated() {
    let tool = lash_core::ToolDefinition::raw(
        "tool:test/read_file",
        "read_file",
        "read a file",
        lash_core::ToolDefinition::default_input_schema(),
        lash_core::JsonSchema::any().into_value(),
    )
    .expect("valid declared tool schemas");

    let err =
        required_tool_executable(&tool.manifest).expect_err("missing explicit binding should fail");

    assert!(matches!(
        err,
        ToolBindingError::MissingBinding {
            tool,
            binding_key: TOOL_BINDING_KEY,
        } if tool == "read_file"
    ));
}

#[test]
fn tool_catalog_imports_declared_static_schema_types() {
    let tool = lash_core::ToolDefinition::raw(
        "tool:test/read_file",
        "read_file",
        "read a file",
        serde_json::json!({
            "type": "object",
            "properties": {
                "path": { "type": "string" },
                "retries": { "type": "integer" }
            },
            "required": ["path"],
            "additionalProperties": false
        }),
        serde_json::json!({
            "type": "array",
            "items": { "type": ["string", "null"] }
        }),
    )
    .expect("valid declared tool schemas")
    .with_tool_binding(ToolBinding::new(["fs"], "read").with_authority_type("Filesystem"));
    let catalog = lash_core::ToolCatalog::from_tool_definitions(vec![tool]);

    let resources = lashlang_resources_from_tool_catalog(&catalog).expect("tool schemas import");
    let operation = resources
        .resolve_operation("Filesystem", "read")
        .expect("operation is registered");

    assert_eq!(
        operation.input_ty,
        lashlang::TypeExpr::Object(vec![
            lashlang::TypeField {
                name: "path".into(),
                ty: lashlang::TypeExpr::Str,
                optional: false,
            },
            lashlang::TypeField {
                name: "retries".into(),
                ty: lashlang::TypeExpr::Int,
                optional: true,
            },
        ])
    );
    assert_eq!(
        operation.output_ty,
        lashlang::TypeExpr::List(Box::new(lashlang::TypeExpr::union(vec![
            lashlang::TypeExpr::Str,
            lashlang::TypeExpr::Null,
        ])))
    );
}

/// A tool contract can say `process` and `handle`, and must say them right.
///
/// Before `x-lash` a tool could not describe a process at all: both sides of
/// the boundary erased it. Now that it can, a contract that says it wrong is
/// refused outright rather than widened to `Any`, because the keyword is only
/// ever written on purpose.
#[test]
fn tool_contracts_carry_lash_types_and_refuse_malformed_ones() {
    let tool = lash_core::ToolDefinition::raw(
        "tool:test/spawn",
        "spawn",
        "spawn a process",
        serde_json::json!({
            "type": "object",
            "properties": { "target": { "x-lash": { "kind": "process_unknown" } } },
            "required": ["target"],
            "additionalProperties": false
        }),
        serde_json::json!({ "x-lash": { "kind": "handle", "payload": { "type": "string" } } }),
    )
    .expect("valid declared tool schemas")
    .with_tool_binding(ToolBinding::new(["spawner"], "spawn").with_authority_type("Spawner"));
    let catalog = lash_core::ToolCatalog::from_tool_definitions(vec![tool]);
    let resources = lashlang_resources_from_tool_catalog(&catalog).expect("tool schemas import");
    let operation = resources
        .resolve_operation("Spawner", "spawn")
        .expect("operation is registered");
    assert_eq!(
        operation.input_ty,
        lashlang::TypeExpr::Object(vec![lashlang::TypeField {
            name: "target".into(),
            ty: lashlang::TypeExpr::Process(lashlang::ProcessType::unknown()),
            optional: false,
        }])
    );
    assert_eq!(
        operation.output_ty,
        lashlang::TypeExpr::TriggerHandle(Box::new(lashlang::TypeExpr::Str))
    );

    let malformed = lash_core::ToolDefinition::raw(
        "tool:test/broken",
        "broken",
        "a contract that says a lash type wrong",
        serde_json::json!({
            "type": "object",
            "properties": { "target": { "x-lash": { "kind": "process" } } },
            "required": ["target"],
            "additionalProperties": false
        }),
        serde_json::json!({}),
    )
    .expect("valid declared tool schemas")
    .with_tool_binding(ToolBinding::new(["broken"], "run").with_authority_type("Broken"));
    let catalog = lash_core::ToolCatalog::from_tool_definitions(vec![malformed]);
    let error = lashlang_resources_from_tool_catalog(&catalog)
        .expect_err("a malformed lash type refuses the whole contract");
    assert!(
        error.to_string().contains("x-lash"),
        "the diagnostic must name the keyword that is wrong: {error}"
    );
}

#[test]
fn from_input_schema_tool_imports_contract_marker_and_default() {
    let tool = lash_core::ToolDefinition::raw(
        "tool:test/generate",
        "generate",
        "generate typed output",
        serde_json::json!({
            "type": "object",
            "properties": { "schema": {} },
            "required": ["schema"],
            "additionalProperties": false
        }),
        serde_json::json!({ "type": "string" }),
    )
    .expect("valid declared tool schemas")
    .with_output_from_input_schema(
        "schema",
        Some(
            lash_sansio::JsonSchema::admit(serde_json::json!({ "type": "string" }))
                .expect("valid output default schema"),
        ),
    )
    .with_tool_binding(ToolBinding::new(["generate"], "run").with_authority_type("Generator"));
    let catalog = lash_core::ToolCatalog::from_tool_definitions(vec![tool]);

    let resources = lashlang_resources_from_tool_catalog(&catalog).expect("tool schemas import");
    let operation = resources
        .resolve_operation("Generator", "run")
        .expect("operation is registered");

    assert_eq!(
        operation.input_ty,
        lashlang::TypeExpr::Object(vec![lashlang::TypeField {
            name: "schema".into(),
            ty: lashlang::TypeExpr::Any,
            optional: false,
        }])
    );
    assert_eq!(operation.output_ty, lashlang::TypeExpr::Any);
    assert_eq!(
        operation.output_from_input,
        Some(lashlang::OutputFromInputBinding {
            input_field: "schema".to_string(),
            default_schema: Some(lashlang::TypeExpr::Str),
        })
    );
}

#[test]
fn representable_type_schema_subset_round_trips() {
    let types = [
        lashlang::TypeExpr::Any,
        lashlang::TypeExpr::Str,
        lashlang::TypeExpr::Int,
        lashlang::TypeExpr::Float,
        lashlang::TypeExpr::Bool,
        lashlang::TypeExpr::Null,
        lashlang::TypeExpr::Enum(vec!["fast".into(), "safe".into()]),
        lashlang::TypeExpr::List(Box::new(lashlang::TypeExpr::Str)),
        lashlang::TypeExpr::union(vec![lashlang::TypeExpr::Str, lashlang::TypeExpr::Null]),
    ];

    for expected in types {
        let schema = lashlang_type_expr_schema(&expected);
        assert_eq!(
            lashlang::json_schema_to_type_expr(&schema).expect("an exported schema imports"),
            expected
        );
    }
}

#[test]
fn dotted_operation_names_are_rejected() {
    let tool = lash_core::ToolDefinition::raw(
        "tool:test/update_plan",
        "update_plan",
        "update a plan",
        lash_core::ToolDefinition::default_input_schema(),
        lash_core::JsonSchema::any().into_value(),
    )
    .expect("valid declared tool schemas")
    .with_tool_binding(ToolBinding::new(["tools"], "update.plan"));

    let err = required_tool_executable(&tool.manifest)
        .expect_err("dotted operation cannot compile as one Lashlang operation");

    assert!(matches!(
        err,
        ToolBindingError::InvalidIdentifier {
            tool,
            part: "operation name",
            value,
        } if tool == "update_plan" && value == "update.plan"
    ));
}

#[test]
fn empty_operation_names_render_as_empty_invalid_identifiers() {
    let tool = lash_core::ToolDefinition::raw(
        "tool:test/empty_operation",
        "empty_operation",
        "an operation with an empty name",
        lash_core::ToolDefinition::default_input_schema(),
        lash_core::JsonSchema::any().into_value(),
    )
    .expect("valid declared tool schemas")
    .with_tool_binding(ToolBinding::new(["tools"], ""));

    let err = required_tool_executable(&tool.manifest)
        .expect_err("an empty operation name cannot compile as a Lashlang operation");

    assert_eq!(
        err.to_string(),
        "tool `empty_operation` has invalid tool-binding operation name `<empty>`"
    );
}

#[test]
fn manifest_tool_binding_accessor_reports_absent_valid_and_malformed() {
    let mut manifest = lash_core::ToolDefinition::raw(
        "tool:test/read_file",
        "read_file",
        "read a file",
        lash_core::ToolDefinition::default_input_schema(),
        lash_core::JsonSchema::any().into_value(),
    )
    .expect("valid declared tool schemas")
    .manifest;
    assert_eq!(manifest.tool_binding().expect("absent binding"), None);

    manifest.bindings.insert(
        TOOL_BINDING_KEY.to_string(),
        serde_json::json!({
            "module_path": ["fs"],
            "operation": "read"
        }),
    );
    let binding = manifest
        .tool_binding()
        .expect("valid binding")
        .expect("present binding");
    assert_eq!(binding.module_path, vec!["fs"]);
    assert_eq!(binding.operation.as_deref(), Some("read"));

    manifest.bindings.insert(
        TOOL_BINDING_KEY.to_string(),
        serde_json::json!({ "module_path": "fs" }),
    );
    assert!(manifest.tool_binding().is_err());
}

#[test]
fn remote_grant_tool_binding_accessor_reports_absent_valid_and_malformed() {
    let grant = remote_tool_grant("read_file");
    assert_eq!(grant.tool_binding().expect("absent binding"), None);

    let grant = grant.with_tool_binding(ToolBinding::new(["fs"], "read"));
    let binding = grant
        .tool_binding()
        .expect("valid binding")
        .expect("present binding");
    assert_eq!(binding.module_path, vec!["fs"]);
    assert_eq!(binding.operation.as_deref(), Some("read"));

    let mut malformed = grant;
    malformed.bindings.insert(
        TOOL_BINDING_KEY.to_string(),
        serde_json::json!({ "module_path": "fs" }),
    );
    assert!(malformed.tool_binding().is_err());
}

#[tokio::test(flavor = "current_thread")]
async fn prepared_start_replays_same_start_key_without_duplicate_child_identity() {
    let store = crate::lib_tests::memory_artifact_store().await;
    let environment = LashlangHostEnvironment::new(
        lashlang::LashlangHostCatalog::new(),
        LashlangAbilities::default(),
    );
    let output = lashlang::compile_module(lashlang::ModuleCompileRequest {
        source: r#"process scan(root: str) -> str { finish root }"#,
        program: scan_module(),
        environment: &environment,
    })
    .expect("module compiles");
    store
        .publish_module_artifact(&crate::lib_tests::host_claim(), &output.artifact)
        .await
        .expect("module publishes");
    let artifact_store: LashlangArtifacts = store;
    let site = test_start_site("child_process:scan", 1);

    let first = prepare_lashlang_process_start(
        &lash_vm_client::service::Service::default(),
        artifact_store.clone(),
        Some("parent:root"),
        test_process_start(&output, site.clone(), "."),
        lash_core::ProcessOriginator::host(),
        lash_core::LifetimeDecision::Detached,
    )
    .await
    .expect("first start prepares");
    let replayed = prepare_lashlang_process_start(
        &lash_vm_client::service::Service::default(),
        artifact_store.clone(),
        Some("parent:root"),
        test_process_start(&output, site.clone(), "."),
        lash_core::ProcessOriginator::host(),
        lash_core::LifetimeDecision::Detached,
    )
    .await
    .expect("replayed start prepares");
    let sibling = prepare_lashlang_process_start(
        &lash_vm_client::service::Service::default(),
        artifact_store.clone(),
        Some("parent:root:2"),
        test_process_start(&output, test_start_site("child_process:scan", 2), "."),
        lash_core::ProcessOriginator::host(),
        lash_core::LifetimeDecision::Detached,
    )
    .await
    .expect("sibling start prepares");

    assert_eq!(first.request.start_key(), replayed.request.start_key());
    assert_eq!(first.request.identity, replayed.request.identity);
    assert_ne!(first.request.start_key(), sibling.request.start_key());
}

#[tokio::test(flavor = "current_thread")]
async fn prepared_start_checks_indirect_process_identity_against_named_signature() {
    let store = crate::lib_tests::memory_artifact_store().await;
    let environment = LashlangHostEnvironment::new(
        lashlang::LashlangHostCatalog::new(),
        LashlangAbilities::default(),
    );
    let matching = lashlang::compile_module(lashlang::ModuleCompileRequest {
        source: "process handler(event: str, other: str) -> bool { finish true }",
        program: handler_module("event", lashlang::TypeExpr::Str, "other"),
        environment: &environment,
    })
    .expect("matching handler compiles");
    let mismatching = lashlang::compile_module(lashlang::ModuleCompileRequest {
        source: "process handler(payload: str, other: str) -> bool { finish true }",
        program: handler_module("payload", lashlang::TypeExpr::Str, "other"),
        environment: &environment,
    })
    .expect("mismatching handler compiles");
    let wrong_type = lashlang::compile_module(lashlang::ModuleCompileRequest {
        source: "process handler(event: int, other: str) -> bool { finish true }",
        program: handler_module("event", lashlang::TypeExpr::Int, "other"),
        environment: &environment,
    })
    .expect("wrong-type handler compiles");
    let wrong_order = lashlang::compile_module(lashlang::ModuleCompileRequest {
        source: "process handler(other: str, event: str) -> bool { finish true }",
        program: handler_module("other", lashlang::TypeExpr::Str, "event"),
        environment: &environment,
    })
    .expect("wrong-order handler compiles");
    let receiver = lashlang::compile_module(lashlang::ModuleCompileRequest {
        source: "process install(envelope: { handler: Process<(event: str, other: str), bool> }) -> bool { finish true }",
        program: b::module(
            vec![b::process_returning(
                "install",
                vec![b::param(
                    "envelope",
                    lashlang::TypeExpr::Object(vec![b::type_field(
                        "handler",
                        b::process_type(
                            vec![
                                b::param("event", lashlang::TypeExpr::Str),
                                b::param("other", lashlang::TypeExpr::Str),
                            ],
                            lashlang::TypeExpr::Bool,
                        ),
                        false,
                    )]),
                )],
                lashlang::TypeExpr::Bool,
                b::finish(b::bool_lit(true)),
            )],
            Vec::new(),
        ),
        environment: &environment,
    })
    .expect("receiver compiles");
    let owner = crate::lib_tests::host_claim();
    for artifact in [
        &matching.artifact,
        &mismatching.artifact,
        &wrong_type.artifact,
        &wrong_order.artifact,
        &receiver.artifact,
    ] {
        store
            .publish_module_artifact(&owner, artifact)
            .await
            .expect("module publishes");
    }
    let artifact_store: LashlangArtifacts = store.clone();

    let start_with = |definition: lashlang::ProcessDefinitionIdentity| {
        let mut envelope = lashlang::Record::new();
        envelope.insert(
            "handler".to_string(),
            lashlang::from_json(definition.to_process_value()),
        );
        let mut args = lashlang::Record::new();
        args.insert(
            "envelope".to_string(),
            lashlang::Value::Record(Arc::new(envelope)),
        );
        lashlang::ProcessStart {
            module_ref: receiver.module_ref.clone(),
            process_ref: receiver.artifact.process_ref("install").unwrap().clone(),
            host_requirements_ref: receiver.host_requirements_ref.clone(),
            start_site: test_start_site("child_process:install", 1),
            process_name: "install".to_string(),
            args,
        }
    };

    prepare_lashlang_process_start(
        &lash_vm_client::service::Service::default(),
        artifact_store.clone(),
        Some("parent:root"),
        start_with(
            lashlang::ProcessDefinitionIdentity::from_artifact_export(
                &matching.artifact,
                "handler",
            )
            .unwrap(),
        ),
        lash_core::ProcessOriginator::host(),
        lash_core::LifetimeDecision::Detached,
    )
    .await
    .expect("matching immutable signature passes");

    let error = prepare_lashlang_process_start(
        &lash_vm_client::service::Service::default(),
        artifact_store.clone(),
        Some("parent:root"),
        start_with(
            lashlang::ProcessDefinitionIdentity::from_artifact_export(
                &mismatching.artifact,
                "handler",
            )
            .unwrap(),
        ),
        lash_core::ProcessOriginator::host(),
        lash_core::LifetimeDecision::Detached,
    )
    .await
    .expect_err("different outer parameter name must fail before registration");
    assert!(matches!(
        error,
        LashlangRuntimeError::InvalidProcessArgument { ref path, .. }
            if path == "envelope.handler"
    ));
    assert!(error.to_string().contains("payload"), "{error}");

    for (description, definition) in [
        (
            "different parameter type",
            lashlang::ProcessDefinitionIdentity::from_artifact_export(
                &wrong_type.artifact,
                "handler",
            )
            .unwrap(),
        ),
        (
            "different parameter order at the same arity",
            lashlang::ProcessDefinitionIdentity::from_artifact_export(
                &wrong_order.artifact,
                "handler",
            )
            .unwrap(),
        ),
    ] {
        let error = prepare_lashlang_process_start(
            &lash_vm_client::service::Service::default(),
            artifact_store.clone(),
            Some("parent:root"),
            start_with(definition),
            lash_core::ProcessOriginator::host(),
            lash_core::LifetimeDecision::Detached,
        )
        .await
        .expect_err(description);
        assert!(matches!(
            error,
            LashlangRuntimeError::InvalidProcessArgument { ref path, .. }
                if path == "envelope.handler"
        ));
    }

    let valid =
        lashlang::ProcessDefinitionIdentity::from_artifact_export(&matching.artifact, "handler")
            .unwrap();
    let wrong_ref = lashlang::ProcessDefinitionIdentity::new(
        valid.module_ref,
        valid.host_requirements_ref,
        lashlang::ProcessRef::new(lashlang::ContentHash::new("wrong-process"), 0),
        valid.process_name,
    );
    let error = prepare_lashlang_process_start(
        &lash_vm_client::service::Service::default(),
        artifact_store.clone(),
        Some("parent:root"),
        start_with(wrong_ref),
        lash_core::ProcessOriginator::host(),
        lash_core::LifetimeDecision::Detached,
    )
    .await
    .expect_err("identity with a different process ref must fail");
    assert!(matches!(
        error,
        LashlangRuntimeError::InvalidProcessArgument { ref path, .. }
            if path == "envelope.handler"
    ));
}

#[tokio::test(flavor = "current_thread")]
async fn process_signature_union_accepts_a_later_matching_nonprocess_arm() {
    let store = crate::lib_tests::memory_artifact_store().await;
    let environment = LashlangHostEnvironment::new(
        lashlang::LashlangHostCatalog::new(),
        LashlangAbilities::default(),
    );
    let receiver = lashlang::compile_module(lashlang::ModuleCompileRequest {
        source: "process install(handler: Process<(event: str), bool> | str) -> bool { finish true }",
        program: process_module(
            "install",
            vec![b::param(
                "handler",
                lashlang::TypeExpr::union(vec![
                    b::process_type(
                        vec![b::param("event", lashlang::TypeExpr::Str)],
                        lashlang::TypeExpr::Bool,
                    ),
                    lashlang::TypeExpr::Str,
                ]),
            )],
            lashlang::TypeExpr::Bool,
            b::bool_lit(true),
        ),
        environment: &environment,
    })
    .expect("union receiver compiles");
    store
        .publish_module_artifact(&crate::lib_tests::host_claim(), &receiver.artifact)
        .await
        .expect("module publishes");
    let mut args = lashlang::Record::new();
    args.insert(
        "handler".to_string(),
        lashlang::Value::String("fallback".into()),
    );
    let start = lashlang::ProcessStart {
        module_ref: receiver.module_ref.clone(),
        process_ref: receiver.artifact.process_ref("install").unwrap().clone(),
        host_requirements_ref: receiver.host_requirements_ref.clone(),
        start_site: test_start_site("child_process:install", 1),
        process_name: "install".to_string(),
        args,
    };
    let artifact_store: LashlangArtifacts = store;

    prepare_lashlang_process_start(
        &lash_vm_client::service::Service::default(),
        artifact_store,
        Some("parent:root"),
        start,
        lash_core::ProcessOriginator::host(),
        lash_core::LifetimeDecision::Detached,
    )
    .await
    .expect("later string union arm accepts the value");
}

/// Masking a path over the catalog's memoized import builds exactly the
/// environment of the catalog and surface without the masked members — down
/// to the serialized bytes — and leaves the unmasked environment intact.
#[test]
fn masked_host_environment_is_the_environment_without_the_masked_members() {
    let tool = |id: &str, module: &str, operation: &str, authority: &str| {
        lash_core::ToolDefinition::raw(
            format!("tool:test/{id}"),
            id,
            format!("{id} fixture"),
            serde_json::json!({
                "type": "object",
                "properties": { "path": { "type": "string" } },
                "required": ["path"]
            }),
            serde_json::json!({ "type": "string" }),
        )
        .expect("valid declared tool schemas")
        .with_tool_binding(ToolBinding::new([module], operation).with_authority_type(authority))
    };
    let unmasked_tools = vec![
        tool("fs_write", "fs", "write", "Filesystem"),
        tool("mirror_read", "mirror", "read", "Filesystem"),
    ];
    let masked_tools = [
        tool("fs_read", "fs", "read", "Filesystem"),
        tool("web_fetch", "web", "fetch", "Web"),
    ];
    let catalog = lash_core::ToolCatalog::from_tool_definitions(
        masked_tools
            .into_iter()
            .chain(unmasked_tools.clone())
            .collect(),
    );
    let masked = [
        "fs.read",
        "web.fetch",
        "tools.lookup",
        "absent.path",
        "undotted",
    ]
    .map(String::from)
    .into_iter()
    .collect::<BTreeSet<_>>();
    let surface = |operations: &[&str]| {
        LashlangSurface::default()
            .with_resources(LashlangHostCatalog::tool_default(
                operations.iter().copied(),
            ))
            .expect("surface resources are unique")
    };

    let environment = surface(&["lookup", "list"])
        .host_environment_masking(&catalog, &masked)
        .expect("masked environment builds");
    let expected = surface(&["list"])
        .host_environment(&lash_core::ToolCatalog::from_tool_definitions(
            unmasked_tools,
        ))
        .expect("environment without the masked members builds");
    assert_eq!(environment, expected);
    assert_eq!(
        serde_json::to_string(&environment).expect("environment serializes"),
        serde_json::to_string(&expected).expect("environment serializes"),
    );

    let unmasked = surface(&["lookup", "list"])
        .host_environment(&catalog)
        .expect("unmasked environment builds");
    assert!(unmasked.resources.provides_module_operation("fs", "read"));
    assert_eq!(
        unmasked,
        surface(&["lookup", "list"])
            .host_environment(&catalog.filtered(|_| true))
            .expect("a fresh import of the same members builds"),
        "masking never edits the catalog's memoized import"
    );
}

#[test]
fn plugin_extensions_return_typed_catalog_conflicts() {
    let contributions = ["first", "second"].map(|_| {
        lash_core::facade_support::PluginExtensionContribution::new(
            LASHLANG_SURFACE_EXTENSION_ID,
            LashlangSurfaceContribution::new(
                LashlangAbilities::default(),
                LashlangLanguageFeatures::default(),
                LashlangHostCatalog::tool_default(["lookup"]),
            ),
        )
        .expect("extension payload serializes")
    });
    let extensions = lash_core::PluginExtensions::from_contributions(contributions);

    assert!(matches!(
        LashlangSurface::default().with_plugin_extensions(&extensions),
        Err(LashlangRuntimeError::HostCatalog {
            source: lashlang::LashlangHostCatalogError::ConflictingModuleOperation {
                module,
                operation,
                ..
            }
        }) if module == "tools" && operation == "lookup"
    ));
}

fn remote_tool_grant(name: &str) -> lash_remote_protocol::RemoteToolGrant {
    lash_remote_protocol::RemoteToolGrant {
        id: format!("remote-tool:{name}"),
        name: name.to_string(),
        description: String::new(),
        input_schema: lash_remote_protocol::RemoteSchemaContract {
            canonical: lash_core::JsonSchema::admit(
                lash_core::ToolDefinition::default_input_schema(),
            )
            .unwrap(),
            projection: lash_remote_protocol::RemoteSchemaProjectionPolicy::default(),
        },
        output_schema: lash_remote_protocol::RemoteSchemaContract::default(),
        output_contract: lash_remote_protocol::RemoteToolOutputContract::Static,
        examples: Vec::new(),
        argument_projection: None,
        execution_policy: None,
        bindings: Default::default(),
    }
}

fn test_start_site(node_id: &str, occurrence: u64) -> lashlang::LashlangExecutionCallSite {
    lashlang::LashlangExecutionCallSite {
        site: lashlang::LashlangExecutionSite {
            node_id: node_id.to_string(),
            node_kind: lash_sansio::ExecutionNodeKind::Call,
            label: "start scan".to_string(),
            branch: None,
            workflow_site: lashlang::WorkflowExecutionSite::new(
                "process:scan",
                [],
                lash_sansio::ExecutionNodeKind::Call,
                "start scan",
            ),
        },
        occurrence,
    }
}

fn test_process_start(
    output: &lashlang::ModuleCompileOutput,
    start_site: lashlang::LashlangExecutionCallSite,
    root: &str,
) -> lashlang::ProcessStart {
    let mut args = lashlang::Record::new();
    args.insert("root".to_string(), lashlang::Value::String(root.into()));
    lashlang::ProcessStart {
        module_ref: output.module_ref.clone(),
        process_ref: output
            .artifact
            .process_ref("scan")
            .expect("scan process export")
            .clone(),
        host_requirements_ref: output.host_requirements_ref.clone(),
        start_site,
        process_name: "scan".to_string(),
        args,
    }
}

#[tokio::test(flavor = "current_thread")]
async fn nested_signal_admission_registers_each_process_payload_independently() {
    let mut catalog = lashlang::LashlangHostCatalog::new();
    for (operation, ty) in [
        ("accept_str", lashlang::TypeExpr::Str),
        ("accept_int", lashlang::TypeExpr::Int),
    ] {
        catalog
            .add_module_operation(
                ["tools"],
                "Tools",
                operation,
                operation,
                ty,
                lashlang::TypeExpr::Null,
            )
            .expect("unique test operation");
    }
    let wait = |name: &str, operation: &str| {
        b::unwrap(b::await_expr(b::module_call(
            &["tools"],
            operation,
            vec![b::wait_signal(name)],
        )))
    };
    let environment = LashlangHostEnvironment::new(catalog, LashlangAbilities::all());
    let output = lashlang::compile_module(lashlang::ModuleCompileRequest {
        source: "nested signal admission fixture",
        program: b::program(vec![b::assign(
            "parent",
            b::process_literal(
                vec![],
                b::block(vec![
                    wait("daily", "accept_str"),
                    b::assign(
                        "first",
                        b::process_literal(vec![], wait("daily", "accept_int")),
                    ),
                    b::assign(
                        "second",
                        b::process_literal(vec![], wait("second", "accept_str")),
                    ),
                    wait("after", "accept_str"),
                ]),
            ),
        )]),
        environment: &environment,
    })
    .expect("nested signal module compiles");
    let store = memory_artifact_store().await;
    store
        .publish_module_artifact(&host_claim(), &output.artifact)
        .await
        .expect("publish corrected definition");
    let processes = output
        .artifact
        .ir()
        .declarations
        .iter()
        .filter_map(|decl| match decl {
            lashlang::Declaration::Process(process) => Some(process),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(processes.len(), 3);
    for (process, expected) in processes.into_iter().zip([
        vec![("signal.daily", "integer")],
        vec![("signal.second", "string")],
        vec![("signal.after", "string"), ("signal.daily", "string")],
    ]) {
        let event_types =
            lashlang_process_signal_event_types(process).expect("valid signal payload schemas");
        let declarations = event_types
            .iter()
            .map(|event| {
                (
                    event.name.as_str(),
                    event.payload_schema.as_value()["type"]
                        .as_str()
                        .expect("typed payload"),
                )
            })
            .collect::<Vec<_>>();
        assert_eq!(declarations, expected, "registration for {}", process.name);
        let prepared = prepare_lashlang_process_start(
            &lash_vm_client::service::Service::default(),
            store.clone(),
            None,
            lashlang::ProcessStart {
                module_ref: output.module_ref.clone(),
                process_ref: output
                    .artifact
                    .process_ref(process.name.as_str())
                    .expect("exported process")
                    .clone(),
                host_requirements_ref: output.host_requirements_ref.clone(),
                start_site: test_start_site("nested-signal-admission", 1),
                process_name: process.name.to_string(),
                args: lashlang::Record::new(),
            },
            lash_core::ProcessOriginator::host(),
            lash_core::LifetimeDecision::Detached,
        )
        .await
        .expect("prepare exported process admission");
        let admitted = prepared
            .request
            .event_types
            .iter()
            .filter(|event| event.name.starts_with("signal."))
            .collect::<Vec<_>>();
        assert_eq!(
            admitted,
            event_types.iter().collect::<Vec<_>>(),
            "admission pins this process's signals"
        );
    }
}

#[tokio::test(flavor = "current_thread")]
async fn nested_process_arguments_reject_forged_aliases_and_try_later_union_arms() {
    use lash_core_execution::StoreSet;
    super::testing::nested_process_arguments_reject_forged_aliases_and_try_later_union_arms(
        sqlite_memory_store_set().await.module_artifacts(),
    )
    .await;
}
