//! A resident runtime's freshness against its session's committed head
//! (FIG-1875, FIG-2479): an unchanged head costs one bounded head read, a
//! changed or indeterminate one rehydrates the session, and every adoption
//! and invalidation reload is head-authoritative.
//!
//! Each law runs a store-backed runtime over SQLite memory. Another writer
//! moves the head the way a session actor's commit does
//! ([`advance_session_head`]); the runtime then refreshes or reloads.

use super::*;
use lash_core::SessionCommitStore as _;

/// A store-backed runtime over a fresh SQLite memory store set, its store
/// under the recording decorator, with [`DialectOwner`] installed.
async fn freshness_runtime() -> (LashRuntime, Arc<RecordingStore>) {
    let backend = sqlite_memory_store_backend().await;
    let store = crate::runtime_support::recording_unbound_store_on(&backend).await;
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

impl lash_core::plugin::PluginDefinition for DialectOwner {
    fn declaration() -> lash_core::plugin::PluginDeclaration {
        lash_core::plugin::PluginDeclaration::initial("dialect_owner")
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
    type RunOptions = lash_core::NoRunOptions;

    fn create(
        &self,
        input: Option<DialectConfig>,
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

    fn apply_run_options(
        &self,
        recorded: &Self::Recorded,
        _options: Self::RunOptions,
    ) -> std::result::Result<Self::Recorded, Self::Refusal> {
        Ok(recorded.clone())
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

/// Commit `depth` plugin nodes past the session's head as another writer,
/// and bring the runtime to that head.
async fn append_history(runtime: &mut LashRuntime, store: &RecordingStore, depth: usize) {
    advance_session_head(store, |state| {
        state.session_graph.append_node_drafts_at(
            "freshness-depth",
            (0..depth).map(|ordinal| {
                lash_core::session_graph::SessionNodeDraft::plugin(
                    "freshness-depth",
                    serde_json::json!({ "ordinal": ordinal }),
                )
            }),
            lash_core::session_graph::NodeTimestamp::new(chrono::Utc::now())
                .expect("a node timestamp"),
        );
    })
    .await;
    runtime
        .refresh_session_graph_from_store()
        .await
        .expect("adopt the appended history");
    assert!(runtime.state().session_graph.nodes.len() >= depth);
}

/// Generation options that state only `seed`.
fn seeded(seed: i64) -> lash_core::GenerationOptions {
    lash_core::GenerationOptions {
        seed: Some(seed),
        ..lash_core::GenerationOptions::default()
    }
}

/// The model config the head records for `name`.
fn head_model(name: &str) -> lash_core::LlmProfileConfig {
    let metadata = lash_core::LlmProfileMetadata::builder(name)
        .context_window_tokens(65_536)
        .build()
        .expect("a model's metadata");
    lash_core::testing::test_llm_profile_config(metadata.wire_model.clone(), metadata)
}

#[tokio::test(flavor = "multi_thread")]
async fn unchanged_session_freshness_is_independent_of_history_depth() {
    for depth in [10, 256] {
        let (mut runtime, store) = freshness_runtime().await;
        append_history(&mut runtime, &store, depth).await;
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
    let (mut runtime, store) = freshness_runtime().await;
    append_history(&mut runtime, &store, 2).await;
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

/// FIG-1875 (head-authoritative adoption): a resident refresh adopts the
/// durable head's generation; the generation the runtime adopted before is
/// not preserved.
#[tokio::test(flavor = "multi_thread")]
async fn resident_refresh_adopts_the_durable_head_generation() {
    let (mut runtime, store) = freshness_runtime().await;
    append_history(&mut runtime, &store, 2).await;
    for generation in [seeded(1), seeded(2)] {
        advance_session_head(&store, |state| {
            state.policy.generation = generation.clone();
        })
        .await;

        runtime
            .refresh_session_graph_from_store()
            .await
            .expect("refresh resident graph");

        assert_eq!(
            runtime.state().effective_policy().generation,
            generation,
            "adoption is head-authoritative: the durable head's generation wins"
        );
    }
}

/// FIG-1875 (head-authoritative adoption): a resident refresh adopts the
/// durable head's model; there is no live-model preservation carve-out.
#[tokio::test(flavor = "multi_thread")]
async fn resident_refresh_adopts_the_durable_head_model() {
    let (mut runtime, store) = freshness_runtime().await;
    append_history(&mut runtime, &store, 2).await;
    for model in ["settled-live-model", "advanced-durable-model"] {
        advance_session_head(&store, |state| {
            state.policy.model = Some(head_model(model));
        })
        .await;

        runtime
            .refresh_session_graph_from_store()
            .await
            .expect("refresh resident graph");

        assert_eq!(
            runtime.state().effective_policy().model,
            Some(head_model(model)),
            "adoption is head-authoritative: the durable head's model wins"
        );
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn freshness_hydrates_when_leaf_changed() {
    let (mut runtime, store) = freshness_runtime().await;
    append_history(&mut runtime, &store, 2).await;
    let first_node_id = runtime.state().session_graph.nodes[0].node_id.clone();
    let mut head = session_view(store.clone(), runtime.session_id().clone())
        .load_session_head_meta()
        .await
        .expect("read head")
        .expect("session head exists");
    assert_ne!(head.leaf_node_id.as_ref(), Some(&first_node_id));
    head.leaf_node_id = Some(first_node_id.clone());
    store.forge_session_head(head);
    let full_loads_before = store.load_session_count();

    runtime
        .refresh_session_graph_from_store()
        .await
        .expect("refresh leaf change");

    assert_eq!(store.load_session_count() - full_loads_before, 1);
    assert_eq!(
        runtime.state().session_graph.leaf_node_id.as_ref(),
        Some(&first_node_id)
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn freshness_hydrates_when_only_checkpoint_ref_changed() {
    let (mut runtime, store) = freshness_runtime().await;
    append_history(&mut runtime, &store, 2).await;
    let mut head = session_view(store.clone(), runtime.session_id().clone())
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

/// FIG-2479 regression: plugin config a commit recorded on the head
/// survives an invalidation reload: the reload restores the head's value,
/// not stale resident or checkpoint state.
#[tokio::test(flavor = "multi_thread")]
async fn an_invalidation_reload_restores_the_heads_plugin_config() {
    let (mut runtime, store) = freshness_runtime().await;
    append_history(&mut runtime, &store, 2).await;
    let expected = serde_json::json!({ "dialect": "survives-invalidation-reload" });
    advance_session_head(&store, |state| {
        state
            .authority
            .plugin_config
            .insert("dialect_owner", expected.clone());
    })
    .await;

    runtime.invalidate_resident_session_state();
    runtime
        .reload_invalidated_resident_session_state_for_session()
        .await
        .expect("reload invalidated resident session state");

    assert_eq!(
        runtime.state().authority.plugin_config.get("dialect_owner"),
        Some(&expected),
        "an invalidation reload must restore the settled config from the head"
    );
    let head = session_view(store.clone(), runtime.session_id().clone())
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

/// FIG-1875 pin (a): a runtime that adopted one model and generation, whose
/// head another writer then advances, adopts the head's values on its
/// invalidation reload: no resident copy masks the advance.
#[tokio::test(flavor = "multi_thread")]
async fn an_invalidation_reload_adopts_the_heads_model_and_generation() {
    let (mut runtime, store) = freshness_runtime().await;
    append_history(&mut runtime, &store, 2).await;
    advance_session_head(&store, |state| {
        state.policy.model = Some(head_model("live-override-model"));
        state.policy.generation = seeded(1);
    })
    .await;
    runtime
        .refresh_session_graph_from_store()
        .await
        .expect("adopt the settled values");
    assert_eq!(runtime.state().effective_policy().generation, seeded(1));

    advance_session_head(&store, |state| {
        state.policy.model = Some(head_model("advanced-head-model"));
        state.policy.generation = seeded(2);
    })
    .await;
    runtime.invalidate_resident_session_state();
    runtime
        .reload_invalidated_resident_session_state_for_session()
        .await
        .expect("reload invalidated resident session state");

    assert_eq!(
        runtime.state().effective_policy().model,
        Some(head_model("advanced-head-model")),
        "the invalidation reload must adopt the head's model"
    );
    assert_eq!(
        runtime.state().effective_policy().generation,
        seeded(2),
        "the invalidation reload must adopt the head's generation"
    );
    assert_eq!(
        *runtime.resident_session.validity(),
        ResidentSessionState::Valid
    );
}

/// FIG-5352: a runtime whose session is not materialized, as a cold runtime
/// is before its recorded plugin transition publishes, adopts the head
/// another writer committed: it reads the head from its own store, which a
/// runtime holds whether or not its session exists.
#[tokio::test(flavor = "multi_thread")]
async fn a_cold_runtime_adopts_the_committed_head() {
    let (mut runtime, store) = freshness_runtime().await;
    append_history(&mut runtime, &store, 2).await;
    runtime.session = None;
    advance_session_head(&store, |state| {
        state.policy.generation = seeded(3);
    })
    .await;

    let adopted = runtime
        .adopt_committed_head()
        .await
        .expect("adopt the committed head");

    assert!(adopted, "a cold runtime must adopt the moved head");
    assert_eq!(
        runtime.state().effective_policy().generation,
        seeded(3),
        "the adoption is the durable head's"
    );
}
