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

fn viewed_definition() -> lash_core::ToolDefinition {
    lash_core::ToolDefinition::raw(
        "tool:search",
        "search",
        "Return search matches",
        lash_core::ToolDefinition::default_input_schema(),
        serde_json::json!({ "type": "object" }),
    )
    .with_tool_binding(lash_lashlang_runtime::ToolBinding::new(["search"], "find"))
}

struct ViewedToolProvider {
    with_view: bool,
}

#[async_trait::async_trait]
impl lash_core::ToolProvider for ViewedToolProvider {
    fn tool_manifests(&self) -> Vec<lash_core::ToolManifest> {
        vec![viewed_definition().manifest()]
    }

    fn resolve_manifest_by_id(&self, id: &lash_core::ToolId) -> Option<lash_core::ToolManifest> {
        (id == &lash_core::ToolId::from("tool:search")).then(|| viewed_definition().manifest())
    }

    fn resolve_contract(&self, name: &str) -> Option<std::sync::Arc<lash_core::ToolContract>> {
        (name == "search" || name == "tool:search")
            .then(|| std::sync::Arc::new(viewed_definition().contract()))
    }

    async fn execute(&self, call: lash_core::ToolCall<'_>) -> lash_core::ToolAttemptOutcome {
        let id = call
            .args
            .get("variant")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("item-0");
        let outcome = lash_core::ToolOutcomeDone::ok(serde_json::json!({
            "items": [{"id": id, "detail": {"excerpt": "complete passage"}}]
        }));
        let outcome = if self.with_view {
            outcome.with_model_view("Search results\n0. item-0: complete passage")
        } else {
            outcome
        };
        lash_core::ToolAttemptOutcome::done_without_intents(outcome)
    }
}

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
    run_cell(context, ECHO_CELL).await
}

async fn run_cell(
    context: lash_core::RuntimeExecutionContext<'_>,
    code: &str,
) -> lash_core::ExecResponse {
    run_cell_in(
        &mut crate::executor::RlmExecutionState::for_engine("typescript"),
        context,
        code,
    )
    .await
}

