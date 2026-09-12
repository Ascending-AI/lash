use super::*;

struct EveryNEffectsController(usize);

#[async_trait::async_trait]
impl lash_core::AwaitEventResolver for EveryNEffectsController {}

#[async_trait::async_trait]
impl lash_core::RuntimeEffectController for EveryNEffectsController {
    fn wants_segment_boundary(
        &self,
        progress: &lash_core::SegmentProgress,
    ) -> Option<lash_core::BoundaryReason> {
        progress
            .effects_executed
            .is_multiple_of(self.0 as u64)
            .then_some(lash_core::BoundaryReason::JournalBudget)
    }

    async fn execute_effect(
        &self,
        _envelope: lash_core::RuntimeEffectEnvelope,
        _local_executor: lash_core::RuntimeEffectLocalExecutor<'_>,
    ) -> Result<lash_core::RuntimeEffectOutcome, lash_core::RuntimeEffectControllerError> {
        unreachable!("predicate test does not execute effects")
    }
}

#[test]
fn every_n_controller_requests_boundaries_and_native_default_does_not() {
    let progress = lash_core::SegmentProgress {
        effects_executed: 2,
        journaled_bytes_estimate: None,
    };
    assert_eq!(
        lash_core::RuntimeEffectController::wants_segment_boundary(
            &EveryNEffectsController(2),
            &progress,
        ),
        Some(lash_core::BoundaryReason::JournalBudget)
    );
    let native = lash_core::facade_support::NativeRuntimeEffectController::default();
    assert_eq!(
        lash_core::RuntimeEffectController::wants_segment_boundary(&native, &progress,),
        None
    );
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
    let output = lashlang::compile_module(lashlang::ModuleCompileRequest {
        source,
        environment: &environment,
        artifact_store: None,
    })
    .await
    .expect("labeled workflow compiles");
    let graph = lashlang::workflow_graph_from_source(source).expect("workflow graph projects");
    let trace_map = trace_lashlang_main_map(&output.artifact);

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
            lashlang::WorkflowNodeKind::Container(
                lashlang::WorkflowContainer::ListComprehension { .. },
            ) => Some("list_comprehension"),
            _ => None,
        })
        .collect::<std::collections::BTreeSet<_>>();
    assert_eq!(
        container_kinds,
        std::collections::BTreeSet::from(["for", "if", "list_comprehension", "while"]),
        "the equality probe must cover every workflow container kind"
    );

    let expected_nodes = graph
        .nodes()
        .flat_map(|node| &node.execution_sites)
        .map(|site| {
            lashlang::runtime_execution_site_for_workflow_site(&output.artifact, site)
                .expect("workflow execution site should exist in the compiled artifact")
                .node_id
        })
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
fn process_input_serializes_as_generic_engine_payload() {
    let hash = lashlang::ContentHash::new("abc123");
    let input = LashlangProcessInput {
        module_ref: lashlang::ModuleRef::new(&hash),
        process_ref: lashlang::ProcessRef::new(hash.clone(), 7),
        host_requirements_ref: lashlang::HostRequirementsRef::new(&hash),
        process_name: "main".to_string(),
        args: serde_json::Map::from_iter([("prompt".to_string(), serde_json::json!("go"))]),
    };

    let process_input = input
        .clone()
        .into_process_input()
        .expect("lashlang process input serializes");

    let lash_core::ProcessInput::Engine { kind, payload } = process_input else {
        panic!("lashlang runtime must use the generic engine process input");
    };
    assert_eq!(kind, LASHLANG_ENGINE_KIND);
    assert_eq!(
        LashlangProcessInput::from_payload(payload)
            .expect("engine payload decodes")
            .process_name,
        input.process_name
    );
}

