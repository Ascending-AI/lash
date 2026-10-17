//! ADR 0137: a host inbox survives lost admission acknowledgements and
//! reconciles through keyed sends, once, on SQLite.

use std::sync::Arc;
use std::time::Duration;

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
async fn a_lost_admission_ack_retries_one_committed_occurrence_as_one_turn() {
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

    let world = tempfile::tempdir().unwrap();
    let path = world.path().join("host.sqlite");
    let mut host = HostStore::open(&path).unwrap();
    let subscription = Subscription {
        id: "watch-tickets".to_owned(),
        generation: 1,
        server: SERVER.to_owned(),
        event: TICKET_CREATED.to_owned(),
        session: session.clone(),
        arguments: Map::new(),
    };
    host.register(&subscription).unwrap();
    let occurrence = McpEventOccurrence {
        event_id: "t-1".to_owned(),
        name: TICKET_CREATED.to_owned(),
        timestamp: "2026-10-17T00:00:00Z".to_owned(),
        data: json!({"title":"printer on fire"}),
        cursor: None,
    };
    let batch = McpEventPoll {
        events: vec![occurrence.clone()],
        cursor: Some("after-1".to_owned()),
        truncated: false,
        has_more: false,
        next_poll_ms: None,
    };
    host.accept(&subscription.id, 1, &batch).unwrap();
    let delivery = host.pending(10).unwrap().pop().unwrap();
    admit(&core, &delivery).await.unwrap();
    // Admission committed, but its acknowledgement never reaches the host
    // inbox. Reopening recovers both the occurrence and its stable key.
    drop(host);
    let mut host = HostStore::open(&path).unwrap();
    assert_eq!(
        host.read(&subscription.id, 1).unwrap().1.as_deref(),
        Some("after-1")
    );
    assert_eq!(host.pending(10).unwrap()[0].turn, delivery.turn);
    host.dispatch_pending(&core, 10).await.unwrap();
    // Equal payloads are two occurrences when their source ids differ.
    let mut second = occurrence;
    second.event_id = "t-2".to_owned();
    let mut second_batch = batch.clone();
    second_batch.events = vec![second];
    second_batch.cursor = Some("after-2".to_owned());
    host.accept(&subscription.id, 1, &second_batch).unwrap();
    let second_delivery = host.pending(10).unwrap().pop().unwrap();
    assert_ne!(second_delivery.turn, delivery.turn);
    admit(&core, &second_delivery).await.unwrap();
    drop(host); // Lose the second acknowledgement as well.
    let mut host = HostStore::open(&path).unwrap();
    assert_eq!(host.pending(10).unwrap()[0].turn, second_delivery.turn);
    host.dispatch_pending(&core, 10).await.unwrap();
    // Cursor changes are delivery progress, not new occurrence identities.
    let mut redelivery = second_batch;
    redelivery.events[0].cursor = Some("source-redelivery".to_owned());
    redelivery.cursor = Some("after-redelivery".to_owned());
    host.accept(&subscription.id, 1, &redelivery).unwrap();
    assert_eq!(
        host.read(&subscription.id, 1).unwrap().1.as_deref(),
        Some("after-redelivery")
    );
    host.accept(&subscription.id, 1, &batch).unwrap(); // source redelivery
    assert!(host.pending(10).unwrap().is_empty());
    let durable = core.session(session).durable().await.unwrap();
    tokio::time::timeout(Duration::from_secs(120), async {
        loop {
            if let Some(view) = durable.read().await.unwrap() {
                let transcript = view
                    .messages()
                    .iter()
                    .flat_map(|message| message.parts.iter())
                    .map(|part| part.content().into_owned())
                    .collect::<Vec<_>>()
                    .join("\n");
                let occurrences = transcript.matches("printer on fire").count();
                assert!(occurrences <= 2, "a retry admitted a second turn");
                if occurrences == 2 && transcript.matches("triaged").count() == 2 {
                    break;
                }
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .expect("the admitted turn finishes");
    let turns = durable
        .committed_turns(None, std::num::NonZeroU32::new(10).unwrap())
        .await
        .unwrap();
    assert_eq!(turns.turns.len(), 2, "one turn per occurrence was recorded");
    let mut ids = turns
        .turns
        .iter()
        .map(|turn| turn.turn_id.as_str())
        .collect::<Vec<_>>();
    ids.sort_unstable();
    let mut expected = [delivery.turn.as_str(), second_delivery.turn.as_str()];
    expected.sort_unstable();
    assert_eq!(ids, expected);
    core.shutdown().await.unwrap();
}
