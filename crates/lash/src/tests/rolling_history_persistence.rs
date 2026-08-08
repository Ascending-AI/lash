use super::*;

fn response_with_usage(text: &str, input_tokens: i64) -> LlmResponse {
    LlmResponse {
        full_text: text.to_string(),
        parts: vec![LlmOutputPart::Text {
            text: text.to_string(),
            response_meta: None,
        }],
        usage: lash_core::llm::types::LlmUsage {
            input_tokens,
            output_tokens: 1,
            ..Default::default()
        },
        response_metadata: Default::default(),
        ..LlmResponse::default()
    }
}

fn rolling_history_provider(responses: Vec<LlmResponse>) -> ProviderHandle {
    let responses = Arc::new(TokioMutex::new(VecDeque::from(responses)));
    crate::testing::TestProvider::builder()
        .kind("rolling-history-persistence-test")
        .complete(move |_request| {
            let responses = Arc::clone(&responses);
            async move {
                Ok(responses
                    .lock()
                    .await
                    .pop_front()
                    .expect("queued rolling-history response"))
            }
        })
        .build()
        .into_handle()
}

fn sqlite_head_and_max_seq(
    store_factory: &lash_sqlite_store::SqliteSessionStoreFactory,
    session_id: &str,
) -> (String, i64) {
    let conn = rusqlite::Connection::open(store_factory.catalog_path())
        .expect("open SQLite session catalog");
    let leaf = conn
        .query_row(
            "SELECT leaf_node_id FROM session_head WHERE session_id = ?1",
            [session_id],
            |row| row.get::<_, String>(0),
        )
        .expect("read durable session leaf");
    let max_seq = conn
        .query_row(
            "SELECT MAX(seq) FROM graph_nodes WHERE session_id = ?1",
            [session_id],
            |row| row.get::<_, Option<i64>>(0),
        )
        .expect("read durable graph sequence")
        .expect("committed graph nodes");
    (leaf, max_seq)
}

fn sqlite_messages(
    store_factory: &lash_sqlite_store::SqliteSessionStoreFactory,
    session_id: &str,
) -> Vec<lash_core::Message> {
    let conn = rusqlite::Connection::open(store_factory.catalog_path())
        .expect("open SQLite session catalog");
    let mut stmt = conn
        .prepare(
            "SELECT node_id, parent_node_id, node_json FROM graph_nodes
             WHERE session_id = ?1 ORDER BY seq ASC",
        )
        .expect("prepare graph-node read");
    stmt.query_map([session_id], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, Option<String>>(1)?,
            row.get::<_, String>(2)?,
        ))
    })
    .expect("read graph nodes")
    .map(|row| {
        let (node_id, parent_node_id, node_json) = row.expect("decode graph-node row");
        lash_core::SessionNodeRecord::decode_storage_body(node_id, parent_node_id, &node_json)
            .expect("decode stored graph node")
    })
    .filter_map(|node| node.message())
    .collect()
}

