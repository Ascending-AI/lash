use super::*;
use crate::SessionCommitStore as _;
use crate::ToolProvider as _;
use crate::plugin::{SessionAuthorityContext, StaticPluginFactory};

#[derive(Clone, Debug)]
struct DynamicToolSpec {
    id: &'static str,
    name: &'static str,
    description: &'static str,
    finish_on_execute: bool,
}

impl DynamicToolSpec {
    const fn new(id: &'static str, name: &'static str, description: &'static str) -> Self {
        Self {
            id,
            name,
            description,
            finish_on_execute: false,
        }
    }

    const fn finishing(mut self) -> Self {
        self.finish_on_execute = true;
        self
    }

    fn definition(&self) -> crate::ToolDefinition {
        crate::ToolDefinition::raw(
            self.id,
            self.name,
            self.description,
            crate::ToolDefinition::default_input_schema(),
            json!({ "type": "object", "additionalProperties": true }),
        )
    }
}

#[derive(Default)]
struct DynamicToolSurface {
    tools: Mutex<Vec<DynamicToolSpec>>,
}

impl DynamicToolSurface {
    fn new(tools: Vec<DynamicToolSpec>) -> Self {
        Self {
            tools: Mutex::new(tools),
        }
    }

    fn replace(&self, tools: Vec<DynamicToolSpec>) {
        *self.tools.lock().expect("dynamic tool surface") = tools;
    }

    fn tool(&self, name: &str) -> Option<DynamicToolSpec> {
        self.tools
            .lock()
            .expect("dynamic tool surface")
            .iter()
            .find(|tool| tool.name == name)
            .cloned()
    }
}

#[async_trait::async_trait]
impl crate::ToolProvider for DynamicToolSurface {
    fn tool_manifests(&self) -> Vec<crate::ToolManifest> {
        self.tools
            .lock()
            .expect("dynamic tool surface")
            .iter()
            .map(|tool| tool.definition().manifest())
            .collect()
    }

    fn resolve_contract(&self, name: &str) -> Option<Arc<crate::ToolContract>> {
        self.tool(name)
            .map(|tool| Arc::new(tool.definition().contract()))
    }

    async fn execute(&self, call: crate::ToolCall<'_>) -> crate::ToolResult {
        let Some(tool) = self.tool(call.name) else {
            return crate::ToolResult::err_fmt(format_args!(
                "dynamic tool `{}` is not live",
                call.name
            ));
        };
        let result = crate::ToolResult::ok(json!({
            "id": tool.id,
            "name": tool.name,
            "description": tool.description,
        }));
        if tool.finish_on_execute {
            result.with_control(crate::ToolControl::Finish {
                value: json!(tool.id).into(),
            })
        } else {
            result
        }
    }
}

fn dynamic_plugin_host(provider: Arc<dyn crate::ToolProvider>) -> Arc<crate::PluginHost> {
    let mut factories = crate::testing::test_standard_protocol_factories();
    factories.push(Arc::new(StaticPluginFactory::new(
        "dynamic_tool_surface",
        crate::PluginSpec::new().with_tool_provider(provider),
    )));
    Arc::new(crate::PluginHost::new(factories))
}

fn runtime_environment(plugin_host: Arc<crate::PluginHost>) -> crate::RuntimeEnvironment {
    crate::RuntimeEnvironment::builder()
        .with_plugin_host(plugin_host)
        .with_runtime_host_config(test_host_config().core)
        .build()
}

fn hidden_authority(tool_name: &str) -> SessionAuthorityContext {
    SessionAuthorityContext {
        tool_access: crate::SessionToolAccess {
            tools: Vec::new(),
            hidden_tools: [tool_name.to_string()].into_iter().collect(),
        },
        ..SessionAuthorityContext::default()
    }
}

fn build_hidden_session(
    plugin_host: &crate::PluginHost,
    session_id: &str,
    hidden_tool_name: &str,
    snapshot: Option<&crate::PluginSessionSnapshot>,
) -> Arc<crate::PluginSession> {
    plugin_host
        .build_session_with_parent(
            session_id,
            Some("parent".to_string()),
            snapshot,
            hidden_authority(hidden_tool_name),
        )
        .expect("hidden child plugin session")
}

