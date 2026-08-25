use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use lash::persistence::SessionStoreFactory as _;
use lash::provider::LlmResponse;
use lash::{CancellationToken, TurnActivity, TurnActivitySink, TurnEvent, TurnOutcome, TurnStop};

const SESSION: &str = "docs-selected-drain-session";

#[derive(Default)]
struct AppEvents(Mutex<Vec<TurnActivity>>);

#[async_trait]
impl TurnActivitySink for AppEvents {
    async fn emit(&self, activity: TurnActivity) {
        self.0.lock().expect("event sink mutex").push(activity);
    }
}

impl AppEvents {
    fn snapshot(&self) -> Vec<TurnActivity> {
        self.0.lock().expect("event sink mutex").clone()
    }
}

fn response(text: &str) -> LlmResponse {
    LlmResponse {
        full_text: text.to_string(),
        parts: vec![lash::direct::LlmOutputPart::Text {
            text: text.to_string(),
            response_meta: None,
        }],
        response_metadata: Default::default(),
        ..LlmResponse::default()
    }
}

fn core(
    store_factory: Arc<lash::persistence::InMemorySessionStoreFactory>,
) -> lash::Result<lash::LashCore> {
    let provider = lash::testing::TestProvider::builder()
        .kind("docs-selected-drain")
        .complete(|_| async { Ok(response("selected drain completed")) })
        .build()
        .into_handle();
    lash::LashCore::standard_builder(lash::TurnBudget::Unbounded)
        .provider(provider)
        .model(
            lash::ModelSpec::builder("docs-selected-drain-model")
                .context_window_tokens(4_096)
                .build()
                .expect("valid selected-drain model"),
        )
        .effect_host(Arc::new(lash::durability::InlineEffectHost::default()))
        .attachment_store(Arc::new(lash::persistence::InMemoryAttachmentStore::new()))
        .process_env_store(Arc::new(
            lash::persistence::InMemoryProcessExecutionEnvStore::new(),
        ))
        .store_factory(store_factory)
        .commit_budget(lash::CommitBudget::bounded(1024 * 1024, 512))
        .queued_work_batching(lash::QueuedWorkBatchingConfig::new(1024))
        .disable_queued_work_driver()
        .build(lash::persistence::LeaseOwnerIdentity::opaque(
            "docs-selected-drain-worker",
            "docs-selected-drain-boot",
        ))
}

async fn enqueue(
    store_factory: &lash::persistence::InMemorySessionStoreFactory,
    session: &lash::LashSession,
    source_key: &str,
) -> anyhow::Result<String> {
    let store = store_factory
        .create_store(&lash::persistence::SessionStoreCreateRequest {
            session_id: session.session_id().to_string(),
            relation: lash::persistence::SessionRelation::Root,
            policy: session.policy_snapshot(),
        })
        .await?;
    let batch = store
        .enqueue_queued_work(
            lash::persistence::QueuedWorkBatchDraft::new(
                session.session_id(),
                lash::persistence::DeliveryPolicy::EarliestSafeBoundary,
                vec![lash::persistence::QueuedWorkPayload::agent_frame_task(
                    "docs-selected-drain-frame",
                    source_key,
                    None,
                )],
            )
            .with_source_key(source_key),
        )
        .await?;
    Ok(batch.batch_id)
}

#[tokio::test]
async fn selected_drain_streams_activity_and_reports_idempotent_satisfaction() -> anyhow::Result<()>
{
    let store_factory = Arc::new(lash::persistence::InMemorySessionStoreFactory::new());
    let core = core(Arc::clone(&store_factory))?;
    let session = core.session(SESSION).open().await?;
    let batch_id = enqueue(store_factory.as_ref(), &session, "docs-selected-stream").await?;
    let events = AppEvents::default();

    let claimed = session
        .queued_turn()
        .batch_ids([batch_id.clone()])
        .stream_to(&events)
        .await?;
    let claimed_exact = vec![lash::SelectedQueuedWorkBatchSatisfaction::ClaimedNow {
        batch_id: batch_id.clone(),
    }];
    assert_eq!(claimed.satisfied, claimed_exact);
    assert!(matches!(
        claimed.turn.expect("selected row executes").outcome,
        TurnOutcome::Finished(_)
    ));
    assert!(
        events
            .snapshot()
            .iter()
            .any(|activity| matches!(activity.event, TurnEvent::AssistantProseDelta { .. }))
    );

    let replay = session
        .queued_turn()
        .batch_ids([batch_id.clone()])
        .stream_to(&events)
        .await?;
    assert!(replay.turn.is_none());
    assert_eq!(
        replay.satisfied,
        vec![lash::SelectedQueuedWorkBatchSatisfaction::AlreadySatisfied { batch_id }]
    );
    Ok(())
}

#[tokio::test]
async fn advanced_selected_drain_observes_cooperative_cancellation() -> anyhow::Result<()> {
    let store_factory = Arc::new(lash::persistence::InMemorySessionStoreFactory::new());
    let core = core(Arc::clone(&store_factory))?;
    let session = core.session(format!("{SESSION}-advanced")).open().await?;
    let batch_id = enqueue(store_factory.as_ref(), &session, "docs-selected-advanced").await?;
    let cancel = CancellationToken::new();
    cancel.cancel();
    let controller = lash::runtime::InlineRuntimeEffectController::default();
    let scoped = lash::runtime::ScopedEffectController::borrowed(
        &controller,
        lash::runtime::ExecutionScope::queue_drain(session.session_id(), "docs-advanced-drain"),
    )?;

    let cancelled = session
        .queued_turn()
        .batch_ids([batch_id.clone()])
        .cancel(cancel)
        .advanced()
        .stream_to_with_scope(&AppEvents::default(), scoped)
        .await?;
    let claimed_exact = vec![lash::SelectedQueuedWorkBatchSatisfaction::ClaimedNow { batch_id }];
    assert_eq!(cancelled.satisfied, claimed_exact);
    let cancelled = cancelled.turn.expect("selected row starts a turn");
    let TurnOutcome::Stopped(TurnStop::Cancelled { evidence }) = &cancelled.outcome else {
        panic!("selected drain under a cancelled token commits a cancelled turn");
    };
    assert_eq!(evidence.origin, None);
    Ok(())
}

#[tokio::test]
async fn selected_drain_preserves_host_cancellation_origin() -> anyhow::Result<()> {
    let store_factory = Arc::new(lash::persistence::InMemorySessionStoreFactory::new());
    let core = core(Arc::clone(&store_factory))?;
    let session = core.session(format!("{SESSION}-origin")).open().await?;
    let batch_id = enqueue(store_factory.as_ref(), &session, "docs-selected-origin").await?;
    let cancel = CancellationToken::new();
    cancel.cancel();

    let cancelled = session
        .queued_turn()
        .batch_ids([batch_id.clone()])
        .cancel_with_origin(cancel, Some("host-shutdown".to_string()))
        .stream_to(&AppEvents::default())
        .await?;
    assert_eq!(
        cancelled.satisfied,
        vec![lash::SelectedQueuedWorkBatchSatisfaction::ClaimedNow { batch_id }]
    );
    let turn = cancelled.turn.expect("selected row starts a turn");
    let evidence = turn.cancellation().expect("host cancellation evidence");
    assert_eq!(evidence.origin.as_deref(), Some("host-shutdown"));
    Ok(())
}