#[tokio::test]
async fn rolling_history_threshold_turn_commits_from_durable_leaf_and_unblocks_compaction()
-> Result<()> {
    let dir = tempfile::tempdir().expect("tempdir");
    let session_id = "rolling-history-durable-parent";
    let trace_path = dir.path().join("trace.jsonl");
    let store_factory = Arc::new(lash_sqlite_store::SqliteSessionStoreFactory::new(
        dir.path().join("sessions"),
    ));
    let provider = rolling_history_provider(vec![
        response_with_usage("first response", 20_000),
        response_with_usage("threshold response", 1),
        response_with_usage("durable summary", 1),
    ]);
    let core = explicit_ephemeral_facets(LashCore::standard_builder())
        .provider(provider)
        .model(model_spec("rolling-history-model", None, 40_000))
        .plugin(Arc::new(
            lash_standard_plugins::rolling_history::RollingHistoryPluginFactory::default(),
        ))
        .store_factory(store_factory.clone())
        .trace_jsonl_path(trace_path.clone())
        .build()?;
    let session = core.session(session_id).open().await?;

    session
        .turn(TurnInput::text("first request"))
        .turn_id("rolling-history-first")
        .run()
        .await?;
    let (durable_leaf_before_threshold, max_seq_before_threshold) =
        sqlite_head_and_max_seq(store_factory.as_ref(), session_id);

    session
        .turn(TurnInput::text("threshold request"))
        .turn_id("rolling-history-threshold")
        .run()
        .await?;

    let conn = rusqlite::Connection::open(store_factory.catalog_path())
        .expect("open SQLite session catalog");
    let first_threshold_parent = conn
        .query_row(
            "SELECT parent_node_id FROM graph_nodes
             WHERE session_id = ?1 AND seq > ?2 ORDER BY seq ASC LIMIT 1",
            rusqlite::params![session_id, max_seq_before_threshold],
            |row| row.get::<_, String>(0),
        )
        .expect("read threshold commit ancestry");
    let threshold_node_count = conn
        .query_row(
            "SELECT COUNT(*) FROM graph_nodes WHERE session_id = ?1 AND seq > ?2",
            rusqlite::params![session_id, max_seq_before_threshold],
            |row| row.get::<_, i64>(0),
        )
        .expect("count threshold commit nodes");
    assert_eq!(
        threshold_node_count, 2,
        "the threshold turn must append exactly its new user message and assistant outcome"
    );
    assert_eq!(
        first_threshold_parent, durable_leaf_before_threshold,
        "the threshold turn must extend the durable leaf that was current when the turn began"
    );

    assert!(
        session
            .admin()
            .state()
            .compact_context(
                Some("retain the durable ancestry result".to_string()),
                runtime_operation_scope(&core, "rolling-history-explicit-compaction"),
            )
            .await?,
        "rolling-history compaction should open a summary frame after the threshold turn commits"
    );
    let (post_compaction_leaf, post_compaction_max_seq) =
        sqlite_head_and_max_seq(store_factory.as_ref(), session_id);
    core.flush_trace_sink()?;

    let trace = std::fs::read_to_string(trace_path).expect("read rolling-history trace");
    let records = trace
        .lines()
        .map(|line| serde_json::from_str::<serde_json::Value>(line).expect("decode trace record"))
        .collect::<Vec<_>>();
    for event_type in [
        "rolling_history_compaction_started",
        "rolling_history_compaction_completed",
    ] {
        let record = records
            .iter()
            .find(|record| {
                record.get("type").and_then(serde_json::Value::as_str) == Some(event_type)
            })
            .unwrap_or_else(|| panic!("missing {event_type} trace record"));
        let context = record.get("context").expect("trace context");
        assert_eq!(
            context
                .get("session_id")
                .and_then(serde_json::Value::as_str),
            Some(session_id)
        );
        assert!(context.get("turn_id").is_none());
        assert_eq!(
            context
                .get("parent_graph_node_id")
                .and_then(serde_json::Value::as_str),
            Some("session:rolling-history-durable-parent")
        );
    }

    let projection_record = records
        .iter()
        .find(|record| {
            record.get("type").and_then(serde_json::Value::as_str) == Some("custom")
                && record.get("name").and_then(serde_json::Value::as_str)
                    == Some("session_graph.read_projection")
        })
        .expect("projection partition trace record");
    assert_eq!(
        projection_record["payload"]["durably_appended_messages"],
        serde_json::json!(1)
    );
    assert_eq!(
        projection_record["payload"]["observation_only_messages"],
        serde_json::json!(0)
    );
    assert_eq!(
        projection_record["payload"]["id_mismatch_message_ids"],
        serde_json::json!([])
    );

    drop(session);
    drop(core);
    let reopened_core = explicit_ephemeral_facets(LashCore::standard_builder())
        .provider(rolling_history_provider(vec![response_with_usage(
            "response after reopen",
            1,
        )]))
        .model(model_spec("rolling-history-model", None, 40_000))
        .plugin(Arc::new(
            lash_standard_plugins::rolling_history::RollingHistoryPluginFactory::default(),
        ))
        .store_factory(store_factory.clone())
        .build()?;
    let reopened_session = reopened_core.session(session_id).open().await?;
    reopened_session
        .turn(TurnInput::text("continue after compaction"))
        .turn_id("rolling-history-reopened")
        .run()
        .await?;
    let conn = rusqlite::Connection::open(store_factory.catalog_path())
        .expect("reopen SQLite session catalog");
    let reopened_first_parent = conn
        .query_row(
            "SELECT parent_node_id FROM graph_nodes
             WHERE session_id = ?1 AND seq > ?2 ORDER BY seq ASC LIMIT 1",
            rusqlite::params![session_id, post_compaction_max_seq],
            |row| row.get::<_, String>(0),
        )
        .expect("read post-reopen ancestry");
    assert_eq!(reopened_first_parent, post_compaction_leaf);

    Ok(())
}

