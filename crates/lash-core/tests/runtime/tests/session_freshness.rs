use super::*;
use lash_core::SessionCommitStore as _;
use lash_core::testing::TestTurnDrive as _;

const SEED: u64 = 0x5_f502;

async fn freshness_runtime(
    double: &lash_restate_test::RestateTestBackend,
) -> (LashRuntime, Arc<RecordingStore>) {
    let backend = double.lash_backend();
    let store = double_unbound_recording_store(double).await;
    let runtime = runtime_with_plugins_and_tools_and_host_and_store(
        vec![Arc::new(DialectOwner)],
        Arc::new(EmptyTools),
        mock_provider(Vec::new()),
        test_host_config(&backend),
        store.clone() as Arc<dyn lash_core::RuntimeStore>,
    )
    .await;
    (runtime, store)
}

/// A plugin that owns the `dialect_owner` config namespace and admits any
/// dialect through its one command (FIG-4379).
struct DialectOwner;

impl lash_core::plugin::PluginFactory for DialectOwner {
    fn id(&self) -> &'static str {
        "dialect_owner"
    }

    fn build(
        &self,
        _ctx: &lash_core::plugin::PluginSessionContext,
    ) -> Result<Arc<dyn lash_core::plugin::SessionPlugin>, lash_core::PluginError> {
        Ok(Arc::new(DialectOwnerPlugin))
    }

    fn register_config(
        &self,
        registrar: &mut lash_core::ConfigRegistrar,
    ) -> Result<(), lash_core::ConfigRegistrationError> {
        registrar.owner(DialectConfigOwner)?;
        registrar.command::<SetDialect>(|_, command| {
            Ok(lash_core::OwnerChange {
                recorded: DialectConfig {
                    dialect: Some(command.dialect),
                },
                output: (),
            })
        })
    }
}

/// The `dialect_owner` namespace.
#[derive(
    Clone,
    Debug,
    Default,
    serde::Serialize,
    serde::Deserialize,
    lash_core::facade_support::JsonSchema,
)]
#[schemars(crate = "lash_core::facade_support::schemars")]
#[serde(deny_unknown_fields)]
struct DialectConfig {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    dialect: Option<String>,
}

/// The `dialect_owner` owner refuses nothing.
#[derive(serde::Serialize, serde::Deserialize, lash_core::facade_support::JsonSchema)]
#[schemars(crate = "lash_core::facade_support::schemars")]
enum DialectRefusal {}

impl std::fmt::Display for DialectRefusal {
    fn fmt(&self, _formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match *self {}
    }
}

struct DialectConfigOwner;

impl lash_core::ConfigOwner for DialectConfigOwner {
    type Create = DialectConfig;
    type Recorded = DialectConfig;
    type Refusal = DialectRefusal;

    fn implementation(&self) -> &str {
        "dialect-owner:1"
    }

    fn create(
        &self,
        input: Option<DialectConfig>,
        _facts: lash_core::CreationFacts<'_, DialectConfig>,
    ) -> Result<Option<DialectConfig>, DialectRefusal> {
        Ok(Some(input.unwrap_or_default()))
    }

    fn validate(
        &self,
        _value: &DialectConfig,
        _base: Option<&DialectConfig>,
        _facts: &lash_core::CandidateFacts<'_>,
    ) -> Result<(), DialectRefusal> {
        Ok(())
    }
}

/// Replace the session's dialect.
#[derive(
    Clone, Debug, serde::Serialize, serde::Deserialize, lash_core::facade_support::JsonSchema,
)]
#[schemars(crate = "lash_core::facade_support::schemars")]
#[serde(deny_unknown_fields)]
struct SetDialect {
    dialect: String,
}

impl lash_core::ConfigCommand for SetDialect {
    type Owner = DialectConfigOwner;
    type Output = ();
    const NAME: &'static str = "set_dialect";
}

struct DialectOwnerPlugin;

impl lash_core::plugin::SessionPlugin for DialectOwnerPlugin {
    fn id(&self) -> &'static str {
        "dialect_owner"
    }

    fn register(
        &self,
        _reg: &mut lash_core::plugin::PluginRegistrar,
    ) -> Result<(), lash_core::PluginError> {
        Ok(())
    }
}

