// FIG-2971: this file is test/tooling/host code; ambient fs/env/process
// access is sanctioned here (the workspace clippy ban targets production
// library code).
#![allow(clippy::disallowed_methods)]

use super::*;
use lash_core::facade_support::SessionNodeProjection as _;
use lash_sansio::SessionId;
use std::collections::VecDeque;

fn message_text(message: &lash_core::Message) -> String {
    super::fixtures::role_and_text(message).1
}
use tokio::sync::Mutex as TokioMutex;

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

fn standard_compaction_provider(responses: Vec<LlmResponse>) -> ProviderHandle {
    standard_compaction_provider_counted(responses).0
}

fn standard_compaction_provider_counted(
    responses: Vec<LlmResponse>,
) -> (ProviderHandle, Arc<AtomicUsize>) {
    let calls = Arc::new(AtomicUsize::new(0));
    let responses = Arc::new(TokioMutex::new(VecDeque::from(responses)));
    let provider = crate::testing::TestProvider::builder()
        .kind("standard-compaction-persistence-test")
        .complete({
            let responses = Arc::clone(&responses);
            let calls = Arc::clone(&calls);
            move |_request| {
                let responses = Arc::clone(&responses);
                let calls = Arc::clone(&calls);
                async move {
                    calls.fetch_add(1, Ordering::SeqCst);
                    Ok(responses
                        .lock()
                        .await
                        .pop_front()
                        .expect("queued standard-compaction response"))
                }
            }
        })
        .build()
        .into_handle();
    (provider, calls)
}

/// A scripted provider that also records the messages of every request it
/// answers, serialized, so a test can read what the model was shown.
fn standard_compaction_provider_recorded(
    responses: Vec<LlmResponse>,
) -> (ProviderHandle, Arc<StdMutex<Vec<String>>>) {
    let requests = Arc::new(StdMutex::new(Vec::new()));
    let responses = Arc::new(TokioMutex::new(VecDeque::from(responses)));
    let provider = crate::testing::TestProvider::builder()
        .kind("standard-compaction-frame-test")
        .complete({
            let responses = Arc::clone(&responses);
            let requests = Arc::clone(&requests);
            move |request: LlmRequest| {
                let responses = Arc::clone(&responses);
                requests.lock_recover().push(
                    serde_json::to_string(&request.messages).expect("serialize request messages"),
                );
                async move {
                    Ok(responses
                        .lock()
                        .await
                        .pop_front()
                        .expect("queued standard-compaction response"))
                }
            }
        })
        .build()
        .into_handle();
    (provider, requests)
}

/// FIG-4029: a frame is the context window. Every standard compaction —
/// context pressure, an explicit `compact_context`, overflow recovery —
/// leaves the session resident in a fresh compaction frame seeded with the
/// summary, with none of the prior frame's nodes in it. Returns the new
/// frame's node id.
fn assert_resident_in_fresh_compaction_frame(
    before: &lash_core::SessionReadView,
    after: &lash_core::SessionReadView,
    summary: &str,
) -> lash_core::FrameNodeId {
    let before_frame = before.to_snapshot().current_frame_node_id;
    let after_snapshot = after.to_snapshot();
    let frame = after_snapshot
        .current_frame_node_id
        .clone()
        .expect("the session is resident in a frame");
    assert_ne!(Some(&frame), before_frame.as_ref(), "a new frame opened");
    let record = after_snapshot
        .agent_frames
        .iter()
        .find(|record| record.frame_node_id == frame)
        .expect("the resident frame has a frame record");
    assert_eq!(
        record.reason.as_str(),
        lash_core::AgentFrameReason::COMPACTION
    );
    assert_eq!(record.previous_frame_node_id, before_frame);

    let prior_ids = before
        .messages()
        .iter()
        .map(|message| message.id.clone())
        .collect::<std::collections::HashSet<_>>();
    let resident = after.messages();
    let carried = resident
        .iter()
        .filter(|message| prior_ids.contains(&message.id))
        .map(|message| message.id.as_str())
        .collect::<Vec<_>>();
    assert!(
        carried.is_empty(),
        "the prior frame's nodes must not be in the resident frame: {carried:?}"
    );
    let seed = resident.first().expect("the fresh frame has its seed");
    assert!(
        message_text(seed).starts_with("Compaction summary:")
            && message_text(seed).contains(summary),
        "the fresh frame is seeded with the summary: {:?}",
        resident.iter().map(message_text).collect::<Vec<_>>()
    );
    frame
}