#[tokio::test]
async fn attachment_pruning_never_rewrites_the_durable_message() -> Result<()> {
    let dir = tempfile::tempdir().expect("tempdir");
    let session_id = "rolling-history-attachment-prune";
    let trace_path = dir.path().join("trace.jsonl");
    let store_factory = Arc::new(lash_sqlite_store::SqliteSessionStoreFactory::new(
        dir.path().join("sessions"),
    ));
    let core = explicit_ephemeral_facets(LashCore::standard_builder())
        .provider(rolling_history_provider(vec![
            response_with_usage("first response", 60_000),
            response_with_usage("second response", 1),
        ]))
        .model(model_spec("attachment-prune-model", None, 100_000))
        .plugin(Arc::new(
            lash_standard_plugins::rolling_history::RollingHistoryPluginFactory::default(),
        ))
        .store_factory(store_factory.clone())
        .trace_jsonl_path(trace_path.clone())
        .build()?;
    let session = core.session(session_id).open().await?;

    session
        .turn(TurnInput::text("remember this image").with_attachment(
            lash_core::AttachmentSource::inline(
                lash_core::MediaType::parse("image/png").expect("image media type"),
                vec![1, 2, 3],
            ),
        ))
        .turn_id("attachment-prune-first")
        .run()
        .await?;
    let original_durable_message = sqlite_messages(store_factory.as_ref(), session_id)
        .into_iter()
        .find(|message| message.id == "m_turn_attachment-prune-first_input")
        .expect("first turn input is durable");
    session
        .turn(TurnInput::text("trigger ephemeral pruning"))
        .turn_id("attachment-prune-second")
        .run()
        .await?;

    let durable_message = sqlite_messages(store_factory.as_ref(), session_id)
        .into_iter()
        .find(|message| message.id == "m_turn_attachment-prune-first_input")
        .expect("first turn input remains durable");
    assert!(
        durable_message
            .parts
            .iter()
            .any(|part| part.attachment.is_some()),
        "durable transcript keeps the original attachment"
    );
    assert_eq!(
        serde_json::to_value(&durable_message)?,
        serde_json::to_value(&original_durable_message)?,
        "attachment pruning must not rewrite the durable message"
    );

    core.flush_trace_sink()?;
    let trace = std::fs::read_to_string(trace_path).expect("read projection trace");
    let mismatch_record = trace.lines().find_map(|line| {
        let record = serde_json::from_str::<serde_json::Value>(line).ok()?;
        (record.get("name").and_then(serde_json::Value::as_str)
            == Some("session_graph.read_projection")
            && record["payload"]["id_mismatch_message_ids"]
                .as_array()
                .is_some_and(|ids| !ids.is_empty()))
        .then_some(record)
    });
    assert_eq!(
        mismatch_record.expect("attachment projection mismatch diagnostic")["payload"]["id_mismatch_message_ids"],
        serde_json::json!(["m_turn_attachment-prune-first_input"])
    );

    Ok(())
}

