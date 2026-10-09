use super::*;

use lash_vm::testing::ast_builders as b;

/// A fresh memory store set's Lash VM artifact store: a storage port a test
/// reaches without an engine.
pub(crate) async fn memory_artifact_store() -> LashVmArtifacts {
    use lash_core_execution::StoreSet;
    LashVmArtifacts::new(sqlite_memory_store_set().await.module_artifacts())
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
mod second_front_end;

/// `process <name>(<params>) -> <return_ty> { finish <body> }` as a one-process
/// module. ADR 0096 retired the Lash VM front-end, so the fixtures that used
/// to be written as source state their AST instead; the source each one stood
/// for is kept as a comment at the call site.
pub(crate) fn process_module(
    name: &str,
    params: Vec<lash_vm::ProcessParam>,
    return_ty: lash_vm::TypeExpr,
    body: lash_vm::Expr,
) -> lash_vm::Program {
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

/// The labelled workflow witness, whose Lash VM source is spelled out at the
/// call site: labelled statements, an if/else, a `for`, a map and a
/// `while`.
fn labeled_workflow_program() -> lash_vm::Program {
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
                lash_vm::CoercingBinaryOp::Less,
                b::num(1.0),
            ),
            b::block(vec![
                b::labelled(b::label("Loop print", None), b::print(b::var("count"))),
                b::assign(
                    "count",
                    b::binary(b::var("count"), lash_vm::CoercingBinaryOp::Add, b::num(1.0)),
                ),
            ]),
        ),
        b::labelled(b::label("Finish value", None), b::finish(b::var("value"))),
    ])
}

/// `process scan(root: str) -> str { finish root }`
fn scan_module() -> lash_vm::Program {
    process_module(
        "scan",
        vec![b::param("root", lash_vm::TypeExpr::Str)],
        lash_vm::TypeExpr::Str,
        b::var("root"),
    )
}

