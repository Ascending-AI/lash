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
fn every_n_controller_requests_boundaries_and_inline_default_does_not() {
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
    let inline = lash_core::facade_support::InlineRuntimeEffectController::default();
    assert_eq!(
        lash_core::RuntimeEffectController::wants_segment_boundary(&inline, &progress,),
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
            "process-env:v3:sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
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

    assert!(
        err.to_string()
            .contains("missing an explicit `lashlang.tool` binding")
    );
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
    .with_lashlang_binding(
        LashlangToolBinding::new(["fs"], "read")
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
    .with_lashlang_binding(
        LashlangToolBinding::new(["fs"], "read").with_authority_type("Filesystem"),
    );
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
    .with_lashlang_binding(
        LashlangToolBinding::new(["generate"], "run").with_authority_type("Generator"),
    );
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
    .with_lashlang_binding(LashlangToolBinding::new(["tools"], "update.plan"));

    let err = required_tool_lashlang_executable(&tool.manifest)
        .expect_err("dotted operation cannot compile as one Lashlang operation");

    assert!(
        err.to_string()
            .contains("invalid Lashlang operation name `update.plan`")
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
    assert_eq!(manifest.lashlang_binding().expect("absent binding"), None);

    manifest.bindings.insert(
        LASHLANG_TOOL_BINDING_KEY.to_string(),
        serde_json::json!({
            "module_path": ["fs"],
            "operation": "read"
        }),
    );
    let binding = manifest
        .lashlang_binding()
        .expect("valid binding")
        .expect("present binding");
    assert_eq!(binding.module_path, vec!["fs"]);
    assert_eq!(binding.operation.as_deref(), Some("read"));

    manifest.bindings.insert(
        LASHLANG_TOOL_BINDING_KEY.to_string(),
        serde_json::json!({ "module_path": "fs" }),
    );
    assert!(manifest.lashlang_binding().is_err());
}

#[test]
fn remote_grant_lashlang_binding_accessor_reports_absent_valid_and_malformed() {
    let grant = remote_tool_grant("read_file");
    assert_eq!(grant.lashlang_binding().expect("absent binding"), None);

    let grant = grant.with_lashlang_binding(LashlangToolBinding::new(["fs"], "read"));
    let binding = grant
        .lashlang_binding()
        .expect("valid binding")
        .expect("present binding");
    assert_eq!(binding.module_path, vec!["fs"]);
    assert_eq!(binding.operation.as_deref(), Some("read"));

    let mut malformed = grant;
    malformed.bindings.insert(
        LASHLANG_TOOL_BINDING_KEY.to_string(),
        serde_json::json!({ "module_path": "fs" }),
    );
    assert!(malformed.lashlang_binding().is_err());
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
    assert!(first.starts_with("process:lashlang:sha256:"));
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

fn remote_tool_grant(name: &str) -> lash_remote_protocol::RemoteToolGrant {
    lash_remote_protocol::RemoteToolGrant {
        protocol_version: lash_remote_protocol::REMOTE_PROTOCOL_VERSION,
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