/// FIG-4029: the explicit `compact_context` path meets the same frame
/// assertion, and the next turn continues in the frame it opened.
#[tokio::test]
async fn explicit_compaction_opens_a_summary_frame_the_next_turn_continues_in() -> Result<()> {
    let session_id = "standard-compaction-explicit-frame";
    let backend = sqlite_memory_store_backend().await;
    let core = explicit_ephemeral_facets(LashCore::standard_builder(backend.clone()))
        .serve_test_llm_profile(
            standard_compaction_provider(vec![
                response_with_usage("first response", 1),
                response_with_usage("second response", 1),
                response_with_usage("explicit summary", 1),
                response_with_usage("after response", 1),
            ]),
            llm_profile_spec("standard-compaction-model", None, 40_000),
        )
        .plugin(Arc::new(
            lash_plugin_standard_compaction::StandardCompactionPluginFactory::default(),
        ))
        .build(crate::testing::runtime_lease_owner())?;
    let session = core
        .session(crate::SessionId::parse(session_id).expect("nonblank host identity"))
        .created_with(session_spec_for(&llm_profile_spec(
            "standard-compaction-model",
            None,
            40_000,
        )))
        .await
        .open()
        .await?;
    for (turn_id, text) in [
        ("standard-compaction-explicit-one", "first request"),
        ("standard-compaction-explicit-two", "second request"),
    ] {
        session
            .send(TurnInput::text(text))
            .id(crate::TurnId::parse(turn_id).expect("nonblank host identity"))
            .output()
            .await?;
    }
    let before = committed(&session).await;
    assert!(
        Box::pin(session.admin().state().compact_context(
            None,
            "host:standard_compaction_persistence:compact_context:186".to_string()
        ))
        .await?
        .settle_with(
            &session.admin().commands(),
            crate::testing::admin_fixture_outcome
        )
        .await?
    );
    let after = committed(&session).await;
    let frame = assert_resident_in_fresh_compaction_frame(&before, &after, "explicit summary");
    assert_eq!(after.messages().len(), 1);

    session
        .send(TurnInput::text("after request"))
        .id(crate::TurnId::parse("standard-compaction-explicit-after")
            .expect("nonblank host identity"))
        .output()
        .await?;
    let next = committed(&session).await;
    assert_eq!(next.to_snapshot().current_frame_node_id, Some(frame));
    assert_eq!(
        next.messages().iter().map(message_text).collect::<Vec<_>>()[1..],
        ["after request", "after response"]
    );
    Ok(())
}

