//! What a session open learns when its persisted tools have no live source
//! (FIG-3367), and what the FIG-3353 sequence actually leaves durable.

use super::*;
use lash_core::ToolProvider as _;
use lash_core::facade_support::ToolStateFacadeOps;
use lash_core::plugin::StaticPluginFactory;

const ALPHA_ID: &str = "tool:fig3367_alpha";
const ALPHA_NAME: &str = "fig3367_alpha";
const BETA_ID: &str = "tool:fig3367_beta";
const BETA_NAME: &str = "fig3367_beta";
const REPLACEMENT_ID: &str = "tool:fig3367_alpha_v2";

/// A fixed two-tool source. The ids, names and descriptions are literals so
/// the generation arithmetic below is about restores, not about a manifest
/// that drifts between builds.
struct FixedTools {
    tools: Vec<(&'static str, &'static str)>,
}

impl FixedTools {
    fn new(tools: Vec<(&'static str, &'static str)>) -> Arc<Self> {
        Arc::new(Self { tools })
    }

    fn definition(id: &str, name: &str) -> lash_core::ToolDefinition {
        lash_core::ToolDefinition::raw(
            id,
            name,
            "fixed fig3367 fixture tool",
            lash_core::ToolDefinition::default_input_schema(),
            json!({ "type": "object", "additionalProperties": true }),
        )
    }
}

#[async_trait::async_trait]
impl lash_core::ToolProvider for FixedTools {
    fn tool_manifests(&self) -> Vec<lash_core::ToolManifest> {
        self.tools
            .iter()
            .map(|(id, name)| Self::definition(id, name).manifest())
            .collect()
    }

    fn resolve_contract(&self, name: &str) -> Option<Arc<lash_core::ToolContract>> {
        self.tools
            .iter()
            .find(|(_, tool_name)| *tool_name == name)
            .map(|(id, tool_name)| Arc::new(Self::definition(id, tool_name).contract()))
    }

    async fn execute(&self, call: lash_core::ToolCall<'_>) -> lash_core::ToolAttemptOutcome {
        lash_core::ToolOutcome::ok(json!({ "name": call.name() })).into()
    }
}

fn plugin_host_with_tools(
    tools: Option<Arc<dyn lash_core::ToolProvider>>,
) -> Arc<lash_core::facade_support::PluginHost> {
    let mut factories = lash_core::testing::test_standard_protocol_factories();
    if let Some(tools) = tools {
        factories.push(Arc::new(StaticPluginFactory::new(
            "fig3367_tools",
            lash_core::facade_support::PluginSpec::new().with_tool_provider(tools),
        )));
    }
    Arc::new(lash_core::testing::test_plugin_host(factories))
}

fn state_for(session_id: &SessionId) -> RuntimeSessionState {
    RuntimeSessionState {
        session_id: SessionId::from(session_id.to_string()),
        policy: standard_test_policy(),
        ..RuntimeSessionState::new(lash_core::SessionPolicy::new(
            lash_core::TurnBudget::Unbounded,
        ))
    }
}

fn environment(
    tools: Option<Arc<dyn lash_core::ToolProvider>>,
    policy: lash_core::ToolSourcePolicy,
) -> lash_core::facade_support::RuntimeEnvironment {
    lash_core::facade_support::RuntimeEnvironment::builder(
        lash_core::CommitBudget::bounded(1024 * 1024, 512),
        lash_core::QueuedWorkBatchingConfig::new(1),
    )
    .with_plugin_host(plugin_host_with_tools(tools))
    .with_runtime_host_config(test_host_config().core)
    .with_session_store_factory(Arc::new(
        lash_core::facade_support::InMemorySessionStoreFactory::new(),
    ))
    .with_tool_source_policy(policy)
    .build()
}

fn owner(label: &str) -> lash_core::LeaseOwnerIdentity {
    lash_core::LeaseOwnerIdentity::opaque(format!("fig3367-{label}"), "fig3367-boot")
}

