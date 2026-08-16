//! Host-owned queued-drain selection (FIG-1313).
//!
//! Lash decides which queued rows *may* share a turn; the host decides how many
//! of them actually drain on one wake. The shipped default is one row per
//! drain, a second mode drains everything pending, and a host wanting anything
//! else implements the trait.

use std::sync::Arc;

use lash::persistence::SessionStoreFactory as _;
use lash::provider::LlmResponse;
use lash::{DrainMode, QueuedDrainPolicy, QueuedDrainRequest, QueuedDrainSelection, TurnEvent};

const SESSION: &str = "docs-drain-policy-session";

fn response(text: &str) -> LlmResponse {
    LlmResponse {
        full_text: text.to_string(),
        parts: vec![lash_core::LlmOutputPart::Text {
            text: text.to_string(),
            response_meta: None,
        }],
        response_metadata: Default::default(),
        ..LlmResponse::default()
    }
}

fn core(
    store_factory: Arc<lash::persistence::InMemorySessionStoreFactory>,
    batching: lash::QueuedWorkBatchingConfig,
    context_window_tokens: usize,
) -> lash::Result<lash::LashCore> {
    let provider = lash::testing::TestProvider::builder()
        .kind("docs-drain-policy")
        .complete(|_| async { Ok(response("drained")) })
        .build()
        .into_handle();
    lash::LashCore::standard_builder(lash::TurnBudget::Unbounded)
        .provider(provider)
        .model(
            lash::ModelSpec::builder("docs-drain-policy-model")
                .context_window_tokens(context_window_tokens)
                .build()
                .expect("valid drain-policy model"),
        )
        .effect_host(Arc::new(lash::durability::InlineEffectHost::default()))
        .attachment_store(Arc::new(lash::persistence::InMemoryAttachmentStore::new()))
        .process_env_store(Arc::new(
            lash::persistence::InMemoryProcessExecutionEnvStore::new(),
        ))
        .store_factory(store_factory)
        .commit_budget(lash::CommitBudget::bounded(1024 * 1024, 512))
        .queued_work_batching(batching)
        .disable_queued_work_driver()
        .build(lash::persistence::LeaseOwnerIdentity::opaque(
            "docs-drain-policy-worker",
            "docs-drain-policy-boot",
        ))
}

async fn enqueue(
    store_factory: &lash::persistence::InMemorySessionStoreFactory,
    session: &lash::LashSession,
    source_key: &str,
    task: &str,
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
                    "docs-drain-policy-frame",
                    task,
                    None,
                )],
            )
            .with_source_key(source_key)
            // A shared merge key is the producer's statement that these rows
            // *may* travel together; the drain policy decides whether they do.
            .with_merge_key("docs-drain-policy"),
        )
        .await?;
    Ok(batch.batch_id)
}

/// Drains once and reports the rows the *initial* claim took.
///
/// A running turn may still absorb newly ready rows at its own checkpoints;
/// this reads the first claim, which is the one the drain policy chose.
async fn drained_batch_ids(session: &lash::LashSession) -> anyhow::Result<Vec<String>> {
    let turn = session
        .queued_turn()
        .run()
        .await?
        .expect("a pending row drains");
    Ok(turn
        .activities
        .iter()
        .find_map(|activity| match &activity.event {
            TurnEvent::QueuedWorkStarted { batch_ids, .. } => Some(batch_ids.clone()),
            _ => None,
        })
        .expect("a drain reports the rows it claimed"))
}

#[tokio::test]
async fn the_default_drain_mode_takes_one_queued_row_per_wake() -> anyhow::Result<()> {
    let store_factory = Arc::new(lash::persistence::InMemorySessionStoreFactory::new());
    // No drain mode named: Lash's documented default is one row at a time.
    let core = core(
        Arc::clone(&store_factory),
        lash::QueuedWorkBatchingConfig::new(1024),
        32_768,
    )?;
    let session = core.session(SESSION).open().await?;
    let first = enqueue(store_factory.as_ref(), &session, "one-a", "first").await?;
    assert_eq!(drained_batch_ids(&session).await?, vec![first]);

    let second = enqueue(store_factory.as_ref(), &session, "one-b", "second").await?;
    assert_eq!(drained_batch_ids(&session).await?, vec![second]);
    Ok(())
}

