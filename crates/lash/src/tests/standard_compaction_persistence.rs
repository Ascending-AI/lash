// FIG-2971: this file is test/tooling/host code; ambient fs/env/process
// access is sanctioned here (the workspace clippy ban targets production
// library code).
#![allow(clippy::disallowed_methods)]

use super::*;
use lash_core::testing::{Script, StoreOp};
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

/// The frame the durable head's leaf belongs to.
fn sqlite_leaf_frame(stores: &lash_sqlite_store::SqliteStoreSet, session_id: &str) -> String {
    rusqlite::Connection::open(stores.database_uri(lash_sqlite_store::SqliteDatabase::DurableCore))
        .expect("open SQLite session catalog")
        .query_row(
            "SELECT g.frame_node_id FROM session_head h JOIN session_revisions r USING (session_id, head_revision)
             JOIN graph_nodes g ON g.node_id = r.leaf_node_id
             WHERE h.session_id = ?1",
            [session_id],
            |row| row.get::<_, String>(0),
        )
        .expect("read the durable leaf's frame")
}

/// A Prompt View transform that runs after standard compaction's and
/// records the window and the committed read view it was handed.
#[derive(Default)]
struct WindowProbe {
    observed: StdMutex<Option<(Vec<String>, Vec<String>)>>,
}

impl WindowProbe {
    fn plugin(self: &Arc<Self>) -> Arc<dyn lash_core::facade_support::PluginFactory> {
        Arc::new(crate::plugins::StaticPluginFactory::new(
            lash_core::plugin::PluginDeclaration::initial("standard-compaction-window-probe"),
            lash_core::facade_support::PluginSpec::new()
                .with_turn_context_transform(0, Arc::clone(self) as _),
        ))
    }

    fn clear(&self) {
        *self.observed.lock_recover() = None;
    }

    fn observed(&self) -> (Vec<String>, Vec<String>) {
        self.observed
            .lock_recover()
            .clone()
            .expect("the probe transform ran")
    }
}

#[async_trait::async_trait]
impl lash_core::facade_support::TurnContextTransform for WindowProbe {
    fn id(&self) -> &'static str {
        "standard-compaction-window-probe"
    }

    async fn transform(
        &self,
        ctx: &lash_core::facade_support::TurnTransformContext<'_>,
        input: lash_core::facade_support::PreparedContext,
    ) -> std::result::Result<
        lash_core::facade_support::PreparedContext,
        lash_core::facade_support::ContextError,
    > {
        *self.observed.lock_recover() = Some((
            input.messages.iter().map(message_text).collect(),
            ctx.state.messages().iter().map(message_text).collect(),
        ));
        Ok(input)
    }
}