async fn open_runtime(
    session_id: &SessionId,
    store: &Arc<dyn lash_core::RuntimePersistence>,
    tools: Option<Arc<dyn lash_core::ToolProvider>>,
    policy: lash_core::ToolSourcePolicy,
) -> Result<LashRuntime, lash_core::SessionError> {
    let env = environment(tools, policy);
    // Mirror what the facade's `open()` does: admitted load first (which
    // claims and releases the Session Execution Lease), then build the runtime
    // on the loaded state. Passing a default state instead would restore
    // nothing and make every assertion below vacuous.
    let loaded = lash_core::store::load_persisted_session_admitted(
        store.as_ref(),
        session_id,
        &owner(session_id.as_str()),
        &uuid::Uuid::new_v4().to_string(),
        env.core.control.lease_timings.ttl_ms(),
    )
    .await
    .map_err(|error| lash_core::SessionError::Store {
        context: format!("failed to load session `{session_id}`"),
        source: error,
    })?;
    let state = loaded.map_or_else(|| state_for(session_id), |loaded| loaded.state);
    Box::pin(LashRuntime::from_environment(
        &env,
        standard_test_policy(),
        state,
        Some(Arc::clone(store)),
        owner(session_id.as_str()),
    ))
    .await
}

/// Read tool state the way a host without a runtime does: straight off the
/// durable head. An `open()` reconciles the live surface in memory and would
/// show a tool whether or not anything was persisted.
async fn persisted_tool_state(
    store: &Arc<dyn lash_core::RuntimePersistence>,
) -> lash_core::ToolState {
    lash_core::store::load_persisted_session_state(store.as_ref())
        .await
        .expect("load persisted state")
        .expect("persisted session state")
        .tool_state_snapshot()
        .cloned()
        .expect("persisted tool state")
}

fn in_memory_store() -> Arc<dyn lash_core::RuntimePersistence> {
    Arc::new(lash_core::facade_support::InMemorySessionStore::default())
}

fn both_tools() -> Arc<dyn lash_core::ToolProvider> {
    FixedTools::new(vec![(ALPHA_ID, ALPHA_NAME), (BETA_ID, BETA_NAME)])
}