fn root_state(session_id: &str) -> RuntimeSessionState {
    RuntimeSessionState {
        session_id: session_id.to_string(),
        policy: standard_test_policy(),
        ..RuntimeSessionState::default()
    }
}

fn catalog_names(runtime: &LashRuntime) -> Vec<String> {
    runtime
        .active_tool_catalog_shared()
        .expect("active tool catalog")
        .iter()
        .filter_map(|entry| entry.get("name").and_then(serde_json::Value::as_str))
        .map(ToOwned::to_owned)
        .collect()
}

#[tokio::test]
async fn parked_resume_keeps_the_store_realized_session_lifetime() {
    let plugin_host = dynamic_plugin_host(Arc::new(DynamicToolSurface::default()));
    let env = runtime_environment(plugin_host);
    let store = Arc::new(RecordingStore::default());
    let runtime = LashRuntime::from_environment(
        &env,
        standard_test_policy(),
        root_state("parked-incarnation"),
        Some(store.clone() as Arc<dyn crate::RuntimePersistence>),
    )
    .await
    .expect("persistent runtime");
    let expected = store
        .load_session_meta()
        .await
        .expect("load realized metadata")
        .expect("realized metadata")
        .incarnation_id;
    assert_eq!(
        runtime
            .export_persistence_state()
            .session_lifetime
            .as_durable(),
        Some(&expected)
    );

    let parked = runtime.park().await.expect("park runtime");
    let resumed = LashRuntime::resume(parked, &env)
        .await
        .expect("resume runtime");
    let state = resumed.export_persistence_state();
    assert_eq!(state.session_lifetime.as_durable(), Some(&expected));
    assert!(matches!(
        state.turn_scope("resumed-turn"),
        crate::ExecutionScope::Turn {
            incarnation_id: Some(ref actual),
            ..
        } if actual == &expected
    ));
}

fn registry(runtime: &LashRuntime) -> Arc<crate::ToolRegistry> {
    runtime
        .session
        .as_ref()
        .expect("runtime session")
        .plugins()
        .tool_registry()
}

async fn assert_executes_by_id(runtime: &LashRuntime, id: &str) {
    let result = registry(runtime)
        .execute_by_id(
            &crate::ToolId::from(id),
            &json!({}),
            &crate::testing::mock_tool_context(),
            None,
        )
        .await;
    assert!(
        result.is_success(),
        "tool `{id}` should execute: {result:?}"
    );
}

async fn assert_rejected_by_id(runtime: &LashRuntime, id: &str) {
    let result = registry(runtime)
        .execute_by_id(
            &crate::ToolId::from(id),
            &json!({}),
            &crate::testing::mock_tool_context(),
            None,
        )
        .await;
    assert!(
        !result.is_success(),
        "tool `{id}` must not execute: {result:?}"
    );
}

fn text_response(text: &str) -> TestProvider {
    mock_provider(vec![MockCall {
        stream_events: Vec::new(),
        response: Ok(LlmResponse {
            full_text: text.to_string(),
            parts: vec![LlmOutputPart::Text {
                text: text.to_string(),
                response_meta: None,
            }],
            response_metadata: Default::default(),
            ..LlmResponse::default()
        }),
    }])
}