#[test]
fn process_input_remote_helpers_use_generic_engine_and_identity() {
    let hash = lashlang::ContentHash::new("abc123");
    let input = LashlangProcessInput {
        module_ref: lashlang::ModuleRef::new(&hash),
        process_ref: lashlang::ProcessRef::new(hash.clone(), 7),
        host_requirements_ref: lashlang::HostRequirementsRef::new(&hash),
        process_name: "main".to_string(),
        args: serde_json::Map::from_iter([("prompt".to_string(), serde_json::json!("go"))]),
    };

    let remote_input: lash_remote_protocol::RemoteProcessInput = input
        .clone()
        .try_into()
        .expect("lashlang process input serializes remotely");
    let lash_remote_protocol::RemoteProcessInput::Engine { kind, payload } = remote_input else {
        panic!("lashlang runtime must use the generic remote engine process input");
    };
    assert_eq!(kind, LASHLANG_ENGINE_KIND);
    assert_eq!(
        LashlangProcessInput::from_payload(payload)
            .expect("remote payload decodes")
            .process_name,
        "main"
    );

    let identity = input.process_identity();
    assert_eq!(identity.kind, LASHLANG_ENGINE_KIND);
    assert_eq!(identity.label.as_deref(), Some("main"));
    assert_eq!(input.remote_identity().label.as_deref(), Some("main"));

    let draft = input
        .remote_trigger_subscription_draft(
            "button-main",
            "process-env:v6:blake3:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
                .parse()
                .expect("canonical env ref"),
            "ui.button.pressed",
            "source-key",
        )
        .expect("remote trigger draft");
    draft.validate().expect("draft validates");
    assert_eq!(draft.target_label.as_deref(), Some("main"));
    assert_eq!(draft.target_identity.label.as_deref(), Some("main"));
}

#[test]
fn missing_tool_binding_is_not_fabricated() {
    let tool = lash_core::ToolDefinition::raw(
        "tool:test/read_file",
        "read_file",
        "read a file",
        lash_core::ToolDefinition::default_input_schema(),
        serde_json::Value::Null,
    );

    let err = required_tool_lashlang_executable(&tool.manifest)
        .expect_err("missing explicit binding should fail");

    assert!(matches!(
        err,
        ToolBindingError::MissingBinding {
            tool,
            binding_key: LASHLANG_TOOL_BINDING_KEY,
        } if tool == "read_file"
    ));
}

#[test]
fn explicit_tool_binding_attaches_lashlang_and_typescript_metadata() {
    let tool = lash_core::ToolDefinition::raw(
        "tool:test/read_file",
        "read_file",
        "read a file",
        lash_core::ToolDefinition::default_input_schema(),
        serde_json::Value::Null,
    )
    .with_tool_binding(
        ToolBinding::new(["fs"], "read")
            .with_authority_type("Filesystem")
            .with_aliases(["cat"]),
    );

    let binding =
        required_tool_lashlang_executable(&tool.manifest).expect("explicit binding resolves");
    let typescript =
        required_tool_typescript_executable(&tool.manifest).expect("TypeScript binding resolves");

    assert_eq!(binding.module_path, vec!["fs"]);
    assert_eq!(binding.operation, "read");
    assert_eq!(binding.authority_type, "Filesystem");
    assert_eq!(binding.aliases, vec!["cat"]);
    assert_eq!(typescript, binding);
    assert!(
        tool.manifest
            .bindings
            .contains_key(TYPESCRIPT_TOOL_BINDING_KEY)
    );
}