/// FIG-4029: crossing the pressure threshold starts a frame the way an
/// explicit compaction does — the summary seeds it — and the turn that
/// crossed the threshold continues inside it, on the same resident session.
#[tokio::test]
async fn pressure_compaction_opens_a_summary_frame_the_turn_continues_in() -> Result<()> {
    let session_id = "standard-compaction-pressure-frame";
    let backend = double_backend().await;
    let store_factory = Arc::clone(
        latest_double()
            .expect("the backend runs on its held double")
            .stores(),
    );
    let (provider, requests) = standard_compaction_provider_recorded(vec![
        // 20,000 prompt tokens reach the 20,000-token threshold of a 40,000-token window.
        response_with_usage("first response", 20_000),
        response_with_usage("pressure summary", 1),
        response_with_usage("threshold response", 1),
        response_with_usage("after response", 1),
    ]);
    let window_probe = Arc::new(WindowProbe::default());
    let core = explicit_ephemeral_facets(LashCore::standard_builder(backend.clone()))
        .serve_test_llm_profile(
            provider,
            llm_profile_spec("standard-compaction-model", None, 40_000),
        )
        .plugin(Arc::new(
            lash_plugin_standard_compaction::StandardCompactionPluginFactory::default(),
        ))
        .plugin(window_probe.plugin())
        .build(crate::testing::runtime_lease_owner())?;
    let session = core
        .session(session_id)
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
        .id("standard-compaction-pressure-first")
        .output()
        .await?;
    let before = session.read_view();
    window_probe.clear();

    let threshold = session
        .send(TurnInput::text("threshold request"))
        .id("standard-compaction-pressure-threshold")
        .output()
        .await?;
    assert!(threshold.result.is_success(), "{:?}", threshold.result);
    let after = session.read_view();
    let frame = assert_resident_in_fresh_compaction_frame(&before, &after, "pressure summary");
    // The turn continued in the new frame: its own request and reply follow
    // the seed there, and the model answered it from the summary window.
    assert_eq!(
        after
            .messages()
            .iter()
            .map(message_text)
            .collect::<Vec<_>>()[1..],
        ["threshold request", "threshold response"]
    );
    // FIG-4110: core opened the frame before the Prompt View transforms ran,
    // so pruning and every transform see the new frame's window and read
    // view, never the frame it left.
    let (window, committed) = window_probe.observed();
    assert_eq!(
        window,
        [
            "Compaction summary:\npressure summary".to_string(),
            "threshold request".to_string()
        ],
        "the transforms see the seed and the turn's own request"
    );
    assert_eq!(
        committed,
        ["Compaction summary:\npressure summary".to_string()],
        "the transforms' read view is the new frame"
    );
    let requests = requests.lock_recover().clone();
    assert_eq!(requests.len(), 3, "turn one, the summarizer, turn two");
    let turn_prompt = &requests[2];
    assert!(
        turn_prompt.contains("pressure summary") && turn_prompt.contains("threshold request"),
        "{turn_prompt}"
    );
    assert!(
        !turn_prompt.contains("first request") && !turn_prompt.contains("first response"),
        "the turn's prompt is the new frame, not the prior one: {turn_prompt}"
    );
    // No reload: the durable head the turn committed is the frame the
    // resident session already stands in.
    assert_eq!(
        sqlite_leaf_frame(store_factory.as_ref(), session_id),
        frame.as_str()
    );

    // The next turn stays in that frame.
    session
        .send(TurnInput::text("after request"))
        .id("standard-compaction-pressure-after")
        .output()
        .await?;
    let next = session.read_view();
    assert_eq!(next.to_snapshot().current_frame_node_id, Some(frame));
    assert_eq!(
        next.messages().iter().map(message_text).collect::<Vec<_>>()[1..],
        [
            "threshold request",
            "threshold response",
            "after request",
            "after response"
        ]
    );
    Ok(())
}

/// FIG-4029: the explicit `compact_context` path meets the same frame
/// assertion, and the next turn continues in the frame it opened.
#[tokio::test]
async fn explicit_compaction_opens_a_summary_frame_the_next_turn_continues_in() -> Result<()> {
    let session_id = "standard-compaction-explicit-frame";
    let backend = double_backend().await;
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
        .session(session_id)
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
            .id(turn_id)
            .output()
            .await?;
    }
    let before = session.read_view();
    assert!(Box::pin(session.admin().state().compact_context(None)).await?);
    let after = session.read_view();
    let frame = assert_resident_in_fresh_compaction_frame(&before, &after, "explicit summary");
    assert_eq!(after.messages().len(), 1);

    session
        .send(TurnInput::text("after request"))
        .id("standard-compaction-explicit-after")
        .output()
        .await?;
    let next = session.read_view();
    assert_eq!(next.to_snapshot().current_frame_node_id, Some(frame));
    assert_eq!(
        next.messages().iter().map(message_text).collect::<Vec<_>>()[1..],
        ["after request", "after response"]
    );
    Ok(())
}

/// FIG-4029: overflow recovery meets the same frame assertion, and the turn
/// that recovered continues inside the recovery frame.
#[tokio::test]
async fn overflow_recovery_opens_a_summary_frame_the_recovered_turn_continues_in() -> Result<()> {
    let session_id = "standard-compaction-recovery-frame";
    let backend = double_backend().await;
    let store_factory = Arc::clone(
        latest_double()
            .expect("the backend runs on its held double")
            .stores(),
    );
    let (provider, requests) = standard_compaction_provider_recorded(vec![
        LlmResponse {
            terminal_reason: lash_core::LlmTerminalReason::ContextOverflow,
            terminal_diagnostic: Some("prompt is too long".to_string()),
            response_metadata: Default::default(),
            ..LlmResponse::default()
        },
        response_with_usage("recovery summary", 1),
        response_with_usage("verdict response", 1),
    ]);
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
        .session(session_id)
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
        .id("standard-compaction-recovery-overflow")
        .output()
        .await?;
    assert!(
        overflow.result.is_context_overflow(),
        "{:?}",
        overflow.result
    );
    let before = session.read_view();

    let recovered = session
        .send(TurnInput::text("now give me the verdict"))
        .id("standard-compaction-recovery-verdict")
        .output()
        .await?;
    assert!(recovered.result.is_success(), "{:?}", recovered.result);
    let after = session.read_view();
    assert_resident_in_fresh_compaction_frame(&before, &after, "recovery summary");
    assert_eq!(
        after
            .messages()
            .iter()
            .map(message_text)
            .collect::<Vec<_>>()[1..],
        ["now give me the verdict", "verdict response"]
    );
    let requests = requests.lock_recover().clone();
    assert_eq!(
        requests.len(),
        3,
        "the overflow, the summarizer, the verdict"
    );
    assert!(
        !requests[2].contains("summarize the report"),
        "{}",
        requests[2]
    );
    // FIG-4110: the marker, then the recovering turn's Completed record in
    // the frame it left, and exactly one recovery frame.
    let records = sqlite_recovery_records(store_factory.as_ref(), session_id);
    assert_eq!(
        records
            .iter()
            .map(|(kind, _, _)| kind.as_str())
            .collect::<Vec<_>>(),
        ["pending", "completed"]
    );
    let frames = sqlite_frame_opens(store_factory.as_ref(), session_id);
    assert_eq!(frames.len(), 2, "the first frame and one recovery frame");
    assert_eq!(
        records[1].1, frames[0],
        "the completed record stays in the frame the recovery left"
    );
    Ok(())
}