#[tokio::test]
async fn cold_resume_discovers_curated_live_surface_and_persists_it_without_flapping() {
    let original = DynamicToolSpec::new(
        "tool:original",
        "original",
        "present in the first persisted surface",
    );
    let discovered = DynamicToolSpec::new(
        "tool:discovered",
        "discovered",
        "advertised while the session is parked",
    );
    let surface = Arc::new(DynamicToolSurface::new(vec![original.clone()]));
    let provider: Arc<dyn crate::ToolProvider> = surface.clone();
    let plugin_host = dynamic_plugin_host(provider);
    let env = runtime_environment(plugin_host);
    let store = Arc::new(RecordingStore::default());
    let store_dyn: Arc<dyn crate::RuntimePersistence> = store.clone();

    let mut runtime = LashRuntime::from_environment(
        &env,
        standard_test_policy(),
        root_state("persisted-live-surface"),
        Some(store_dyn),
    )
    .await
    .expect("initial persistent runtime");
    let mut curated = runtime.tool_state().expect("initial tool state");
    curated
        .set_membership(&crate::ToolId::from(original.id), false)
        .expect("opt out of original tool");
    runtime
        .apply_tool_state(curated)
        .await
        .expect("apply host curation");
    let persisted_generation = runtime
        .tool_state()
        .expect("curated tool state")
        .generation();
    let parked = runtime.park().await.expect("park initial runtime");

    surface.replace(vec![original.clone(), discovered.clone()]);
    let mut resumed = LashRuntime::resume(parked, &env)
        .await
        .expect("cold resume with changed live source");
    let resumed_state = resumed.tool_state().expect("resumed tool state");
    assert_eq!(resumed_state.generation(), persisted_generation + 1);
    assert!(
        !resumed_state
            .get(&crate::ToolId::from(original.id))
            .expect("original state entry")
            .is_member(),
        "persisted opt-out remains attached to the original id"
    );
    assert!(
        resumed_state
            .get(&crate::ToolId::from(discovered.id))
            .expect("new live entry")
            .is_member(),
        "newly advertised ids default to catalog membership"
    );
    let resumed_catalog = catalog_names(&resumed);
    assert!(resumed_catalog.contains(&discovered.name.to_string()));
    assert!(!resumed_catalog.contains(&original.name.to_string()));
    assert_executes_by_id(&resumed, discovered.id).await;

    set_runtime_provider(
        &mut resumed,
        text_response("commit rebuilt surface").into_handle(),
    );
    resumed
        .run_turn_assembled(
            TurnInput::text("commit the rebuilt surface"),
            CancellationToken::new(),
            named_turn_scope("persisted-live-surface", "surface-commit"),
        )
        .await
        .expect("commit after live rebuild");

    let persisted = crate::load_persisted_session_state(store.as_ref())
        .await
        .expect("load committed state")
        .expect("persisted session");
    let persisted_tools = persisted
        .tool_state_snapshot
        .as_ref()
        .expect("changed generation re-exported on next commit");
    assert_eq!(
        serde_json::to_value(persisted_tools).expect("serialize persisted tools"),
        serde_json::to_value(&resumed_state).expect("serialize resumed tools")
    );

    let parked = resumed.park().await.expect("park rebuilt runtime");
    let resumed_again = LashRuntime::resume(parked, &env)
        .await
        .expect("second cold resume");
    assert_eq!(
        serde_json::to_value(resumed_again.tool_state().expect("stable resumed state"))
            .expect("serialize stable resumed state"),
        serde_json::to_value(&resumed_state).expect("serialize first resumed state"),
        "an unchanged live surface restores exactly without another generation bump"
    );
    let stable_catalog = catalog_names(&resumed_again);
    assert!(stable_catalog.contains(&discovered.name.to_string()));
    assert!(!stable_catalog.contains(&original.name.to_string()));
    assert_executes_by_id(&resumed_again, discovered.id).await;
}