#[test]
fn pre_rename_manifest_binding_payload_round_trips_identically() {
    let legacy_bindings = serde_json::json!({
        "lashlang.tool": {
            "module_path": ["workspace", "files"],
            "operation": "write",
            "authority_type": "Filesystem",
            "aliases": ["write_text"]
        },
        "typescript.tool": {
            "module_path": ["workspace", "files"],
            "operation": "write",
            "authority_type": "Filesystem",
            "aliases": ["write_text"]
        }
    });
    let mut legacy_manifest = lash_core::ToolDefinition::raw(
        "tool:test/write_file",
        "write_file",
        "write a file",
        lash_core::ToolDefinition::default_input_schema(),
        serde_json::Value::Null,
    )
    .manifest;
    legacy_manifest.bindings =
        serde_json::from_value(legacy_bindings.clone()).expect("legacy bindings decode");

    let binding = legacy_manifest
        .tool_binding()
        .expect("legacy binding payload decodes")
        .expect("legacy binding is present");
    let rewritten = lash_core::ToolDefinition::raw(
        "tool:test/write_file",
        "write_file",
        "write a file",
        lash_core::ToolDefinition::default_input_schema(),
        serde_json::Value::Null,
    )
    .with_tool_binding(binding);

    assert_eq!(
        serde_json::to_value(&rewritten.manifest.bindings).expect("rewritten bindings encode"),
        legacy_bindings
    );
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
        lashlang::TypeExpr::List(Box::new(lashlang::TypeExpr::Union(vec![
            lashlang::TypeExpr::Str,
            lashlang::TypeExpr::Null,
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
    .with_output_from_input_schema("schema", Some(serde_json::json!({ "type": "string" })))
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
        lashlang::TypeExpr::Union(vec![lashlang::TypeExpr::Str, lashlang::TypeExpr::Null]),
    ];

    for expected in types {
        let schema = lashlang_type_expr_schema(&expected);
        assert_eq!(lashlang::json_schema_to_type_expr(&schema), expected);
    }
}

#[test]
fn dotted_operation_names_are_rejected() {
    let tool = lash_core::ToolDefinition::raw(
        "tool:test/update_plan",
        "update_plan",
        "update a plan",
        lash_core::ToolDefinition::default_input_schema(),
        serde_json::Value::Null,
    )
    .with_tool_binding(ToolBinding::new(["tools"], "update.plan"));

    let err = required_tool_lashlang_executable(&tool.manifest)
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
        serde_json::Value::Null,
    )
    .with_tool_binding(ToolBinding::new(["tools"], ""));

    let err = required_tool_lashlang_executable(&tool.manifest)
        .expect_err("an empty operation name cannot compile as a Lashlang operation");

    assert_eq!(
        err.to_string(),
        "tool `empty_operation` has invalid tool-binding operation name `<empty>`"
    );
}

#[test]
fn manifest_lashlang_binding_accessor_reports_absent_valid_and_malformed() {
    let mut manifest = lash_core::ToolDefinition::raw(
        "tool:test/read_file",
        "read_file",
        "read a file",
        lash_core::ToolDefinition::default_input_schema(),
        serde_json::Value::Null,
    )
    .manifest;
    assert_eq!(manifest.tool_binding().expect("absent binding"), None);

    manifest.bindings.insert(
        LASHLANG_TOOL_BINDING_KEY.to_string(),
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
        LASHLANG_TOOL_BINDING_KEY.to_string(),
        serde_json::json!({ "module_path": "fs" }),
    );
    assert!(manifest.tool_binding().is_err());
}

#[test]
fn remote_grant_lashlang_binding_accessor_reports_absent_valid_and_malformed() {
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
        LASHLANG_TOOL_BINDING_KEY.to_string(),
        serde_json::json!({ "module_path": "fs" }),
    );
    assert!(malformed.tool_binding().is_err());
}

#[test]
fn deterministic_process_id_reuses_replayed_start_site_and_args() {
    let input = test_process_input(serde_json::json!({ "root": "." }));
    let site = test_start_site("child_process:scan", 1);

    let first = deterministic_lashlang_process_id("parent:root", &site, &input)
        .expect("process id derives");
    let second = deterministic_lashlang_process_id("parent:root", &site, &input)
        .expect("process id derives");

    assert_eq!(first, second);
    assert!(first.starts_with("process:lashlang:v2:blake3:"));
}

#[test]
fn deterministic_process_id_separates_parallel_sites_ordinals_and_parents() {
    let input = test_process_input(serde_json::json!({ "root": "." }));
    let left = deterministic_lashlang_process_id(
        "parent:root",
        &test_start_site("child_process:left", 1),
        &input,
    )
    .expect("left id derives");
    let right = deterministic_lashlang_process_id(
        "parent:root",
        &test_start_site("child_process:right", 1),
        &input,
    )
    .expect("right id derives");
    let second_ordinal = deterministic_lashlang_process_id(
        "parent:root",
        &test_start_site("child_process:left", 2),
        &input,
    )
    .expect("second ordinal id derives");
    let nested_parent = deterministic_lashlang_process_id(
        "parent:nested",
        &test_start_site("child_process:left", 1),
        &input,
    )
    .expect("nested parent id derives");

    assert_ne!(left, right);
    assert_ne!(left, second_ordinal);
    assert_ne!(left, nested_parent);
}

#[tokio::test(flavor = "current_thread")]
async fn prepared_start_replays_same_registration_id_without_duplicate_child_identity() {
    let store = Arc::new(InMemoryLashlangArtifactStore::new());
    let environment = LashlangHostEnvironment::new(
        lashlang::LashlangHostCatalog::new(),
        LashlangAbilities::default().with_processes(),
    );
    let output = lashlang::compile_module(lashlang::ModuleCompileRequest {
        source: r#"process scan(root: str) -> str { finish root }"#,
        environment: &environment,
        artifact_store: Some(store.as_ref()),
    })
    .await
    .expect("module compiles and persists");
    let artifact_store: Arc<dyn LashlangArtifactStore> = store;
    let site = test_start_site("child_process:scan", 1);

    let first = prepare_lashlang_process_start(
        Arc::clone(&artifact_store),
        "parent:root",
        test_process_start(&output, site.clone(), "."),
    )
    .await
    .expect("first start prepares");
    let replayed = prepare_lashlang_process_start(
        Arc::clone(&artifact_store),
        "parent:root",
        test_process_start(&output, site.clone(), "."),
    )
    .await
    .expect("replayed start prepares");
    let sibling = prepare_lashlang_process_start(
        Arc::clone(&artifact_store),
        "parent:root",
        test_process_start(&output, test_start_site("child_process:scan", 2), "."),
    )
    .await
    .expect("sibling start prepares");

    assert_eq!(first.registration.id, replayed.registration.id);
    assert_eq!(first.registration.identity, replayed.registration.identity);
    assert_ne!(first.registration.id, sibling.registration.id);
}

#[tokio::test(flavor = "current_thread")]
async fn process_admission_four_shape_table_preserves_codes_and_prepare_omission() {
    let store = Arc::new(InMemoryLashlangArtifactStore::new());
    let required_environment = LashlangHostEnvironment::new(
        lashlang::LashlangHostCatalog::new(),
        LashlangAbilities::default().with_processes(),
    );
    let output = lashlang::compile_module(lashlang::ModuleCompileRequest {
        source: r#"process scan(root: str) -> str { finish root }"#,
        environment: &required_environment,
        artifact_store: Some(store.as_ref()),
    })
    .await
    .expect("module compiles");
    let start = test_process_start(&output, test_start_site("child_process:scan", 1), ".");
    let input = LashlangProcessInput {
        module_ref: start.module_ref.clone(),
        process_ref: start.process_ref.clone(),
        host_requirements_ref: start.host_requirements_ref.clone(),
        process_name: start.process_name.clone(),
        args: serde_json::Map::new(),
    };

    let mut requirements_mismatch = input.clone();
    requirements_mismatch.host_requirements_ref =
        lashlang::HostRequirementsRef::new(&lashlang::ContentHash::new("mismatch"));
    let mut process_mismatch = input.clone();
    process_mismatch.process_ref =
        lashlang::ProcessRef::new(lashlang::ContentHash::new("wrong-process"), 0);
    for (mut bad_start, expected_code, expected_message) in [
        (
            start.clone(),
            LashlangProcessFailureCode::ProcessHostRequirementsMismatch,
            "requested surface",
        ),
        (
            start.clone(),
            LashlangProcessFailureCode::ProcessRefMismatch,
            "does not export process",
        ),
    ] {
        if expected_code == LashlangProcessFailureCode::ProcessHostRequirementsMismatch {
            bad_start.host_requirements_ref = requirements_mismatch.host_requirements_ref.clone();
        } else {
            bad_start.process_ref = process_mismatch.process_ref.clone();
        }
        let error = prepare_lashlang_process_start(
            Arc::clone(&store) as Arc<dyn LashlangArtifactStore>,
            "parent:four-shape",
            bad_start,
        )
        .await
        .expect_err("the real prepare entry point must reject immutable mismatches");
        let LashlangRuntimeError::ProcessAdmission(refusal) = error else {
            panic!("prepare must preserve the typed admission refusal: {error:?}")
        };
        assert_eq!(refusal.failure_code(), expected_code);
        assert!(refusal.to_string().contains(expected_message), "{refusal}");
    }

    let mut malformed_tool = lash_core::ToolDefinition::raw(
        "four-shape-invalid-host",
        "four_shape_invalid_host",
        "malformed Lashlang binding fixture",
        serde_json::json!({"type": "object"}),
        serde_json::Value::Null,
    );
    malformed_tool.manifest.bindings.insert(
        LASHLANG_TOOL_BINDING_KEY.to_string(),
        serde_json::json!({"not": "a tool binding"}),
    );
    let invalid_host_catalog = Arc::new(lash_core::ToolCatalog::from_tool_definitions(vec![
        malformed_tool,
    ]));
    assert!(
        LashlangSurface::default()
            .for_process_registry(true)
            .host_environment(&invalid_host_catalog)
            .is_err(),
        "the invalid-host fixture must genuinely fail catalog conversion"
    );
    let incompatible_host_catalog = Arc::new(lash_core::ToolCatalog::default());
    let incompatible_environment = LashlangSurface::default()
        .for_process_registry(false)
        .host_environment(&incompatible_host_catalog)
        .expect("the incompatible-host fixture must itself be valid");
    assert!(
        lashlang_host_environment_satisfies_requirements(
            &output.artifact.host_requirements,
            &incompatible_environment,
        )
        .is_err(),
        "the valid fixture must genuinely lack the artifact's required process surface"
    );

    prepare_lashlang_process_start(
        Arc::clone(&store) as Arc<dyn LashlangArtifactStore>,
        "parent:four-shape",
        start,
    )
    .await
    .expect("the real prepare entry point explicitly omits both live-host fixtures");

    let artifact_store: Arc<dyn LashlangArtifactStore> = store;
    let cases = [
        (
            requirements_mismatch,
            Arc::new(lash_core::ToolCatalog::default()),
            false,
            LashlangProcessFailureCode::ProcessHostRequirementsMismatch,
            "requested surface",
        ),
        (
            process_mismatch,
            Arc::new(lash_core::ToolCatalog::default()),
            false,
            LashlangProcessFailureCode::ProcessRefMismatch,
            "does not export process",
        ),
        (
            input.clone(),
            invalid_host_catalog,
            true,
            LashlangProcessFailureCode::ProcessHostEnvironmentInvalid,
            "missing an explicit tool-binding module path",
        ),
        (
            input,
            incompatible_host_catalog,
            false,
            LashlangProcessFailureCode::ProcessHostEnvironmentIncompatible,
            "incompatible with this host surface",
        ),
    ];
    for (index, (input, catalog, registry_available, expected_code, expected_message)) in
        cases.into_iter().enumerate()
    {
        let payload = serde_json::to_value(&input).expect("valid process payload");
        let registration = lash_core::ProcessRegistration::new(
            format!("four-shape-run-{index}"),
            input.to_process_input().expect("valid engine input"),
            lash_core::RecoveryContract::Rerunnable,
            lash_core::ProcessProvenance::host(),
        )
        .with_identity(input.process_identity());
        let context = lash_core::testing::process_engine_run_context_for_validation(
            registration,
            catalog,
            registry_available,
        );
        let run_outcome = Box::pin(crate::process::run_lashlang_process(
            LashlangProcessEngine::new(Arc::clone(&artifact_store), LashlangSurface::default()),
            context,
            payload,
        ))
        .await
        .expect("admission mismatches are durable process outcomes, not infra errors");
        let run_output = run_outcome
            .terminal_output()
            .expect("admission refusal must be terminal");
        let lash_core::ProcessAwaitOutput::Settled { output } = run_output else {
            panic!("admission refusal must be a settled durable process failure")
        };
        let lash_core::ToolCallOutcome::Failure(failure) = &output.outcome else {
            panic!("admission refusal must map to a durable failure")
        };
        assert_eq!(failure.code, expected_code.as_str());
        assert!(failure.message.contains(expected_message), "{failure:?}");
    }
}

#[tokio::test(flavor = "current_thread")]
async fn prepared_start_checks_indirect_process_identity_against_named_signature() {
    let store = Arc::new(InMemoryLashlangArtifactStore::new());
    let environment = LashlangHostEnvironment::new(
        lashlang::LashlangHostCatalog::new(),
        LashlangAbilities::default().with_processes(),
    );
    let matching = lashlang::compile_module(lashlang::ModuleCompileRequest {
        source: "process handler(event: str, other: str) -> bool { finish true }",
        environment: &environment,
        artifact_store: Some(store.as_ref()),
    })
    .await
    .expect("matching handler compiles");
    let mismatching = lashlang::compile_module(lashlang::ModuleCompileRequest {
        source: "process handler(payload: str, other: str) -> bool { finish true }",
        environment: &environment,
        artifact_store: Some(store.as_ref()),
    })
    .await
    .expect("mismatching handler compiles");
    let wrong_type = lashlang::compile_module(lashlang::ModuleCompileRequest {
        source: "process handler(event: int, other: str) -> bool { finish true }",
        environment: &environment,
        artifact_store: Some(store.as_ref()),
    })
    .await
    .expect("wrong-type handler compiles");
    let wrong_order = lashlang::compile_module(lashlang::ModuleCompileRequest {
        source: "process handler(other: str, event: str) -> bool { finish true }",
        environment: &environment,
        artifact_store: Some(store.as_ref()),
    })
    .await
    .expect("wrong-order handler compiles");
    let receiver = lashlang::compile_module(lashlang::ModuleCompileRequest {
        source: "type Handler = Process<(event: str, other: str), bool>\ntype Envelope = { handler: Handler }\nprocess install(envelope: Envelope) -> bool { finish true }",
        environment: &environment,
        artifact_store: Some(store.as_ref()),
    })
    .await
    .expect("receiver compiles");
    let artifact_store: Arc<dyn LashlangArtifactStore> = store.clone();

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
        Arc::clone(&artifact_store),
        "parent:root",
        start_with(
            lashlang::ProcessDefinitionIdentity::from_artifact_export(
                &matching.artifact,
                "handler",
            )
            .unwrap(),
        ),
    )
    .await
    .expect("matching immutable signature passes");

    let error = prepare_lashlang_process_start(
        Arc::clone(&artifact_store),
        "parent:root",
        start_with(
            lashlang::ProcessDefinitionIdentity::from_artifact_export(
                &mismatching.artifact,
                "handler",
            )
            .unwrap(),
        ),
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
            Arc::clone(&artifact_store),
            "parent:root",
            start_with(definition),
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
        Arc::clone(&artifact_store),
        "parent:root",
        start_with(wrong_ref),
    )
    .await
    .expect_err("identity with a different process ref must fail");
    assert!(matches!(
        error,
        LashlangRuntimeError::InvalidProcessArgument { ref path, .. }
            if path == "envelope.handler"
    ));

    let mismatching_identity =
        lashlang::ProcessDefinitionIdentity::from_artifact_export(&mismatching.artifact, "handler")
            .unwrap();
    let mut forged = mismatching.artifact.clone();
    let process = forged
        .canonical_ir
        .declarations
        .iter_mut()
        .find_map(|declaration| match declaration {
            lashlang::Declaration::Process(process) if process.name == "handler" => Some(process),
            _ => None,
        })
        .expect("handler declaration exists");
    process.params[0].name = "event".into();
    assert!(forged.verify().is_err(), "forged artifact must not verify");
    store
        .put_module_artifact(&forged)
        .await
        .expect("test store accepts public artifact values");
    let error = prepare_lashlang_process_start(
        Arc::clone(&artifact_store),
        "parent:root",
        start_with(mismatching_identity),
    )
    .await
    .expect_err("forged signature with unchanged refs must fail before registration");
    assert!(matches!(
        error,
        LashlangRuntimeError::InvalidProcessArgument { ref path, .. }
            if path == "envelope.handler"
    ));
}

