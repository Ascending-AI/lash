//! The example end to end: a SQLite-backed core with the MCP plugin over
//! the plugin's SEP-3415 fixture server; each published ticket becomes one
//! turn of the watching session, and the report shows both.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use lash::mcp::events::testing::McpEventsFixture;
use serde_json::json;

use super::*;

const SERVER: &str = "tickets";
const MODEL: &str = "triage-model";

/// The triage model: it acknowledges every ticket.
fn model() -> lash::provider::ProviderHandle {
    lash::testing::TestProvider::builder()
        .kind("mcp-events-example")
        .complete(|_request: lash::provider::LlmRequest| async move {
            Ok(lash::provider::LlmResponse {
                parts: vec![lash::direct::LlmOutputPart::Text {
                    text: "triaged".to_owned(),
                    response_meta: None,
                }],
                ..lash::provider::LlmResponse::default()
            })
        })
        .build()
        .into_handle()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn each_published_ticket_becomes_one_turn() {
    let world = tempfile::tempdir().expect("the fixture's world");
    let fixture = McpEventsFixture::new(world.path());
    fixture.control(json!({ "heartbeat_ms": 100 }));
    let plugin = Arc::new(
        lash::mcp::McpPluginFactory::new(BTreeMap::from([(
            SERVER.to_owned(),
            fixture.server(ticket_events()),
        )]))
        .await
        .expect("the MCP plugin builds"),
    );
    let stores = lash::sqlite::SqliteStoreSet::memory()
        .await
        .expect("the store set opens");
    let backend = lash::durable::DurableBackendBuilder::new(Arc::new(stores))
        .build()
        .expect("the backend builds");
    let metadata = lash::LlmProfileMetadata::builder(MODEL)
        .cache_retention(lash::provider::CacheRetention::Short)
        .context_window_tokens(200_000)
        .build()
        .expect("the model's metadata");
    let core = lash::LashCore::standard_builder(backend)
        .plugin(Arc::clone(&plugin) as Arc<dyn lash::plugins::PluginFactory>)
        .commit_budget(lash::CommitBudget::bounded(16 * 1024 * 1024, 4096))
        .data_retention(lash::DataRetention::standard())
        .queued_work_batching(lash::QueuedWorkBatchingConfig::new(1))
        .execution_budgets(lash::ExecutionBudgets::recommended())
        .delta_coalescing(lash::DeltaCoalescing::recommended())
        .serve_test_llm_profile(model(), metadata)
        .build(lash::persistence::LeaseOwnerIdentity::opaque(
            lash::persistence::LeaseOwnerId::new("mcp-events-example"),
            lash::persistence::LeaseIncarnationId::new("boot"),
        ))
        .expect("the core builds");
    core.bind_mcp_events(&plugin.events());

    let session = SessionId::try_from("triage".to_owned()).expect("a session id");
    core.session(session.clone())
        .create(lash::SessionCreation::root(
            lash::SessionSpec::new(
                MODEL,
                lash::TurnBudget::bounded(4),
                lash::MaxToolCalls::new(8),
            )
            .no_progress_budget(lash::NoProgressBudget::bounded(12)),
        ))
        .await
        .expect("the session is created");
    let subscription = watch_tickets(&plugin.events(), SERVER, &session)
        .await
        .expect("the subscription starts");
    assert_eq!(
        watch_tickets(&plugin.events(), SERVER, &session).await.ok(),
        Some(subscription.clone()),
        "asking again answers the same subscription"
    );
    // Push starts from now: the stream is open before the tickets arrive.
    while fixture.requests_of("events/stream").is_empty() {
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    fixture.emit("t-1", json!({ "title": "printer on fire" }));
    fixture.emit("t-2", json!({ "title": "coffee machine empty" }));

    let durable = core
        .session(session.clone())
        .durable()
        .await
        .expect("the durable session");
    tokio::time::timeout(Duration::from_secs(120), async {
        loop {
            let transcript = match durable.read().await.expect("the committed read") {
                Some(view) => view
                    .messages()
                    .iter()
                    .flat_map(|message| message.parts.iter())
                    .map(|part| part.content().into_owned())
                    .collect::<Vec<_>>()
                    .join("\n"),
                None => String::new(),
            };
            let turns = ["t-1", "t-2"].map(|id| {
                transcript
                    .matches(&format!("\"eventId\": \"{id}\""))
                    .count()
            });
            if turns == [1, 1] {
                break;
            }
            assert!(
                turns.iter().all(|count| *count <= 1),
                "a ticket reacted twice"
            );
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .expect("both tickets become turns");

    let lines = report(&plugin.events(), &session)
        .await
        .expect("the report");
    // A push window may hand an event over twice; its key absorbs the
    // second, which the status counts as delivered.
    assert_eq!(lines.len(), 1, "{lines:?}");
    assert!(lines[0].contains("by Push"), "{lines:?}");
    assert!(!lines[0].contains("degraded"), "{lines:?}");
    plugin
        .events()
        .unsubscribe(&subscription)
        .await
        .expect("the unsubscribe");
    core.shutdown().await.expect("the core shuts down");
}