#[tokio::test]
async fn session_fork_discovers_live_tools_and_preserves_curation_and_hidden_policy() {
    let curated = DynamicToolSpec::new(
        "tool:curated",
        "curated",
        "host opts this tool out before the fork",
    );
    let discovered = DynamicToolSpec::new(
        "tool:fork_discovered",
        "fork_discovered",
        "appears between the parent snapshot and child fork",
    );
    let hidden = DynamicToolSpec::new(
        "tool:fork_hidden",
        "fork_hidden",
        "must never enter the child authority",
    );
    let surface = Arc::new(DynamicToolSurface::new(vec![curated.clone()]));
    let provider: Arc<dyn crate::ToolProvider> = surface.clone();
    let plugin_host = dynamic_plugin_host(provider);
    let env = runtime_environment(plugin_host);
    let mut runtime = LashRuntime::from_environment(
        &env,
        standard_test_policy(),
        root_state("fork-parent"),
        None,
    )
    .await
    .expect("parent runtime");

    let mut parent_state = runtime.tool_state().expect("parent tool state");
    parent_state
        .set_membership(&crate::ToolId::from(curated.id), false)
        .expect("curate parent tool");
    runtime
        .apply_tool_state(parent_state)
        .await
        .expect("apply parent curation");

    let manager = runtime.session_state_service().expect("session manager");
    let lifecycle = runtime
        .session_lifecycle_service()
        .expect("session lifecycle");
    surface.replace(vec![curated.clone(), discovered.clone(), hidden.clone()]);
    let handle = lifecycle
        .create_session(
            crate::SessionCreateRequest::child_session(
                "fork-parent",
                crate::SessionStartPoint::CurrentSession,
                crate::PluginOptions::default(),
            )
            .with_session_id("fork-child")
            .with_plugin_source(crate::SessionPluginSource::CurrentSessionFork)
            .with_tool_access(crate::SessionToolAccess {
                tools: Vec::new(),
                hidden_tools: [hidden.name.to_string()].into_iter().collect(),
            }),
        )
        .await
        .expect("fork child from standing parent");

    let child_handle = runtime
        .managed_sessions
        .lock()
        .await
        .get(&handle.session_id)
        .cloned()
        .expect("managed child runtime");
    let child = child_handle.runtime.lock().await;
    let child_state = child.tool_state().expect("child tool state");
    assert!(
        !child_state
            .get(&crate::ToolId::from(curated.id))
            .expect("curated child entry")
            .is_member(),
        "membership curation survives a real session fork"
    );
    assert!(
        child_state
            .get(&crate::ToolId::from(discovered.id))
            .expect("new child entry")
            .is_member(),
        "the fork reconciles the stale parent snapshot over current providers"
    );
    assert!(
        !child_state
            .get(&crate::ToolId::from(hidden.id))
            .expect("hidden child entry")
            .is_member(),
        "hidden policy is registry authority, even for a newly discovered id"
    );
    let names = manager
        .tool_catalog(&handle.session_id)
        .await
        .expect("child model-facing catalog")
        .into_iter()
        .filter_map(|entry| entry["name"].as_str().map(ToOwned::to_owned))
        .collect::<Vec<_>>();
    assert!(names.contains(&discovered.name.to_string()));
    assert!(!names.contains(&curated.name.to_string()));
    assert!(!names.contains(&hidden.name.to_string()));
    assert_executes_by_id(&child, discovered.id).await;
    assert_rejected_by_id(&child, hidden.id).await;
}

#[tokio::test]
async fn composed_session_catalog_discovers_callable_tool_without_exposing_hidden_tool() {
    let original = DynamicToolSpec::new(
        "tool:compose_original",
        "compose_original",
        "present before context composition",
    );
    let discovered = DynamicToolSpec::new(
        "tool:compose_discovered",
        "compose_discovered",
        "discovered by the next context catalog composition",
    )
    .finishing();
    let hidden = DynamicToolSpec::new(
        "tool:compose_hidden",
        "compose_hidden",
        "new live tool excluded by child authority",
    );
    let surface = Arc::new(DynamicToolSurface::new(vec![original.clone()]));
    let provider: Arc<dyn crate::ToolProvider> = surface.clone();
    let plugin_host = dynamic_plugin_host(provider);
    let plugins = build_hidden_session(plugin_host.as_ref(), "compose-child", hidden.name, None);
    let transport = mock_provider(vec![MockCall {
        stream_events: Vec::new(),
        response: Ok(LlmResponse {
            parts: vec![LlmOutputPart::ToolCall {
                call_id: "compose-live-call".to_string(),
                tool_name: discovered.name.to_string(),
                input_json: "{}".to_string(),
                replay: None,
            }],
            response_metadata: Default::default(),
            ..LlmResponse::default()
        }),
    }]);
    let mut runtime = LashRuntime::from_embedded_state(
        standard_test_policy(),
        test_host_config(),
        crate::RuntimeServices::new(plugins),
        root_state("compose-child"),
    )
    .await
    .expect("hidden child runtime");
    set_runtime_provider(&mut runtime, transport.into_handle());
    assert!(!catalog_names(&runtime).contains(&discovered.name.to_string()));

    surface.replace(vec![original, discovered.clone(), hidden.clone()]);
    let turn = runtime
        .run_turn_assembled(
            TurnInput::text("use the newly composed tool"),
            CancellationToken::new(),
            named_turn_scope("compose-child", "compose-boundary"),
        )
        .await
        .expect("turn through compose_session_catalog boundary");
    assert!(
        matches!(
            turn.outcome,
            TurnOutcome::Finished(TurnFinish::ToolValue { ref tool_name, ref value })
                if tool_name == discovered.name && *value == json!(discovered.id)
        ),
        "newly composed tool should be model-callable: {:?}",
        turn.outcome
    );

    let names = catalog_names(&runtime);
    assert!(names.contains(&discovered.name.to_string()));
    assert!(!names.contains(&hidden.name.to_string()));
    assert_executes_by_id(&runtime, discovered.id).await;
    assert_rejected_by_id(&runtime, hidden.id).await;
}