/// The recovery records the store holds, in commit order: each record's kind
/// and the frame it belongs to.
fn sqlite_recovery_records(
    store_factory: &lash_sqlite_store::SqliteStoreSet,
    session_id: &str,
) -> Vec<(String, String, serde_json::Value)> {
    let conn = rusqlite::Connection::open(
        store_factory.database_uri(lash_sqlite_store::SqliteDatabase::DurableCore),
    )
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
        let kind = body["kind"]
            .as_str()
            .expect("typed recovery record kind")
            .to_string();
        Some((kind, frame, body.clone()))
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
    let backend = double_backend().await;
    let store_factory = Arc::clone(
        latest_double()
            .expect("the backend runs on its held double")
            .stores(),
    );
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
        .session(session_id)
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
        .id("standard-compaction-exhausted-overflow")
        .output()
        .await?;
    assert!(
        overflow.result.is_context_overflow(),
        "{:?}",
        overflow.result
    );
    let first_frame = session.read_view().to_snapshot().current_frame_node_id;

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
            session.read_view().to_snapshot().current_frame_node_id,
            first_frame,
            "a failed attempt opens no frame"
        );
    }
    session
        .send(TurnInput::text("after the cap"))
        .id("standard-compaction-exhausted-after")
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

/// ADR 0112 §14.6: overflow recovery starts a new frame without a reload.
/// After the recovered turn's commits the head, the window base and the
/// resident state all name the recovery frame, the resident graph is that
/// frame alone, and nothing read the window back.
#[tokio::test]
async fn overflow_recovery_starts_a_frame_without_a_reload() -> Result<()> {
    let session_id = "standard-compaction-recovery-residency";
    let base = double_backend().await;
    let catalog = base.session_store_factory();
    let script = Arc::new(Script::new());
    let counted = Arc::clone(&script);
    let backend = DecoratedBackend::over(base)
        .session_store_factory(move |inner| counted.wrap("window", inner));
    let (provider, _requests) = standard_compaction_provider_recorded(vec![
        LlmResponse {
            terminal_reason: lash_core::LlmTerminalReason::ContextOverflow,
            terminal_diagnostic: Some("prompt is too long".to_string()),
            response_metadata: Default::default(),
            ..LlmResponse::default()
        },
        response_with_usage("recovery summary", 1),
        response_with_usage("verdict response", 1),
    ]);
    let core = explicit_ephemeral_facets(LashCore::standard_builder(backend.into()))
        .serve_test_llm_profile(
            provider,
            llm_profile_spec("standard-compaction-model", None, 200_000),
        )
        .plugin(Arc::new(
            lash_plugin_standard_compaction::StandardCompactionPluginFactory::default(),
        ))
        .build(crate::testing::runtime_lease_owner())?;
    let session = core
        .session(session_id)
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
        .id("standard-compaction-residency-overflow")
        .output()
        .await?;
    assert!(
        overflow.result.is_context_overflow(),
        "{:?}",
        overflow.result
    );
    let old_frame = session
        .read_view()
        .to_snapshot()
        .current_frame_node_id
        .expect("the first frame");
    let loads_before = script.calls(StoreOp::load_session_window);
    assert!(
        loads_before > 0,
        "the counter sits on the open's window read"
    );

    let recovered = session
        .send(TurnInput::text("now give me the verdict"))
        .id("standard-compaction-residency-verdict")
        .output()
        .await?;
    assert!(recovered.result.is_success(), "{:?}", recovered.result);
    assert_eq!(
        script.calls(StoreOp::load_session_window),
        loads_before,
        "the recovery frame is adopted from its commit; nothing reloads the window"
    );

    let writer = session.runtime.writer();
    let state = writer
        .lock()
        .await
        .export_persisted_state()
        .await
        .expect("export the resident state");
    let new_frame = state
        .current_frame_node_id
        .clone()
        .expect("the recovered state names its frame");
    assert_ne!(new_frame, old_frame, "recovery starts a new frame");
    let window = lash_core::runtime::live_session_view(&catalog, &SessionId::from(session_id))
        .await?
        .expect("the session is live")
        .load_session_window(lash_core::store::WindowSelector::Current)
        .await?
        .expect("the session has a head");
    assert_eq!(window.current_frame_node_id.as_ref(), Some(&new_frame));
    let anchor = window
        .window
        .anchor()
        .expect("a durable window is anchored");
    assert_eq!(
        anchor.frame_node_id, new_frame,
        "the window base is the recovery FrameOpen"
    );
    assert_eq!(anchor.previous_frame_node_id.as_ref(), Some(&old_frame));
    let resident = state
        .session_graph
        .nodes
        .iter()
        .map(|node| node.node_id.clone())
        .collect::<std::collections::HashSet<_>>();
    let durable = window
        .window
        .nodes
        .iter()
        .map(|node| node.node_id.clone())
        .collect::<std::collections::HashSet<_>>();
    assert_eq!(
        resident, durable,
        "the resident nodes are the recovery frame's"
    );
    assert!(
        state
            .persisted_node_ids
            .iter()
            .all(|node_id| resident.contains(node_id)),
        "every persisted id is resident"
    );
    assert_eq!(state.agent_frames.len(), 1, "one frame record is resident");
    assert_eq!(state.agent_frames[0].frame_node_id, new_frame);
    assert_eq!(
        state.agent_frames[0].previous_frame_node_id.as_ref(),
        Some(&old_frame)
    );
    Ok(())
}