/// The recovery records the store holds, in commit order: each record's kind
/// and the frame it belongs to.
fn sqlite_recovery_records(
    store_factory: &lash_sqlite_store::SqliteStoreSet,
    session_id: &str,
) -> Vec<(String, String, serde_json::Value)> {
    let conn = rusqlite::Connection::open(store_factory.database_uri())
        .expect("open SQLite session catalog");
    let mut stmt = conn
        .prepare(
            "SELECT node_id, parent_node_id, node_json, frame_node_id FROM graph_nodes
             WHERE session_id = ?1 ORDER BY generation ASC",
        )
        .expect("prepare graph-node read");
    stmt.query_map([session_id], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, Option<String>>(1)?,
            row.get::<_, String>(2)?,
            row.get::<_, String>(3)?,
        ))
    })
    .expect("read graph nodes")
    .filter_map(|row| {
        let (node_id, parent_node_id, node_json, frame) = row.expect("decode graph-node row");
        let node =
            lash_core::SessionNodeRecord::decode_storage_body(node_id, parent_node_id, &node_json)
                .expect("decode stored graph node");
        let (plugin_type, body) = node.plugin()?;
        if plugin_type != "standard_compaction.overflow_recovery" {
            return None;
        }
        // Each marker is stamped with its format around the record (FIG-5028).
        let record = &body["record"];
        let kind = record["kind"]
            .as_str()
            .expect("typed recovery record kind")
            .to_string();
        Some((kind, frame, record.clone()))
    })
    .collect()
}

/// The `FrameOpen` node ids the store holds, in commit order.
fn sqlite_frame_opens(
    store_factory: &lash_sqlite_store::SqliteStoreSet,
    session_id: &str,
) -> Vec<String> {
    sqlite_nodes(store_factory, &SessionId::fixture(session_id))
        .into_iter()
        .filter(|node| {
            matches!(
                node.payload,
                lash_core::SessionNodePayload::FrameOpen { .. }
            )
        })
        .map(|node| node.node_id.to_string())
        .collect()
}

/// FIG-4110: a recovery whose summarizer fails records `Failed` for each
/// attempt, the attempt at the cap also records `Exhausted`, no attempt opens
/// a frame, and after the cap no turn asks the summarizer again.
#[tokio::test]
async fn overflow_recovery_failures_record_failed_then_exhausted_without_a_frame() -> Result<()> {
    let session_id = "standard-compaction-recovery-exhausted";
    let store_factory = sqlite_memory_store_set().await;
    let backend = lash_conformance::backend_over(store_factory.clone());
    let mut responses = vec![LlmResponse {
        terminal_reason: lash_core::LlmTerminalReason::ContextOverflow,
        terminal_diagnostic: Some("prompt is too long".to_string()),
        response_metadata: Default::default(),
        ..LlmResponse::default()
    }];
    // Each recovering turn: an empty summary, then
    // the turn's own answer on the frame it stayed in.
    for attempt in 1..=lash_plugin_standard_compaction::OVERFLOW_RECOVERY_MAX_ATTEMPTS {
        responses.push(response_with_usage("", 1));
        responses.push(response_with_usage(&format!("answer {attempt}"), 1));
    }
    responses.push(response_with_usage("answer after the cap", 1));
    let (provider, calls) = standard_compaction_provider_counted(responses);
    let core = explicit_ephemeral_facets(LashCore::standard_builder(backend.clone()))
        .serve_test_llm_profile(
            provider,
            llm_profile_spec("standard-compaction-model", None, 200_000),
        )
        .plugin(Arc::new(
            lash_plugin_standard_compaction::StandardCompactionPluginFactory::default(),
        ))
        .build(crate::testing::runtime_lease_owner())?;
    let session = core
        .session(crate::SessionId::parse(session_id).expect("nonblank host identity"))
        .created_with(session_spec_for(&llm_profile_spec(
            "standard-compaction-model",
            None,
            200_000,
        )))
        .await
        .open()
        .await?;
    let overflow = session
        .send(TurnInput::text("summarize the report"))
        .id(
            crate::TurnId::parse("standard-compaction-exhausted-overflow")
                .expect("nonblank host identity"),
        )
        .output()
        .await?;
    assert!(
        overflow.result.is_context_overflow(),
        "{:?}",
        overflow.result
    );
    let first_frame = committed(&session)
        .await
        .to_snapshot()
        .current_frame_node_id;

    for attempt in 1..=lash_plugin_standard_compaction::OVERFLOW_RECOVERY_MAX_ATTEMPTS {
        let turn = session
            .send(TurnInput::text(format!("try again {attempt}")))
            .id(lash_core::TurnId::fixture(format!(
                "standard-compaction-exhausted-{attempt}"
            )))
            .output()
            .await?;
        assert!(turn.result.is_success(), "{:?}", turn.result);
        assert_eq!(
            committed(&session)
                .await
                .to_snapshot()
                .current_frame_node_id,
            first_frame,
            "a failed attempt opens no frame"
        );
    }
    session
        .send(TurnInput::text("after the cap"))
        .id(crate::TurnId::parse("standard-compaction-exhausted-after")
            .expect("nonblank host identity"))
        .output()
        .await?;

    assert_eq!(
        calls.load(Ordering::SeqCst),
        2 + 2 * lash_plugin_standard_compaction::OVERFLOW_RECOVERY_MAX_ATTEMPTS,
        "the overflow, a summarizer call and an answer per attempt, and one answer after the cap"
    );
    let records = sqlite_recovery_records(store_factory.as_ref(), session_id);
    assert_eq!(
        records
            .iter()
            .map(|(kind, _, _)| kind.as_str())
            .collect::<Vec<_>>(),
        ["pending", "failed", "failed", "failed", "exhausted"]
    );
    for (attempt, (_, _, body)) in records
        [1..=lash_plugin_standard_compaction::OVERFLOW_RECOVERY_MAX_ATTEMPTS]
        .iter()
        .enumerate()
    {
        assert_eq!(
            body,
            &serde_json::json!({
                "kind": "failed", "attempt": attempt + 1, "cause": {"kind": "empty_summary"},
            })
        );
    }
    assert_eq!(
        sqlite_frame_opens(store_factory.as_ref(), session_id).len(),
        1,
        "no recovery frame opened"
    );
    Ok(())
}

