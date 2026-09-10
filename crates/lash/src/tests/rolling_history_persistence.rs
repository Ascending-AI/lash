use super::*;
use lash_sansio::SessionId;

fn response_with_usage(text: &str, input_tokens: i64) -> LlmResponse {
    LlmResponse {
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

fn sqlite_head_and_max_generation(
    store_factory: &lash_sqlite_store::SqliteSessionStoreFactory,
    session_id: &SessionId,
) -> (String, i64) {
    let conn = rusqlite::Connection::open(store_factory.catalog_path())
        .expect("open SQLite session catalog");
    let leaf = conn
        .query_row(
            "SELECT leaf_node_id FROM session_head WHERE session_id = ?1",
            [session_id.as_str()],
            |row| row.get::<_, String>(0),
        )
        .expect("read durable session leaf");
    let max_generation = conn
        .query_row(
            "SELECT MAX(generation) FROM graph_nodes WHERE session_id = ?1",
            [session_id.as_str()],
            |row| row.get::<_, Option<i64>>(0),
        )
        .expect("read durable graph generation")
        .expect("committed graph nodes");
    (leaf, max_generation)
}

fn sqlite_nodes(
    store_factory: &lash_sqlite_store::SqliteSessionStoreFactory,
    session_id: &SessionId,
) -> Vec<lash_core::SessionNodeRecord> {
    let conn = rusqlite::Connection::open(store_factory.catalog_path())
        .expect("open SQLite session catalog");
    let mut stmt = conn
        .prepare(
            "SELECT node_id, parent_node_id, node_json FROM graph_nodes
             WHERE session_id = ?1 ORDER BY generation ASC",
        )
        .expect("prepare graph-node read");
    stmt.query_map([session_id.as_str()], |row| {
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
    .collect()
}

fn sqlite_messages(
    store_factory: &lash_sqlite_store::SqliteSessionStoreFactory,
    session_id: &SessionId,
) -> Vec<lash_core::Message> {
    sqlite_nodes(store_factory, session_id)
        .iter()
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
    let core = explicit_ephemeral_facets(LashCore::standard_builder(crate::TurnBudget::Unbounded))
        .provider(provider)
        .model(model_spec("rolling-history-model", None, 40_000))
        .plugin(Arc::new(
            lash_standard_plugins::rolling_history::RollingHistoryPluginFactory::default(),
        ))
        .store_factory(store_factory.clone())
        .trace_jsonl_path(trace_path.clone())
        .build(crate::testing::runtime_lease_owner())?;
    let session = core.session(session_id).open().await?;

    session
        .turn(TurnInput::text("first request"))
        .turn_id("rolling-history-first")
        .run()
        .await?;
    let (durable_leaf_before_threshold, max_generation_before_threshold) =
        sqlite_head_and_max_generation(store_factory.as_ref(), &SessionId::from(session_id));

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
             WHERE session_id = ?1 AND generation > ?2 ORDER BY generation ASC LIMIT 1",
            rusqlite::params![session_id, max_generation_before_threshold],
            |row| row.get::<_, String>(0),
        )
        .expect("read threshold commit ancestry");
    let threshold_node_count = conn
        .query_row(
            "SELECT COUNT(*) FROM graph_nodes WHERE session_id = ?1 AND generation > ?2",
            rusqlite::params![session_id, max_generation_before_threshold],
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
    let (post_compaction_leaf, post_compaction_max_generation) =
        sqlite_head_and_max_generation(store_factory.as_ref(), &SessionId::from(session_id));
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
    let reopened_core =
        explicit_ephemeral_facets(LashCore::standard_builder(crate::TurnBudget::Unbounded))
            .provider(rolling_history_provider(vec![response_with_usage(
                "response after reopen",
                1,
            )]))
            .model(model_spec("rolling-history-model", None, 40_000))
            .plugin(Arc::new(
                lash_standard_plugins::rolling_history::RollingHistoryPluginFactory::default(),
            ))
            .store_factory(store_factory.clone())
            .build(crate::testing::runtime_lease_owner())?;
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
             WHERE session_id = ?1 AND generation > ?2 ORDER BY generation ASC LIMIT 1",
            rusqlite::params![session_id, post_compaction_max_generation],
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
    let core = explicit_ephemeral_facets(LashCore::standard_builder(crate::TurnBudget::Unbounded))
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
        .build(crate::testing::runtime_lease_owner())?;
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
    // The turn's input is admitted durably before it drives (ADR 0069), so its
    // committed message is addressed by the acceptance it came from rather than
    // by a turn-shaped id.
    let original_durable_message =
        sqlite_messages(store_factory.as_ref(), &SessionId::from(session_id))
            .into_iter()
            .find(|message| {
                matches!(
                    &message.origin,
                    Some(lash_core::MessageOrigin::TurnInput { turn_id, .. })
                        if turn_id == "attachment-prune-first"
                )
            })
            .expect("first turn input is durable");
    let first_input_message_id = original_durable_message.id.clone();
    session
        .turn(TurnInput::text("trigger ephemeral pruning"))
        .turn_id("attachment-prune-second")
        .run()
        .await?;

    let durable_message = sqlite_messages(store_factory.as_ref(), &SessionId::from(session_id))
        .into_iter()
        .find(|message| message.id == first_input_message_id)
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
        serde_json::json!([first_input_message_id])
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
                    lash_core::facade_support::TurnPluginDirective::EnqueueMessages(
                        lash_core::facade_support::EnqueueMessagesDirective {
                            messages: vec![lash_core::PluginMessage::text(
                                lash_core::MessageRole::User,
                                format!("plugin injection {ordinal}"),
                            )],
                        },
                    ),
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
    let core = explicit_ephemeral_facets(LashCore::standard_builder(crate::TurnBudget::Unbounded))
        .provider(rolling_history_provider(responses))
        .model(model_spec("plugin-message-id-model", None, 40_000))
        .plugin(Arc::new(
            lash_standard_plugins::rolling_history::RollingHistoryPluginFactory::default(),
        ))
        .plugin(Arc::new(injection_plugin))
        .store_factory(store_factory.clone())
        .build(crate::testing::runtime_lease_owner())?;
    let session = core.session(session_id).open().await?;

    for ordinal in 0..=THRESHOLD_TURNS {
        session
            .turn(TurnInput::text(format!("request {ordinal}")))
            .turn_id(format!("plugin-injection-{ordinal}"))
            .run()
            .await?;
    }

    let plugin_messages = sqlite_messages(store_factory.as_ref(), &SessionId::from(session_id))
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
    let core = explicit_ephemeral_facets(LashCore::rlm_builder(
        crate::TurnBudget::Unbounded,
        rlm_factory(),
    ))
    .provider(provider)
    .model(model_spec("rolling-history-rlm-model", None, 40_000))
    .plugin(Arc::new(
        lash_standard_plugins::rolling_history::RollingHistoryPluginFactory::default(),
    ))
    .store_factory(store_factory.clone())
    .build(crate::testing::runtime_lease_owner())?;
    let session = core.session(session_id).open().await?;

    let primed = session
        .turn(TurnInput::text("prime durable history"))
        .turn_id("rolling-history-rlm-first")
        .run()
        .await?;
    assert_eq!(primed.final_value(), Some(&serde_json::json!("primed")));
    let (durable_leaf_before_switch, max_generation_before_switch) =
        sqlite_head_and_max_generation(store_factory.as_ref(), &SessionId::from(session_id));

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
             WHERE session_id = ?1 AND generation > ?2 ORDER BY generation ASC LIMIT 1",
            rusqlite::params![session_id, max_generation_before_switch],
            |row| row.get::<_, String>(0),
        )
        .expect("read threshold continue_as ancestry");
    assert_eq!(
        first_switch_parent, durable_leaf_before_switch,
        "the threshold-crossing continue_as turn must extend the leaf from before the frame switch"
    );

    Ok(())
}

fn sqlite_node_rows(
    store_factory: &lash_sqlite_store::SqliteSessionStoreFactory,
    session_id: &SessionId,
) -> Vec<(String, Option<String>, i64)> {
    let conn = rusqlite::Connection::open(store_factory.catalog_path())
        .expect("open SQLite session catalog");
    let mut stmt = conn
        .prepare(
            "SELECT node_id, parent_node_id, generation FROM graph_nodes
             WHERE session_id = ?1 ORDER BY generation ASC",
        )
        .expect("prepare graph-node read");
    stmt.query_map([session_id.as_str()], |row| {
        Ok((row.get(0)?, row.get(1)?, row.get(2)?))
    })
    .expect("read graph nodes")
    .map(|row| row.expect("decode graph-node row"))
    .collect()
}

/// Mirrors lash-core's draft node id derivation so the test can name the ids a
/// projection-namespaced (`unscoped-replacement:*`) append would mint.
fn draft_node_id(namespace: &str, ordinal: u64) -> String {
    let preimage = format!("{}:{namespace}:{ordinal}", namespace.len());
    format!(
        "draft-node/v3/{}",
        lash_sansio::core_support::blake3_domain_hash_hex(
            "lash-draft-node/v3",
            preimage.as_bytes()
        )
    )
}

fn synthetic_replacement_ids(persisted_ids: &[String]) -> std::collections::HashSet<String> {
    std::iter::once("root".to_string())
        .chain(persisted_ids.iter().cloned())
        .flat_map(|leaf| {
            (0..8)
                .map(move |ordinal| draft_node_id(&format!("unscoped-replacement:{leaf}"), ordinal))
        })
        .collect()
}

#[tokio::test]
async fn after_turn_enqueue_resident_next_turn_commits_from_durable_leaf() -> Result<()> {
    let dir = tempfile::tempdir().expect("tempdir");
    let session_id = "after-turn-enqueue-resident";
    let store_factory = Arc::new(lash_sqlite_store::SqliteSessionStoreFactory::new(
        dir.path().join("sessions"),
    ));
    let plugin = crate::plugins::StaticPluginFactory::new(
        "after-turn-injection",
        lash_core::facade_support::PluginSpec::new().with_after_turn(Arc::new(|_| {
            Box::pin(async {
                Ok(vec![
                    lash_core::facade_support::AfterTurnPluginDirective::EnqueueMessages(
                        lash_core::facade_support::EnqueueMessagesDirective {
                            messages: vec![lash_core::PluginMessage::text(
                                lash_core::MessageRole::User,
                                "enqueued after turn",
                            )],
                        },
                    ),
                ])
            })
        })),
    );
    let core = explicit_ephemeral_facets(LashCore::standard_builder(crate::TurnBudget::Unbounded))
        .provider(rolling_history_provider(vec![
            response_with_usage("first response", 1),
            response_with_usage("second response", 1),
        ]))
        .model(model_spec("after-turn-model", None, 40_000))
        .plugin(Arc::new(plugin))
        .store_factory(store_factory.clone())
        .build(crate::testing::runtime_lease_owner())?;
    let session = core.session(session_id).open().await?;
    session
        .turn(TurnInput::text("first request"))
        .turn_id("enqueue-first")
        .run()
        .await?;
    assert!(
        session
            .observe()
            .current_observation()
            .read_view
            .messages()
            .iter()
            .any(|message| message_text(message) == "enqueued after turn")
    );

    let turn_one_rows = sqlite_node_rows(store_factory.as_ref(), &SessionId::from(session_id));
    let persisted_ids = turn_one_rows
        .iter()
        .map(|(id, _, _)| id.clone())
        .collect::<Vec<_>>();
    let synthetic = synthetic_replacement_ids(&persisted_ids);
    let persisted_synthetic = persisted_ids
        .iter()
        .filter(|id| synthetic.contains(*id))
        .cloned()
        .collect::<Vec<_>>();
    assert!(
        persisted_synthetic.is_empty(),
        "projection-namespaced `unscoped-replacement:*` nodes must never be persisted, found {persisted_synthetic:?}"
    );
    let (leaf, generation) =
        sqlite_head_and_max_generation(store_factory.as_ref(), &SessionId::from(session_id));
    let real_leaf = turn_one_rows
        .iter()
        .rev()
        .map(|(id, _, _)| id.clone())
        .find(|id| !synthetic.contains(id))
        .expect("a real durable node");
    assert_eq!(
        leaf, real_leaf,
        "durable head must be the last real append, not a projection node"
    );

    session
        .turn(TurnInput::text("second request"))
        .turn_id("enqueue-second")
        .run()
        .await?;
    let next = sqlite_node_rows(store_factory.as_ref(), &SessionId::from(session_id))
        .into_iter()
        .find(|(_, _, node_generation)| *node_generation > generation)
        .expect("turn two committed nodes");
    assert_eq!(
        next.1.as_deref(),
        Some(real_leaf.as_str()),
        "next commit must extend the last real durable node, not a projection node"
    );
    assert_eq!(
        next.1.as_deref(),
        Some(leaf.as_str()),
        "next commit must extend the durable head"
    );
    Ok(())
}

#[tokio::test]
async fn mid_turn_graph_append_never_replicates_the_read_tail_durably() -> Result<()> {
    let dir = tempfile::tempdir().expect("tempdir");
    let session_id = "mid-turn-graph-append";
    let store_factory = Arc::new(lash_sqlite_store::SqliteSessionStoreFactory::new(
        dir.path().join("sessions"),
    ));
    let appended = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let hook_appended = Arc::clone(&appended);
    let completions = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let append_error = Arc::new(std::sync::Mutex::new(None::<String>));
    let hook_append_error = Arc::clone(&append_error);
    let plugin = crate::plugins::StaticPluginFactory::new(
        "mid-turn-append",
        lash_core::facade_support::PluginSpec::new().with_checkpoint(Arc::new(move |ctx| {
            let appended = Arc::clone(&hook_appended);
            let completions = Arc::clone(&completions);
            let append_error = Arc::clone(&hook_append_error);
            Box::pin(async move {
                if ctx.checkpoint != lash_core::CheckpointKind::BeforeCompletion {
                    return Ok(Vec::new());
                }
                // Fire on the second turn only: turn one must have committed a durable read tail.
                if completions.fetch_add(1, std::sync::atomic::Ordering::SeqCst) == 0
                    || appended.swap(true, std::sync::atomic::Ordering::SeqCst)
                {
                    return Ok(Vec::new());
                }
                let outcome = match ctx
                    .session_graph
                    .append_session_nodes(
                        &ctx.session_id,
                        lash_core::AppendSessionNodesRequest {
                            operation_id: "mid-turn-append".to_string(),
                            nodes: vec![lash_core::SessionAppendNode::plugin(
                                "test.mid-turn",
                                serde_json::json!({"probe": true}),
                            )],
                            requires_ancestor_node_id: None,
                        },
                    )
                    .await
                {
                    Ok(outcome) => outcome,
                    Err(error) => {
                        *append_error.lock().expect("append error slot") = Some(error.to_string());
                        return Err(error);
                    }
                };
                assert!(matches!(
                    outcome,
                    lash_core::AppendSessionNodesOutcome::Appended { .. }
                ));
                Ok(Vec::new())
            })
        })),
    );
    let core = explicit_ephemeral_facets(LashCore::standard_builder(crate::TurnBudget::Unbounded))
        .provider(rolling_history_provider(vec![
            response_with_usage("first response", 1),
            response_with_usage("second response", 1),
        ]))
        .model(model_spec("mid-turn-model", None, 40_000))
        .plugin(Arc::new(plugin))
        .store_factory(store_factory.clone())
        .build(crate::testing::runtime_lease_owner())?;
    let session = core.session(session_id).open().await?;
    session
        .turn(TurnInput::text("first request"))
        .turn_id("append-first")
        .run()
        .await?;
    let (durable_leaf_before_second, _) =
        sqlite_head_and_max_generation(store_factory.as_ref(), &SessionId::from(session_id));
    // Second turn: the checkpoint hook appends through the in-turn graph service while the
    // durable read tail already holds two messages.
    session
        .turn(TurnInput::text("second request"))
        .turn_id("append-second")
        .run()
        .await?;
    assert!(
        appended.load(std::sync::atomic::Ordering::SeqCst),
        "hook ran"
    );
    let rows = sqlite_node_rows(store_factory.as_ref(), &SessionId::from(session_id));
    let texts = sqlite_messages(store_factory.as_ref(), &SessionId::from(session_id))
        .iter()
        .map(message_text)
        .collect::<Vec<_>>();
    let append_error = append_error.lock().expect("append error slot").clone();
    assert_eq!(
        append_error, None,
        "an in-turn graph append must extend the durable leaf {durable_leaf_before_second}, not a projection parent"
    );
    let mut counts = std::collections::BTreeMap::<String, usize>::new();
    for text in &texts {
        *counts.entry(text.clone()).or_default() += 1;
    }
    let duplicated = counts
        .iter()
        .filter(|(_, n)| **n > 1)
        .map(|(t, n)| format!("{t:?} x{n}"))
        .collect::<Vec<_>>();
    assert!(
        duplicated.is_empty(),
        "durable history must hold each message once; projection replicas persisted: {duplicated:?}"
    );
    let mut children = std::collections::BTreeMap::<String, usize>::new();
    for (_, parent, _) in &rows {
        if let Some(parent) = parent {
            *children.entry(parent.clone()).or_default() += 1;
        }
    }
    let forks = children
        .iter()
        .filter(|(_, n)| **n > 1)
        .map(|(p, n)| format!("{p} -> {n} children"))
        .collect::<Vec<_>>();
    assert!(
        forks.is_empty(),
        "durable graph must stay a single chain; projection forks persisted: {forks:?}"
    );
    Ok(())
}

/// FIG-1059: an in-turn `SessionGraphService::append_session_nodes` on the
/// current store-backed session rides the turn's commit draft. On an EMPTY
/// durable tail (turn one) it must neither commit ahead of the turn (which
/// used to leave the turn's own final commit with a head-revision conflict)
/// nor derive its nodes from the read projection: the turn's nodes and the
/// appended nodes land in one commit, in that order, on one chain.
#[tokio::test]
async fn in_turn_graph_append_on_an_empty_durable_tail_commits_with_the_turn() -> Result<()> {
    let dir = tempfile::tempdir().expect("tempdir");
    let session_id = "same-turn-graph-append";
    let store_factory = Arc::new(lash_sqlite_store::SqliteSessionStoreFactory::new(
        dir.path().join("sessions"),
    ));
    let draft_node_ids = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
    let hook_draft_node_ids = Arc::clone(&draft_node_ids);
    let visible_in_turn = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let hook_visible_in_turn = Arc::clone(&visible_in_turn);
    let plugin = crate::plugins::StaticPluginFactory::new(
        "same-turn-append",
        lash_core::facade_support::PluginSpec::new().with_checkpoint(Arc::new(move |ctx| {
            let draft_node_ids = Arc::clone(&hook_draft_node_ids);
            let visible_in_turn = Arc::clone(&hook_visible_in_turn);
            Box::pin(async move {
                if ctx.checkpoint != lash_core::CheckpointKind::BeforeCompletion
                    || !draft_node_ids.lock().expect("draft ids").is_empty()
                {
                    return Ok(Vec::new());
                }
                let outcome = ctx
                    .session_graph
                    .append_session_nodes(
                        &ctx.session_id,
                        lash_core::AppendSessionNodesRequest {
                            operation_id: "same-turn-append".to_string(),
                            nodes: vec![lash_core::SessionAppendNode::plugin(
                                "test.same-turn",
                                serde_json::json!({"probe": "same-turn"}),
                            )],
                            requires_ancestor_node_id: None,
                        },
                    )
                    .await?;
                let lash_core::AppendSessionNodesOutcome::Appended {
                    node_ids,
                    leaf_node_id,
                } = outcome
                else {
                    panic!("an unconditional append on a fresh session is never a stale branch");
                };
                assert_eq!(node_ids.len(), 1);
                assert_eq!(leaf_node_id, node_ids[0]);
                // In-turn readers see the appended node before the turn commits.
                let snapshot = ctx.sessions.snapshot_session(&ctx.session_id).await?;
                visible_in_turn.store(
                    snapshot.session_graph.find_node(&node_ids[0]).is_some(),
                    std::sync::atomic::Ordering::SeqCst,
                );
                *draft_node_ids.lock().expect("draft ids") = node_ids;
                Ok(Vec::new())
            })
        })),
    );
    let core = explicit_ephemeral_facets(LashCore::standard_builder(crate::TurnBudget::Unbounded))
        .provider(rolling_history_provider(vec![
            response_with_usage("first response", 1),
            response_with_usage("second response", 1),
        ]))
        .model(model_spec("same-turn-model", None, 40_000))
        .plugin(Arc::new(plugin))
        .store_factory(store_factory.clone())
        .build(crate::testing::runtime_lease_owner())?;
    let session = core.session(session_id).open().await?;
    // Before the fix this turn failed its own final commit:
    // `store head revision conflict: expected 0, actual 1`.
    session
        .turn(TurnInput::text("first request"))
        .turn_id("same-turn-first")
        .run()
        .await?;
    let draft_node_ids = draft_node_ids.lock().expect("draft ids").clone();
    assert_eq!(draft_node_ids.len(), 1, "the hook appended exactly once");
    assert!(
        visible_in_turn.load(std::sync::atomic::Ordering::SeqCst),
        "the in-turn read snapshot must show the appended node immediately"
    );

    let nodes = sqlite_nodes(store_factory.as_ref(), &SessionId::from(session_id));
    for pair in nodes.windows(2) {
        assert_eq!(
            pair[1].parent_node_id.as_deref(),
            Some(pair[0].node_id.as_str()),
            "the durable graph is one continuous chain in commit order"
        );
    }
    assert_eq!(
        nodes
            .iter()
            .filter_map(|node| node.message())
            .map(|message| message_text(&message))
            .collect::<Vec<_>>(),
        vec!["first request".to_string(), "first response".to_string()],
        "the turn's own messages persist once each, in order"
    );
    let appended = nodes
        .iter()
        .filter(|node| {
            matches!(
                &node.payload,
                lash_core::SessionNodePayload::Plugin { plugin_type, .. }
                    if plugin_type == "test.same-turn"
            )
        })
        .collect::<Vec<_>>();
    assert_eq!(appended.len(), 1, "the appended node persists exactly once");
    let appended = appended[0];
    // The append was queued at `BeforeCompletion`; the next boundary the turn
    // applies carries the assistant reply, so the append lands behind it. No
    // boundary writes the graph on its own: the turn's commit is where the
    // queue becomes durable, together with the messages it followed.
    assert_eq!(
        nodes.last().map(|node| node.node_id.as_str()),
        Some(appended.node_id.as_str()),
        "the in-turn append follows the messages the next boundary carried"
    );
    let (durable_leaf, max_generation) =
        sqlite_head_and_max_generation(store_factory.as_ref(), &SessionId::from(session_id));
    assert_eq!(
        durable_leaf, appended.node_id,
        "the durable head is the appended node"
    );
    // The id answered in-turn is a draft id; the turn's commit derives the
    // durable id like it does for every other draft node.
    assert_ne!(appended.node_id, draft_node_ids[0]);
    assert!(
        nodes.iter().all(|node| node.node_id != draft_node_ids[0]),
        "draft ids never persist"
    );

    session
        .turn(TurnInput::text("second request"))
        .turn_id("same-turn-second")
        .run()
        .await?;
    let next = sqlite_node_rows(store_factory.as_ref(), &SessionId::from(session_id))
        .into_iter()
        .find(|(_, _, generation)| *generation > max_generation)
        .expect("turn two committed nodes");
    assert_eq!(
        next.1.as_deref(),
        Some(durable_leaf.as_str()),
        "the next turn extends the appended node, the durable head"
    );
    Ok(())
}

/// FIG-2478: an after-turn `EnqueueMessages` directive lands the enqueued
/// message after the reply inside the same final commit. Terminal
/// materialization must recognize the reply the protocol already appended by
/// identity, not by last-message position, or the reply is persisted twice
/// and becomes durable ancestry for every later turn.
#[tokio::test]
async fn after_turn_enqueue_persists_the_reply_exactly_once() -> Result<()> {
    let dir = tempfile::tempdir().expect("tempdir");
    let session_id = "after-turn-enqueue-single-reply";
    let store_factory = Arc::new(lash_sqlite_store::SqliteSessionStoreFactory::new(
        dir.path().join("sessions"),
    ));
    let plugin = crate::plugins::StaticPluginFactory::new(
        "after-turn-injection",
        lash_core::facade_support::PluginSpec::new().with_after_turn(Arc::new(|_| {
            Box::pin(async {
                Ok(vec![
                    lash_core::facade_support::AfterTurnPluginDirective::EnqueueMessages(
                        lash_core::facade_support::EnqueueMessagesDirective {
                            messages: vec![lash_core::PluginMessage::text(
                                lash_core::MessageRole::User,
                                "enqueued after turn",
                            )],
                        },
                    ),
                ])
            })
        })),
    );
    let core = explicit_ephemeral_facets(LashCore::standard_builder(crate::TurnBudget::Unbounded))
        .provider(rolling_history_provider(vec![response_with_usage(
            "first response",
            1,
        )]))
        .model(model_spec("after-turn-model", None, 40_000))
        .plugin(Arc::new(plugin))
        .store_factory(store_factory.clone())
        .build(crate::testing::runtime_lease_owner())?;
    let session = core.session(session_id).open().await?;
    session
        .turn(TurnInput::text("first request"))
        .turn_id("enqueue-once")
        .run()
        .await?;

    let durable = sqlite_messages(store_factory.as_ref(), &SessionId::from(session_id));
    let described = durable
        .iter()
        .map(|message| {
            format!(
                "{} {:?} {:?} {:?}",
                message.id,
                message.role,
                message_text(message),
                message.origin
            )
        })
        .collect::<Vec<_>>();
    let texts = durable.iter().map(message_text).collect::<Vec<_>>();
    assert_eq!(
        texts,
        vec![
            "first request".to_string(),
            "first response".to_string(),
            "enqueued after turn".to_string(),
        ],
        "durable messages must carry the reply exactly once: {described:?}"
    );
    let replies = durable
        .iter()
        .filter(|message| message.role == lash_core::MessageRole::Assistant)
        .count();
    assert_eq!(
        replies, 1,
        "exactly one durable reply node expected: {described:?}"
    );
    Ok(())
}