/// `process handler(<first>: <first_ty>, <second>: str) -> bool { finish true }`
fn handler_module(first: &str, first_ty: lash_vm::TypeExpr, second: &str) -> lash_vm::Program {
    process_module(
        "handler",
        vec![
            b::param(first, first_ty),
            b::param(second, lash_vm::TypeExpr::Str),
        ],
        lash_vm::TypeExpr::Bool,
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
    let environment = LashVmHostEnvironment::new(lash_vm::LashVmHostCatalog::new())
        .with_language_features(
            lash_vm::LashVmLanguageFeatures::default().with_label_annotations(),
        );
    let program = labeled_workflow_program();
    let output = lash_vm::compile_module(lash_vm::ModuleCompileRequest {
        source,
        program: program.clone(),
        environment: &environment,
    })
    .expect("labeled workflow compiles");
    // The projection is language-neutral and lives beside the IR (ADR 0100
    // R8): this witness is direct IR — `@label` and a list comprehension have
    // no TypeScript form — and projects with no dialect in the graph.
    let graph = lash_vm::workflow_graph_from_program(&program);
    let trace_graph = lash_vm::workflow_graph_from_artifact(&output.artifact);
    let trace_map =
        trace_lashlang_main_map(&lash_vm::workflow_graph_from_artifact(&output.artifact));
    assert_eq!(
        Some(output.artifact.source_identity()),
        trace_graph.source_identity,
        "the trace integration must retain the projector's source identity"
    );

    let container_kinds = graph
        .nodes()
        .filter_map(|node| match &node.kind {
            lash_vm::WorkflowNodeKind::Container(lash_vm::WorkflowContainer::If { .. }) => {
                Some("if")
            }
            lash_vm::WorkflowNodeKind::Container(lash_vm::WorkflowContainer::For { .. }) => {
                Some("for")
            }
            lash_vm::WorkflowNodeKind::Container(lash_vm::WorkflowContainer::While { .. }) => {
                Some("while")
            }
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

#[test]
fn missing_tool_binding_is_not_fabricated() {
    let tool = lash_core::ToolDefinition::raw(
        "tool:test/read_file",
        "read_file",
        "read a file",
        lash_core::ToolDefinition::default_input_schema(),
        lash_core::JsonSchema::any().into_value(),
    )
    .expect("valid declared tool schemas")
    .with_execution(std::time::Duration::from_secs(120));

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
    .with_execution(std::time::Duration::from_secs(120))
    .with_tool_binding(ToolBinding::new(["fs"], "read").with_authority_type("Filesystem"));
    let catalog = lash_core::ToolCatalog::from_tool_definitions(vec![tool]);

    let resources = lash_vm_resources_from_tool_catalog(&catalog).expect("tool schemas import");
    let operation = resources
        .resolve_operation("Filesystem", "read")
        .expect("operation is registered");

    assert_eq!(
        operation.input_ty,
        lash_vm::TypeExpr::Object(vec![
            lash_vm::TypeField {
                name: "path".into(),
                ty: lash_vm::TypeExpr::Str,
                optional: false,
            },
            lash_vm::TypeField {
                name: "retries".into(),
                ty: lash_vm::TypeExpr::Int,
                optional: true,
            },
        ])
    );
    assert_eq!(
        operation.output_ty,
        lash_vm::TypeExpr::List(Box::new(lash_vm::TypeExpr::union(vec![
            lash_vm::TypeExpr::Str,
            lash_vm::TypeExpr::Null,
        ])))
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
    .with_execution(std::time::Duration::from_secs(120))
    .with_output_from_input_schema(
        "schema",
        Some(
            lash_sansio::JsonSchema::admit(serde_json::json!({ "type": "string" }))
                .expect("valid output default schema"),
        ),
    )
    .with_tool_binding(ToolBinding::new(["generate"], "run").with_authority_type("Generator"));
    let catalog = lash_core::ToolCatalog::from_tool_definitions(vec![tool]);

    let resources = lash_vm_resources_from_tool_catalog(&catalog).expect("tool schemas import");
    let operation = resources
        .resolve_operation("Generator", "run")
        .expect("operation is registered");

    assert_eq!(
        operation.input_ty,
        lash_vm::TypeExpr::Object(vec![lash_vm::TypeField {
            name: "schema".into(),
            ty: lash_vm::TypeExpr::Any,
            optional: false,
        }])
    );
    assert_eq!(operation.output_ty, lash_vm::TypeExpr::Any);
    assert_eq!(
        operation.output_from_input,
        Some(lash_vm::OutputFromInputBinding {
            input_field: "schema".to_string(),
            default_schema: Some(lash_vm::TypeExpr::Str),
        })
    );
}

#[test]
fn representable_type_schema_subset_round_trips() {
    let types = [
        lash_vm::TypeExpr::Any,
        lash_vm::TypeExpr::Str,
        lash_vm::TypeExpr::Int,
        lash_vm::TypeExpr::Float,
        lash_vm::TypeExpr::Bool,
        lash_vm::TypeExpr::Null,
        lash_vm::TypeExpr::Enum(vec!["fast".into(), "safe".into()]),
        lash_vm::TypeExpr::List(Box::new(lash_vm::TypeExpr::Str)),
        lash_vm::TypeExpr::union(vec![lash_vm::TypeExpr::Str, lash_vm::TypeExpr::Null]),
    ];

    for expected in types {
        let schema = lash_vm_type_expr_schema(&expected);
        assert_eq!(
            lash_vm::json_schema_to_type_expr(&schema).expect("an exported schema imports"),
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
    .with_execution(std::time::Duration::from_secs(120))
    .with_tool_binding(ToolBinding::new(["tools"], "update.plan"));

    let err = required_tool_executable(&tool.manifest)
        .expect_err("dotted operation cannot compile as one Lash VM operation");

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
    .with_execution(std::time::Duration::from_secs(120))
    .with_tool_binding(ToolBinding::new(["tools"], ""));

    let err = required_tool_executable(&tool.manifest)
        .expect_err("an empty operation name cannot compile as a Lash VM operation");

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
    .with_execution(std::time::Duration::from_secs(120))
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

#[tokio::test(flavor = "current_thread")]
async fn prepared_start_replays_same_start_key_without_duplicate_child_identity() {
    let store = crate::lib_tests::memory_artifact_store().await;
    let environment = LashVmHostEnvironment::new(lash_vm::LashVmHostCatalog::new());
    let output = lash_vm::compile_module(lash_vm::ModuleCompileRequest {
        source: r#"process scan(root: str) -> str { finish root }"#,
        program: scan_module(),
        environment: &environment,
    })
    .expect("module compiles");
    store
        .publish_module_artifact(&crate::lib_tests::host_claim(), &output.artifact)
        .await
        .expect("module publishes");
    let artifact_store: LashVmArtifacts = store;
    let site = test_start_site("child_process:scan", 1);

    let first = prepare_lash_vm_process_start(
        &lash_vm_client::service::Service::default(),
        artifact_store.clone(),
        Some("parent:root"),
        test_process_start(&output, site.clone(), "."),
        lash_core::ProcessOriginator::host(),
        lash_core::LifetimeDecision::Detached,
    )
    .await
    .expect("first start prepares");
    let replayed = prepare_lash_vm_process_start(
        &lash_vm_client::service::Service::default(),
        artifact_store.clone(),
        Some("parent:root"),
        test_process_start(&output, site.clone(), "."),
        lash_core::ProcessOriginator::host(),
        lash_core::LifetimeDecision::Detached,
    )
    .await
    .expect("replayed start prepares");
    let sibling = prepare_lash_vm_process_start(
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
    let environment = LashVmHostEnvironment::new(lash_vm::LashVmHostCatalog::new());
    let matching = lash_vm::compile_module(lash_vm::ModuleCompileRequest {
        source: "process handler(event: str, other: str) -> bool { finish true }",
        program: handler_module("event", lash_vm::TypeExpr::Str, "other"),
        environment: &environment,
    })
    .expect("matching handler compiles");
    let mismatching = lash_vm::compile_module(lash_vm::ModuleCompileRequest {
        source: "process handler(payload: str, other: str) -> bool { finish true }",
        program: handler_module("payload", lash_vm::TypeExpr::Str, "other"),
        environment: &environment,
    })
    .expect("mismatching handler compiles");
    let wrong_type = lash_vm::compile_module(lash_vm::ModuleCompileRequest {
        source: "process handler(event: int, other: str) -> bool { finish true }",
        program: handler_module("event", lash_vm::TypeExpr::Int, "other"),
        environment: &environment,
    })
    .expect("wrong-type handler compiles");
    let wrong_order = lash_vm::compile_module(lash_vm::ModuleCompileRequest {
        source: "process handler(other: str, event: str) -> bool { finish true }",
        program: handler_module("other", lash_vm::TypeExpr::Str, "event"),
        environment: &environment,
    })
    .expect("wrong-order handler compiles");
    let receiver = lash_vm::compile_module(lash_vm::ModuleCompileRequest {
        source: "process install(envelope: { handler: Process<(event: str, other: str), bool> }) -> bool { finish true }",
        program: b::module(
            vec![b::process_returning(
                "install",
                vec![b::param(
                    "envelope",
                    lash_vm::TypeExpr::Object(vec![b::type_field(
                        "handler",
                        b::process_type(
                            vec![
                                b::param("event", lash_vm::TypeExpr::Str),
                                b::param("other", lash_vm::TypeExpr::Str),
                            ],
                            lash_vm::TypeExpr::Bool,
                        ),
                        false,
                    )]),
                )],
                lash_vm::TypeExpr::Bool,
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
    let artifact_store: LashVmArtifacts = store.clone();

    let start_with = |definition: lash_vm::ProcessDefinitionIdentity| {
        let mut envelope = lash_vm::Record::new();
        envelope.insert(
            "handler".to_string(),
            lash_vm::from_json(definition.to_process_value()),
        );
        let mut args = lash_vm::Record::new();
        args.insert(
            "envelope".to_string(),
            lash_vm::Value::Record(Arc::new(envelope)),
        );
        lash_vm::ProcessStart {
            module_ref: receiver.module_ref.clone(),
            process_ref: receiver.artifact.process_ref("install").unwrap().clone(),
            host_requirements_ref: receiver.host_requirements_ref.clone(),
            start_site: test_start_site("child_process:install", 1),
            process_name: "install".to_string(),
            args,
        }
    };

    prepare_lash_vm_process_start(
        &lash_vm_client::service::Service::default(),
        artifact_store.clone(),
        Some("parent:root"),
        start_with(
            lash_vm::ProcessDefinitionIdentity::from_artifact_export(&matching.artifact, "handler")
                .unwrap(),
        ),
        lash_core::ProcessOriginator::host(),
        lash_core::LifetimeDecision::Detached,
    )
    .await
    .expect("matching immutable signature passes");

    let error = prepare_lash_vm_process_start(
        &lash_vm_client::service::Service::default(),
        artifact_store.clone(),
        Some("parent:root"),
        start_with(
            lash_vm::ProcessDefinitionIdentity::from_artifact_export(
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
        LashVmRuntimeError::InvalidProcessArgument { ref path, .. }
            if path == "envelope.handler"
    ));
    assert!(error.to_string().contains("payload"), "{error}");

    for (description, definition) in [
        (
            "different parameter type",
            lash_vm::ProcessDefinitionIdentity::from_artifact_export(
                &wrong_type.artifact,
                "handler",
            )
            .unwrap(),
        ),
        (
            "different parameter order at the same arity",
            lash_vm::ProcessDefinitionIdentity::from_artifact_export(
                &wrong_order.artifact,
                "handler",
            )
            .unwrap(),
        ),
    ] {
        let error = prepare_lash_vm_process_start(
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
            LashVmRuntimeError::InvalidProcessArgument { ref path, .. }
                if path == "envelope.handler"
        ));
    }

    let valid =
        lash_vm::ProcessDefinitionIdentity::from_artifact_export(&matching.artifact, "handler")
            .unwrap();
    let wrong_ref = lash_vm::ProcessDefinitionIdentity::new(
        valid.module_ref,
        valid.host_requirements_ref,
        lash_vm::ProcessRef::new(lash_vm::ContentHash::new("wrong-process"), 0),
        valid.process_name,
    );
    let error = prepare_lash_vm_process_start(
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
        LashVmRuntimeError::InvalidProcessArgument { ref path, .. }
            if path == "envelope.handler"
    ));
}

#[tokio::test(flavor = "current_thread")]
async fn process_signature_union_accepts_a_later_matching_nonprocess_arm() {
    let store = crate::lib_tests::memory_artifact_store().await;
    let environment = LashVmHostEnvironment::new(lash_vm::LashVmHostCatalog::new());
    let receiver = lash_vm::compile_module(lash_vm::ModuleCompileRequest {
        source: "process install(handler: Process<(event: str), bool> | str) -> bool { finish true }",
        program: process_module(
            "install",
            vec![b::param(
                "handler",
                lash_vm::TypeExpr::union(vec![
                    b::process_type(
                        vec![b::param("event", lash_vm::TypeExpr::Str)],
                        lash_vm::TypeExpr::Bool,
                    ),
                    lash_vm::TypeExpr::Str,
                ]),
            )],
            lash_vm::TypeExpr::Bool,
            b::bool_lit(true),
        ),
        environment: &environment,
    })
    .expect("union receiver compiles");
    store
        .publish_module_artifact(&crate::lib_tests::host_claim(), &receiver.artifact)
        .await
        .expect("module publishes");
    let mut args = lash_vm::Record::new();
    args.insert(
        "handler".to_string(),
        lash_vm::Value::String("fallback".into()),
    );
    let start = lash_vm::ProcessStart {
        module_ref: receiver.module_ref.clone(),
        process_ref: receiver.artifact.process_ref("install").unwrap().clone(),
        host_requirements_ref: receiver.host_requirements_ref.clone(),
        start_site: test_start_site("child_process:install", 1),
        process_name: "install".to_string(),
        args,
    };
    let artifact_store: LashVmArtifacts = store;

    prepare_lash_vm_process_start(
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
        .with_execution(std::time::Duration::from_secs(120))
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
        LashVmSurface::default()
            .with_resources(LashVmHostCatalog::tool_default(operations.iter().copied()))
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
            LASH_VM_SURFACE_EXTENSION_ID,
            LashVmSurfaceContribution::new(
                LashVmLanguageFeatures::default(),
                LashVmHostCatalog::tool_default(["lookup"]),
            ),
        )
        .expect("extension payload serializes")
    });
    let extensions = lash_core::PluginExtensions::from_contributions(contributions);

    assert!(matches!(
        LashVmSurface::default().with_plugin_extensions(&extensions),
        Err(LashVmRuntimeError::HostCatalog {
            source: lash_vm::LashVmHostCatalogError::ConflictingModuleOperation {
                module,
                operation,
                ..
            }
        }) if module == "tools" && operation == "lookup"
    ));
}

pub(crate) fn test_start_site(node_id: &str, occurrence: u64) -> lash_vm::LashVmExecutionCallSite {
    lash_vm::LashVmExecutionCallSite {
        site: lash_vm::LashVmExecutionSite {
            node_id: node_id.to_string(),
            node_kind: lash_sansio::ExecutionNodeKind::Call,
            label: "start scan".to_string(),
            branch: None,
            workflow_site: lash_vm::WorkflowExecutionSite::new(
                "process:scan",
                [],
                lash_sansio::ExecutionNodeKind::Call,
                "start scan",
            ),
        },
        occurrence,
        loops: Default::default(),
    }
}

fn test_process_start(
    output: &lash_vm::ModuleCompileOutput,
    start_site: lash_vm::LashVmExecutionCallSite,
    root: &str,
) -> lash_vm::ProcessStart {
    let mut args = lash_vm::Record::new();
    args.insert("root".to_string(), lash_vm::Value::String(root.into()));
    lash_vm::ProcessStart {
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
async fn nested_process_arguments_reject_forged_aliases_and_try_later_union_arms() {
    use lash_core_execution::StoreSet;
    super::testing::nested_process_arguments_reject_forged_aliases_and_try_later_union_arms(
        sqlite_memory_store_set().await.module_artifacts(),
    )
    .await;
}

/// FIG-5571: retired substrate names cannot enter a VM runtime as aliases.
#[tokio::test(flavor = "current_thread")]
async fn retired_engine_kind_and_module_store_are_refused() {
    let engine = LashVmProcessEngine::new(memory_artifact_store().await, LashVmSurface::default());
    let registry = lash_core::ProcessEngineRegistry::new()
        .with_registration(lash_vm_process_engine_registration(engine));
    let refusal = registry
        .resolve(&lash_core::ProcessDefinitionRef::unclaimed(
            "lashlang",
            serde_json::Value::Null,
        ))
        .await
        .expect_err("the retired engine has no registration");
    assert!(matches!(
        refusal,
        lash_core::ProcessDefinitionRefusal::UnknownEngine { engine_kind }
            if engine_kind.as_str() == "lashlang"
    ));
    let old_store = serde_json::from_value::<lash_core::ArtifactStoreId>(
        serde_json::json!({"store": "lashlang_module"}),
    )
    .expect_err("the retired artifact discriminant is not an alias");
    assert_eq!(old_store.classify(), serde_json::error::Category::Data);
    let current_store = lash_core::ArtifactStoreId::module();
    assert_eq!(
        serde_json::from_value::<lash_core::ArtifactStoreId>(
            serde_json::to_value(&current_store).unwrap(),
        )
        .unwrap(),
        current_store,
    );
}