#[tokio::test]
async fn hidden_tool_stays_denied_across_cold_store_rebuild() {
    let visible = DynamicToolSpec::new(
        "tool:cold_visible",
        "cold_visible",
        "visible before and after rebuild",
    );
    let discovered = DynamicToolSpec::new(
        "tool:cold_discovered",
        "cold_discovered",
        "new visible tool after rebuild",
    );
    let hidden = DynamicToolSpec::new(
        "tool:cold_hidden",
        "cold_hidden",
        "new hidden tool after rebuild",
    );
    let surface = Arc::new(DynamicToolSurface::new(vec![
        visible.clone(),
        hidden.clone(),
    ]));
    let provider: Arc<dyn crate::ToolProvider> = surface.clone();
    let plugin_host = dynamic_plugin_host(provider);
    let store = Arc::new(RecordingStore::default());
    let plugins =
        build_hidden_session(plugin_host.as_ref(), "cold-hidden-child", hidden.name, None);
    let mut runtime = LashRuntime::from_persistent_embedded_state(
        standard_test_policy(),
        test_host_config(),
        crate::PersistentRuntimeServices::new(plugins, store.clone()),
        root_state("cold-hidden-child"),
    )
    .await
    .expect("initial hidden persistent runtime");
    assert!(!catalog_names(&runtime).contains(&hidden.name.to_string()));
    assert!(
        !runtime
            .tool_state()
            .expect("initial hidden state")
            .get(&crate::ToolId::from(hidden.id))
            .expect("initial hidden entry")
            .is_member()
    );
    assert_rejected_by_id(&runtime, hidden.id).await;
    runtime.stamp_live_plugin_state();
    drop(runtime.park().await.expect("persist hidden child"));

    surface.replace(vec![visible, discovered.clone(), hidden.clone()]);
    let state = crate::load_persisted_session_state(store.as_ref())
        .await
        .expect("load hidden child state")
        .expect("persisted hidden child");
    let plugins = build_hidden_session(
        plugin_host.as_ref(),
        "cold-hidden-child",
        hidden.name,
        state.plugin_snapshot.as_ref(),
    );
    let rebuilt = LashRuntime::from_persistent_embedded_state(
        standard_test_policy(),
        test_host_config(),
        crate::PersistentRuntimeServices::new(plugins, store),
        state,
    )
    .await
    .expect("cold rebuild with host-supplied child authority");

    let names = catalog_names(&rebuilt);
    assert!(names.contains(&discovered.name.to_string()));
    assert!(!names.contains(&hidden.name.to_string()));
    assert_executes_by_id(&rebuilt, discovered.id).await;
    assert_rejected_by_id(&rebuilt, hidden.id).await;
}