#[tokio::test(flavor = "current_thread")]
async fn prepared_start_rejects_a_forged_receiving_artifact() {
    let store = Arc::new(InMemoryLashlangArtifactStore::new());
    let environment = LashlangHostEnvironment::new(
        lashlang::LashlangHostCatalog::new(),
        LashlangAbilities::default().with_processes(),
    );
    let output = lashlang::compile_module(lashlang::ModuleCompileRequest {
        source: "process install(value: str) -> bool { finish true }",
        environment: &environment,
        artifact_store: Some(store.as_ref()),
    })
    .await
    .expect("receiver compiles");
    let mut forged = output.artifact.clone();
    let process = forged
        .canonical_ir
        .declarations
        .iter_mut()
        .find_map(|declaration| match declaration {
            lashlang::Declaration::Process(process) if process.name == "install" => Some(process),
            _ => None,
        })
        .expect("install declaration exists");
    process.params[0].name = "forged".into();
    assert!(forged.verify().is_err(), "forged artifact must not verify");
    store
        .put_module_artifact(&forged)
        .await
        .expect("test store accepts public artifact values");
    let artifact_store: Arc<dyn LashlangArtifactStore> = store;
    let mut args = lashlang::Record::new();
    args.insert("value".to_string(), lashlang::Value::String("value".into()));
    let start = lashlang::ProcessStart {
        module_ref: output.module_ref.clone(),
        process_ref: output.artifact.process_ref("install").unwrap().clone(),
        host_requirements_ref: output.host_requirements_ref.clone(),
        start_site: test_start_site("child_process:install", 1),
        process_name: "install".to_string(),
        args,
    };

    let error = prepare_lashlang_process_start(artifact_store, "parent:root", start)
        .await
        .expect_err("forged receiving artifact must fail before registration");
    assert!(matches!(
        error,
        LashlangRuntimeError::InvalidArtifact { .. }
    ));
}