async fn append_history(
    runtime: &mut LashRuntime,
    double: &lash_restate_test::RestateTestBackend,
    depth: usize,
) {
    assert!(depth >= 1, "the initial frame is the first history node");
    if depth == 1 {
        return;
    }
    Box::pin(crate::runtime_support::apply_host_append(
        runtime,
        double,
        lash_core::AppendSessionNodesRequest {
            operation_id: format!("freshness-depth-{depth}"),
            nodes: (1..depth)
                .map(|ordinal| {
                    lash_core::SessionAppendNode::plugin(
                        "freshness-depth",
                        serde_json::json!({ "ordinal": ordinal }),
                    )
                })
                .collect(),
            requires_ancestor_node_id: None,
        },
    ))
    .await;
    assert_eq!(runtime.state().session_graph.nodes.len(), depth);
}

/// Open the frame `material` keys as a host's frame open, a session command
/// the runtime's next drive applies (FIG-4202).
async fn open_frame(
    runtime: &mut LashRuntime,
    double: &lash_restate_test::RestateTestBackend,
    material: &str,
) -> lash_core::runtime::OpenAgentFrameCommandOutcome {
    let command = lash_core::runtime::SessionCommand::OpenAgentFrame {
        request: Box::new(
            lash_core::testing::runtime_internals::OpenAgentFrameRequest::new(
                lash_core::FrameKey::from_caller_material(material)
                    .expect("non-empty frame material"),
                lash_core::AgentFrameReason::new("test"),
            ),
        ),
    };
    match Box::pin(crate::runtime_support::apply_host_command(
        runtime, double, command, material,
    ))
    .await
    {
        lash_core::runtime::SessionCommandOutcome::OpenAgentFrame { outcome } => outcome,
        other => panic!("a frame open settles with its own outcome: {other:?}"),
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn historical_frame_switch_refuses_and_keeps_resident_config() {
    let double = kernel_double(SEED, lash_restate_test::ServerConfig::default()).await;
    let (mut runtime, store) = Box::pin(freshness_runtime(&double)).await;
    let lash_core::runtime::OpenAgentFrameCommandOutcome::Opened { outcome: opened } =
        Box::pin(open_frame(&mut runtime, &double, "changed-policy-frame")).await
    else {
        panic!("open a second agent frame");
    };
    assert!(opened.opened, "the second frame must be newly opened");

    let changed_model = lash_core::ModelSpec::builder("changed-frame-model")
        .context_window_tokens(123_456)
        .build()
        .expect("changed model");
    crate::runtime_support::configure(
        &mut runtime,
        &double,
        lash_core::ConfigTransaction::of(lash_core::plugin::config::core::SetModel {
            model: changed_model.clone(),
        }),
        "changed-frame-model",
    )
    .await;
    assert_eq!(runtime.state().effective_policy().model, changed_model);

    let resident_policy_before_refusal = runtime.state().effective_policy().clone();
    let resident_plugin_config_before_refusal = runtime.state().authority.plugin_config.clone();
    let resident_frame_before_refusal = runtime.state().current_frame_node_id.clone();
    let durable_head_before_refusal = session_view(store.clone(), "root")
        .load_session_head_meta()
        .await
        .expect("load durable head before historical-frame refusal");

    let lash_core::runtime::OpenAgentFrameCommandOutcome::Refused { code, .. } =
        Box::pin(open_frame(&mut runtime, &double, "initial-frame")).await
    else {
        panic!("switching to a pre-existing historical frame must refuse");
    };

    assert_eq!(
        code,
        lash_core::RuntimeErrorCode::HistoricalAgentFrameSwitchUnsupported
    );
    assert_eq!(
        runtime.state().effective_policy(),
        &resident_policy_before_refusal
    );
    assert_eq!(
        runtime.state().authority.plugin_config,
        resident_plugin_config_before_refusal
    );
    assert_eq!(
        runtime.state().current_frame_node_id,
        resident_frame_before_refusal
    );
    let durable_head_after_refusal = session_view(store.clone(), "root")
        .load_session_head_meta()
        .await
        .expect("load durable head after historical-frame refusal");
    // The open is a session command (FIG-4202): its refusal settles in one
    // commit, which completes the command and moves nothing else.
    assert_eq!(
        durable_head_after_refusal
            .as_ref()
            .map(|head| head.head_revision),
        durable_head_before_refusal
            .as_ref()
            .map(|head| head.head_revision + 1)
    );
    assert_eq!(
        durable_head_after_refusal
            .as_ref()
            .and_then(|head| head.current_frame_node_id.as_ref()),
        durable_head_before_refusal
            .as_ref()
            .and_then(|head| head.current_frame_node_id.as_ref())
    );
    assert_eq!(
        durable_head_after_refusal
            .as_ref()
            .and_then(|head| head.leaf_node_id.as_deref()),
        durable_head_before_refusal
            .as_ref()
            .and_then(|head| head.leaf_node_id.as_deref())
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn unchanged_session_freshness_is_independent_of_history_depth() {
    for depth in [10, 256] {
        let double = kernel_double(SEED + 1, lash_restate_test::ServerConfig::default()).await;
        let (mut runtime, store) = Box::pin(freshness_runtime(&double)).await;
        Box::pin(append_history(&mut runtime, &double, depth)).await;
        let head_reads_before = store.load_session_head_meta_count();
        let full_loads_before = store.load_session_count();

        runtime
            .refresh_session_graph_from_store()
            .await
            .expect("refresh unchanged session");

        assert_eq!(
            store.load_session_count() - full_loads_before,
            0,
            "unchanged freshness must not hydrate the session at depth {depth}"
        );
        assert_eq!(
            store.load_session_head_meta_count() - head_reads_before,
            1,
            "freshness must read exactly one head projection at depth {depth}"
        );
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn freshness_falls_back_to_full_read_when_head_is_indeterminate() {
    let double = kernel_double(SEED + 2, lash_restate_test::ServerConfig::default()).await;
    let (mut runtime, store) = Box::pin(freshness_runtime(&double)).await;
    Box::pin(append_history(&mut runtime, &double, 2)).await;
    let head_reads_before = store.load_session_head_meta_count();
    let full_loads_before = store.load_session_count();
    runtime.resident_session.mark_graph_head_stale();
    store.fail_next_load_session_head_meta();

    runtime
        .refresh_session_graph_from_store()
        .await
        .expect("an indeterminate head must fall back to the full session read");

    assert_eq!(
        store.load_session_head_meta_count() - head_reads_before,
        1,
        "freshness must attempt the bounded head projection first"
    );
    assert_eq!(
        store.load_session_count() - full_loads_before,
        1,
        "a failed head projection must not be treated as a fresh session"
    );
    assert!(!runtime.resident_session.graph_head_is_stale());
}

#[tokio::test(flavor = "multi_thread")]
async fn freshness_hydrates_when_revision_changed() {
    let double = kernel_double(SEED + 3, lash_restate_test::ServerConfig::default()).await;
    let (mut runtime, store) = Box::pin(freshness_runtime(&double)).await;
    Box::pin(append_history(&mut runtime, &double, 2)).await;
    let head = advance_session_head(store.as_ref(), |_| {}).await;
    let full_loads_before = store.load_session_count();

    runtime
        .refresh_session_graph_from_store()
        .await
        .expect("refresh revision change");

    assert_eq!(store.load_session_count() - full_loads_before, 1);
    assert_eq!(runtime.state().head_revision, head.head_revision);
}

/// FIG-1875 (head-authoritative adoption): a resident refresh adopts the
/// durable head's prompt. Session config settles through the commanded
/// durable write (FIG-1555/FIG-1895), so the head already carries every
/// committed override — no resident copy is preserved across adoption.
#[tokio::test(flavor = "multi_thread")]
async fn resident_refresh_adopts_the_durable_head_prompt() {
    let double = kernel_double(SEED + 4, lash_restate_test::ServerConfig::default()).await;
    let (mut runtime, store) = Box::pin(freshness_runtime(&double)).await;
    Box::pin(append_history(&mut runtime, &double, 2)).await;
    crate::runtime_support::configure(
        &mut runtime,
        &double,
        lash_core::ConfigTransaction::of(lash_core::plugin::config::core::AddPromptContribution {
            contribution: lash_core::PromptContribution::guidance(
                "Settled host change",
                "COMMITTED THROUGH THE COMMANDED WRITE",
            ),
        }),
        "settled-prompt",
    )
    .await;

    let head_prompt = lash_core::PromptLayer::new().with_contribution(
        lash_core::PromptContribution::guidance("Advanced durable value", "THE HEAD WINS"),
    );
    advance_session_head(store.as_ref(), |state| {
        state.policy.prompt = head_prompt.clone();
    })
    .await;

    runtime
        .refresh_session_graph_from_store()
        .await
        .expect("refresh resident graph");

    assert_eq!(
        runtime.state().effective_policy().prompt,
        head_prompt,
        "adoption is head-authoritative: the durable head's prompt wins"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn prompt_helper_composes_with_reloaded_prompt_on_invalidated_resident_path() {
    let double = kernel_double(SEED + 5, lash_restate_test::ServerConfig::default()).await;
    let (mut runtime, store) = Box::pin(freshness_runtime(&double)).await;
    Box::pin(append_history(&mut runtime, &double, 2)).await;

    advance_session_head(store.as_ref(), |state| {
        state.policy.prompt = lash_core::PromptLayer::new().with_contribution(
            lash_core::PromptContribution::guidance("Durable base", "KEEP THE DURABLE PROMPT"),
        );
    })
    .await;
    runtime.invalidate_resident_session_state();

    crate::runtime_support::configure(
        &mut runtime,
        &double,
        lash_core::ConfigTransaction::of(lash_core::plugin::config::core::AddPromptContribution {
            contribution: lash_core::PromptContribution::guidance(
                "Live edit",
                "KEEP THE LIVE EDIT",
            ),
        }),
        "live-prompt-edit",
    )
    .await;

    assert_eq!(
        runtime.state().effective_policy().prompt,
        lash_core::PromptLayer::new()
            .with_contribution(lash_core::PromptContribution::guidance(
                "Durable base",
                "KEEP THE DURABLE PROMPT",
            ))
            .with_contribution(lash_core::PromptContribution::guidance(
                "Live edit",
                "KEEP THE LIVE EDIT",
            )),
        "the helper must edit the prompt reloaded by the config applier"
    );
}

/// FIG-1875 (head-authoritative adoption): a resident refresh adopts the
/// durable head's model — there is no live-model preservation carve-out.
#[tokio::test(flavor = "multi_thread")]
async fn resident_refresh_adopts_the_durable_head_model() {
    let double = kernel_double(SEED + 6, lash_restate_test::ServerConfig::default()).await;
    let (mut runtime, store) = Box::pin(freshness_runtime(&double)).await;
    Box::pin(append_history(&mut runtime, &double, 2)).await;
    let settled_model = lash_core::ModelSpec::builder("settled-live-model")
        .context_window_tokens(123_456)
        .build()
        .expect("settled model");
    crate::runtime_support::configure(
        &mut runtime,
        &double,
        lash_core::ConfigTransaction::of(lash_core::plugin::config::core::SetModel {
            model: settled_model.clone(),
        }),
        "settled-model",
    )
    .await;

    let head_model = lash_core::ModelSpec::builder("advanced-durable-model")
        .context_window_tokens(65_536)
        .build()
        .expect("advanced durable model");
    advance_session_head(store.as_ref(), |state| {
        state.policy.model = head_model.clone();
    })
    .await;

    runtime
        .refresh_session_graph_from_store()
        .await
        .expect("refresh resident graph");

    assert_eq!(
        runtime.state().effective_policy().model,
        head_model,
        "adoption is head-authoritative: the durable head's model wins"
    );
}

/// FIG-2987: the subagent context is the second input of the plugin catalog
/// projection, so adopting a durable head that changes it must publish it to
/// the live plugin session along with tool access.
#[tokio::test(flavor = "multi_thread")]
async fn resident_refresh_publishes_the_durable_head_subagent_context_to_live_plugins() {
    let double = kernel_double(SEED + 20, lash_restate_test::ServerConfig::default()).await;
    let (mut runtime, store) = Box::pin(freshness_runtime(&double)).await;
    Box::pin(append_history(&mut runtime, &double, 2)).await;
    let live_subagent = |runtime: &LashRuntime| {
        runtime
            .plugin_session()
            .expect("live plugin session")
            .subagent_context()
    };
    assert_eq!(runtime.state().authority.subagent, None);
    assert_eq!(live_subagent(&runtime), None);

    let head_subagent = lash_core::SubagentSessionContext {
        parent_session_id: SessionId::from("durable-parent"),
        capability: "durable-capability".to_string(),
        depth: 1,
        max_depth: 3,
    };
    advance_session_head(store.as_ref(), |state| {
        state.authority.subagent = Some(head_subagent.clone());
    })
    .await;
    runtime
        .refresh_session_graph_from_store()
        .await
        .expect("refresh resident graph");

    assert_eq!(
        runtime.state().authority.subagent.as_ref(),
        Some(&head_subagent),
        "adoption is head-authoritative: the durable head's subagent context wins"
    );
    assert_eq!(
        live_subagent(&runtime),
        Some(head_subagent),
        "the live plugin session must see the adopted subagent context"
    );
}

/// FIG-1875 (head-authoritative adoption): a resident refresh adopts the
/// durable head's provider id. The provider *resolver* stays live-owned — it
/// is not part of the durable head — but the recorded provider id is a
/// durable fact and the head wins on it.
#[tokio::test(flavor = "multi_thread")]
async fn resident_refresh_adopts_the_durable_head_provider_id() {
    let double = kernel_double(SEED + 7, lash_restate_test::ServerConfig::default()).await;
    let (mut runtime, store) = Box::pin(freshness_runtime(&double)).await;
    Box::pin(append_history(&mut runtime, &double, 2)).await;
    let settled_provider = TestProvider::builder()
        .kind("settled-live-provider")
        .complete_error("provider must not be called by refresh")
        .build()
        .into_handle();
    serve_runtime_providers(&mut runtime, [settled_provider.clone()]);
    crate::runtime_support::configure(
        &mut runtime,
        &double,
        lash_core::ConfigTransaction::of(lash_core::plugin::config::core::SetProvider {
            provider_id: settled_provider.kind().to_string(),
        }),
        "settled-provider",
    )
    .await;

    advance_session_head(store.as_ref(), |state| {
        state.policy.provider_id = "advanced-durable-provider".to_string();
    })
    .await;

    runtime
        .refresh_session_graph_from_store()
        .await
        .expect("refresh resident graph");

    assert_eq!(
        runtime.state().effective_policy().provider_id,
        "advanced-durable-provider",
        "adoption is head-authoritative: the durable head's provider id wins"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn freshness_hydrates_when_leaf_changed() {
    let double = kernel_double(SEED + 8, lash_restate_test::ServerConfig::default()).await;
    let (mut runtime, store) = Box::pin(freshness_runtime(&double)).await;
    Box::pin(append_history(&mut runtime, &double, 2)).await;
    let frame_node_id = runtime.state().session_graph.nodes[0].node_id.clone();
    let mut head = session_view(store.clone(), "root")
        .load_session_head_meta()
        .await
        .expect("read head")
        .expect("session head exists");
    assert_ne!(head.leaf_node_id.as_deref(), Some(frame_node_id.as_str()));
    head.leaf_node_id = Some(frame_node_id.clone());
    store.forge_session_head(head);
    let full_loads_before = store.load_session_count();

    runtime
        .refresh_session_graph_from_store()
        .await
        .expect("refresh leaf change");

    assert_eq!(store.load_session_count() - full_loads_before, 1);
    assert_eq!(
        runtime.state().session_graph.leaf_node_id.as_deref(),
        Some(frame_node_id.as_str())
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn freshness_hydrates_when_only_checkpoint_ref_changed() {
    let double = kernel_double(SEED + 9, lash_restate_test::ServerConfig::default()).await;
    let (mut runtime, store) = Box::pin(freshness_runtime(&double)).await;
    Box::pin(append_history(&mut runtime, &double, 2)).await;
    let mut head = session_view(store.clone(), "root")
        .load_session_head_meta()
        .await
        .expect("read head")
        .expect("session head exists");
    let original_revision = head.head_revision;
    let original_leaf = head.leaf_node_id.clone();
    let changed_checkpoint_ref: lash_core::BlobRef =
        "checkpoint-ref-only-change".to_string().into();
    assert_ne!(head.checkpoint_ref.as_ref(), Some(&changed_checkpoint_ref));
    head.checkpoint_ref = Some(changed_checkpoint_ref.clone());
    store.forge_session_head(head);
    let full_loads_before = store.load_session_count();

    runtime
        .refresh_session_graph_from_store()
        .await
        .expect("refresh checkpoint-ref-only change");

    assert_eq!(store.load_session_count() - full_loads_before, 1);
    assert_eq!(runtime.state().head_revision, original_revision);
    assert_eq!(runtime.state().session_graph.leaf_node_id, original_leaf);
    assert_eq!(
        runtime.state().checkpoint_ref.as_ref(),
        Some(&changed_checkpoint_ref)
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn freshness_skips_hydration_when_nothing_changed() {
    let double = kernel_double(SEED + 10, lash_restate_test::ServerConfig::default()).await;
    let (mut runtime, store) = Box::pin(freshness_runtime(&double)).await;
    Box::pin(append_history(&mut runtime, &double, 2)).await;
    let resident_head = (
        runtime.state().head_revision,
        runtime.state().session_graph.leaf_node_id.clone(),
        runtime.state().checkpoint_ref.clone(),
    );
    let full_loads_before = store.load_session_count();

    runtime
        .refresh_session_graph_from_store()
        .await
        .expect("refresh unchanged session");

    assert_eq!(store.load_session_count() - full_loads_before, 0);
    assert_eq!(
        (
            runtime.state().head_revision,
            runtime.state().session_graph.leaf_node_id.clone(),
            runtime.state().checkpoint_ref.clone(),
        ),
        resident_head
    );
}

fn commanded_dialect(dialect: &str) -> lash_core::ConfigTransaction {
    lash_core::ConfigTransaction::of(SetDialect {
        dialect: dialect.to_string(),
    })
}

/// FIG-2479, FIG-4379: a plugin's config command settles through the
/// commanded durable write — the session head accepts the owner's namespace
/// before resident state publishes it.
#[tokio::test(flavor = "multi_thread")]
async fn a_plugin_config_command_settles_through_the_commanded_write() {
    let double = kernel_double(SEED + 11, lash_restate_test::ServerConfig::default()).await;
    let (mut runtime, store) = Box::pin(freshness_runtime(&double)).await;
    let revision_before = runtime.state().config_revision;

    crate::runtime_support::configure(
        &mut runtime,
        &double,
        commanded_dialect("commanded-durable"),
        "plugin-config",
    )
    .await;

    let expected = serde_json::json!({ "dialect": "commanded-durable" });
    assert_eq!(
        runtime.state().authority.plugin_config.get("dialect_owner"),
        Some(&expected),
        "resident state must publish the settled value"
    );
    assert!(runtime.state().config_revision > revision_before);
    let head = session_view(store.clone(), "root")
        .load_session_head_meta()
        .await
        .expect("read durable head")
        .expect("session head exists");
    assert_eq!(
        head.config.plugin_config.get("dialect_owner"),
        Some(&expected),
        "the durable head must have accepted the value at settlement time"
    );
}

/// FIG-2479 regression: plugin config a command changed before an
/// invalidation reload survives it via the head — the reload restores the
/// commanded head value, not stale resident or checkpoint state.
#[tokio::test(flavor = "multi_thread")]
async fn a_plugin_config_command_before_an_invalidation_reload_survives_via_the_head() {
    let double = kernel_double(SEED + 12, lash_restate_test::ServerConfig::default()).await;
    let (mut runtime, store) = Box::pin(freshness_runtime(&double)).await;
    Box::pin(append_history(&mut runtime, &double, 2)).await;

    crate::runtime_support::configure(
        &mut runtime,
        &double,
        commanded_dialect("survives-invalidation-reload"),
        "reload-plugin-config",
    )
    .await;
    runtime.invalidate_resident_session_state();
    runtime
        .reload_invalidated_resident_session_state_for_session()
        .await
        .expect("reload invalidated resident session state");

    let expected = serde_json::json!({ "dialect": "survives-invalidation-reload" });
    assert_eq!(
        runtime.state().authority.plugin_config.get("dialect_owner"),
        Some(&expected),
        "an invalidation reload must restore the settled config from the head"
    );
    let head = session_view(store.clone(), "root")
        .load_session_head_meta()
        .await
        .expect("read durable head")
        .expect("session head exists");
    assert_eq!(
        head.config.plugin_config.get("dialect_owner"),
        Some(&expected),
        "the reload source is the durable head row"
    );
}

/// FIG-1875 pin (a): a live policy override followed by an invalidation
/// reload yields the durable head's values. The override settles through the
/// commanded write; when a competing executor then advances the head, the
/// reload adopts that head head-authoritatively — no resident-copy
/// preservation masks the advance.
#[tokio::test(flavor = "multi_thread")]
async fn live_policy_override_then_invalidation_reload_yields_the_head_values() {
    let double = kernel_double(SEED + 14, lash_restate_test::ServerConfig::default()).await;
    let (mut runtime, store) = Box::pin(freshness_runtime(&double)).await;
    Box::pin(append_history(&mut runtime, &double, 2)).await;
    let overridden_model = lash_core::ModelSpec::builder("live-override-model")
        .context_window_tokens(123_456)
        .build()
        .expect("override model");
    crate::runtime_support::configure(
        &mut runtime,
        &double,
        lash_core::ConfigTransaction::of(lash_core::plugin::config::core::SetModel {
            model: overridden_model,
        }),
        "model-override",
    )
    .await;
    crate::runtime_support::configure(
        &mut runtime,
        &double,
        lash_core::ConfigTransaction::of(lash_core::plugin::config::core::AddPromptContribution {
            contribution: lash_core::PromptContribution::guidance(
                "Live override",
                "SETTLED THROUGH THE COMMANDED WRITE",
            ),
        }),
        "prompt-override",
    )
    .await;

    let head_model = lash_core::ModelSpec::builder("advanced-head-model")
        .context_window_tokens(65_536)
        .build()
        .expect("advanced head model");
    let head_prompt = lash_core::PromptLayer::new().with_contribution(
        lash_core::PromptContribution::guidance("Advanced durable value", "THE HEAD WINS"),
    );
    advance_session_head(store.as_ref(), |state| {
        state.policy.model = head_model.clone();
        state.policy.prompt = head_prompt.clone();
    })
    .await;

    runtime.invalidate_resident_session_state();
    runtime
        .reload_invalidated_resident_session_state_for_session()
        .await
        .expect("reload invalidated resident session state");

    assert_eq!(
        runtime.state().effective_policy().model,
        head_model,
        "the invalidation reload must adopt the head's model"
    );
    assert_eq!(
        runtime.state().effective_policy().prompt,
        head_prompt,
        "the invalidation reload must adopt the head's prompt"
    );
    assert_eq!(
        *runtime.resident_session.validity(),
        ResidentSessionState::Valid
    );
}

/// FIG-1875 pin (b): a successful invalidation reload settles the freshness
/// facts — `Valid` plus `graph_loaded_from_store`. The turn still reads one
/// bounded head projection to verify the admitted drive epoch.
#[tokio::test(flavor = "multi_thread")]
async fn successful_invalidation_reload_issues_no_extra_head_meta_probe() {
    let double = kernel_double(SEED + 15, lash_restate_test::ServerConfig::default()).await;
    let backend = double.lash_backend();
    let store = double_unbound_recording_store(&double).await;
    let mut runtime = runtime_with_plugins_and_tools_and_host_and_store(
        Vec::new(),
        Arc::new(EmptyTools),
        mock_provider(vec![MockCall {
            stream_events: Vec::new(),
            response: Ok(LlmResponse {
                parts: vec![LlmOutputPart::Text {
                    text: "reload settled the freshness facts".to_string(),
                    response_meta: None,
                }],
                response_metadata: Default::default(),
                ..LlmResponse::default()
            }),
        }]),
        test_host_config(&backend),
        store.clone() as Arc<dyn lash_core::RuntimeStore>,
    )
    .await;
    Box::pin(append_history(&mut runtime, &double, 2)).await;

    runtime.invalidate_resident_session_state();
    let head_probes_before = store.load_session_head_meta_count();
    let full_loads_before = store.load_session_count();

    let handler = double
        .open_handler(AdmittedScope::turn(
            SessionId::from("root"),
            TurnId::from("reload-settles-freshness"),
        ))
        .await
        .expect("open the turn's handler");
    let turn = runtime
        .drive_turn(
            TurnInput::text("drive the invalidated turn"),
            lash_core::facade_support::TurnOptions::new(CancellationToken::new(), handler.scoped()),
        )
        .await
        .expect("the invalidated turn reloads and runs");
    handler.close().await.expect("close the turn's handler");
    assert_eq!(
        turn.assistant_output.safe_text,
        "reload settled the freshness facts"
    );

    assert_eq!(
        store.load_session_count() - full_loads_before,
        1,
        "the invalidation reload performs exactly one full durable read"
    );
    // The drive admission's pending-follow-on probe answers from the head,
    // and the root then verifies its epoch once after the full reload.
    assert_eq!(
        store.load_session_head_meta_count() - head_probes_before,
        2,
        "the drive verifies its epoch once after the full freshness reload"
    );
    assert_eq!(
        *runtime.resident_session.validity(),
        ResidentSessionState::Valid
    );
    assert!(
        runtime.resident_session.graph_loaded_from_store(),
        "the reload settles graph_loaded_from_store"
    );
}