/// The FIG-3353 sequence, end to end: a commit taken while every tool is
/// orphaned does not persist them as non-members for good.
///
/// open with source → open on a core without it (Tolerate) → commit →
/// open with the source again. Between the grantless commit and the final
/// reopen the durable snapshot is read *without a runtime*, because that is
/// the only reading that shows what was actually written.
#[tokio::test]
async fn fig3353_sequence_keeps_curation_across_an_orphaned_commit() {
    let session_id = SessionId::from("fig3367-sequence");
    let store = in_memory_store();

    // Step 1: open with the source and record a deliberate opt-out of beta.
    let mut granted = open_runtime(
        &session_id,
        &store,
        Some(both_tools()),
        lash_core::ToolSourcePolicy::Tolerate,
    )
    .await
    .expect("granted open");
    assert!(
        granted.tool_restore_report().is_none(),
        "a first open has no persisted tool state to install"
    );
    let mut curated = granted.tool_state().expect("live tool state");
    curated
        .set_membership(&lash_core::ToolId::from(BETA_ID), false)
        .expect("opt out beta");
    Box::pin(granted.apply_tool_state(curated))
        .await
        .expect("apply the opt-out");
    let generation_before_orphaning = granted.tool_state().expect("tool state").generation();
    Box::pin(granted.park())
        .await
        .expect("park the granted open");

    let seeded = persisted_tool_state(&store).await;
    assert_eq!(seeded.generation(), generation_before_orphaning);
    assert!(
        seeded
            .get(&lash_core::ToolId::from(ALPHA_ID))
            .is_some_and(|entry| entry.is_member() && !entry.is_orphaned()),
        "alpha starts as a bound catalog member"
    );
    assert!(
        seeded
            .get(&lash_core::ToolId::from(BETA_ID))
            .is_some_and(|entry| !entry.member && !entry.is_orphaned()),
        "beta starts as a bound opt-out"
    );

    // Step 2: open on a core that does not carry the source. Tolerate, so the
    // session opens and the report names the loss.
    let grantless = open_runtime(
        &session_id,
        &store,
        None,
        lash_core::ToolSourcePolicy::Tolerate,
    )
    .await
    .expect("grantless open succeeds under Tolerate");
    let report = grantless
        .tool_restore_report()
        .expect("the open delivers its report to the host")
        .clone();
    assert_eq!(
        report.lost_members,
        vec![lash_core::ToolId::from(ALPHA_ID)],
        "the curated member is the only lost capability"
    );
    assert_eq!(
        report.parked_opt_outs,
        vec![lash_core::ToolId::from(BETA_ID)],
        "an unresolved tool the host already opted out of is not loss"
    );
    assert!(report.superseded_identities.is_empty());
    assert_eq!(
        report.generation,
        generation_before_orphaning + 1,
        "orphaning both entries changed the surface, so the restore bumped once"
    );
    assert!(
        !grantless
            .tool_state()
            .expect("grantless tool state")
            .tool_manifests()
            .iter()
            .any(|manifest| manifest.name == ALPHA_NAME),
        "an orphan is not a catalog member while its source is gone"
    );

    // Step 3: commit in that grantless state. A host append is a real durable
    // commit taken while every tool is orphaned — FIG-3353's exact question.
    let mut grantless = grantless;
    grantless.stamp_live_plugin_state();
    Box::pin(
        grantless.append_session_nodes(lash_core::AppendSessionNodesRequest {
            operation_id: "fig3367-grantless-commit".to_string(),
            nodes: vec![lash_core::SessionAppendNode::message(
                lash_core::PluginMessage::text(
                    lash_core::session_model::MessageRole::Assistant,
                    "committed while every tool is orphaned",
                ),
            )],
            requires_ancestor_node_id: None,
        }),
    )
    .await
    .expect("commit while orphaned");
    Box::pin(grantless.park())
        .await
        .expect("park the grantless open");

    // The durable trace of the orphaning, read with no runtime in the way.
    let orphaned_snapshot = persisted_tool_state(&store).await;
    assert_eq!(
        orphaned_snapshot.generation(),
        generation_before_orphaning + 1,
        "the grantless commit persisted the bumped generation"
    );
    let alpha = orphaned_snapshot
        .get(&lash_core::ToolId::from(ALPHA_ID))
        .expect("alpha survives the grantless commit");
    assert!(
        alpha.is_orphaned(),
        "the persisted snapshot between the grantless commit and the reopen carries the orphan flag"
    );
    assert!(
        alpha.member,
        "the host's curation bit survives orphaning: membership is derived, never rewritten"
    );
    let beta = orphaned_snapshot
        .get(&lash_core::ToolId::from(BETA_ID))
        .expect("beta survives the grantless commit");
    assert!(beta.is_orphaned(), "beta is orphaned too");
    assert!(!beta.member, "and its opt-out is still an opt-out");

    // Step 4: the source returns.
    let regranted = open_runtime(
        &session_id,
        &store,
        Some(both_tools()),
        lash_core::ToolSourcePolicy::Tolerate,
    )
    .await
    .expect("regranted open");
    let regranted_report = regranted
        .tool_restore_report()
        .expect("the reopen reports too");
    assert!(
        regranted_report.is_clean(),
        "every persisted id resolves again: {regranted_report:?}"
    );
    assert_eq!(
        regranted_report.generation,
        generation_before_orphaning + 2,
        "exactly two restores changed the surface, so the generation advanced by exactly two"
    );
    let regranted_state = regranted.tool_state().expect("regranted tool state");
    let alpha = regranted_state
        .get(&lash_core::ToolId::from(ALPHA_ID))
        .expect("alpha rebound");
    assert!(
        !alpha.is_orphaned() && alpha.is_member(),
        "alpha is a catalog member again"
    );
    let beta = regranted_state
        .get(&lash_core::ToolId::from(BETA_ID))
        .expect("beta rebound");
    assert!(
        !beta.is_orphaned() && !beta.member,
        "the opt-out made before the grantless open is still an opt-out"
    );
}