fn sqlite_head_and_max_generation(
    stores: &lash_sqlite_store::SqliteStoreSet,
    session_id: &SessionId,
) -> (String, i64) {
    let conn =
        rusqlite::Connection::open(stores.database_uri()).expect("open SQLite session catalog");
    let leaf = conn
        .query_row(
            "SELECT leaf_node_id FROM session_head JOIN session_revisions USING (session_id, head_revision) WHERE session_id = ?1",
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
    stores: &lash_sqlite_store::SqliteStoreSet,
    session_id: &SessionId,
) -> Vec<lash_core::SessionNodeRecord> {
    let conn =
        rusqlite::Connection::open(stores.database_uri()).expect("open SQLite session catalog");
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
    stores: &lash_sqlite_store::SqliteStoreSet,
    session_id: &SessionId,
) -> Vec<lash_core::Message> {
    sqlite_nodes(stores, session_id)
        .iter()
        .filter_map(|node| node.message())
        .collect()
}

/// Repeated administrative compactions over a changed snapshot each open
/// their own frame, and each settles its summarizer's billed usage in the
/// commit that opens it (FIG-3374, FIG-4201).
#[tokio::test]
async fn repeated_admin_compactions_distinguish_changed_snapshots() -> Result<()> {
    let session_id = "standard-compaction-repeat-admin";
    let expected_summaries = ["first summary", "second summary", "third summary"];
    let mut responses = vec![
        response_with_usage("first response", 1),
        response_with_usage("second response", 1),
    ];
    responses.extend(
        expected_summaries
            .iter()
            .map(|summary| response_with_usage(summary, 1)),
    );
    let backend = sqlite_memory_store_backend().await;
    let core = explicit_ephemeral_facets(LashCore::standard_builder(backend.clone()))
        .serve_test_llm_profile(
            standard_compaction_provider(responses),
            llm_profile_spec("standard-compaction-model", None, 40_000),
        )
        .plugin(Arc::new(
            lash_plugin_standard_compaction::StandardCompactionPluginFactory::default(),
        ))
        .build(crate::testing::runtime_lease_owner())?;
    let session = core
        .session(crate::SessionId::parse(session_id).expect("nonblank host identity"))
        .created_with(session_spec_for(&llm_profile_spec(
            "standard-compaction-model",
            None,
            40_000,
        )))
        .await
        .open()
        .await?;
    for (turn_id, text) in [
        ("standard-compaction-same-parent-one", "first request"),
        ("standard-compaction-same-parent-two", "second request"),
    ] {
        session
            .send(TurnInput::text(text))
            .id(crate::TurnId::parse(turn_id).expect("nonblank host identity"))
            .output()
            .await?;
    }

    for (index, expected_summary) in expected_summaries.into_iter().enumerate() {
        assert!(
            Box::pin(session.admin().state().compact_context(
                Some("keep the same administrative focus".to_string()),
                format!("administrative-focus:{index}")
            ))
            .await?
            .settle_with(
                &session.admin().commands(),
                crate::testing::admin_fixture_outcome
            )
            .await?,
            "each changed snapshot remains a valid administrative compaction request"
        );
        let view = committed(&session).await;
        assert_eq!(view.messages().len(), 1);
        assert!(
            view.messages()[0].parts[0]
                .content()
                .contains(expected_summary)
        );
    }
    Ok(())
}

#[tokio::test]
async fn standard_compaction_threshold_turn_commits_from_durable_leaf_and_unblocks_compaction()
-> Result<()> {
    let dir = tempfile::tempdir().expect("tempdir");
    let session_id = "standard-compaction-durable-parent";
    let trace_path = dir.path().join("trace.jsonl");
    let store_factory = sqlite_memory_store_set().await;
    let backend = lash_conformance::backend_over(store_factory.clone());
    let provider = standard_compaction_provider(vec![
        response_with_usage("first response", 20_000),
        response_with_usage("threshold summary", 1),
        response_with_usage("threshold response", 1),
        response_with_usage("durable summary", 1),
    ]);
    let core = explicit_ephemeral_facets(LashCore::standard_builder(backend.clone()))
        .serve_test_llm_profile(
            provider,
            llm_profile_spec("standard-compaction-model", None, 40_000),
        )
        .plugin(Arc::new(
            lash_plugin_standard_compaction::StandardCompactionPluginFactory::default(),
        ))
        .trace_jsonl_path(trace_path.clone())
        .build(crate::testing::runtime_lease_owner())?;
    let session = core
        .session(crate::SessionId::parse(session_id).expect("nonblank host identity"))
        .created_with(session_spec_for(&llm_profile_spec(
            "standard-compaction-model",
            None,
            40_000,
        )))
        .await
        .open()
        .await?;

    session
        .send(TurnInput::text("first request"))
        .id(crate::TurnId::parse("standard-compaction-first").expect("nonblank host identity"))
        .output()
        .await?;
    let (durable_leaf_before_threshold, max_generation_before_threshold) =
        sqlite_head_and_max_generation(store_factory.as_ref(), &SessionId::from(session_id));

    session
        .send(TurnInput::text("threshold request"))
        .id(crate::TurnId::parse("standard-compaction-threshold").expect("nonblank host identity"))
        .output()
        .await?;

    let conn = rusqlite::Connection::open(store_factory.database_uri())
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
        threshold_node_count, 4,
        "the threshold turn must append exactly its compaction frame, the summary seed, its new user message and its assistant outcome"
    );
    assert_eq!(
        first_threshold_parent, durable_leaf_before_threshold,
        "the threshold turn must extend the durable leaf that was current when the turn began"
    );

    assert!(
        Box::pin(session.admin().state().compact_context(
            Some("retain the durable ancestry result".to_string()),
            "host:standard_compaction_persistence:compact_context:594".to_string()
        ))
        .await?
        .settle_with(
            &session.admin().commands(),
            crate::testing::admin_fixture_outcome
        )
        .await?,
        "standard-compaction compaction should open a summary frame after the threshold turn commits"
    );
    let (post_compaction_leaf, post_compaction_max_generation) =
        sqlite_head_and_max_generation(store_factory.as_ref(), &SessionId::from(session_id));
    core.flush_trace_sink()?;

    let trace = std::fs::read_to_string(trace_path).expect("read standard-compaction trace");
    let records =
        lash_trace::parse_jsonl_records::<serde_json::Value>(&trace).expect("decode trace records");
    for event_type in ["compaction_started", "compaction_completed"] {
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
            Some("session:standard-compaction-durable-parent")
        );
    }

    // FIG-4029: the threshold turn's prompt is the frame it switched to, not
    // a pruned view over the frame it left, so no turn projected its prompt.
    assert!(
        !records.iter().any(|record| {
            record.get("name").and_then(serde_json::Value::as_str)
                == Some("session_graph.read_projection")
        }),
        "no prompt projection over durable history"
    );

    drop(session);
    drop(core);
    let reopened_core = explicit_ephemeral_facets(LashCore::standard_builder(backend.clone()))
        .serve_test_llm_profile(
            standard_compaction_provider(vec![response_with_usage("response after reopen", 1)]),
            llm_profile_spec("standard-compaction-model", None, 40_000),
        )
        .plugin(Arc::new(
            lash_plugin_standard_compaction::StandardCompactionPluginFactory::default(),
        ))
        .build(crate::testing::runtime_lease_owner())?;
    let reopened_session = reopened_core
        .session(crate::SessionId::parse(session_id).expect("nonblank host identity"))
        .created_with(session_spec_for(&llm_profile_spec(
            "standard-compaction-model",
            None,
            40_000,
        )))
        .await
        .open()
        .await?;
    reopened_session
        .send(TurnInput::text("continue after compaction"))
        .id(crate::TurnId::parse("standard-compaction-reopened").expect("nonblank host identity"))
        .output()
        .await?;
    let conn = rusqlite::Connection::open(store_factory.database_uri())
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
    let session_id = "standard-compaction-attachment-prune";
    let trace_path = dir.path().join("trace.jsonl");
    let store_factory = sqlite_memory_store_set().await;
    let backend = lash_conformance::backend_over(store_factory.clone());
    let (provider, requests) = standard_compaction_provider_recorded(vec![
        response_with_usage("first response", 60_000),
        response_with_usage("second response", 1),
    ]);
    let core = explicit_ephemeral_facets(LashCore::standard_builder(backend.clone()))
        .serve_test_llm_profile(
            provider,
            llm_profile_spec("attachment-prune-model", None, 100_000),
        )
        .plugin(Arc::new(
            lash_plugin_standard_compaction::StandardCompactionPluginFactory::default(),
        ))
        .trace_jsonl_path(trace_path.clone())
        .build(crate::testing::runtime_lease_owner())?;
    let session = core
        .session(crate::SessionId::parse(session_id).expect("nonblank host identity"))
        .created_with(session_spec_for(&llm_profile_spec(
            "attachment-prune-model",
            None,
            100_000,
        )))
        .await
        .open()
        .await?;

    session
        .send(
            TurnInput::text("remember this image").with_attachment(
                session
                    .put_attachment(
                        vec![1, 2, 3],
                        lash_core::AttachmentCreateMeta::new(
                            lash_core::MediaType::parse("image/png").expect("image media type"),
                            None,
                            None,
                        ),
                    )
                    .await?,
            ),
        )
        .id(crate::TurnId::parse("attachment-prune-first").expect("nonblank host identity"))
        .output()
        .await?;
    // The turn's input is admitted durably before it executes (ADR 0069), so its
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
        .send(TurnInput::text("trigger ephemeral pruning"))
        .id(crate::TurnId::parse("attachment-prune-second").expect("nonblank host identity"))
        .output()
        .await?;

    let durable_message = sqlite_messages(store_factory.as_ref(), &SessionId::from(session_id))
        .into_iter()
        .find(|message| message.id == first_input_message_id)
        .expect("first turn input remains durable");
    assert!(
        durable_message
            .parts
            .iter()
            .any(|part| part.attachment().is_some()),
        "durable transcript keeps the original attachment"
    );
    assert_eq!(
        serde_json::to_value(&durable_message)?,
        serde_json::to_value(&original_durable_message)?,
        "attachment pruning must not rewrite the durable message"
    );

    let requests = requests.lock_recover().clone();
    assert_eq!(requests.len(), 2);
    assert!(
        requests[0].contains("attachment"),
        "the first model request sees the attachment"
    );
    assert!(
        !requests[1].contains("attachment_ref"),
        "the next Prompt View prunes the old attachment"
    );
    let api_message = session
        .durable()
        .read()
        .await?
        .expect("the session has a head")
        .messages()
        .iter()
        .find(|message| message.id == first_input_message_id)
        .cloned()
        .expect("API input");
    assert_eq!(
        serde_json::to_value(api_message)?,
        serde_json::to_value(&original_durable_message)?,
        "the API keeps the attachment pruned from the Prompt View"
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
    assert!(
        mismatch_record.is_none(),
        "a Prompt View never reaches the durable projection"
    );

    Ok(())
}