async fn run_cell_in(
    state: &mut crate::executor::RlmExecutionState,
    context: lash_core::RuntimeExecutionContext<'_>,
    code: &str,
) -> lash_core::ExecResponse {
    crate::executor::execute_code_with_channel_and_bounds(
        state,
        context,
        lash_core::ExecRequest {
            language: "typescript".to_string(),
            code: code.to_string(),
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

#[tokio::test]
async fn a_tool_model_view_prints_without_changing_the_program_result() {
    for with_view in [false, true] {
        let seed = if with_view { SEED + 1 } else { SEED + 2 };
        let double = super::kernel_double(seed, lash_restate_test::ServerConfig::default()).await;
        let handler = double
            .open_handler(super::default_cell_scope())
            .await
            .expect("open the handler");
        let context = lash_core::testing::code_execution_context_with_tool_provider_and_catalog(
            super::double_ports(&double, &handler),
            std::sync::Arc::new(ViewedToolProvider { with_view }),
            lash_core::ToolCatalog::from_tool_definitions(vec![viewed_definition()]),
        );
        let response = run_cell(
            context,
            "const r = await search.find({}); function same(x) { return x; } const alias = r; const boxed = [r]; console.log(r); print(r); print(alias); print(boxed[0]); print(same(r)); print(JSON.stringify(r)); console.log(r, 'tail'); print(r.items[0].id); print(r); print([r]); await search.find(r); finish(r.items[0].id);",
        )
        .await;
        assert_eq!(response.error, None);
        assert_eq!(response.terminal_finish, Some(serde_json::json!("item-0")));
        assert_eq!(response.observations.len(), 10);
        if with_view {
            assert_eq!(
                response.observations[0].text,
                "Search results\n0. item-0: complete passage"
            );
        } else {
            assert!(response.observations[0].text.contains("\"items\""));
            assert!(response.observations[0].text.contains("complete passage"));
        }
        assert_eq!(response.observations[0].is_model_view, with_view);
        for observation in &response.observations[1..5] {
            assert_eq!(observation.text, response.observations[0].text);
            assert_eq!(observation.is_model_view, with_view);
        }
        assert!(response.observations[5].text.contains("\"items\""));
        assert!(!response.observations[5].is_model_view);
        assert!(response.observations[6].text.contains("\"items\""));
        assert!(response.observations[6].text.ends_with(" tail"));
        assert!(!response.observations[6].is_model_view);
        assert_eq!(response.observations[7].text, "item-0");
        assert!(!response.observations[7].is_model_view);
        assert_eq!(response.observations[8].text, response.observations[0].text);
        assert_eq!(response.observations[8].is_model_view, with_view);
        assert!(response.observations[9].text.contains("\"items\""));
        assert!(!response.observations[9].is_model_view);
        let record = response.calls[0]
            .host_record
            .as_ref()
            .expect("recorded tool call");
        assert_eq!(
            record.output.value_for_projection()["items"][0]["id"],
            "item-0"
        );
        assert_eq!(
            record.output.model_view.as_deref(),
            with_view.then_some("Search results\n0. item-0: complete passage")
        );
        assert_eq!(
            response.calls[1]
                .host_record
                .as_ref()
                .expect("forwarded call is recorded")
                .args,
            record.output.value_for_projection(),
            "a result used as the only argument keeps the ordinary object payload"
        );
        handler.close().await.expect("close the handler");
    }
}

#[tokio::test]
async fn viewed_results_mutate_like_plain_results() {
    let mutations = [
        "r.extra = 1",
        "const alias = r; alias.extra = 1",
        "const a = [r]; a[0].extra = 1",
        "const box = { r }; box.r.extra = 1",
        "function set(x) { x.extra = 1; } set(r)",
        "r.items.push({ id: 'new' })",
        "const a = [r]; a[0].items.push({ id: 'new' })",
        "r.items[0].detail.excerpt = 'changed'",
    ];
    for mutation in mutations {
        let code = format!(
            "const r = await search.find({{}}); {mutation}; print(r); finish(JSON.stringify(r));"
        );
        let mut replies = Vec::new();
        for with_view in [false, true] {
            let double =
                super::kernel_double(SEED + 11, lash_restate_test::ServerConfig::default()).await;
            let handler = double
                .open_handler(super::default_cell_scope())
                .await
                .expect("open the handler");
            let context = lash_core::testing::code_execution_context_with_tool_provider_and_catalog(
                super::double_ports(&double, &handler),
                std::sync::Arc::new(ViewedToolProvider { with_view }),
                lash_core::ToolCatalog::from_tool_definitions(vec![viewed_definition()]),
            );
            let response = run_cell(context, &code).await;
            assert_eq!(response.error, None, "{mutation}");
            assert_eq!(response.observations.len(), 1, "{mutation}");
            assert!(!response.observations[0].is_model_view, "{mutation}");
            replies.push(response);
            handler.close().await.expect("close the handler");
        }
        assert_eq!(
            replies[0].observations, replies[1].observations,
            "{mutation}"
        );
        assert_eq!(
            replies[0].terminal_finish, replies[1].terminal_finish,
            "{mutation}"
        );
    }
}

async fn run_viewed_cell_in(
    state: &mut crate::executor::RlmExecutionState,
    code: &str,
) -> lash_core::ExecResponse {
    let double = super::kernel_double(SEED + 20, lash_restate_test::ServerConfig::default()).await;
    let handler = double
        .open_handler(super::default_cell_scope())
        .await
        .expect("open the handler");
    let context = lash_core::testing::code_execution_context_with_tool_provider_and_catalog(
        super::double_ports(&double, &handler),
        std::sync::Arc::new(ViewedToolProvider { with_view: true }),
        lash_core::ToolCatalog::from_tool_definitions(vec![viewed_definition()]),
    );
    let response = run_cell_in(state, context, code).await;
    handler.close().await.expect("close the handler");
    response
}

#[tokio::test]
async fn identical_results_restore_as_independent_objects_with_views() {
    let mut state = crate::executor::RlmExecutionState::for_engine("typescript");
    let first = run_viewed_cell_in(
        &mut state,
        "let first = await search.find({}); let second = await search.find({}); print(first);",
    )
    .await;
    assert_eq!(first.error, None);
    assert!(first.observations[0].is_model_view);
    let hydrated = state
        .hydrated_execution_state(lash_core::FleetFormat::current())
        .expect("snapshot both results and the view table");
    let mut restored = crate::executor::RlmExecutionState::for_engine("typescript");
    restored
        .restore_execution_state(&hydrated, lash_core::FleetFormat::current())
        .expect("restore both results and the view table");
    let second = run_viewed_cell_in(
        &mut restored,
        "print(first); console.log(second); first.extra = 1; print(first); print(second); finish({first, second});",
    )
    .await;
    assert_eq!(second.error, None);
    assert_eq!(second.observations.len(), 4);
    assert!(second.observations[0].is_model_view);
    assert!(second.observations[1].is_model_view);
    assert!(!second.observations[2].is_model_view);
    assert!(second.observations[3].is_model_view);
    let result = second
        .terminal_finish
        .expect("finish both restored results");
    assert_eq!(result["first"]["extra"], 1);
    assert!(result["second"].get("extra").is_none());
}

#[tokio::test]
async fn replacing_a_result_with_the_same_view_saves_its_new_content() {
    let mut state = crate::executor::RlmExecutionState::for_engine("typescript");
    let response = run_viewed_cell_in(
        &mut state,
        "let r = await search.find({variant: 'item-0'}); r = await search.find({variant: 'item-1'}); print(r);",
    )
    .await;
    assert_eq!(response.error, None);
    assert!(response.observations[0].is_model_view);
    let hydrated = state
        .hydrated_execution_state(lash_core::FleetFormat::current())
        .expect("snapshot the replacement");
    let mut restored = crate::executor::RlmExecutionState::for_engine("typescript");
    restored
        .restore_execution_state(&hydrated, lash_core::FleetFormat::current())
        .expect("restore the replacement");
    let after = run_viewed_cell_in(&mut restored, "print(r); finish(r.items[0].id);").await;
    assert_eq!(after.error, None);
    assert!(after.observations[0].is_model_view);
    assert_eq!(after.terminal_finish, Some(serde_json::json!("item-1")));
}

#[tokio::test]
async fn structured_operations_are_the_same_with_and_without_a_view() {
    let code = r#"
        const r = await search.find({});
        const { items } = r;
        const entries = Object.entries(r);
        const map = new Map([['result', r]]);
        print([r]);
        print({r});
        print(`id:${items[0].id}`);
        print({...r});
        print(JSON.stringify(r));
        print(entries);
        console.log(map);
        finish({json: JSON.stringify(r), entries, spread: {...r}, mapped: map.get('result')});
    "#;
    let mut replies = Vec::new();
    for with_view in [false, true] {
        let double =
            super::kernel_double(SEED + 30, lash_restate_test::ServerConfig::default()).await;
        let handler = double
            .open_handler(super::default_cell_scope())
            .await
            .expect("open the handler");
        let context = lash_core::testing::code_execution_context_with_tool_provider_and_catalog(
            super::double_ports(&double, &handler),
            std::sync::Arc::new(ViewedToolProvider { with_view }),
            lash_core::ToolCatalog::from_tool_definitions(vec![viewed_definition()]),
        );
        let response = run_cell(context, code).await;
        assert_eq!(response.error, None);
        assert_eq!(response.observations.len(), 7);
        for (index, observation) in response.observations.iter().enumerate() {
            assert_eq!(observation.is_model_view, with_view && index == 3);
        }
        replies.push(response);
        handler.close().await.expect("close the handler");
    }
    for index in [0, 1, 2, 4, 5, 6] {
        assert_eq!(
            replies[0].observations[index],
            replies[1].observations[index]
        );
    }
    assert_eq!(replies[0].terminal_finish, replies[1].terminal_finish);
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

/// Fails every group settlement read with a store I/O error, where a real
/// one lands.
struct FailingSettlementReads;

#[async_trait::async_trait]
impl lash_core::testing::EffectLayer for FailingSettlementReads {
    async fn await_next_settlement(
        &self,
        _inner: &dyn lash_core::RuntimeEffectController,
        _handle: &mut lash_core::EffectGroupHandle,
        _cancel: lash_core::TurnCancelWait,
    ) -> Result<lash_core::GroupSettlement, lash_core::RuntimeEffectControllerError> {
        Err(lash_core::RuntimeEffectControllerError::new(
            lash_core::RuntimeErrorCode::RuntimeStore,
            "settlement read failed: disk I/O error",
        ))
    }
}

/// ADR 0099 §10 L3: an aggregate's infrastructure failure travels on the
/// host-control channel, never as a leaf rejection (FIG-3397). The
/// controller the handler lent fails every settlement read on store I/O; a
/// `try`/`catch` around each aggregate never runs, so no cell commits a
/// fallback a redrive — which reads the same settlement successfully — would
/// answer differently. (Moved from the facade's aggregate oracle: a facade
/// turn's handler controller takes no layer.)
#[tokio::test(flavor = "multi_thread")]
async fn a_settlement_store_failure_is_not_caught_by_the_cell() {
    for aggregate in [
        "Promise.any",
        "Promise.race",
        "Promise.all",
        "Promise.allSettled",
    ] {
        let double = super::kernel_double(SEED, lash_restate_test::ServerConfig::default()).await;
        let handler = double
            .open_handler(super::default_cell_scope())
            .await
            .expect("open the cell's handler");
        let context = lash_core::testing::code_execution_context_with_tool_provider_and_catalog(
            super::double_ports_over_layer(
                &double,
                &handler,
                std::sync::Arc::new(FailingSettlementReads),
            ),
            std::sync::Arc::new(EchoToolProvider),
            lash_core::ToolCatalog::from_tool_definitions(vec![echo_definition()]),
        );
        let response = run_cell(
            context,
            &format!(
                r#"try {{
  await {aggregate}([echo.say({{ text: "a" }}), echo.say({{ text: "b" }})]);
  finish("resolved");
}} catch (error) {{
  finish("caught");
}}"#
            ),
        )
        .await;
        assert_ne!(
            response.terminal_finish,
            Some(serde_json::json!("caught")),
            "{aggregate}: the cell's catch saw a host-control failure"
        );
        assert_ne!(
            response.terminal_finish,
            Some(serde_json::json!("resolved")),
            "{aggregate}: the aggregate cannot resolve without a settlement"
        );
        assert!(
            response.error.as_ref().is_some_and(|failure| {
                failure.kind == lash_core::CellFailureKind::Host
                    && failure.message.contains("disk I/O error")
            }),
            "{aggregate}: the cell fails on the host failure itself: {:?}",
            response.error
        );
        handler.close().await.expect("close the cell's handler");
    }
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