#[tokio::test(flavor = "current_thread")]
async fn process_signature_union_accepts_a_later_matching_nonprocess_arm() {
    let store = Arc::new(InMemoryLashlangArtifactStore::new());
    let environment = LashlangHostEnvironment::new(
        lashlang::LashlangHostCatalog::new(),
        LashlangAbilities::default().with_processes(),
    );
    let receiver = lashlang::compile_module(lashlang::ModuleCompileRequest {
        source: "process install(handler: Process<(event: str), bool> | str) -> bool { finish true }",
        environment: &environment,
        artifact_store: Some(store.as_ref()),
    })
    .await
    .expect("union receiver compiles");
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
    let artifact_store: Arc<dyn LashlangArtifactStore> = store;

    prepare_lashlang_process_start(artifact_store, "parent:root", start)
        .await
        .expect("later string union arm accepts the value");
}

#[test]
fn surface_merges_plugin_extensions() {
    let contribution = LashlangSurfaceContribution::new(
        LashlangAbilities::default().with_processes(),
        LashlangLanguageFeatures::default().with_label_annotations(),
        LashlangHostCatalog::tool_default(["lookup"]),
    );
    let extensions = lash_core::PluginExtensions::from_contributions([
        lash_core::facade_support::PluginExtensionContribution::new(
            LASHLANG_SURFACE_EXTENSION_ID,
            contribution,
        )
        .expect("extension payload serializes"),
    ]);

    let surface = LashlangSurface::default()
        .with_plugin_extensions(&extensions)
        .expect("lashlang surface extension merges");
    let environment = surface
        .host_environment(&lash_core::ToolCatalog::default())
        .expect("empty tool catalog has no Lashlang bindings to validate");

    assert!(environment.abilities.sleep);
    assert!(environment.abilities.processes);
    assert!(environment.language_features.label_annotations);
    assert!(
        environment
            .resources
            .resolve_module_operation("Tools", "tools", "lookup")
            .is_some()
    );
}