#[cfg(feature = "rlm")]
#[tokio::test]
async fn threshold_continue_as_extends_the_pre_switch_durable_leaf() -> Result<()> {
    let session_id = "standard-compaction-continue-as-parent";
    let store_factory = sqlite_memory_store_set().await;
    let backend = lash_conformance::backend_over(store_factory.clone());
    let provider = standard_compaction_provider(vec![
        response_with_usage(&typescript_block(r#"finish("primed");"#), 20_000),
        response_with_usage(
            &typescript_block(
                r#"await control.continue_as({ task: "finish from the new frame" });"#,
            ),
            1,
        ),
        response_with_usage(&typescript_block(r#"finish("continued");"#), 1),
    ]);
    let core = explicit_ephemeral_facets(rlm_core_builder_over(backend.clone()))
        .serve_test_llm_profile(
            provider,
            llm_profile_spec("standard-compaction-rlm-model", None, 40_000),
        )
        .build(crate::testing::runtime_lease_owner())?;
    let session = core
        .session(crate::SessionId::parse(session_id).expect("nonblank host identity"))
        .created_with(session_spec_for(&llm_profile_spec(
            "standard-compaction-rlm-model",
            None,
            40_000,
        )))
        .await
        .open()
        .await?;

    let primed = session
        .send(TurnInput::text("prime durable history"))
        .id(crate::TurnId::parse("standard-compaction-rlm-first").expect("nonblank host identity"))
        .output()
        .await?;
    assert_eq!(primed.final_value(), Some(&serde_json::json!("primed")));
    let (durable_leaf_before_switch, max_generation_before_switch) =
        sqlite_head_and_max_generation(store_factory.as_ref(), &SessionId::from(session_id));

    // The switch answers its send; the frame's task runs next as its own
    // run (FIG-5232).
    let switched = session
        .send(TurnInput::text("cross the threshold and continue"))
        .id(crate::TurnId::parse("standard-compaction-rlm-threshold")
            .expect("nonblank host identity"))
        .output()
        .await?;
    let lash_core::facade_support::TurnOutcome::AgentFrameSwitch { frame_key, .. } =
        &switched.result.outcome
    else {
        panic!("the switch answers its send: {:?}", switched.result.outcome);
    };
    let continued = session
        .attach_id(lash_core::runtime::durable::session_mail::frame_task_run(
            frame_key,
        ))
        .output()
        .await?;
    assert_eq!(
        continued.final_value(),
        Some(&serde_json::json!("continued"))
    );

    let conn = rusqlite::Connection::open(store_factory.database_uri())
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
