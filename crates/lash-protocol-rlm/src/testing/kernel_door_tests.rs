//! This crate's twins on the Restate server double and the storage-only
//! store set (D1 F8, PR-S2).

use lash_lashlang_runtime::ToolDefinitionBindingExt as _;

const SEED: u64 = 0x5_2d30;

/// An open handler on this crate's double lends a turn-scoped controller and
/// closes cleanly.
#[tokio::test(flavor = "multi_thread")]
async fn an_open_handler_lends_its_scope_on_the_double() {
    let double = super::kernel_double(SEED, lash_restate_test::ServerConfig::default()).await;
    let admitted = lash_core::AdmittedScope::turn(
        lash_core::SessionId::from("root"),
        lash_core::TurnId::from("t"),
    );
    let handler = double
        .open_handler(admitted.clone())
        .await
        .expect("open the handler");
    assert_eq!(handler.scoped().admitted_scope(), &admitted);
    handler.close().await.expect("close the handler");
}

/// The storage-only twins hand out the artifact and trigger ports a law that
/// runs no effect reaches.
#[tokio::test]
async fn the_storage_only_twins_serve_artifacts_and_triggers() {
    let backend = super::memory_store_backend().await;
    let _artifacts = lashlang::LashlangArtifacts::of_backend(&backend);
    let stores = super::memory_store_set().await;
    assert_ne!(
        lash_core::StoreSet::binding_identity(stores.as_ref()),
        &backend.binding_identity(),
        "each twin call opens a fresh store set"
    );
    let _triggers = lash_core::StoreSet::trigger_store(stores.as_ref());
}

fn echo_definition() -> lash_core::ToolDefinition {
    lash_core::ToolDefinition::raw(
        "tool:echo",
        "echo",
        "Echo the text back",
        lash_core::ToolDefinition::default_input_schema(),
        serde_json::json!({ "type": "object" }),
    )
    .with_tool_binding(lash_lashlang_runtime::ToolBinding::new(["echo"], "say"))
}

struct EchoToolProvider;

#[async_trait::async_trait]
impl lash_core::ToolProvider for EchoToolProvider {
    fn tool_manifests(&self) -> Vec<lash_core::ToolManifest> {
        vec![echo_definition().manifest()]
    }

    fn resolve_manifest_by_id(&self, id: &lash_core::ToolId) -> Option<lash_core::ToolManifest> {
        (id == &lash_core::ToolId::from("tool:echo")).then(|| echo_definition().manifest())
    }

    fn resolve_contract(&self, name: &str) -> Option<std::sync::Arc<lash_core::ToolContract>> {
        (name == "echo" || name == "tool:echo")
            .then(|| std::sync::Arc::new(echo_definition().contract()))
    }

    async fn execute(&self, call: lash_core::ToolCall<'_>) -> lash_core::ToolAttemptOutcome {
        let text = call
            .args
            .get("text")
            .cloned()
            .unwrap_or(serde_json::Value::Null);
        lash_core::ToolAttemptOutcome::done_without_intents(lash_core::ToolOutcomeDone::ok(text))
    }
}

/// A TypeScript cell that awaits one echo call and then two in one
/// aggregate, and finishes with all three answers.
const ECHO_CELL: &str = r#"
    const single = await echo.say({ text: "one" });
    const pair = await Promise.all([
        echo.say({ text: "a" }),
        echo.say({ text: "b" })
    ]);
    finish({ single, pair });
"#;

async fn run_echo_cell(context: lash_core::RuntimeExecutionContext<'_>) -> lash_core::ExecResponse {
    crate::executor::execute_code_with_channel_and_bounds(
        &mut crate::executor::RlmExecutionState::for_engine("typescript"),
        context,
        lash_core::ExecRequest {
            language: "typescript".to_string(),
            code: ECHO_CELL.to_string(),
        },
        super::memory_artifact_store().await,
        lash_lashlang_runtime::LashlangSurface::default(),
        None,
        crate::projection::RlmProjectedBindings::default(),
        std::sync::Arc::new(crate::projection::ProjectionRegistry::new()),
        crate::executor::RlmLashlangExecutionTraceConfig::default(),
        lashlang::ExecutionBounds::unbounded(),
        crate::plugin::RlmChannel::Cell,
    )
    .await
}

fn assert_echo_cell_answered(response: &lash_core::ExecResponse) {
    assert_eq!(response.error, None, "the cell runs to its finish");
    assert_eq!(
        response.terminal_finish,
        Some(serde_json::json!({ "single": "one", "pair": ["a", "b"] })),
        "every tool call answers through the handler's controller"
    );
}