/// A provider that replaced a tool with a new id under the same model-facing
/// name is a superseded identity, not a loss — and Require does not refuse it.
#[tokio::test]
async fn alias_replacement_is_reported_as_superseded_and_never_refuses() {
    let session_id = SessionId::from("fig3367-superseded");
    let store = in_memory_store();

    let granted = open_runtime(
        &session_id,
        &store,
        Some(FixedTools::new(vec![(ALPHA_ID, ALPHA_NAME)])),
        lash_core::ToolSourcePolicy::Tolerate,
    )
    .await
    .expect("granted open");
    Box::pin(granted.park()).await.expect("park");

    let replaced = open_runtime(
        &session_id,
        &store,
        Some(FixedTools::new(vec![(REPLACEMENT_ID, ALPHA_NAME)])),
        lash_core::ToolSourcePolicy::Require,
    )
    .await
    .expect("a replaced identity never refuses, even under Require");
    let report = replaced.tool_restore_report().expect("report");
    assert!(report.lost_members.is_empty());
    assert!(report.parked_opt_outs.is_empty());
    assert_eq!(report.superseded_identities.len(), 1);
    let superseded = &report.superseded_identities[0];
    assert_eq!(superseded.retired_id, lash_core::ToolId::from(ALPHA_ID));
    assert_eq!(superseded.live_id, lash_core::ToolId::from(REPLACEMENT_ID));
    assert_eq!(superseded.name, ALPHA_NAME);
    assert!(
        replaced
            .tool_state()
            .expect("tool state")
            .get(&lash_core::ToolId::from(REPLACEMENT_ID))
            .is_some_and(lash_core::facade_support::ToolStateEntry::is_member),
        "the replacement is a default member"
    );
}

/// A session whose only unresolved tools are opt-outs has lost nothing, so
/// Require opens it.
#[tokio::test]
async fn an_opt_out_only_snapshot_does_not_refuse_under_require() {
    let session_id = SessionId::from("fig3367-opt-out-only");
    let store = in_memory_store();

    let mut granted = open_runtime(
        &session_id,
        &store,
        Some(FixedTools::new(vec![(BETA_ID, BETA_NAME)])),
        lash_core::ToolSourcePolicy::Tolerate,
    )
    .await
    .expect("granted open");
    let mut curated = granted.tool_state().expect("tool state");
    curated
        .set_membership(&lash_core::ToolId::from(BETA_ID), false)
        .expect("opt out beta");
    Box::pin(granted.apply_tool_state(curated))
        .await
        .expect("apply the opt-out");
    Box::pin(granted.park()).await.expect("park");

    let strict = open_runtime(
        &session_id,
        &store,
        None,
        lash_core::ToolSourcePolicy::Require,
    )
    .await
    .expect("an opt-out-only snapshot opens under Require");
    let report = strict.tool_restore_report().expect("report");
    assert!(
        report.lost_members.is_empty(),
        "an opt-out is not a lost member"
    );
    assert_eq!(
        report.parked_opt_outs,
        vec![lash_core::ToolId::from(BETA_ID)]
    );
}