#[tokio::test]
async fn orphan_lifecycle_rebinds_by_id_and_supersedes_same_name_without_duplicates() {
    let original = DynamicToolSpec::new(
        "tool:orphan-original",
        "orphaned_name",
        "persisted original manifest",
    );
    let rebound = DynamicToolSpec::new(
        original.id,
        original.name,
        "fresh live manifest after source recovery",
    );
    let replacement = DynamicToolSpec::new(
        "tool:orphan-replacement",
        original.name,
        "different id reusing the orphaned name",
    );
    let surface = Arc::new(DynamicToolSurface::new(vec![original.clone()]));
    let provider: Arc<dyn crate::ToolProvider> = surface.clone();
    let plugin_host = dynamic_plugin_host(provider);
    let env = runtime_environment(plugin_host);
    let store = Arc::new(RecordingStore::default());
    let mut runtime = LashRuntime::from_environment(
        &env,
        standard_test_policy(),
        root_state("orphan-lifecycle"),
        Some(store),
    )
    .await
    .expect("initial persistent runtime");
    let mut curated = runtime.tool_state().expect("original tool state");
    curated
        .set_membership(&crate::ToolId::from(original.id), false)
        .expect("opt out before source loss");
    runtime
        .apply_tool_state(curated)
        .await
        .expect("apply original opt-out");
    runtime.stamp_live_plugin_state();
    let parked = runtime.park().await.expect("persist original source");

    surface.replace(Vec::new());
    let mut resumed = LashRuntime::resume(parked, &env)
        .await
        .expect("session opens while dynamic source is absent");
    let orphaned = resumed.tool_state().expect("orphaned state");
    let orphan_entry = orphaned
        .get(&crate::ToolId::from(original.id))
        .expect("orphan retained");
    assert!(orphan_entry.is_orphaned());
    assert!(!orphan_entry.is_member());
    assert!(!catalog_names(&resumed).contains(&original.name.to_string()));
    assert_rejected_by_id(&resumed, original.id).await;

    surface.replace(vec![rebound.clone()]);
    resumed
        .refresh_session_tool_catalog()
        .await
        .expect("rebind returning source by id");
    let rebound_state = resumed.tool_state().expect("rebound state");
    let rebound_entry = rebound_state
        .get(&crate::ToolId::from(original.id))
        .expect("same id rebound");
    assert!(!rebound_entry.is_orphaned());
    assert!(
        !rebound_entry.is_member(),
        "same-id rebind preserves the original host opt-out"
    );
    assert_eq!(rebound_entry.manifest().description, rebound.description);
    assert_rejected_by_id(&resumed, original.id).await;

    surface.replace(Vec::new());
    resumed
        .refresh_session_tool_catalog()
        .await
        .expect("orphan the rebound tool again");
    surface.replace(vec![replacement.clone()]);
    resumed
        .refresh_session_tool_catalog()
        .await
        .expect("replace orphan with same-name new id");
    let replaced = resumed.tool_state().expect("replacement state");
    assert!(!replaced.contains(&crate::ToolId::from(original.id)));
    assert!(
        replaced
            .get(&crate::ToolId::from(replacement.id))
            .expect("new id owns reused name")
            .is_member(),
        "membership from the old id must not transfer to the replacement"
    );
    let names = catalog_names(&resumed);
    assert_eq!(
        names
            .iter()
            .filter(|name| *name == replacement.name)
            .count(),
        1,
        "a superseded orphan can never create duplicate model-facing names"
    );
    assert_executes_by_id(&resumed, replacement.id).await;
}

#[tokio::test]
async fn public_apply_tool_state_round_trip_keeps_delta_and_generation_fencing() {
    let first = DynamicToolSpec::new("tool:apply-first", "apply_first", "first live tool");
    let second = DynamicToolSpec::new("tool:apply-second", "apply_second", "second live tool");
    let surface = Arc::new(DynamicToolSurface::new(vec![first.clone(), second.clone()]));
    let provider: Arc<dyn crate::ToolProvider> = surface;
    let plugin_host = dynamic_plugin_host(provider);
    let env = runtime_environment(plugin_host);
    let mut runtime = LashRuntime::from_environment(
        &env,
        standard_test_policy(),
        root_state("apply-state-round-trip"),
        None,
    )
    .await
    .expect("live runtime");

    let stale = runtime.tool_state().expect("export base state");
    let mut edited = stale.clone();
    edited
        .set_membership(&crate::ToolId::from(first.id), false)
        .expect("edit membership through public snapshot API");
    let applied_generation = runtime
        .apply_tool_state(edited)
        .await
        .expect("apply generation-matched delta");
    assert_eq!(applied_generation, stale.generation() + 1);
    let applied = runtime.tool_state().expect("export applied state");
    assert!(
        !applied
            .get(&crate::ToolId::from(first.id))
            .expect("first entry retained")
            .is_member()
    );
    assert!(
        applied
            .get(&crate::ToolId::from(second.id))
            .expect("untouched second entry")
            .is_member(),
        "apply_state is a delta over the submitted snapshot, not a blanket opt-out"
    );
    assert!(!catalog_names(&runtime).contains(&first.name.to_string()));
    assert_rejected_by_id(&runtime, first.id).await;

    let err = runtime
        .apply_tool_state(stale)
        .await
        .expect_err("stale generation must be fenced");
    let message = err.to_string();
    assert!(message.contains("generation mismatch"), "{message}");
    assert!(
        message.contains(&applied_generation.to_string()),
        "{message}"
    );
}
