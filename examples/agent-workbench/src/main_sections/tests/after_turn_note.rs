use super::*;

const MODEL: &str = "workbench-after-turn-model";

/// The plugin-node bodies of `plugin_type` on `view`'s graph, in node order.
fn plugin_bodies(
    view: &lash::persistence::SessionReadView,
    plugin_type: &str,
) -> Vec<serde_json::Value> {
    use lash::persistence::SessionNodeProjection as _;
    view.session_graph()
        .nodes
        .iter()
        .filter_map(|node| {
            let (kind, body) = node.plugin()?;
            (kind == plugin_type).then(|| body.clone())
        })
        .collect()
}

/// The workbench's after-turn callback writes its note back on the durable
/// path (FIG-5283): a turn's commit holds the note summarizing the turn
/// before it, fenced to the leaf that turn committed, and the first turn,
/// with nothing before it, writes none.
#[tokio::test]
async fn the_after_turn_note_commits_with_the_turn_after_the_one_it_summarizes() {
    let stores = lash::sqlite::SqliteStoreSet::memory()
        .await
        .expect("an in-memory store set opens");
    let backend = lash::durable::DurableBackendBuilder::new(Arc::new(stores))
        .build()
        .expect("the durable backend builds");
    let provider = lash::testing::TestProvider::builder()
        .kind("workbench-after-turn")
        .complete(|_request| async {
            Ok(lash::provider::LlmResponse {
                parts: vec![lash::direct::LlmOutputPart::Text {
                    text: "noted".to_owned(),
                    response_meta: None,
                }],
                ..Default::default()
            })
        })
        .build()
        .into_handle();
    let factory = WorkbenchPluginFactory::new();
    let mail_world = factory.mail_world.clone();
    let core = lash::LashCore::standard_builder(backend)
        .serve_test_llm_profile(
            provider,
            lash::LlmProfileMetadata::builder(MODEL)
                .cache_retention(lash::provider::CacheRetention::Short)
                .context_window_tokens(200_000)
                .build()
                .expect("the model's metadata"),
        )
        .plugin(Arc::new(factory))
        .commit_budget(lash::CommitBudget::bounded(16 * 1024 * 1024, 4096))
        .data_retention(lash::DataRetention::standard())
        .queued_work_batching(lash::QueuedWorkBatchingConfig::new(1))
        .tool_source_policy(lash::tools::ToolSourcePolicy::Tolerate)
        .execution_budgets(lash::ExecutionBudgets::recommended())
        .delta_coalescing(lash::DeltaCoalescing::recommended())
        .build(lash::persistence::LeaseOwnerIdentity::opaque(
            "workbench-after-turn",
            "workbench-after-turn-boot",
        ))
        .expect("the core builds");
    let spec = lash::SessionSpec::new(
        MODEL,
        lash::TurnBudget::Unbounded,
        lash::MaxToolCalls::new(8),
    )
    .no_progress_budget(lash::NoProgressBudget::bounded(12))
    .plugin(
        "agent_workbench",
        workbench_session_prompt(
            crate::session_protocol::SessionProtocol::Standard,
            &mail_world,
        ),
    )
    .expect("the workbench prompt records");
    let session = core
        .session(lash::SessionId::from("workbench-after-turn"))
        .create(lash::SessionCreation::root(
            lash::plugins::SessionToolAccess::ambient(),
            spec,
        ))
        .await
        .expect("the session is created");

    let mut leaves = Vec::new();
    for text in ["first", "second"] {
        let turn = session
            .send(lash::TurnInput::text(text))
            .output()
            .await
            .expect("the turn answers");
        assert!(turn.is_success(), "{turn:?}");
        let view = session
            .read()
            .await
            .expect("the committed read")
            .expect("the committed session");
        leaves.push(view);
    }
    assert_eq!(
        plugin_bodies(&leaves[0], WORKBENCH_DERIVED_NOTE_PLUGIN_TYPE),
        Vec::<serde_json::Value>::new(),
        "the first turn has no turn before it to summarize"
    );
    let notes = plugin_bodies(&leaves[1], WORKBENCH_DERIVED_NOTE_PLUGIN_TYPE);
    let first_leaf = leaves[0]
        .session_graph()
        .leaf_node_id
        .clone()
        .expect("the first turn's commit has a leaf");
    assert_eq!(
        notes
            .iter()
            .map(|note| note["derived_from_node_id"].as_str())
            .collect::<Vec<_>>(),
        vec![Some(first_leaf.as_str())],
        "the second turn's commit holds one note, derived from the leaf the first committed: \
         {notes:?}"
    );
    core.shutdown().await.expect("the core shuts down");
}