/// A cell built over [`super::double_ports`] runs its tool calls, a single
/// one and an aggregate group, on the controller the double's handler lent,
/// and the handler then closes cleanly: the lent execution served every
/// effect in stream. The test runs on a current-thread runtime, as the
/// executor laws' `block_on` does.
#[tokio::test]
async fn a_cell_on_the_doubles_lent_ports_runs_its_tool_calls_in_the_handler() {
    let double = super::kernel_double(SEED, lash_restate_test::ServerConfig::default()).await;
    let handler = double
        .open_handler(super::default_cell_scope())
        .await
        .expect("open the cell's handler");
    let context = lash_core::testing::code_execution_context_with_tool_provider_and_catalog(
        super::double_ports(&double, &handler),
        std::sync::Arc::new(EchoToolProvider),
        lash_core::ToolCatalog::from_tool_definitions(vec![echo_definition()]),
    );
    let response = run_echo_cell(context).await;
    assert_echo_cell_answered(&response);
    handler.close().await.expect("close the cell's handler");
}

/// A cell whose context installs its own parent invocation claims that
/// invocation's scope, so the handler it runs in is opened for that scope.
#[tokio::test(flavor = "multi_thread")]
async fn a_cell_under_an_installed_invocation_runs_in_a_handler_for_its_scope() {
    let double = super::kernel_double(SEED, lash_restate_test::ServerConfig::default()).await;
    let handler = double
        .open_handler(lash_core::AdmittedScope::turn(
            lash_core::SessionId::from("door-session"),
            lash_core::TurnId::from("door-turn"),
        ))
        .await
        .expect("open the invocation's handler");
    let context =
        lash_core::testing::code_execution_context_with_tool_provider_catalog_and_invocation(
            super::double_ports(&double, &handler),
            std::sync::Arc::new(EchoToolProvider),
            lash_core::ToolCatalog::from_tool_definitions(vec![echo_definition()]),
            lash_core::testing::exec_code_invocation(
                "door-session",
                "door-turn",
                0,
                0,
                "door-exec",
                "exec:door",
            ),
        );
    let response = run_echo_cell(context).await;
    assert_echo_cell_answered(&response);
    handler
        .close()
        .await
        .expect("close the invocation's handler");
}

/// The effects a layer sees, by seam operation.
#[derive(Default)]
struct SeamCount {
    effects: std::sync::atomic::AtomicUsize,
    groups: std::sync::atomic::AtomicUsize,
}

#[async_trait::async_trait]
impl lash_core::testing::EffectLayer for SeamCount {
    async fn execute_effect(
        &self,
        inner: &dyn lash_core::RuntimeEffectController,
        envelope: lash_core::RuntimeEffectEnvelope,
        local_executor: lash_core::RuntimeEffectLocalExecutor<'_>,
    ) -> Result<lash_core::RuntimeEffectOutcome, lash_core::RuntimeEffectControllerError> {
        self.effects
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        inner.execute_effect(envelope, local_executor).await
    }

    async fn open_effect_group(
        &self,
        inner: &dyn lash_core::RuntimeEffectController,
        group: lash_core::RuntimeEffectGroup,
    ) -> Result<lash_core::EffectGroupHandle, lash_core::RuntimeEffectControllerError> {
        self.groups
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        inner.open_effect_group(group).await
    }
}

/// [`super::double_ports_over_layer`] puts its layer in front of the
/// controller the handler lent, so the layer sees the cell's effects and
/// groups while they still run, and answer, in the handler.
#[tokio::test(flavor = "multi_thread")]
async fn a_layer_over_the_doubles_lent_ports_sees_the_cells_seam() {
    let double = super::kernel_double(SEED, lash_restate_test::ServerConfig::default()).await;
    let seam = std::sync::Arc::new(SeamCount::default());
    let handler = double
        .open_handler(super::default_cell_scope())
        .await
        .expect("open the cell's handler");
    let context = lash_core::testing::code_execution_context_with_tool_provider_and_catalog(
        super::double_ports_over_layer(&double, &handler, seam.clone()),
        std::sync::Arc::new(EchoToolProvider),
        lash_core::ToolCatalog::from_tool_definitions(vec![echo_definition()]),
    );
    let response = run_echo_cell(context).await;
    assert_echo_cell_answered(&response);
    handler.close().await.expect("close the cell's handler");
    assert!(
        seam.effects.load(std::sync::atomic::Ordering::SeqCst) > 0,
        "the layer sees the cell's effects"
    );
    assert!(
        seam.groups.load(std::sync::atomic::Ordering::SeqCst) > 0,
        "the layer sees the aggregate's group"
    );
}

/// A context that claims a scope other than the one its lent controller
/// admits is refused when it is built, before any effect runs.
#[tokio::test(flavor = "multi_thread")]
#[should_panic(expected = "the lent controller admits a scope the context does not claim")]
async fn a_context_claiming_another_scope_than_its_lent_controller_is_refused() {
    let double = super::kernel_double(SEED, lash_restate_test::ServerConfig::default()).await;
    let handler = double
        .open_handler(lash_core::AdmittedScope::turn(
            lash_core::SessionId::from("other-session"),
            lash_core::TurnId::from("other-turn"),
        ))
        .await
        .expect("open a handler for another scope");
    let _context =
        lash_core::testing::code_execution_context(super::double_ports(&double, &handler));
}