fn sqlite_head_and_max_generation(
    stores: &lash_sqlite_store::SqliteStoreSet,
    session_id: &SessionId,
) -> (String, i64) {
    let conn = rusqlite::Connection::open(
        stores.database_uri(lash_sqlite_store::SqliteDatabase::DurableCore),
    )
    .expect("open SQLite session catalog");
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
    let conn = rusqlite::Connection::open(
        stores.database_uri(lash_sqlite_store::SqliteDatabase::DurableCore),
    )
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
    let backend = double_backend().await;
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
        .session(session_id)
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
            .id(turn_id)
            .output()
            .await?;
    }

    for expected_summary in expected_summaries {
        assert!(
            Box::pin(
                session
                    .admin()
                    .state()
                    .compact_context(Some("keep the same administrative focus".to_string()))
            )
            .await?,
            "each changed snapshot remains a valid administrative compaction request"
        );
        let view = session.read_view();
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
    let backend = double_backend().await;
    let store_factory = Arc::clone(
        latest_double()
            .expect("the backend runs on its held double")
            .stores(),
    );
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
        .session(session_id)
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
        .id("standard-compaction-first")
        .output()
        .await?;
    let (durable_leaf_before_threshold, max_generation_before_threshold) =
        sqlite_head_and_max_generation(store_factory.as_ref(), &SessionId::from(session_id));

    session
        .send(TurnInput::text("threshold request"))
        .id("standard-compaction-threshold")
        .output()
        .await?;

    let conn = rusqlite::Connection::open(
        store_factory.database_uri(lash_sqlite_store::SqliteDatabase::DurableCore),
    )
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
        Box::pin(
            session
                .admin()
                .state()
                .compact_context(Some("retain the durable ancestry result".to_string()))
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
        .session(session_id)
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
        .id("standard-compaction-reopened")
        .output()
        .await?;
    let conn = rusqlite::Connection::open(
        store_factory.database_uri(lash_sqlite_store::SqliteDatabase::DurableCore),
    )
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
async fn repeated_compactions_use_distinct_physical_parents() -> Result<()> {
    let session_id = "standard-compaction-repeated-compactions";
    let backend = double_backend().await;
    let core = explicit_ephemeral_facets(LashCore::standard_builder(backend.clone()))
        .serve_test_llm_profile(
            standard_compaction_provider(vec![
                response_with_usage("first response", 1),
                response_with_usage("second response", 1),
                response_with_usage("first summary", 1),
                response_with_usage("third response", 1),
                response_with_usage("fourth response", 1),
                response_with_usage("second summary", 1),
            ]),
            llm_profile_spec("standard-compaction-model", None, 40_000),
        )
        .plugin(Arc::new(
            lash_plugin_standard_compaction::StandardCompactionPluginFactory::default(),
        ))
        .build(crate::testing::runtime_lease_owner())?;
    let session = core
        .session(session_id)
        .created_with(session_spec_for(&llm_profile_spec(
            "standard-compaction-model",
            None,
            40_000,
        )))
        .await
        .open()
        .await?;

    for (turn_id, text) in [
        ("standard-compaction-repeat-one", "first request"),
        ("standard-compaction-repeat-two", "second request"),
    ] {
        session
            .send(TurnInput::text(text))
            .id(turn_id)
            .output()
            .await?;
    }
    assert!(
        Box::pin(
            session
                .admin()
                .state()
                .compact_context(Some("first compaction".to_string()))
        )
        .await?
    );

    for (turn_id, text) in [
        ("standard-compaction-repeat-three", "third request"),
        ("standard-compaction-repeat-four", "fourth request"),
    ] {
        session
            .send(TurnInput::text(text))
            .id(turn_id)
            .output()
            .await?;
    }
    assert!(
        Box::pin(
            session
                .admin()
                .state()
                .compact_context(Some("second compaction".to_string()))
        )
        .await?,
        "a later physical parent must not replay the earlier compaction child"
    );
    assert_eq!(session.read_view().messages().len(), 1);
    assert!(
        session.read_view().messages()[0].parts[0]
            .content()
            .contains("second summary")
    );
    Ok(())
}

#[tokio::test]
async fn attachment_pruning_never_rewrites_the_durable_message() -> Result<()> {
    let dir = tempfile::tempdir().expect("tempdir");
    let session_id = "standard-compaction-attachment-prune";
    let trace_path = dir.path().join("trace.jsonl");
    let backend = double_backend().await;
    let store_factory = Arc::clone(
        latest_double()
            .expect("the backend runs on its held double")
            .stores(),
    );
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
        .session(session_id)
        .created_with(session_spec_for(&llm_profile_spec(
            "attachment-prune-model",
            None,
            100_000,
        )))
        .await
        .open()
        .await?;

    session
        .send(TurnInput::text("remember this image").with_attachment(
            lash_core::AttachmentSource::inline(
                lash_core::MediaType::parse("image/png").expect("image media type"),
                vec![1, 2, 3],
            ),
        ))
        .id("attachment-prune-first")
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
        .id("attachment-prune-second")
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
        .read_view()
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

#[tokio::test]
async fn before_turn_plugin_messages_remain_durable_across_threshold_turns() -> Result<()> {
    const THRESHOLD_TURNS: usize = 3;
    let session_id = "standard-compaction-plugin-message-ids";
    let backend = double_backend().await;
    let store_factory = Arc::clone(
        latest_double()
            .expect("the backend runs on its held double")
            .stores(),
    );
    let next_injection = Arc::new(AtomicUsize::new(0));
    let injection_hook = {
        let next_injection = Arc::clone(&next_injection);
        Arc::new(move |_| {
            let ordinal = next_injection.fetch_add(1, Ordering::SeqCst);
            Box::pin(async move {
                Ok(lash_core::plugin::TurnContributions {
                    messages: vec![lash_core::PluginMessage::text(
                        lash_core::MessageRole::User,
                        format!("plugin injection {ordinal}"),
                    )],
                    ..Default::default()
                })
            }) as lash_core::plugin::PluginFuture<_>
        })
    };
    let injection_plugin = crate::plugins::StaticPluginFactory::new(
        lash_core::plugin::PluginDeclaration::initial("standard-compaction-injection-test"),
        lash_core::facade_support::PluginSpec::new()
            .with_before_turn(crate::hook_key!("before-turn-1"), injection_hook),
    );
    // Every turn after the first crosses the threshold, so its pressure
    // compaction's summary is answered before the turn's own response.
    let responses = (0..=THRESHOLD_TURNS)
        .flat_map(|ordinal| {
            let summary =
                (ordinal > 0).then(|| response_with_usage(&format!("summary {ordinal}"), 1));
            summary
                .into_iter()
                .chain([response_with_usage(&format!("response {ordinal}"), 20_000)])
        })
        .collect();
    let core = explicit_ephemeral_facets(LashCore::standard_builder(backend.clone()))
        .serve_test_llm_profile(
            standard_compaction_provider(responses),
            llm_profile_spec("plugin-message-id-model", None, 40_000),
        )
        .plugin(Arc::new(
            lash_plugin_standard_compaction::StandardCompactionPluginFactory::default(),
        ))
        .plugin(Arc::new(injection_plugin))
        .build(crate::testing::runtime_lease_owner())?;
    let session = core
        .session(session_id)
        .created_with(session_spec_for(&llm_profile_spec(
            "plugin-message-id-model",
            None,
            40_000,
        )))
        .await
        .open()
        .await?;

    for ordinal in 0..=THRESHOLD_TURNS {
        session
            .send(TurnInput::text(format!("request {ordinal}")))
            .id(lash_core::TurnId::fixture(format!(
                "plugin-injection-{ordinal}"
            )))
            .output()
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
async fn threshold_continue_as_extends_the_pre_switch_durable_leaf() -> Result<()> {
    let session_id = "standard-compaction-continue-as-parent";
    let backend = double_backend().await;
    let store_factory = Arc::clone(
        latest_double()
            .expect("the backend runs on its held double")
            .stores(),
    );
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
        .session(session_id)
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
        .id("standard-compaction-rlm-first")
        .output()
        .await?;
    assert_eq!(primed.final_value(), Some(&serde_json::json!("primed")));
    let (durable_leaf_before_switch, max_generation_before_switch) =
        sqlite_head_and_max_generation(store_factory.as_ref(), &SessionId::from(session_id));

    let continued = session
        .send(TurnInput::text("cross the threshold and continue"))
        .id("standard-compaction-rlm-threshold")
        .output()
        .await?;
    assert_eq!(
        continued.final_value(),
        Some(&serde_json::json!("continued"))
    );

    let conn = rusqlite::Connection::open(
        store_factory.database_uri(lash_sqlite_store::SqliteDatabase::DurableCore),
    )
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
    stores: &lash_sqlite_store::SqliteStoreSet,
    session_id: &SessionId,
) -> Vec<(String, Option<String>, i64)> {
    let conn = rusqlite::Connection::open(
        stores.database_uri(lash_sqlite_store::SqliteDatabase::DurableCore),
    )
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
    let session_id = "after-turn-enqueue-resident";
    let backend = double_backend().await;
    let store_factory = Arc::clone(
        latest_double()
            .expect("the backend runs on its held double")
            .stores(),
    );
    let plugin = crate::plugins::StaticPluginFactory::new(
        lash_core::plugin::PluginDeclaration::initial("after-turn-injection"),
        lash_core::facade_support::PluginSpec::new().with_after_turn(
            crate::hook_key!("after-turn-2"),
            Arc::new(|_| {
                Box::pin(async {
                    Ok(lash_core::plugin::AfterTurnContributions {
                        messages: vec![lash_core::PluginMessage::text(
                            lash_core::MessageRole::User,
                            "enqueued after turn",
                        )],
                        ..Default::default()
                    })
                })
            }),
        ),
    );
    let core = explicit_ephemeral_facets(LashCore::standard_builder(backend.clone()))
        .serve_test_llm_profile(
            standard_compaction_provider(vec![
                response_with_usage("first response", 1),
                response_with_usage("second response", 1),
            ]),
            llm_profile_spec("after-turn-model", None, 40_000),
        )
        .plugin(Arc::new(plugin))
        .build(crate::testing::runtime_lease_owner())?;
    let session = core
        .session(session_id)
        .created_with(session_spec_for(&llm_profile_spec(
            "after-turn-model",
            None,
            40_000,
        )))
        .await
        .open()
        .await?;
    session
        .send(TurnInput::text("first request"))
        .id("enqueue-first")
        .output()
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
        .send(TurnInput::text("second request"))
        .id("enqueue-second")
        .output()
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
async fn after_turn_enqueue_persists_the_reply_exactly_once() -> Result<()> {
    let session_id = "after-turn-enqueue-single-reply";
    let backend = double_backend().await;
    let store_factory = Arc::clone(
        latest_double()
            .expect("the backend runs on its held double")
            .stores(),
    );
    let plugin = crate::plugins::StaticPluginFactory::new(
        lash_core::plugin::PluginDeclaration::initial("after-turn-injection"),
        lash_core::facade_support::PluginSpec::new().with_after_turn(
            crate::hook_key!("after-turn-5"),
            Arc::new(|_| {
                Box::pin(async {
                    Ok(lash_core::plugin::AfterTurnContributions {
                        messages: vec![lash_core::PluginMessage::text(
                            lash_core::MessageRole::User,
                            "enqueued after turn",
                        )],
                        ..Default::default()
                    })
                })
            }),
        ),
    );
    let core = explicit_ephemeral_facets(LashCore::standard_builder(backend.clone()))
        .serve_test_llm_profile(
            standard_compaction_provider(vec![response_with_usage("first response", 1)]),
            llm_profile_spec("after-turn-model", None, 40_000),
        )
        .plugin(Arc::new(plugin))
        .build(crate::testing::runtime_lease_owner())?;
    let session = core
        .session(session_id)
        .created_with(session_spec_for(&llm_profile_spec(
            "after-turn-model",
            None,
            40_000,
        )))
        .await
        .open()
        .await?;
    session
        .send(TurnInput::text("first request"))
        .id("enqueue-once")
        .output()
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

/// An administrative compaction whose commit fails once is applied on the
/// engine's retry of its command shift: the command stays open, its
/// journaled summary is read back rather than requested again, the summary
/// lands once, and the summarizer's billed usage settles exactly once
/// (FIG-4201).
#[tokio::test]
async fn admin_compaction_commit_failure_applies_once_on_the_engines_retry() -> Result<()> {
    let session_id = "standard-compaction-commit-failure";
    let sqlite = double_backend().await;
    let script = Arc::new(Script::new());
    let commits = Arc::clone(&script);
    let backend = DecoratedBackend::over(sqlite)
        .session_store_factory(move |inner| commits.wrap("compaction", inner));
    let (provider, provider_calls) = standard_compaction_provider_counted(vec![
        response_with_usage("first response", 1),
        response_with_usage("second response", 1),
        response_with_usage("failed-then-summarized", 1),
        // A spare the retry must not ask for: its summary is journaled.
        response_with_usage("failed-then-summarized", 1),
    ]);
    let core = explicit_ephemeral_facets(LashCore::standard_builder(backend.into()))
        .serve_test_llm_profile(
            provider,
            llm_profile_spec("standard-compaction-model", None, 40_000),
        )
        .plugin(Arc::new(
            lash_plugin_standard_compaction::StandardCompactionPluginFactory::default(),
        ))
        .build(crate::testing::runtime_lease_owner())?;
    let session = core
        .session(session_id)
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
        .id("standard-compaction-commit-failure-one")
        .output()
        .await?;
    session
        .send(TurnInput::text("second request"))
        .id("standard-compaction-commit-failure-two")
        .output()
        .await?;
    let calls_before = provider_calls.load(Ordering::SeqCst);

    script
        .on(StoreOp::commit_runtime_state)
        .nth(script.calls(StoreOp::commit_runtime_state) + 1)
        .before()
        .fail(|| StoreError::Backend("injected compaction settlement commit failure".to_string()));
    assert!(
        Box::pin(
            session
                .admin()
                .state()
                .compact_context(Some("summarize".to_string()))
        )
        .await?,
        "the engine's retry applies the compaction once the injected failure clears"
    );
    assert!(
        script.trace().iter().any(|call| {
            call.op == StoreOp::commit_runtime_state.into()
                && matches!(call.outcome, lash_core::testing::Outcome::Failed(_))
        }),
        "the compaction's commit met the injected failure"
    );
    let view = session.read_view();
    assert_eq!(
        view.messages()
            .iter()
            .filter(|message| message.parts[0].content().contains("summarized"))
            .count(),
        1,
        "the summary lands once: {:?}",
        view.messages()
            .iter()
            .map(|message| message.parts[0].content().to_string())
            .collect::<Vec<_>>()
    );
    assert_eq!(
        provider_calls.load(Ordering::SeqCst),
        calls_before + 1,
        "the retry reads the journaled summary back"
    );

    Ok(())
}

// ADR 0001 / FIG-4972: Prompt Views are never session content.
struct PromptViewProbe {
    remove: bool,
}

#[async_trait]
impl lash_core::facade_support::TurnContextTransform for PromptViewProbe {
    fn id(&self) -> &'static str {
        "prompt-view-probe"
    }

    async fn transform(
        &self,
        _: &lash_core::facade_support::TurnTransformContext<'_>,
        mut input: lash_core::facade_support::PreparedContext,
    ) -> std::result::Result<
        lash_core::facade_support::PreparedContext,
        lash_core::facade_support::ContextError,
    > {
        if self.remove {
            input
                .messages
                .make_mut()
                .retain(|message| !message_text(message).contains("real input"));
        } else {
            input.messages.make_mut().push(lash_core::Message {
                id: "prompt-only".to_owned(),
                role: lash_core::MessageRole::User,
                parts: vec![lash_core::Part::text(
                    "prompt-only.p0".to_owned(),
                    "ephemeral note".to_owned(),
                    None,
                )]
                .into(),
                origin: None,
                reply_marker: None,
            });
        }
        Ok(input)
    }
}

async fn prompt_view_history_law(remove: bool) -> Result<()> {
    let mut histories = Vec::new();
    for crash in [false, true] {
        let backend = double_backend().await;
        let double = latest_double().expect("held double");
        let stores = Arc::clone(double.stores());
        let (provider, requests) =
            standard_compaction_provider_recorded(vec![response_with_usage("answer", 1)]);
        let factory = crate::plugins::StaticPluginFactory::new(
            lash_core::plugin::PluginDeclaration::initial("prompt-view-law"),
            lash_core::facade_support::PluginSpec::new()
                .with_turn_context_transform(0, Arc::new(PromptViewProbe { remove })),
        );
        let profile = llm_profile_spec("prompt-view-model", None, 100_000);
        let core = explicit_ephemeral_facets(LashCore::standard_builder(backend))
            .serve_test_llm_profile(provider, profile.clone())
            .plugin(Arc::new(factory))
            .build(crate::testing::runtime_lease_owner())?;
        let session = core
            .session("prompt-view-law")
            .created_with(session_spec_for(&profile))
            .await
            .open()
            .await?;
        if crash {
            double.server().crash_on(
                lash_restate_test::CrashRule::new(
                    lash_restate_test::CrashPoint::BeforeStateWrite {
                        key: "outcome".to_owned(),
                        value_contains: Some("\"run\":\"prompt-view-turn\"".to_owned()),
                    },
                )
                .service(lash_restate_test::TURN_DRIVER_SERVICE)
                .within_attempts(1),
            );
        }
        session
            .send(TurnInput::text("real input"))
            .id("prompt-view-turn")
            .output()
            .await?;
        double.server().settle().await;
        if crash {
            assert!(
                double
                    .server()
                    .invocations()
                    .iter()
                    .any(|run| run.attempts == 2 && run.status == "completed"),
                "the cold replay must execute"
            );
        }
        let requests = requests.lock_recover().clone();
        assert_eq!(requests.len(), 1, "replay uses the recorded model result");
        assert_eq!(requests[0].contains("real input"), !remove);
        assert_eq!(requests[0].contains("ephemeral note"), !remove);
        let stored = sqlite_messages(stores.as_ref(), &SessionId::from("prompt-view-law"));
        let transcript = session
            .read_view()
            .messages()
            .iter()
            .map(message_text)
            .collect::<Vec<_>>();
        let parked = Box::pin(session.park()).await?;
        let reopened = Box::pin(core.resume(parked)).await?;
        let reopened = reopened
            .read_view()
            .messages()
            .iter()
            .map(message_text)
            .collect::<Vec<_>>();
        assert_eq!(
            reopened, transcript,
            "cold session reload retains the same history"
        );
        histories.push((
            stored.iter().map(message_text).collect::<Vec<_>>(),
            transcript,
        ));
    }
    assert_eq!(
        histories[0], histories[1],
        "live and replay commit the same history"
    );
    for (stored, transcript) in histories {
        assert!(
            stored.iter().any(|text| text == "real input"),
            "the store keeps real input removed from the Prompt View"
        );
        assert!(
            !stored.iter().any(|text| text == "ephemeral note"),
            "the store never commits Prompt View additions"
        );
        assert!(
            transcript.iter().any(|text| text == "real input"),
            "the API transcript keeps real input"
        );
        assert!(
            !transcript.iter().any(|text| text == "ephemeral note"),
            "the API transcript contains no Prompt View additions"
        );
        assert!(transcript.iter().any(|text| text == "answer"));
    }
    Ok(())
}

#[tokio::test]
async fn prompt_view_additions_reach_only_the_model_across_cold_replay() -> Result<()> {
    prompt_view_history_law(false).await
}

#[tokio::test]
async fn prompt_view_removals_preserve_history_across_cold_replay() -> Result<()> {
    prompt_view_history_law(true).await
}