/// Direct construction under Require refuses with the typed error, and the
/// refusal carries the report.
#[tokio::test]
async fn require_refuses_a_direct_construction_that_lost_a_member() {
    let session_id = SessionId::from("fig3367-require-direct");
    let store = in_memory_store();

    let granted = open_runtime(
        &session_id,
        &store,
        Some(FixedTools::new(vec![(ALPHA_ID, ALPHA_NAME)])),
        lash_core::ToolSourcePolicy::Tolerate,
    )
    .await
    .expect("granted open");
    Box::pin(granted.park()).await.expect("park");
    let head_before = lash_core::store::load_persisted_session_state(store.as_ref())
        .await
        .expect("load head")
        .expect("state")
        .head_revision;

    let refusal = match open_runtime(
        &session_id,
        &store,
        None,
        lash_core::ToolSourcePolicy::Require,
    )
    .await
    {
        Ok(_) => panic!("Require must refuse an open that lost a catalog member"),
        Err(refusal) => refusal,
    };
    match &refusal {
        lash_core::SessionError::ToolSourcesUnavailable { report, .. } => {
            assert_eq!(report.lost_members, vec![lash_core::ToolId::from(ALPHA_ID)]);
        }
        other => panic!("expected a typed tool-source refusal, got {other:?}"),
    }

    // The refusal's promises: nothing was committed, and the next open can
    // still take the Session Execution Lease.
    let head_after = lash_core::store::load_persisted_session_state(store.as_ref())
        .await
        .expect("load head")
        .expect("state")
        .head_revision;
    assert_eq!(
        head_after, head_before,
        "a refused open commits no config or state"
    );
    let reopened = open_runtime(
        &session_id,
        &store,
        Some(FixedTools::new(vec![(ALPHA_ID, ALPHA_NAME)])),
        lash_core::ToolSourcePolicy::Require,
    )
    .await
    .expect("a following open acquires the lease the refusal released");
    Box::pin(reopened.park()).await.expect("park");
}

/// Resume rebuilds a runtime through the same install owner, so it honours the
/// policy the environment carries.
#[tokio::test]
async fn require_refuses_a_resume_that_lost_a_member() {
    let session_id = SessionId::from("fig3367-require-resume");
    let store = in_memory_store();

    let granted = open_runtime(
        &session_id,
        &store,
        Some(FixedTools::new(vec![(ALPHA_ID, ALPHA_NAME)])),
        lash_core::ToolSourcePolicy::Tolerate,
    )
    .await
    .expect("granted open");
    let parked = Box::pin(granted.park()).await.expect("park");

    let grantless_env = environment(None, lash_core::ToolSourcePolicy::Require);

    let refusal =
        match LashRuntime::resume(parked, &grantless_env, owner(session_id.as_str())).await {
            Ok(_) => panic!("Require must refuse a resume that lost a catalog member"),
            Err(refusal) => refusal,
        };
    assert!(
        matches!(
            refusal,
            lash_core::SessionError::ToolSourcesUnavailable { .. }
        ),
        "expected a typed tool-source refusal, got {refusal:?}"
    );
}

