use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

struct ReassignableProcessTool {
    label: &'static str,
    active: Arc<AtomicBool>,
}

impl ReassignableProcessTool {
    fn definition(&self) -> crate::ToolDefinition {
        crate::ToolDefinition::raw(
            "tool:process-route",
            "process_route",
            format!("process route {}", self.label),
            serde_json::json!({
                "type": "object",
                "properties": { self.label: { "type": "string" } },
                "required": [self.label],
                "additionalProperties": false
            }),
            serde_json::json!({ "type": "string" }),
        )
        .expect("valid declared tool schemas")
        .with_execution(std::time::Duration::from_secs(120))
    }
}

#[async_trait::async_trait]
impl crate::ToolProvider for ReassignableProcessTool {
    fn tool_manifests(&self) -> Vec<crate::ToolManifest> {
        self.active
            .load(Ordering::SeqCst)
            .then(|| self.definition().manifest())
            .into_iter()
            .collect()
    }

    fn resolve_contract(&self, name: &str) -> Option<Arc<crate::ToolContract>> {
        (name == "process_route").then(|| Arc::new(self.definition().contract()))
    }

    async fn execute(&self, _call: crate::ToolCall<'_>) -> crate::ToolAttemptOutcome {
        crate::ToolOutcome::ok(serde_json::json!(self.label)).into()
    }
}

async fn execute_process_dispatch(
    services: &crate::runtime::RuntimeSessionServices,
    surface: crate::plugin::ResolvedToolSurface,
) -> serde_json::Value {
    let scoped = crate::testing::runtime_helpers::host_process_scope(
        &services.current.host.core,
        &crate::ProcessId::fixture("process-route"),
    );
    let dispatch = services
        .process_step_dispatch(surface, scoped)
        .expect("process step dispatch");
    let attempt = crate::testing::ToolCallFixture::from_dispatch(Arc::clone(&dispatch))
        .attempt("process-route");
    let manifest = dispatch
        .tools
        .resolve_manifest_by_id(&crate::ToolId::from("tool:process-route"))
        .expect("process route manifest resolves");
    let outcome = dispatch
        .tools
        .execute(crate::ToolCall::new(
            &manifest,
            &serde_json::json!({}),
            &attempt,
        ))
        .await;
    drop(attempt);
    drop(dispatch);
    match outcome {
        crate::ToolAttemptOutcome::Done { result, .. } => {
            result.into_output().value_for_projection()
        }
        crate::ToolAttemptOutcome::HostFailed(error) => panic!("unexpected host fault: {error}"),
        crate::ToolAttemptOutcome::Pending(_) => serde_json::Value::Null,
    }
}

#[tokio::test]
async fn process_run_context_captures_catalog_and_execution_route_together() {
    let backend = crate::testing::sqlite_recording_backend().await;
    let a_active = Arc::new(AtomicBool::new(true));
    let b_active = Arc::new(AtomicBool::new(false));
    let spec = crate::PluginSpec::new()
        .with_tool_provider(Arc::new(ReassignableProcessTool {
            label: "route_a",
            active: Arc::clone(&a_active),
        }))
        .with_tool_provider(Arc::new(ReassignableProcessTool {
            label: "route_b",
            active: Arc::clone(&b_active),
        }));
    let mut factories = crate::testing::test_standard_protocol_factories();
    factories.push(Arc::new(crate::plugin::StaticPluginFactory::new(
        crate::plugin::PluginDeclaration::initial("process_route"),
        spec,
    )));
    let runtime = crate::runtime::tests::helpers::runtime_with_plugins(
        &backend,
        factories,
        crate::runtime::tests::helpers::mock_provider(Vec::new()),
    )
    .await;
    let services = runtime
        .runtime_session_services()
        .expect("runtime session services");
    let old_surface = services
        .current
        .plugins
        .pin_resolved_tool_surface()
        .expect("provider A surface");
    let old_entry = old_surface
        .catalog
        .tools
        .iter()
        .find(|entry| entry.manifest.id.as_str() == "tool:process-route")
        .expect("provider A is resident");
    assert!(
        old_entry
            .contract
            .input_schema
            .canonical()
            .pointer("/properties/route_a")
            .is_some()
    );

    a_active.store(false, Ordering::SeqCst);
    b_active.store(true, Ordering::SeqCst);
    services
        .current
        .plugins
        .tool_registry()
        .refresh_sources()
        .expect("provider B refresh");
    let fresh_surface = services
        .current
        .plugins
        .pin_resolved_tool_surface()
        .expect("provider B surface");

    assert_eq!(
        execute_process_dispatch(&services, old_surface).await,
        serde_json::json!("route_a")
    );
    assert_eq!(
        execute_process_dispatch(&services, fresh_surface).await,
        serde_json::json!("route_b")
    );
}
