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

/// FIG-5431: cancellation owns submitted inputs, never the linked session.
#[tokio::test]
async fn process_cancel_withdraws_its_inputs_and_preserves_foreign_child_inputs() {
    let backend = crate::testing::sqlite_recording_backend().await;
    let runtime = crate::runtime::tests::helpers::runtime_with_plugins(
        &backend,
        crate::testing::test_standard_protocol_factories(),
        crate::runtime::tests::helpers::mock_provider(Vec::new()),
    )
    .await;
    let services = runtime
        .runtime_session_services()
        .expect("session services");
    let process_id = crate::ProcessId::fixture("input-owner");
    let turn_id = crate::TurnId::from("owned-turn");
    let session_id = crate::SessionId::from("linked-input-child");
    let factory = backend.session_store_factory();
    let store = crate::testing::runtime_helpers::create_session_store(
        &factory,
        &crate::SessionStoreCreateRequest {
            session_id: session_id.clone(),
            relation: crate::SessionRelation::Child {
                parent_session_id: runtime.session_id().into(),
                caused_by: Some(crate::CausalRef::Process {
                    process_id: process_id.clone(),
                }),
            },
            pending_observer_intents: Vec::new(),
            config: crate::PersistedSessionConfig::from_policy(
                &runtime.state.policy().clone(),
                crate::SessionToolAccess::ambient(),
            ),
            head: crate::SessionCreationHead::Config,
            owning_process_id: Some(process_id.clone()),
            retention: crate::Retention::UntilGc,
        },
    )
    .await
    .expect("create linked child");
    for source in [turn_id.as_str(), "foreign-host-input"] {
        store
            .store()
            .enqueue_pending_turn_input(
                crate::PendingTurnInputDraft::new(
                    session_id.clone(),
                    crate::TurnInputIngress::next_turn(),
                    crate::TurnInput::text(source),
                )
                .with_source_key(source),
            )
            .await
            .expect("mail input");
    }
    // The missing requested id covers discovery after a crash during creation.
    services
        .withdraw_process_child_inputs(None, &process_id, &turn_id)
        .await
        .expect("cancel process inputs");
    let inputs = store.list_pending_turn_inputs().await.expect("read inputs");
    assert!(
        !inputs
            .iter()
            .any(|read| read.input.source_key.as_deref() == Some(turn_id.as_str())),
        "the process input is withdrawn"
    );
    let foreign = inputs
        .iter()
        .find(|read| read.input.source_key.as_deref() == Some("foreign-host-input"))
        .expect("foreign input");
    assert!(
        !foreign.input.state.is_terminal(),
        "lineage grants no input ownership"
    );
}