/// A managed child that inherits a parent snapshot naming a tool the live
/// surface no longer advertises is refused under Require, at creation.
///
/// The child is the construction the FIG-3367 inventory calls out as inheriting
/// a snapshot (Current, Existing and Snapshot start points do; Empty does not),
/// so the policy has to reach it below the facade.
#[tokio::test]
async fn require_refuses_a_managed_child_whose_inherited_snapshot_lost_a_member() {
    let surface = MutableTools::new(vec![(ALPHA_ID, ALPHA_NAME)]);
    let tools: Arc<dyn lash_core::ToolProvider> =
        Arc::clone(&surface) as Arc<dyn lash_core::ToolProvider>;
    let plugin_host = plugin_host_with_tools(Some(tools));
    let plugin_session = plugin_host
        .build_session("fig3367-child-parent")
        .expect("plugins");
    let mut host = test_host_config();
    host.core.control.tool_source_policy = lash_core::ToolSourcePolicy::Require;
    let mut runtime = Box::pin(LashRuntime::from_embedded_state(
        standard_test_policy(),
        host,
        lash_core::testing::runtime_internals::RuntimeServices::new(plugin_session),
        RuntimeSessionState {
            session_id: SessionId::from("fig3367-child-parent"),
            policy: standard_test_policy(),
            ..RuntimeSessionState::new(lash_core::SessionPolicy::new(
                lash_core::TurnBudget::Unbounded,
            ))
        },
        lash_core::testing::runtime_lease_owner(),
    ))
    .await
    .expect("parent runtime");
    set_runtime_provider(&mut runtime, mock_provider(Vec::new()).into_handle());
    // The parent's snapshot records the tool while its source is live.
    runtime.stamp_live_plugin_state();
    assert!(
        runtime
            .state
            .tool_state_snapshot()
            .is_some_and(|snapshot| snapshot.contains(&lash_core::ToolId::from(ALPHA_ID))),
        "precondition: the inherited snapshot names the tool"
    );

    // The source goes away; the child inherits a snapshot nothing resolves.
    surface.clear();

    let lifecycle = runtime
        .session_lifecycle_service()
        .expect("session lifecycle");
    let plugin_init = runtime
        .session_state_service()
        .expect("session state")
        .session_plugin_init(&lash_core::SessionId::from("fig3367-child-parent"))
        .await
        .expect("plugin init");
    let refusal = match lifecycle
        .create_session(
            lash_core::SessionCreateRequest::child_session(
                "fig3367-child-parent",
                lash_core::SessionStartPoint::Empty,
                lash_core::PluginOptions::default(),
            )
            .with_session_id("fig3367-child")
            .with_plugin_source(lash_core::SessionPluginSource::ParentFork)
            .with_plugin_init(plugin_init),
        )
        .await
    {
        Ok(_) => panic!("Require must refuse a child whose inherited snapshot lost a member"),
        Err(error) => error.to_string(),
    };
    assert!(
        refusal.contains(ALPHA_ID),
        "the child's refusal names the lost tool: {refusal}"
    );
}

/// A source whose advertised set can be emptied while the runtime lives.
struct MutableTools {
    tools: Mutex<Vec<(&'static str, &'static str)>>,
}

impl MutableTools {
    fn new(tools: Vec<(&'static str, &'static str)>) -> Arc<Self> {
        Arc::new(Self {
            tools: Mutex::new(tools),
        })
    }

    fn clear(&self) {
        self.tools.lock_recover().clear();
    }
}

#[async_trait::async_trait]
impl lash_core::ToolProvider for MutableTools {
    fn tool_manifests(&self) -> Vec<lash_core::ToolManifest> {
        self.tools
            .lock_recover()
            .iter()
            .map(|(id, name)| FixedTools::definition(id, name).manifest())
            .collect()
    }

    fn resolve_contract(&self, _name: &str) -> Option<Arc<lash_core::ToolContract>> {
        None
    }

    async fn execute(&self, _call: lash_core::ToolCall<'_>) -> lash_core::ToolAttemptOutcome {
        lash_core::ToolOutcome::ok(json!({})).into()
    }
}

/// A live runtime on a **Require** core, holding a snapshot that names alpha,
/// with alpha's source gone. Everything a live install could be asked to do
/// happens from here.
async fn live_require_runtime(
    session_id: &SessionId,
    store: &Arc<dyn lash_core::RuntimePersistence>,
) -> (LashRuntime, Arc<MutableTools>, lash_core::ToolState) {
    let surface = MutableTools::new(vec![(ALPHA_ID, ALPHA_NAME)]);
    let tools: Arc<dyn lash_core::ToolProvider> =
        Arc::clone(&surface) as Arc<dyn lash_core::ToolProvider>;
    let mut runtime = open_runtime(
        session_id,
        store,
        Some(tools),
        lash_core::ToolSourcePolicy::Require,
    )
    .await
    .expect("the open itself has nothing to lose yet");
    runtime.stamp_live_plugin_state();
    let snapshot = runtime.tool_state().expect("live tool state");
    assert!(
        snapshot.contains(&lash_core::ToolId::from(ALPHA_ID)),
        "precondition: the snapshot names the tool while its source is live"
    );
    surface.clear();
    (runtime, surface, snapshot)
}