#[test]
fn surface_resources_return_typed_catalog_conflicts() {
    let surface = LashlangSurface::default()
        .with_resources(LashlangHostCatalog::tool_default(["lookup"]))
        .expect("first resource contribution is unique");

    assert!(matches!(
        surface.with_resources(LashlangHostCatalog::tool_default(["lookup"])),
        Err(LashlangRuntimeError::HostCatalog {
            source: lashlang::LashlangHostCatalogError::ConflictingModuleOperation {
                module,
                operation,
                ..
            }
        }) if module == "tools" && operation == "lookup"
    ));
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
            canonical: lash_core::ToolDefinition::default_input_schema(),
            projection: lash_remote_protocol::RemoteSchemaProjectionPolicy::default(),
        },
        output_schema: lash_remote_protocol::RemoteSchemaContract::default(),
        output_contract: lash_remote_protocol::RemoteToolOutputContract::Static,
        examples: Vec::new(),
        activation: None,
        argument_projection: None,
        retry_policy: None,
        bindings: Default::default(),
    }
}

fn test_process_input(args: serde_json::Value) -> LashlangProcessInput {
    let hash = lashlang::ContentHash::new("abc123");
    let args = args
        .as_object()
        .expect("test args must be an object")
        .clone();
    LashlangProcessInput {
        module_ref: lashlang::ModuleRef::new(&hash),
        process_ref: lashlang::ProcessRef::new(hash.clone(), 7),
        host_requirements_ref: lashlang::HostRequirementsRef::new(&hash),
        process_name: "scan".to_string(),
        args,
    }
}

fn test_start_site(node_id: &str, occurrence: u64) -> lashlang::LashlangExecutionCallSite {
    lashlang::LashlangExecutionCallSite {
        site: lashlang::LashlangExecutionSite {
            node_id: node_id.to_string(),
            node_kind: "child_process".to_string(),
            label: "start scan".to_string(),
            branch: None,
            workflow_site: lashlang::WorkflowExecutionSite::new(
                "process:scan",
                [],
                "child_process",
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