#[tokio::test]
async fn drain_mode_all_coalesces_every_compatible_pending_row() -> anyhow::Result<()> {
    let store_factory = Arc::new(lash::persistence::InMemorySessionStoreFactory::new());
    let core = core(
        Arc::clone(&store_factory),
        lash::QueuedWorkBatchingConfig::new(1024).with_drain_mode(DrainMode::All),
        32_768,
    )?;
    let session = core.session(format!("{SESSION}-all")).open().await?;
    let first = enqueue(store_factory.as_ref(), &session, "all-a", "first").await?;
    let second = enqueue(store_factory.as_ref(), &session, "all-b", "second").await?;

    assert_eq!(drained_batch_ids(&session).await?, vec![first, second]);
    Ok(())
}

/// A host policy that pairs rows up — the escape hatch for selection Lash does
/// not ship.
#[derive(Debug)]
struct PairwiseDrainPolicy;

impl QueuedDrainPolicy for PairwiseDrainPolicy {
    fn name(&self) -> &str {
        "docs_pairwise"
    }

    fn select_drain(&self, request: &QueuedDrainRequest<'_>) -> QueuedDrainSelection {
        // Every offered row carries its own conservative projection, and the
        // request carries the window budget, so a custom policy can weigh both.
        assert!(request.available_tokens() <= request.max_context_tokens());
        QueuedDrainSelection::leading(2.min(request.candidates().len()))
    }
}

#[tokio::test]
async fn a_custom_drain_policy_owns_the_selection() -> anyhow::Result<()> {
    let store_factory = Arc::new(lash::persistence::InMemorySessionStoreFactory::new());
    let core = core(
        Arc::clone(&store_factory),
        lash::QueuedWorkBatchingConfig::new(1024).with_drain_policy(Arc::new(PairwiseDrainPolicy)),
        32_768,
    )?;
    let session = core.session(format!("{SESSION}-custom")).open().await?;
    let first = enqueue(store_factory.as_ref(), &session, "pair-a", "first").await?;
    let second = enqueue(store_factory.as_ref(), &session, "pair-b", "second").await?;
    let _third = enqueue(store_factory.as_ref(), &session, "pair-c", "third").await?;

    assert_eq!(drained_batch_ids(&session).await?, vec![first, second]);
    Ok(())
}

#[tokio::test]
async fn a_row_larger_than_the_window_is_refused_by_name() -> anyhow::Result<()> {
    let store_factory = Arc::new(lash::persistence::InMemorySessionStoreFactory::new());
    let core = core(
        Arc::clone(&store_factory),
        lash::QueuedWorkBatchingConfig::new(64),
        1_024,
    )?;
    let session = core.session(format!("{SESSION}-oversized")).open().await?;
    let batch_id = enqueue(
        store_factory.as_ref(),
        &session,
        "oversized",
        &"w".repeat(4_096),
    )
    .await?;

    let error = session
        .queued_turn()
        .batch_ids([batch_id.clone()])
        .run()
        .await
        .expect_err("a row larger than the window cannot drain");
    let lash::EmbedError::SelectedQueuedWorkDrainRefused {
        cause:
            lash::SelectedQueuedWorkDrainRefusalCause::QueuedItemExceedsContextWindow {
                batch_id: refused,
                required_context_tokens,
                max_context_tokens,
                ..
            },
    } = error
    else {
        panic!("expected the typed oversized-row refusal, got {error:?}");
    };
    assert_eq!(refused, batch_id);
    assert_eq!(max_context_tokens, 1_024);
    assert!(required_context_tokens > max_context_tokens);
    // The row stays queued for host recovery: compact it, split it, or move the
    // session to a larger-window model.
    assert_eq!(
        session
            .queued_work()
            .await?
            .into_iter()
            .map(|batch| batch.batch_id)
            .collect::<Vec<_>>(),
        vec![batch_id]
    );
    Ok(())
}