/// `restore_tool_state` is a host asking a session it already holds to install
/// a snapshot. Under Require it still succeeds: the policy governs opening a
/// session, not a restore onto a live one. Refusing here would leave the
/// registry reconciled, the catalog stale and the report unretained.
#[tokio::test]
async fn a_host_restore_on_a_require_core_reports_instead_of_refusing() {
    let session_id = SessionId::from("fig3367-live-host-restore");
    let store = in_memory_store();
    let (mut runtime, _surface, snapshot) = live_require_runtime(&session_id, &store).await;

    let report = Box::pin(runtime.restore_tool_state(snapshot))
        .await
        .expect("a live host restore never refuses, whatever the open policy is");
    assert_eq!(
        report.lost_members,
        vec![lash_core::ToolId::from(ALPHA_ID)],
        "the caller is handed the loss"
    );
    assert_eq!(
        runtime
            .tool_restore_report()
            .expect("the live runtime retains the report")
            .lost_members,
        vec![lash_core::ToolId::from(ALPHA_ID)],
    );
    // The steps after the install ran: the catalog no longer serves the
    // orphan, and the stamped snapshot carries the restore's generation.
    assert!(
        !runtime
            .active_tool_catalog_shared()
            .expect("active catalog")
            .iter()
            .any(|entry| entry.get("name").and_then(serde_json::Value::as_str) == Some(ALPHA_NAME)),
        "the tool catalog was refreshed against the reconciled registry"
    );
    assert_eq!(
        runtime
            .state
            .tool_state_snapshot()
            .expect("stamped snapshot")
            .generation(),
        report.generation,
        "stamp_live_plugin_state ran after the install"
    );
}

/// A mid-turn resident re-sync whose source went away degrades the session; it
/// does not fail the reload. This is exactly the "the MCP server is down" case
/// Tolerate-by-default exists for, and it must not become reachable on a
/// Require core at a moment nobody chose to open anything.
#[tokio::test]
async fn a_resident_resync_on_a_require_core_reloads_and_reports() {
    let session_id = SessionId::from("fig3367-live-resync");
    let store = in_memory_store();
    let (mut runtime, _surface, _snapshot) = live_require_runtime(&session_id, &store).await;
    // The durable head names the tool; the live surface no longer does.
    Box::pin(
        runtime.append_session_nodes(lash_core::AppendSessionNodesRequest {
            operation_id: "fig3367-live-resync-commit".to_string(),
            nodes: vec![lash_core::SessionAppendNode::message(
                lash_core::PluginMessage::text(
                    lash_core::session_model::MessageRole::Assistant,
                    "committed before the source went away",
                ),
            )],
            requires_ancestor_node_id: None,
        }),
    )
    .await
    .expect("commit a durable head carrying the tool");

    lash_core::testing::invalidate_resident_session_state_for_testing(&mut runtime);
    Box::pin(runtime.reload_invalidated_resident_session_state_for_session())
        .await
        .expect("a lost tool source must not fail an invalidated resident reload");

    assert_eq!(
        runtime
            .tool_restore_report()
            .expect("the re-sync leaves its report where the host reads it")
            .lost_members,
        vec![lash_core::ToolId::from(ALPHA_ID)],
    );
}

/// Installing a persisted state envelope onto a live runtime is the same
/// reading: tolerate, retain, report.
#[tokio::test]
async fn a_persisted_state_install_on_a_require_core_reports_instead_of_refusing() {
    let session_id = SessionId::from("fig3367-live-state-install");
    let store = in_memory_store();
    let (mut runtime, _surface, _snapshot) = live_require_runtime(&session_id, &store).await;

    let state = runtime.export_persistence_state();
    runtime
        .apply_persistence_state(state)
        .expect("a live persisted-state install never refuses");

    assert_eq!(
        runtime
            .tool_restore_report()
            .expect("the install leaves its report where the host reads it")
            .lost_members,
        vec![lash_core::ToolId::from(ALPHA_ID)],
    );
}
