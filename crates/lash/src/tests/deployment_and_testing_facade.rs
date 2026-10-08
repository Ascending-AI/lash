use super::*;

#[tokio::test]
async fn deployment_drain_status_keeps_waiting_process_non_drained() {
    let backend = sqlite_memory_store_backend().await;
    let registry = backend.process_registry();
    let core = explicit_ephemeral_facets(LashCore::standard_builder(backend))
        .build(crate::testing::runtime_lease_owner())
        .expect("build core with a process registry");
    let process_id = registry
        .register_process(
            lash_core::ProcessRegistration::new(
                lash_core::ProcessInput::Engine {
                    kind: "test-engine".to_string(),
                    payload: serde_json::Value::Null,
                },
                lash_core::ProcessProvenance::host(),
                lash_core::Lifetime::Detached,
            )
            .with_execution_env_ref(Some(lash_core::testing::process_execution_env_fixture_ref())),
        )
        .await
        .expect("register waiting process")
        .id;
    let authority = lash_core::ProcessExecutionWriteAuthority::invocation(
        process_id.clone(),
        "deployment-drain-status-waiting-run",
    )
    .bind_attempt(1);
    let started = authority
        .invocation_started()
        .expect("attempt-bound invocation has a start fact");
    registry
        .record_first_started_with_authority(&process_id, started, &authority)
        .await
        .expect("record process start");
    registry
        .set_process_wait_with_authority(
            &process_id,
            lash_core::WaitState {
                since_ms: 1,
                kind: lash_core::WaitKind::Signal {
                    name: "deployment-drain-status".to_string(),
                    event_type: "deployment.drain_status".to_string(),
                    key: "deployment-drain-status-waiting:signal".to_string(),
                    ordinal: 1,
                },
            },
            Vec::new(),
            &authority,
        )
        .await
        .expect("set process waiting");

    let status = core
        .drain_status(false)
        .await
        .expect("read deployment drain status");
    assert_eq!(status.remaining_invocations, 1);
    assert!(!status.drained());
}

/// A provider whose `execute` forwards through its granted branch only when
/// the attempt context carries the grant's execution binding — the same seam
/// a deferred-resolution host branches on (FIG-3436). Its tool is a grant
/// target, not a catalog member, so `tool_manifests` is empty.
struct GrantBoundTools;

#[async_trait]
impl ToolProvider for GrantBoundTools {
    fn tool_manifests(&self) -> Vec<lash_core::ToolManifest> {
        Vec::new()
    }

    fn resolve_contract(&self, _name: &str) -> Option<Arc<lash_core::ToolContract>> {
        None
    }

    async fn execute(&self, call: lash_core::ToolCall<'_>) -> lash_core::ToolAttemptOutcome {
        (async {
            lash_core::ToolOutcome::ok(serde_json::json!({
                "binding": call.context.tool_execution_binding().clone(),
            }))
        })
        .await
        .into()
    }
}

fn grant_bound_tool_definition() -> lash_core::ToolDefinition {
    lash_core::ToolDefinition::raw(
        "tool:grant_bound",
        "grant_bound",
        "Executes only under a granted route.",
        serde_json::json!({ "type": "object", "additionalProperties": false }),
        serde_json::json!({ "type": "object" }),
    )
    .expect("valid declared tool schemas")
    .with_execution(std::time::Duration::from_secs(120))
}

#[tokio::test]
async fn testing_facade_run_tool_granted_honors_the_granted_source_binding() {
    let grant = lash_core::ToolExecutionGrant::from_definition(
        lash_core::plugin::PluginRevision::new("mock", lash_core::plugin::BehaviorRevision::ONE),
        grant_bound_tool_definition(),
    )
    .with_source_id("grant-source")
    .with_execution_binding(serde_json::json!({ "route": "deferred" }));
    let args = serde_json::json!({});

    let outcome = crate::testing::run_tool_granted(&GrantBoundTools, &grant, &args).await;
    let lash_core::ToolAttemptOutcome::Done { result, intents } = outcome else {
        panic!("granted call must complete inline");
    };
    assert!(intents.is_empty());
    let output = result.into_output();
    assert!(output.is_success());
    assert_eq!(
        output.value_for_projection(),
        serde_json::json!({ "binding": { "route": "deferred" } })
    );

    // The ungranted route cannot admit the tool: its manifest lives on the
    // grant, outside the provider's catalog membership.
    let outcome = crate::testing::run_tool(&GrantBoundTools, "grant_bound", &args).await;
    let lash_core::ToolAttemptOutcome::Done { result, .. } = outcome else {
        panic!("the catalog-route probe resolves to a completed failure");
    };
    assert!(!result.into_output().is_success());
}