#[tokio::test]
async fn before_turn_plugin_messages_remain_durable_across_threshold_turns() -> Result<()> {
    const THRESHOLD_TURNS: usize = 3;
    let dir = tempfile::tempdir().expect("tempdir");
    let session_id = "rolling-history-plugin-message-ids";
    let store_factory = Arc::new(lash_sqlite_store::SqliteSessionStoreFactory::new(
        dir.path().join("sessions"),
    ));
    let next_injection = Arc::new(AtomicUsize::new(0));
    let injection_hook = {
        let next_injection = Arc::clone(&next_injection);
        Arc::new(move |_| {
            let ordinal = next_injection.fetch_add(1, Ordering::SeqCst);
            Box::pin(async move {
                Ok(vec![
                    lash_core::facade_support::PluginDirective::EnqueueMessages {
                        messages: vec![lash_core::PluginMessage::text(
                            lash_core::MessageRole::User,
                            format!("plugin injection {ordinal}"),
                        )],
                    },
                ])
            }) as lash_core::plugin::PluginFuture<_>
        })
    };
    let injection_plugin = crate::plugins::StaticPluginFactory::new(
        "rolling-history-injection-test",
        lash_core::facade_support::PluginSpec::new().with_before_turn(injection_hook),
    );
    let responses = (0..=THRESHOLD_TURNS)
        .map(|ordinal| response_with_usage(&format!("response {ordinal}"), 20_000))
        .collect();
    let core = explicit_ephemeral_facets(LashCore::standard_builder())
        .provider(rolling_history_provider(responses))
        .model(model_spec("plugin-message-id-model", None, 40_000))
        .plugin(Arc::new(
            lash_standard_plugins::rolling_history::RollingHistoryPluginFactory::default(),
        ))
        .plugin(Arc::new(injection_plugin))
        .store_factory(store_factory.clone())
        .build()?;
    let session = core.session(session_id).open().await?;

    for ordinal in 0..=THRESHOLD_TURNS {
        session
            .turn(TurnInput::text(format!("request {ordinal}")))
            .turn_id(format!("plugin-injection-{ordinal}"))
            .run()
            .await?;
    }

    let plugin_messages = sqlite_messages(store_factory.as_ref(), session_id)
        .into_iter()
        .filter(|message| {
            matches!(
                message.origin,
                Some(lash_core::MessageOrigin::Plugin { ref plugin_id, .. })
                    if plugin_id == "plugin"
            )
        })
        .collect::<Vec<_>>();
    assert_eq!(plugin_messages.len(), THRESHOLD_TURNS + 1);
    assert_eq!(
        plugin_messages
            .iter()
            .map(|message| message.id.as_str())
            .collect::<Vec<_>>(),
        (0..=THRESHOLD_TURNS)
            .map(|ordinal| format!("m_plugin_plugin-injection-{ordinal}:before_turn_0"))
            .collect::<Vec<_>>()
    );

    Ok(())
}

#[cfg(feature = "rlm")]
#[tokio::test]
async fn rolling_history_threshold_continue_as_extends_the_pre_switch_durable_leaf() -> Result<()> {
    let dir = tempfile::tempdir().expect("tempdir");
    let session_id = "rolling-history-continue-as-parent";
    let store_factory = Arc::new(lash_sqlite_store::SqliteSessionStoreFactory::new(
        dir.path().join("sessions"),
    ));
    let provider = rolling_history_provider(vec![
        response_with_usage(&lashlang_block(r#"finish "primed""#), 20_000),
        response_with_usage(
            &lashlang_block(r#"await control.continue_as({ task: "finish from the new frame" })?"#),
            1,
        ),
        response_with_usage(&lashlang_block(r#"finish "continued""#), 1),
    ]);
    let core = explicit_ephemeral_facets(LashCore::rlm_builder(rlm_factory()))
        .provider(provider)
        .model(model_spec("rolling-history-rlm-model", None, 40_000))
        .plugin(Arc::new(
            lash_standard_plugins::rolling_history::RollingHistoryPluginFactory::default(),
        ))
        .store_factory(store_factory.clone())
        .build()?;
    let session = core.session(session_id).open().await?;

    let primed = session
        .turn(TurnInput::text("prime durable history"))
        .turn_id("rolling-history-rlm-first")
        .run()
        .await?;
    assert_eq!(primed.final_value(), Some(&serde_json::json!("primed")));
    let (durable_leaf_before_switch, max_seq_before_switch) =
        sqlite_head_and_max_seq(store_factory.as_ref(), session_id);

    let continued = session
        .turn(TurnInput::text("cross the threshold and continue"))
        .turn_id("rolling-history-rlm-threshold")
        .run()
        .await?;
    assert_eq!(
        continued.final_value(),
        Some(&serde_json::json!("continued"))
    );

    let conn = rusqlite::Connection::open(store_factory.catalog_path())
        .expect("open SQLite session catalog");
    let first_switch_parent = conn
        .query_row(
            "SELECT parent_node_id FROM graph_nodes
             WHERE session_id = ?1 AND seq > ?2 ORDER BY seq ASC LIMIT 1",
            rusqlite::params![session_id, max_seq_before_switch],
            |row| row.get::<_, String>(0),
        )
        .expect("read threshold continue_as ancestry");
    assert_eq!(
        first_switch_parent, durable_leaf_before_switch,
        "the threshold-crossing continue_as turn must extend the leaf from before the frame switch"
    );

    Ok(())
}