/// FIG-5411: hosts check partial mappings and complete inputs against the
/// retained signature, without admitting a process or accepting a forged claim.
#[cfg(feature = "rlm")]
#[tokio::test]
async fn definition_args_checks_partial_and_complete_inputs_without_starting() {
    use crate::process::{ArgsMismatch, ArgsMode};
    use lashlang::testing::ast_builders as b;
    let backend = sqlite_memory_store_backend().await;
    let core = explicit_ephemeral_facets(rlm_core_builder_over(backend))
        .build(crate::testing::runtime_lease_owner())
        .expect("the core builds");
    let environment = lash_lashlang_runtime::LashlangSurface::default()
        .for_process_registry(true)
        .host_environment(&lash_core::ToolCatalog::default())
        .unwrap();
    let compiled = lashlang::compile_module(lashlang::ModuleCompileRequest {
        source: "args-check",
        program: b::module(
            vec![b::process_returning(
                "handler",
                vec![
                    b::param("event", lashlang::TypeExpr::Str),
                    b::param("count", lashlang::TypeExpr::Int),
                ],
                lashlang::TypeExpr::Str,
                b::finish(b::var("event")),
            )],
            Vec::new(),
        ),
        environment: &environment,
    })
    .unwrap();
    let pin = crate::process::HostArtifactPin::mint();
    core.host_artifacts()
        .publish_module(&pin, &compiled.artifact)
        .await
        .unwrap();
    let draft =
        lashlang::ProcessDefinitionIdentity::from_artifact_export(&compiled.artifact, "handler")
            .unwrap()
            .draft()
            .unwrap();
    let definition = core
        .host_artifacts()
        .publish_definition(&pin, &draft)
        .await
        .unwrap();
    let checker = core.process_definitions();
    let partial = serde_json::json!({"event": "ready"})
        .as_object()
        .unwrap()
        .clone();
    checker
        .check_args(&definition, &partial, ArgsMode::Partial)
        .await
        .unwrap();
    assert!(
        matches!(checker.check_args(&definition, &partial, ArgsMode::Complete).await, Err(ArgsMismatch::Argument { path, .. }) if path == "count")
    );
    let complete = serde_json::json!({"event": "ready", "count": 2})
        .as_object()
        .unwrap()
        .clone();
    checker
        .check_args(&definition, &complete, ArgsMode::Complete)
        .await
        .unwrap();
    let bad = serde_json::json!({"count": "two"})
        .as_object()
        .unwrap()
        .clone();
    assert!(
        matches!(checker.check_args(&definition, &bad, ArgsMode::Partial).await, Err(ArgsMismatch::Argument { path, .. }) if path == "count")
    );
    let extra = serde_json::json!({"other": 1}).as_object().unwrap().clone();
    assert!(
        matches!(checker.check_args(&definition, &extra, ArgsMode::Partial).await, Err(ArgsMismatch::Argument { path, .. }) if path == "other")
    );
    let forged = lash_core::ProcessDefinition::new(
        definition.id,
        lash_core::ProcessSignature::known(serde_json::json!({"forged": true})),
    );
    assert!(matches!(
        checker
            .check_args(&forged, &complete, ArgsMode::Complete)
            .await,
        Err(ArgsMismatch::DefinitionRefused { .. })
    ));
    assert!(
        core.backend()
            .process_registry()
            .list_processes(&Default::default())
            .await
            .unwrap()
            .is_empty(),
        "argument checking admits no process"
    );
    core.shutdown().await.unwrap();
}
