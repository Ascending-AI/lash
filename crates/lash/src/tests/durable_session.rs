//! Durable Session: acquisition, refusals, observation, and coexistence with a
//! live writer (FIG-3366, ADR 0097).

use super::*;
use lash_sansio::SessionId;

const SEED: u64 = 0xd0a4_b1e5;

/// Catalog wrapper that counts which seam a caller reached for.
///
/// A Durable Session must resolve through the non-creating by-id seam exactly
/// once per handle and must never reach `create_store`; these counters are what
/// makes that a test rather than a claim.
struct CountingSessionStoreFactory {
    inner: Arc<dyn SessionStoreFactory>,
    creates: Arc<AtomicUsize>,
    by_id_opens: Arc<AtomicUsize>,
    /// Delay inside the by-id seam so concurrent callers overlap in it.
    open_delay_ms: u64,
}

impl CountingSessionStoreFactory {
    fn new(inner: Arc<dyn SessionStoreFactory>, open_delay_ms: u64) -> Self {
        Self {
            inner,
            creates: Arc::new(AtomicUsize::new(0)),
            by_id_opens: Arc::new(AtomicUsize::new(0)),
            open_delay_ms,
        }
    }
}

#[async_trait]
impl lash_core::AttachmentRootSet for CountingSessionStoreFactory {
    async fn live_attachment_refs(
        &self,
        intent_grace_cutoff_epoch_ms: u64,
    ) -> std::result::Result<
        std::collections::BTreeSet<lash_core::AttachmentId>,
        lash_core::StoreError,
    > {
        lash_core::AttachmentRootSet::live_attachment_refs(
            self.inner.as_ref(),
            intent_grace_cutoff_epoch_ms,
        )
        .await
    }

    async fn has_live_attachment_ref(
        &self,
        id: &lash_core::AttachmentId,
        intent_grace_cutoff_epoch_ms: u64,
    ) -> std::result::Result<bool, lash_core::StoreError> {
        lash_core::AttachmentRootSet::has_live_attachment_ref(
            self.inner.as_ref(),
            id,
            intent_grace_cutoff_epoch_ms,
        )
        .await
    }
}

#[async_trait]
impl SessionStoreFactory for CountingSessionStoreFactory {
    async fn create_store(
        &self,
        request: &lash_core::SessionStoreCreateRequest,
    ) -> std::result::Result<Arc<dyn lash_core::RuntimePersistence>, lash_core::StoreError> {
        self.creates.fetch_add(1, Ordering::SeqCst);
        self.inner.create_store(request).await
    }

    async fn open_existing_store(
        &self,
        request: &lash_core::SessionStoreCreateRequest,
    ) -> std::result::Result<Option<Arc<dyn lash_core::RuntimePersistence>>, String> {
        self.inner.open_existing_store(request).await
    }

    async fn open_existing_store_by_id(
        &self,
        session_id: &SessionId,
    ) -> std::result::Result<Option<Arc<dyn lash_core::RuntimePersistence>>, lash_core::StoreError>
    {
        self.by_id_opens.fetch_add(1, Ordering::SeqCst);
        if self.open_delay_ms > 0 {
            tokio::time::sleep(std::time::Duration::from_millis(self.open_delay_ms)).await;
        }
        self.inner.open_existing_store_by_id(session_id).await
    }

    async fn read_session(
        &self,
        session_id: &SessionId,
    ) -> std::result::Result<Option<lash_core::SessionReadView>, lash_core::StoreError> {
        self.inner.read_session(session_id).await
    }

    async fn session_was_deleted(
        &self,
        session_id: &SessionId,
    ) -> std::result::Result<bool, String> {
        lash_core::SessionStoreFactory::session_was_deleted(self.inner.as_ref(), session_id).await
    }

    async fn delete_session(
        &self,
        session_id: &SessionId,
    ) -> lash_core::MaintenanceResult<lash_core::SessionBlobReclaimReport> {
        self.inner.delete_session(session_id).await
    }

    // A decorator forwards the deployment turn count to the catalog it wraps.
    async fn count_unsettled_turns(
        &self,
    ) -> std::result::Result<lash_core::store::UnsettledTurnCounts, lash_core::StoreError> {
        self.inner.count_unsettled_turns().await
    }

    async fn list_turn_parks(
        &self,
        query: &lash_core::store::TurnParkQuery,
    ) -> std::result::Result<Vec<lash_core::store::TurnPark>, lash_core::StoreError> {
        self.inner.list_turn_parks(query).await
    }

    async fn turn_park_feed(
        &self,
        after: lash_core::store::ParkFeedCursor,
        limit: std::num::NonZeroUsize,
    ) -> std::result::Result<
        lash_core::store::ParkFeedPage<lash_core::store::TurnParkTarget>,
        lash_core::StoreError,
    > {
        self.inner.turn_park_feed(after, limit).await
    }

    async fn root_terminal(
        &self,
        session_id: &lash_core::SessionId,
        root: &lash_core::TurnId,
    ) -> std::result::Result<Option<lash_core::store::RootTerminal>, lash_core::StoreError> {
        self.inner.root_terminal(session_id, root).await
    }

    async fn compact_turn_park_feed(
        &self,
        through: lash_core::store::ParkFeedCursor,
    ) -> std::result::Result<(), lash_core::StoreError> {
        self.inner.compact_turn_park_feed(through).await
    }
}

#[async_trait::async_trait]
impl lash_core::store::ControlIntentStore for CountingSessionStoreFactory {
    async fn begin_session_close(
        &self,
        session_id: &SessionId,
        at_ms: u64,
    ) -> std::result::Result<Option<lash_core::store::ControlIntent>, lash_core::StoreError> {
        self.inner.begin_session_close(session_id, at_ms).await
    }

    async fn claim_intent_application(
        &self,
        id: lash_core::store::ControlIntentId,
        at_ms: u64,
    ) -> std::result::Result<lash_core::store::IntentApplication, lash_core::StoreError> {
        self.inner.claim_intent_application(id, at_ms).await
    }

    async fn acknowledge_intent(
        &self,
        id: lash_core::store::ControlIntentId,
        claim: &lash_core::store::ClaimToken,
        at_ms: u64,
    ) -> std::result::Result<lash_core::store::IntentSettle, lash_core::StoreError> {
        self.inner.acknowledge_intent(id, claim, at_ms).await
    }

    async fn record_intent_failure(
        &self,
        id: lash_core::store::ControlIntentId,
        claim: &lash_core::store::ClaimToken,
        error: &str,
        retryable: bool,
        at_ms: u64,
    ) -> std::result::Result<lash_core::store::IntentSettle, lash_core::StoreError> {
        self.inner
            .record_intent_failure(id, claim, error, retryable, at_ms)
            .await
    }

    async fn load_intent(
        &self,
        id: lash_core::store::ControlIntentId,
    ) -> std::result::Result<Option<lash_core::store::ControlIntent>, lash_core::StoreError> {
        self.inner.load_intent(id).await
    }
}

/// A counting catalog over `inner`'s own.
fn counting_factory(
    inner: &lash_core::Backend,
    open_delay_ms: u64,
) -> (DecoratedBackend, Arc<CountingSessionStoreFactory>) {
    let factory = Arc::new(CountingSessionStoreFactory::new(
        inner.session_store_factory(),
        open_delay_ms,
    ));
    let catalog = Arc::clone(&factory);
    let backend = DecoratedBackend::over(inner.clone()).session_store_factory(move |_| catalog);
    (backend, factory)
}

fn counting_core(backend: DecoratedBackend) -> Result<LashCore> {
    explicit_ephemeral_facets(LashCore::standard_builder(
        backend.into(),
        crate::TurnBudget::Unbounded,
    ))
    .provider(mock_provider())
    .model(mock_model_spec())
    .build(crate::testing::runtime_lease_owner())
}

#[tokio::test]
async fn durable_acquisition_is_non_creating_and_happens_once_per_handle() -> Result<()> {
    let double = restate_double(SEED).await;
    let (backend, factory) = counting_factory(&double.lash_backend(), 20);
    let creates = Arc::clone(&factory.creates);
    let by_id_opens = Arc::clone(&factory.by_id_opens);
    let core = counting_core(backend.clone())?;

    // One create: the open that brings the session into existence.
    drop(core.session("durable-acquisition").open().await?);
    let creates_after_open = creates.load(Ordering::SeqCst);
    assert_eq!(
        creates_after_open, 1,
        "open creates the session exactly once"
    );
    by_id_opens.store(0, Ordering::SeqCst);

    let durable = core.session("durable-acquisition").durable().await?;
    // Five concurrent operations on one handle (and its clones) must share one
    // acquisition.
    let handles = (0..5)
        .map(|index| {
            let durable = durable.clone();
            tokio::spawn(async move {
                if index % 2 == 0 {
                    durable.pending_turn_inputs().await.map(|_| ())
                } else {
                    durable.queued_work().await.map(|_| ())
                }
            })
        })
        .collect::<Vec<_>>();
    for handle in handles {
        handle.await.expect("durable operation task")?;
    }

    assert_eq!(
        by_id_opens.load(Ordering::SeqCst),
        1,
        "concurrent operations on one handle make exactly one acquisition"
    );
    assert_eq!(
        creates.load(Ordering::SeqCst),
        creates_after_open,
        "a Durable Session never reaches the creating seam"
    );
    Ok(())
}

#[tokio::test]
async fn durable_enqueue_to_an_unknown_id_stores_nothing_and_creates_nothing() -> Result<()> {
    let double = restate_double(SEED).await;
    let (backend, factory) = counting_factory(&double.lash_backend(), 0);
    let creates = Arc::clone(&factory.creates);
    let core = counting_core(backend.clone())?;

    let durable = core.session("never-created").durable().await?;
    let error = durable
        .send(TurnInput::text("queued to a session that does not exist"))
        .id("orphan-enqueue")
        .accepted()
        .await
        .expect_err("enqueue to an unknown id is refused");
    assert!(
        matches!(&error, EmbedError::UnknownSession { session_id } if session_id.as_str() == "never-created"),
        "unknown ids get the typed not-found refusal, got {error:?}"
    );
    assert_eq!(
        creates.load(Ordering::SeqCst),
        0,
        "a refused enqueue must not materialise session metadata"
    );
    assert!(
        lash_core::SessionStoreFactory::open_existing_store_by_id(
            factory.as_ref(),
            &SessionId::from("never-created"),
        )
        .await
        .expect("probe the catalog")
        .is_none(),
        "the refused enqueue left no store behind"
    );
    // The settled reads answer about the id instead of failing.
    assert!(!durable.exists().await?);
    assert!(!durable.was_deleted().await?);
    assert!(durable.read().await?.is_none());
    Ok(())
}

#[tokio::test]
async fn durable_operations_on_a_deleted_id_report_the_tombstone() -> Result<()> {
    let double = restate_double(SEED).await;
    let (backend, factory) = counting_factory(&double.lash_backend(), 0);
    let core = counting_core(backend.clone())?;
    drop(core.session("deleted-durable").open().await?);
    lash_core::SessionStoreFactory::delete_session(
        factory.as_ref(),
        &SessionId::from("deleted-durable"),
    )
    .await
    .expect("delete the session");

    let durable = core.session("deleted-durable").durable().await?;
    let error = durable
        .send(TurnInput::text("queued after deletion"))
        .accepted()
        .await
        .expect_err("enqueue to a deleted id is refused");
    assert!(
        matches!(
            &error,
            EmbedError::Store(StoreError::SessionDeleted { session_id })
                if session_id.as_str() == "deleted-durable"
        ),
        "deleted ids get the store's typed deletion error, got {error:?}"
    );
    assert!(durable.was_deleted().await?);
    assert!(!durable.exists().await?);
    Ok(())
}

#[tokio::test]
async fn durable_serves_a_metadata_only_session_and_a_checkpointed_one() -> Result<()> {
    let double = restate_double(SEED).await;
    let (backend, _) = counting_factory(&double.lash_backend(), 0);
    let core = counting_core(backend.clone())?;

    // Metadata only: created through the catalog, never committed.
    crate::tests::create_catalog_session(&core, "metadata-only").await?;
    let metadata_only = core.session("metadata-only").durable().await?;
    assert!(metadata_only.exists().await?);
    assert!(metadata_only.pending_turn_inputs().await?.is_empty());
    // A facade send asks the engine to drive; the pending input below must
    // stay pending for the read, so it is written through the store port
    // instead (the send path's enqueue event is not under test here), parked
    // on a turn that never runs so the engine cannot claim it either.
    let accepted = double
        .lash_backend()
        .session_store_factory()
        .open_existing_store_by_id(&SessionId::from("metadata-only"))
        .await?
        .expect("the created session has a store")
        .enqueue_pending_turn_input(lash_core::PendingTurnInputDraft {
            input_id: Some("metadata-only-input".to_string()),
            ..lash_core::PendingTurnInputDraft::new(
                SessionId::from("metadata-only"),
                lash_core::TurnInputIngress::active_turn(
                    lash_core::TurnId::from("metadata-only-parked-turn"),
                    lash_core::TurnInputCheckpointBoundary::AfterWork,
                ),
                TurnInput::text("queued against metadata-only"),
            )
        })
        .await
        .expect("enqueue the pending input");
    assert_eq!(
        metadata_only
            .pending_turn_inputs()
            .await?
            .iter()
            .map(|read| read.input.input_id.to_string())
            .collect::<Vec<_>>(),
        vec![accepted.input_id.to_string()]
    );

    // Checkpointed: a committed turn behind it. The send needs the engine's
    // queued-work port, so this leg runs on a second core whose driver is
    // dropped before the pending reads below.
    let drive_core = explicit_ephemeral_facets(LashCore::standard_builder(
        double.lash_backend(),
        crate::TurnBudget::Unbounded,
    ))
    .provider(mock_provider())
    .model(mock_model_spec())
    .build(crate::testing::runtime_lease_owner())?;
    let session = drive_core.session("checkpointed").open().await?;
    session
        .send(TurnInput::text("commit a turn"))
        .output()
        .await?;
    drop(session);
    drop(drive_core);
    let checkpointed = core.session("checkpointed").durable().await?;
    assert!(checkpointed.exists().await?);
    assert!(checkpointed.read().await?.is_some());
    double
        .lash_backend()
        .session_store_factory()
        .open_existing_store_by_id(&SessionId::from("checkpointed"))
        .await?
        .expect("the committed session has a store")
        .enqueue_pending_turn_input(lash_core::PendingTurnInputDraft {
            input_id: Some("checkpointed-input".to_string()),
            ..lash_core::PendingTurnInputDraft::new(
                SessionId::from("checkpointed"),
                lash_core::TurnInputIngress::active_turn(
                    lash_core::TurnId::from("checkpointed-parked-turn"),
                    lash_core::TurnInputCheckpointBoundary::AfterWork,
                ),
                TurnInput::text("queued against a checkpointed head"),
            )
        })
        .await
        .expect("enqueue the pending input");
    assert_eq!(checkpointed.pending_turn_inputs().await?.len(), 1);
    Ok(())
}

#[tokio::test]
async fn sqlite_durable_acquisition_covers_absent_metadata_only_and_checkpointed_ids() -> Result<()>
{
    // The "absent id creates no store" bound is the SQLite session
    // factory's, read here through the double's SQLite store set.
    let backend = double_backend().await;
    let factory = latest_double()
        .expect("the backend runs on its held double")
        .stores()
        .session_store_factory();
    let core = explicit_ephemeral_facets(LashCore::standard_builder(
        backend,
        crate::TurnBudget::Unbounded,
    ))
    .provider(mock_provider())
    .model(mock_model_spec())
    .build(crate::testing::runtime_lease_owner())?;

    let absent = core.session("sqlite-absent").durable().await?;
    assert!(!absent.exists().await?);
    assert!(
        matches!(
            absent
                .send(TurnInput::text("nope"))
                .accepted()
                .await
                .expect_err("absent sqlite id is refused"),
            EmbedError::UnknownSession { .. }
        ),
        "an absent sqlite id is refused without creating a store"
    );

    crate::tests::create_catalog_session(&core, "sqlite-metadata-only").await?;
    let metadata_only = core.session("sqlite-metadata-only").durable().await?;
    assert!(metadata_only.exists().await?);
    metadata_only
        .send(TurnInput::text("queued on sqlite metadata"))
        .id("sqlite-metadata-input")
        .accepted()
        .await?;
    assert_eq!(metadata_only.pending_turn_inputs().await?.len(), 1);

    let session = core.session("sqlite-checkpointed").open().await?;
    session
        .send(TurnInput::text("commit a turn"))
        .output()
        .await?;
    drop(session);
    let checkpointed = core.session("sqlite-checkpointed").durable().await?;
    assert!(checkpointed.exists().await?);
    checkpointed
        .send(TurnInput::text("queued on a sqlite checkpoint"))
        .id("sqlite-checkpoint-input")
        .accepted()
        .await?;
    assert_eq!(checkpointed.pending_turn_inputs().await?.len(), 1);

    lash_core::SessionStoreFactory::delete_session(
        factory.as_ref(),
        &SessionId::from("sqlite-checkpointed"),
    )
    .await
    .expect("delete the sqlite session");
    let deleted = core.session("sqlite-checkpointed").durable().await?;
    assert!(deleted.was_deleted().await?);
    assert!(
        matches!(
            deleted
                .send(TurnInput::text("nope"))
                .accepted()
                .await
                .expect_err("deleted sqlite id is refused"),
            EmbedError::Store(StoreError::SessionDeleted { .. })
        ),
        "a deleted sqlite id reports the tombstone"
    );
    Ok(())
}

#[tokio::test]
async fn a_live_observer_sees_queue_events_from_a_separately_acquired_durable_session() -> Result<()>
{
    let core = explicit_ephemeral_facets(LashCore::standard_builder(
        double_backend().await,
        crate::TurnBudget::Unbounded,
    ))
    .provider(mock_provider())
    .model(mock_model_spec())
    .build(crate::testing::runtime_lease_owner())?;
    let session = core.session("durable-observation").open().await?;
    let cursor = session.observe().current_observation().cursor;
    // The enqueue must publish `Enqueued` yet stay pending for the cancel:
    // the engine claims admitted input on its own schedule, so the session's
    // drive is held while the input is queued and cancelled.
    let _hold = held_double(&core)
        .expect("the core runs on its held double")
        .hold_session_drive(&SessionId::from("durable-observation"))
        .await;

    // A handle acquired from the core, not from the open session.
    let durable = core.session("durable-observation").durable().await?;
    let pending = durable
        .send(TurnInput::text("queued from a separate handle"))
        .id("separate-handle")
        .accepted()
        .await?;
    let cancelled = durable.cancel_pending_turn_input(&pending.input_id).await?;
    assert!(matches!(
        cancelled,
        crate::PendingTurnInputCancelOutcome::Cancelled(_)
    ));

    let SessionResume::Replayed { events } = session.observe().resume_from_cursor(&cursor)? else {
        panic!("the already-subscribed observer must replay the queue events");
    };
    assert!(
        events.iter().any(|event| matches!(
            &event.payload,
            lash_core::SessionObservationEventPayload::QueueChanged { kind, batch_ids }
                if *kind == lash_core::SessionQueueEventKind::Enqueued
                    && batch_ids.as_slice() == std::slice::from_ref(&pending.input_id)
        )),
        "the live observer receives Enqueued from the separately acquired handle"
    );
    assert!(
        events.iter().any(|event| matches!(
            &event.payload,
            lash_core::SessionObservationEventPayload::QueueChanged { kind, batch_ids }
                if *kind == lash_core::SessionQueueEventKind::Cancelled
                    && batch_ids.as_slice() == std::slice::from_ref(&pending.input_id)
        )),
        "the live observer receives Cancelled from the separately acquired handle"
    );
    Ok(())
}

#[tokio::test]
async fn queue_events_publish_with_no_live_runtime_and_replay_from_a_cursor() -> Result<()> {
    let double = restate_double(SEED).await;
    let core = explicit_ephemeral_facets(LashCore::standard_builder(
        double.lash_backend(),
        crate::TurnBudget::Unbounded,
    ))
    .provider(mock_provider())
    .model(mock_model_spec())
    .build(crate::testing::runtime_lease_owner())?;
    let session_id = SessionId::from("durable-no-runtime");
    // Create the session, then release every runtime: nothing is live.
    let session = core.session(session_id.clone()).open().await?;
    session
        .send(TurnInput::text("commit a turn"))
        .output()
        .await?;
    // A cursor minted while the session was live, at its committed head: the
    // runtime-free publication that follows must be reachable from it.
    let cursor = session.observe().current_observation().cursor;
    Box::pin(session.close()).await?;
    // The engine lane's writer claim frees when the closed lane settles;
    // both admits below race that release under the double.
    let durable = retry_when_claim_frees(|| core.session(session_id.clone()).durable()).await?;
    let pending = durable
        .send(TurnInput::text("queued with nothing live"))
        .id("no-runtime")
        .accepted()
        .await?;

    let reopened = retry_when_claim_frees(|| core.session(session_id.clone()).open()).await?;
    let SessionResume::Replayed { events } = reopened.observe().resume_from_cursor(&cursor)? else {
        panic!("a cursor minted before the publication must replay it");
    };
    assert!(
        events.iter().any(|event| matches!(
            &event.payload,
            lash_core::SessionObservationEventPayload::QueueChanged { kind, batch_ids }
                if *kind == lash_core::SessionQueueEventKind::Enqueued
                    && batch_ids.as_slice() == std::slice::from_ref(&pending.input_id)
        )),
        "the runtime-free publication replays from the cursor"
    );
    Ok(())
}

#[tokio::test]
async fn two_durable_handles_operate_beside_an_independently_leased_writer() -> Result<()> {
    let core = explicit_ephemeral_facets(LashCore::standard_builder(
        double_backend().await,
        crate::TurnBudget::Unbounded,
    ))
    .provider(mock_provider())
    .model(mock_model_spec())
    .build(crate::testing::runtime_lease_owner())?;
    let session_id = SessionId::from("durable-beside-writer");
    let writer = core.session(session_id.clone()).open().await?;

    let recorded_parent_before = writer.parent_session_id().map(ToString::to_string);
    // The engine drives an accepted input on its own schedule; hold the
    // session's drive so the read below sees both writes still pending.
    let hold = held_double(&core)
        .expect("the core runs on its held double")
        .hold_session_drive(&session_id)
        .await;
    let first = core.session(session_id.clone()).durable().await?;
    let second = core.session(session_id.clone()).durable().await?;

    let a = first
        .send(TurnInput::text("from the first durable handle"))
        .id("beside-a")
        .accepted()
        .await?;
    let b = second
        .send(TurnInput::text("from the second durable handle"))
        .id("beside-b")
        .accepted()
        .await?;

    // A third read sees both writes: the two handles address one queue.
    let pending_before_turn = second
        .pending_turn_inputs()
        .await?
        .iter()
        .map(|read| read.input.input_id.to_string())
        .collect::<Vec<_>>();
    assert!(
        pending_before_turn.contains(&a.input_id.to_string())
            && pending_before_turn.contains(&b.input_id.to_string()),
        "both handles wrote into the same durable queue beside the live writer, got {pending_before_turn:?}"
    );

    // The writer keeps committing turns on its own lease throughout, and the
    // durable handles keep answering across the commit.
    drop(hold);
    writer
        .send(TurnInput::text("writer keeps its lease"))
        .output()
        .await?;
    assert!(
        first.exists().await?,
        "the first handle still answers after the writer committed"
    );
    let applications = second
        .turn_input_applications()
        .await?
        .iter()
        .map(|application| application.input_id.to_string())
        .collect::<Vec<_>>();
    let still_pending = second
        .pending_turn_inputs()
        .await?
        .iter()
        .map(|read| read.input.input_id.to_string())
        .collect::<Vec<_>>();
    for id in [a.input_id.to_string(), b.input_id.to_string()] {
        assert!(
            applications.contains(&id) || still_pending.contains(&id),
            "every durably accepted input is either settled or still pending after the commit; \
             missing {id} (applications={applications:?}, pending={still_pending:?})"
        );
    }
    assert_eq!(
        writer.parent_session_id().map(ToString::to_string),
        recorded_parent_before,
        "durable access beside a writer leaves the recorded Session Relation alone"
    );
    let settled = first
        .read()
        .await?
        .expect("the committed session reads back through the catalog");
    assert_eq!(settled.session_id(), session_id.as_str());
    assert!(
        settled
            .durable_relation()
            .is_none_or(|relation| matches!(relation, lash_core::SessionRelation::Root)),
        "the session is still recorded as the root it was admitted as"
    );
    Ok(())
}

/// Counters for everything `open()` does that `durable()` must not.
#[derive(Default)]
struct RuntimeBuildCounters {
    plugin_materializations: AtomicUsize,
    session_restored_events: AtomicUsize,
    process_admissions: AtomicUsize,
}

impl RuntimeBuildCounters {
    fn snapshot(&self) -> (usize, usize, usize) {
        (
            self.plugin_materializations.load(Ordering::SeqCst),
            self.session_restored_events.load(Ordering::SeqCst),
            self.process_admissions.load(Ordering::SeqCst),
        )
    }
}

struct RuntimeBuildProbeFactory {
    counters: Arc<RuntimeBuildCounters>,
}

impl lash_core::facade_support::PluginFactory for RuntimeBuildProbeFactory {
    fn id(&self) -> &'static str {
        "durable-session-runtime-probe"
    }

    fn build(
        &self,
        _ctx: &lash_core::facade_support::PluginSessionContext,
    ) -> std::result::Result<
        Arc<dyn lash_core::facade_support::SessionPlugin>,
        lash_core::PluginError,
    > {
        self.counters
            .plugin_materializations
            .fetch_add(1, Ordering::SeqCst);
        Ok(Arc::new(RuntimeBuildProbePlugin {
            counters: Arc::clone(&self.counters),
        }))
    }
}

struct RuntimeBuildProbePlugin {
    counters: Arc<RuntimeBuildCounters>,
}

impl lash_core::facade_support::SessionPlugin for RuntimeBuildProbePlugin {
    fn id(&self) -> &'static str {
        "durable-session-runtime-probe"
    }

    fn register(
        &self,
        reg: &mut lash_core::facade_support::PluginRegistrar,
    ) -> std::result::Result<(), lash_core::PluginError> {
        let counters = Arc::clone(&self.counters);
        reg.session().on_event(Arc::new(move |event| {
            let counters = Arc::clone(&counters);
            Box::pin(async move {
                if matches!(
                    event,
                    lash_core::facade_support::PluginLifecycleEvent::SessionRestored(_)
                ) {
                    counters
                        .session_restored_events
                        .fetch_add(1, Ordering::SeqCst);
                }
                Ok(())
            })
        }));
        Ok(())
    }
}

struct CountingProcessWork {
    counters: Arc<RuntimeBuildCounters>,
}

#[async_trait]
impl lash_core::ProcessWorkSubstrate for CountingProcessWork {
    async fn deliver_process_start(
        &self,
        _record: &lash_core::ProcessRecord,
    ) -> std::result::Result<(), lash_core::PluginError> {
        self.counters
            .process_admissions
            .fetch_add(1, Ordering::SeqCst);
        Ok(())
    }

    async fn await_process_terminal(
        &self,
        process_id: &lash_core::ProcessId,
    ) -> std::result::Result<lash_core::ProcessTerminalWait, lash_core::PluginError> {
        panic!("unexpected terminal wait for {process_id}")
    }

    async fn deliver_cancel(
        &self,
        _process_id: &lash_core::ProcessId,
        _request: &lash_core::CancelRequest,
        _key: &str,
    ) -> std::result::Result<(), lash_core::PluginError> {
        Ok(())
    }

    async fn publish_process_terminal(
        &self,
        process_id: &lash_core::ProcessId,
        output: &lash_core::ProcessAwaitOutput,
        key: &str,
    ) -> std::result::Result<(), lash_core::PluginError> {
        let _ = (process_id, output, key);
        Ok(())
    }
}

/// Read the persisted checkpoint's `tool_state` component bytes.
async fn persisted_tool_state_bytes(
    factory: &dyn SessionStoreFactory,
    session_id: &SessionId,
) -> Result<Vec<u8>> {
    let store = factory
        .open_existing_store_by_id(session_id)
        .await
        .expect("open the persisted session store")
        .expect("the session exists");
    let read = lash_core::SessionCommitStore::load_session(store.as_ref())
        .await?
        .expect("the session has committed state");
    let checkpoint = read.checkpoint.expect("the session has a checkpoint");
    let component = checkpoint
        .component(lash_core::store::TOOL_STATE_CHECKPOINT_COMPONENT)
        .expect("the checkpoint carries tool state");
    Ok(serde_json::to_vec(component).expect("serialize the tool-state component"))
}

/// FIG-3353: a grantless core polls and edits a session's queue without
/// building a runtime, so nothing is orphaned and nothing is restored.
///
/// The test asserts its own precondition — `open()` on this core *does* orphan
/// the persisted tool — and then counts, on the `durable()` path, every step of
/// the runtime build. Swapping `durable()` for `open()` here fails: the
/// counters below all move, and the byte-identity assertion fails with them.
#[tokio::test]
async fn durable_queue_access_on_a_grantless_core_builds_no_runtime() -> Result<()> {
    let session_id = SessionId::from("fig-3353-durable-poll");
    let double = restate_double(SEED).await;
    let backend = double.lash_backend();
    let factory: Arc<dyn SessionStoreFactory> = backend.session_store_factory();

    // A core that carries the session's tool source, to persist tool state.
    // Its send needs the engine's queued-work port; the pending enqueue that
    // follows must stay pending, so it goes through the grantless core — the
    // only core left without a driver once this one is dropped.
    let granting_core = explicit_ephemeral_facets(LashCore::standard_builder(
        backend.clone(),
        crate::TurnBudget::Unbounded,
    ))
    .provider(mock_provider())
    .model(mock_model_spec())
    .tools(Arc::new(AppTools))
    .build(crate::testing::runtime_lease_owner())?;
    let granted = granting_core.session(session_id.clone()).open().await?;
    assert!(
        granted
            .admin()
            .tools()
            .state()
            .await?
            .contains(&lash_core::ToolId::from("tool:app_lookup")),
        "the granting core registers the tool this session will persist"
    );
    granted
        .send(TurnInput::text("persist a checkpoint with tool state"))
        .output()
        .await?;

    Box::pin(granted.close()).await?;
    drop(granting_core);

    // The engine may still run a session drive the send or close scheduled —
    // a reconcile, say — and under the double that drive's admit materialises
    // a runtime on the grantless core's session-work handle, landing after
    // the counter baseline below. Gate the drive for the measurement window;
    // the durable ops under test are store reads and never need it.
    let _hold = double.hold_session_drive(&session_id).await;

    let tool_state_before = persisted_tool_state_bytes(factory.as_ref(), &session_id).await?;

    // The grantless core: same store, no tool source, fully instrumented.
    let counters = Arc::new(RuntimeBuildCounters::default());
    let grantless_core = explicit_ephemeral_facets(LashCore::standard_builder(
        DecoratedBackend::over(backend.clone())
            .process_work({
                let counters = Arc::clone(&counters);
                move |registry| {
                    lash_core::ProcessWorkWiring::new(
                        lash_core::facade_support::watch_process_registry(registry),
                        Arc::new(CountingProcessWork { counters }),
                    )
                }
            })
            .into(),
        crate::TurnBudget::Unbounded,
    ))
    .provider(mock_provider())
    .model(mock_model_spec())
    .plugin(Arc::new(RuntimeBuildProbeFactory {
        counters: Arc::clone(&counters),
    }))
    .build(crate::testing::runtime_lease_owner())?;

    // Precondition: on this core, `open()` really does orphan the tool. A
    // negative test whose premise does not hold proves nothing.
    {
        let opened =
            retry_when_claim_frees(|| grantless_core.session(session_id.clone()).open()).await?;
        let state = opened.admin().tools().state().await?;
        let entry = state
            .get(&lash_core::ToolId::from("tool:app_lookup"))
            .expect("the persisted tool survives as a state entry");
        assert!(
            entry.is_orphaned(),
            "precondition: opening on a core without the tool's source orphans it"
        );
        let (plugins, restored, admissions) = counters.snapshot();
        assert!(
            plugins > 0 && restored > 0,
            "precondition: open() materialises plugins ({plugins}) and restores the session ({restored})"
        );
        let _ = admissions;
        Box::pin(opened.close()).await?;
    }
    // Restore the durable tool state the orphaning open may have rewritten,
    // then measure the durable path from a clean baseline.
    let tool_state_before = persisted_tool_state_bytes(factory.as_ref(), &session_id)
        .await
        .unwrap_or(tool_state_before);
    counters.plugin_materializations.store(0, Ordering::SeqCst);
    counters.session_restored_events.store(0, Ordering::SeqCst);
    counters.process_admissions.store(0, Ordering::SeqCst);

    // The FIG-3353 poll, through the Durable Session. The pending input is
    // seeded through the store port and parked on a turn that never runs:
    // a facade send would ask the engine to drive, and a next-turn row the
    // engine could claim would race the pending reads below.
    let durable = grantless_core.session(session_id.clone()).durable().await?;
    let queued = factory
        .open_existing_store_by_id(&session_id)
        .await?
        .expect("the persisted session has a store")
        .enqueue_pending_turn_input(lash_core::PendingTurnInputDraft {
            input_id: Some("fig-3353-pending".to_string()),
            ..lash_core::PendingTurnInputDraft::new(
                session_id.clone(),
                lash_core::TurnInputIngress::active_turn(
                    lash_core::TurnId::from("fig-3353-parked-turn"),
                    lash_core::TurnInputCheckpointBoundary::AfterWork,
                ),
                TurnInput::text("left pending for the grantless core"),
            )
        })
        .await
        .expect("enqueue the pending input");
    let pending = durable.pending_turn_inputs().await?;
    assert_eq!(
        pending
            .iter()
            .map(|read| read.input.input_id.to_string())
            .collect::<Vec<_>>(),
        vec![queued.input_id.to_string()],
        "the grantless core lists the pending input it never granted tools for"
    );
    let cancelled = durable.cancel_pending_turn_input(&queued.input_id).await?;
    assert!(
        matches!(
            cancelled,
            crate::PendingTurnInputCancelOutcome::Cancelled(_)
        ),
        "the grantless core cancels the pending input, got {cancelled:?}"
    );
    assert!(durable.pending_turn_inputs().await?.is_empty());

    let (plugins, restored, admissions) = counters.snapshot();
    assert_eq!(
        (plugins, restored, admissions),
        (0, 0, 0),
        "durable queue access builds no plugin session, restores nothing, and admits no process"
    );
    assert_eq!(
        persisted_tool_state_bytes(factory.as_ref(), &session_id).await?,
        tool_state_before,
        "the persisted tool state is byte-identical after a durable poll and cancel"
    );
    Ok(())
}

/// A catalog that creates and deletes but cannot resolve a session by id.
///
/// This is the shape a backend without a by-id lookup has to take now that
/// `open_existing_store_by_id` is required: it states the missing capability
/// instead of inheriting `Ok(None)`, which would have reported every existing
/// session as absent.
struct NoByIdLookupFactory {
    inner: Arc<dyn SessionStoreFactory>,
}

const NO_BY_ID_LOOKUP_OPERATION: &str = "SessionStoreFactory::open_existing_store_by_id";

#[async_trait]
impl lash_core::AttachmentRootSet for NoByIdLookupFactory {
    async fn live_attachment_refs(
        &self,
        intent_grace_cutoff_epoch_ms: u64,
    ) -> std::result::Result<
        std::collections::BTreeSet<lash_core::AttachmentId>,
        lash_core::StoreError,
    > {
        lash_core::AttachmentRootSet::live_attachment_refs(
            self.inner.as_ref(),
            intent_grace_cutoff_epoch_ms,
        )
        .await
    }

    async fn has_live_attachment_ref(
        &self,
        id: &lash_core::AttachmentId,
        intent_grace_cutoff_epoch_ms: u64,
    ) -> std::result::Result<bool, lash_core::StoreError> {
        lash_core::AttachmentRootSet::has_live_attachment_ref(
            self.inner.as_ref(),
            id,
            intent_grace_cutoff_epoch_ms,
        )
        .await
    }
}

#[async_trait]
impl SessionStoreFactory for NoByIdLookupFactory {
    async fn create_store(
        &self,
        request: &lash_core::SessionStoreCreateRequest,
    ) -> std::result::Result<Arc<dyn lash_core::RuntimePersistence>, lash_core::StoreError> {
        self.inner.create_store(request).await
    }

    async fn open_existing_store_by_id(
        &self,
        _session_id: &SessionId,
    ) -> std::result::Result<Option<Arc<dyn lash_core::RuntimePersistence>>, lash_core::StoreError>
    {
        Err(lash_core::StoreError::UnsupportedStoreOperation {
            operation: NO_BY_ID_LOOKUP_OPERATION,
        })
    }

    async fn session_was_deleted(
        &self,
        session_id: &SessionId,
    ) -> std::result::Result<bool, String> {
        lash_core::SessionStoreFactory::session_was_deleted(self.inner.as_ref(), session_id).await
    }

    async fn delete_session(
        &self,
        session_id: &SessionId,
    ) -> lash_core::MaintenanceResult<lash_core::SessionBlobReclaimReport> {
        self.inner.delete_session(session_id).await
    }

    // A decorator forwards the deployment turn count to the catalog it wraps.
    async fn count_unsettled_turns(
        &self,
    ) -> std::result::Result<lash_core::store::UnsettledTurnCounts, lash_core::StoreError> {
        self.inner.count_unsettled_turns().await
    }

    async fn list_turn_parks(
        &self,
        query: &lash_core::store::TurnParkQuery,
    ) -> std::result::Result<Vec<lash_core::store::TurnPark>, lash_core::StoreError> {
        self.inner.list_turn_parks(query).await
    }

    async fn turn_park_feed(
        &self,
        after: lash_core::store::ParkFeedCursor,
        limit: std::num::NonZeroUsize,
    ) -> std::result::Result<
        lash_core::store::ParkFeedPage<lash_core::store::TurnParkTarget>,
        lash_core::StoreError,
    > {
        self.inner.turn_park_feed(after, limit).await
    }

    async fn root_terminal(
        &self,
        session_id: &lash_core::SessionId,
        root: &lash_core::TurnId,
    ) -> std::result::Result<Option<lash_core::store::RootTerminal>, lash_core::StoreError> {
        self.inner.root_terminal(session_id, root).await
    }

    async fn compact_turn_park_feed(
        &self,
        through: lash_core::store::ParkFeedCursor,
    ) -> std::result::Result<(), lash_core::StoreError> {
        self.inner.compact_turn_park_feed(through).await
    }
}

#[async_trait::async_trait]
impl lash_core::store::ControlIntentStore for NoByIdLookupFactory {
    async fn begin_session_close(
        &self,
        session_id: &SessionId,
        at_ms: u64,
    ) -> std::result::Result<Option<lash_core::store::ControlIntent>, lash_core::StoreError> {
        self.inner.begin_session_close(session_id, at_ms).await
    }

    async fn claim_intent_application(
        &self,
        id: lash_core::store::ControlIntentId,
        at_ms: u64,
    ) -> std::result::Result<lash_core::store::IntentApplication, lash_core::StoreError> {
        self.inner.claim_intent_application(id, at_ms).await
    }

    async fn acknowledge_intent(
        &self,
        id: lash_core::store::ControlIntentId,
        claim: &lash_core::store::ClaimToken,
        at_ms: u64,
    ) -> std::result::Result<lash_core::store::IntentSettle, lash_core::StoreError> {
        self.inner.acknowledge_intent(id, claim, at_ms).await
    }

    async fn record_intent_failure(
        &self,
        id: lash_core::store::ControlIntentId,
        claim: &lash_core::store::ClaimToken,
        error: &str,
        retryable: bool,
        at_ms: u64,
    ) -> std::result::Result<lash_core::store::IntentSettle, lash_core::StoreError> {
        self.inner
            .record_intent_failure(id, claim, error, retryable, at_ms)
            .await
    }

    async fn load_intent(
        &self,
        id: lash_core::store::ControlIntentId,
    ) -> std::result::Result<Option<lash_core::store::ControlIntent>, lash_core::StoreError> {
        self.inner.load_intent(id).await
    }
}

/// A catalog without the by-id seam must not make an existing session look
/// absent. Before the seam was required, its inherited `Ok(None)` did exactly
/// that: every durable operation on a live session reported `UnknownSession`
/// and sent the host hunting for a session that was there.
#[tokio::test]
async fn a_catalog_without_the_by_id_seam_names_the_capability_not_a_missing_session() -> Result<()>
{
    let double = restate_double(SEED).await;
    let backend = DecoratedBackend::over(double.lash_backend())
        .session_store_factory(|inner| Arc::new(NoByIdLookupFactory { inner }));
    let core = explicit_ephemeral_facets(LashCore::standard_builder(
        backend.into(),
        crate::TurnBudget::Unbounded,
    ))
    .provider(mock_provider())
    .model(mock_model_spec())
    .build(crate::testing::runtime_lease_owner())?;

    // The session genuinely exists: this open created it.
    crate::tests::create_catalog_session(&core, "no-by-id-seam").await?;

    let durable = core.session("no-by-id-seam").durable().await?;
    let error = durable
        .send(TurnInput::text(
            "queued through a catalog with no by-id seam",
        ))
        .id("no-by-id-seam-input")
        .accepted()
        .await
        .expect_err("a catalog that cannot resolve by id refuses the acquisition");
    match &error {
        EmbedError::StoreFactory {
            session_id,
            message,
        } => {
            assert_eq!(session_id.as_str(), "no-by-id-seam");
            assert!(
                message.contains(NO_BY_ID_LOOKUP_OPERATION),
                "the error names the missing capability, got {message}"
            );
        }
        other => {
            panic!("a missing by-id seam must not be reported as an absent session, got {other:?}")
        }
    }
    assert!(
        !matches!(error, EmbedError::UnknownSession { .. }),
        "an existing session must never be reported as unknown"
    );
    Ok(())
}

/// A held row is still reported, held, through the Durable Session.
///
/// The ticket's list contract includes a held input, and "held" is a fact only
/// a live claimant produces. This gets one without hand-building lease
/// authority: a queued drain claims the enqueued input and then blocks inside
/// the provider, and a *second*, separately acquired Durable Session lists the
/// queue while the claim is live.
#[tokio::test]
async fn a_held_input_is_still_listed_held_by_a_separate_durable_handle() -> Result<()> {
    let entered = Arc::new(tokio::sync::Notify::new());
    let release = Arc::new(tokio::sync::Semaphore::new(0));
    let provider = crate::testing::TestProvider::builder()
        .kind("embed-test")
        .complete({
            let entered = Arc::clone(&entered);
            let release = Arc::clone(&release);
            move |_request| {
                let entered = Arc::clone(&entered);
                let release = Arc::clone(&release);
                async move {
                    // The drain has claimed the input by the time it asks the
                    // provider; hold it here so the claim stays live.
                    entered.notify_one();
                    release
                        .acquire()
                        .await
                        .expect("release semaphore remains open")
                        .forget();
                    Ok(text_response("drained"))
                }
            }
        })
        .build()
        .into_handle();
    let core = explicit_ephemeral_facets(LashCore::standard_builder(
        double_backend().await,
        crate::TurnBudget::Unbounded,
    ))
    .provider(provider)
    .model(mock_model_spec())
    .build(crate::testing::runtime_lease_owner())?;
    let session_id = SessionId::from("durable-held-input");
    let session = core.session(session_id.clone()).open().await?;
    // The engine claims an accepted row on its own schedule: hold the
    // session's drive so the pre-claim read sees the row pending.
    let hold = held_double(&core)
        .expect("the core runs on its held double")
        .hold_session_drive(&session_id)
        .await;
    let accepted = session
        .durable()
        .send(TurnInput::text("claimed by the drain"))
        .id("held-input")
        .accepted()
        .await?;

    let observer = core.session(session_id.clone()).durable().await?;
    assert!(
        matches!(
            observer
                .pending_turn_inputs()
                .await?
                .first()
                .map(|read| &read.status),
            Some(lash_core::runtime::PendingTurnInputReadStatus::Open)
        ),
        "before the drain admits it, the row reads as open"
    );
    drop(hold);

    let drain = tokio::spawn({
        let session = session.clone();
        let input = accepted.input_id.clone();
        async move { session.attach(input).output().await }
    });
    tokio::time::timeout(std::time::Duration::from_secs(5), entered.notified())
        .await
        .expect("the drain reaches the provider with the input admitted");

    let held = observer.pending_turn_inputs().await?;
    let row = held
        .iter()
        .find(|read| read.input.input_id == accepted.input_id)
        .expect("an admitted input is still reported by the Durable Session, not hidden");
    assert!(
        matches!(
            row.status,
            lash_core::runtime::PendingTurnInputReadStatus::Admitted { .. }
        ),
        "the input the drain took must read as admitted to its root, got {:?}",
        row.status
    );

    release.add_permits(1);
    drain.await.expect("drain task")?;
    Ok(())
}

/// `create()` is the one verb that creates, and it creates nothing else: no
/// runtime, no lease, no lifecycle event.
#[tokio::test]
async fn create_admits_an_absent_id_and_builds_no_runtime() -> Result<()> {
    let counters = Arc::new(RuntimeBuildCounters::default());
    let double = restate_double(SEED).await;
    let idle = explicit_ephemeral_facets(LashCore::standard_builder(
        DecoratedBackend::over(double.lash_backend())
            .process_work({
                let counters = Arc::clone(&counters);
                move |registry| {
                    lash_core::ProcessWorkWiring::new(
                        lash_core::facade_support::watch_process_registry(registry),
                        Arc::new(CountingProcessWork { counters }),
                    )
                }
            })
            .into(),
        crate::TurnBudget::Unbounded,
    ))
    .provider(mock_provider())
    .model(mock_model_spec())
    .plugin(Arc::new(RuntimeBuildProbeFactory {
        counters: Arc::clone(&counters),
    }))
    .build(crate::testing::runtime_lease_owner())?;

    // Building the core itself materialises its plugin host once; that is the
    // baseline `create()` must not move.
    let baseline = counters.snapshot();

    // Enqueue to an id nobody created is refused...
    assert!(matches!(
        idle.session("created-then-queued")
            .durable()
            .await?
            .send(TurnInput::text("too early"))
            .accepted()
            .await
            .expect_err("an uncreated id is refused"),
        EmbedError::UnknownSession { .. }
    ));

    // ...and `create()` is the explicit two-step's first half. The queued
    // input is seeded through the store port: a facade send would ask the
    // engine to drive, racing the pending read below. A store-seeded row is
    // never scheduled, so nothing claims it before the drive core below.
    let durable = idle.session("created-then-queued").create().await?;
    let accepted = idle
        .store_factory
        .open_existing_store_by_id(&SessionId::from("created-then-queued"))
        .await?
        .expect("the created session has a store")
        .enqueue_pending_turn_input(lash_core::PendingTurnInputDraft {
            input_id: Some("created-then-queued-input".to_string()),
            ..lash_core::PendingTurnInputDraft::new(
                SessionId::from("created-then-queued"),
                lash_core::TurnInputIngress::NextTurn,
                TurnInput::text("queued before the first turn"),
            )
        })
        .await
        .expect("enqueue the pending input");
    assert_eq!(durable.pending_turn_inputs().await?.len(), 1);
    assert!(durable.exists().await?);

    assert_eq!(
        counters.snapshot(),
        baseline,
        "create() materialises no plugin session, restores nothing, admits no process"
    );

    // The session a host creates this way is an ordinary session: opening it
    // runs the input that was waiting. The store-seeded row was never
    // scheduled, so a second core supplies the drive that reconciles it.
    let drive_core = explicit_ephemeral_facets(LashCore::standard_builder(
        double.lash_backend(),
        crate::TurnBudget::Unbounded,
    ))
    .provider(mock_provider())
    .model(mock_model_spec())
    .build(crate::testing::runtime_lease_owner())?;
    // `idle`'s created handle still holds a writer claim: the drive core's
    // open races its release under the double.
    let session =
        retry_when_claim_frees(|| drive_core.session("created-then-queued").open()).await?;
    let drained = session.attach(accepted.input_id.clone()).output().await?;
    assert_eq!(
        drained.assistant_message(),
        Some("echo: queued before the first turn")
    );
    assert!(session.durable().pending_turn_inputs().await?.is_empty());
    assert_eq!(
        session
            .durable()
            .turn_input_applications()
            .await?
            .iter()
            .map(|application| application.input_id.to_string())
            .collect::<Vec<_>>(),
        vec![accepted.input_id.to_string()],
        "the created session's queued input settles as a durable application"
    );
    Ok(())
}

/// Creating an id twice is a no-op that keeps the durable facts the first
/// create recorded, including the Session Relation.
#[tokio::test]
async fn create_is_idempotent_and_preserves_the_recorded_relation() -> Result<()> {
    let double = restate_double(SEED).await;
    let core = explicit_ephemeral_facets(LashCore::standard_builder(
        double.lash_backend(),
        crate::TurnBudget::Unbounded,
    ))
    .provider(mock_provider())
    .model(mock_model_spec())
    .build(crate::testing::runtime_lease_owner())?;
    drop(core.session("create-parent").create().await?);

    let _first = core
        .session("create-idempotent")
        .parent("create-parent")
        .create()
        .await?;
    // Seeded through the store port: a facade send would ask the engine to
    // drive, racing the pending read after the second create. A store-seeded
    // row is never scheduled, so nothing claims it before the drive below.
    let accepted = core
        .store_factory
        .open_existing_store_by_id(&SessionId::from("create-idempotent"))
        .await?
        .expect("the created session has a store")
        .enqueue_pending_turn_input(lash_core::PendingTurnInputDraft {
            input_id: Some("idempotent-input".to_string()),
            ..lash_core::PendingTurnInputDraft::new(
                SessionId::from("create-idempotent"),
                lash_core::TurnInputIngress::NextTurn,
                TurnInput::text("survives the second create"),
            )
        })
        .await
        .expect("enqueue the pending input");

    // A second create, naming no parent, must not rewrite the relation or drop
    // the queue.
    let second = core.session("create-idempotent").create().await?;
    assert_eq!(
        second
            .pending_turn_inputs()
            .await?
            .iter()
            .map(|read| read.input.input_id.to_string())
            .collect::<Vec<_>>(),
        vec![accepted.input_id.to_string()],
        "re-creating an existing id keeps its durable queue"
    );
    // `second`'s writer claim is still live; the reopen races its release
    // under the double.
    let reopened = retry_when_claim_frees(|| core.session("create-idempotent").open()).await?;
    assert_eq!(
        reopened.parent_session_id(),
        Some("create-parent"),
        "re-creating an existing id preserves its recorded Session Relation"
    );
    Ok(())
}

/// Session ids are single-use, so `create()` refuses a tombstoned one.
#[tokio::test]
async fn create_on_a_deleted_id_is_refused_with_the_tombstone() -> Result<()> {
    let double = restate_double(SEED).await;
    let backend = double.lash_backend();
    let factory = backend.session_store_factory();
    let core = explicit_ephemeral_facets(LashCore::standard_builder(
        backend.clone(),
        crate::TurnBudget::Unbounded,
    ))
    .provider(mock_provider())
    .model(mock_model_spec())
    .build(crate::testing::runtime_lease_owner())?;
    drop(core.session("create-deleted").create().await?);
    lash_core::SessionStoreFactory::delete_session(
        factory.as_ref(),
        &SessionId::from("create-deleted"),
    )
    .await
    .expect("delete the session");

    let error = core
        .session("create-deleted")
        .create()
        .await
        .err()
        .expect("creating a retired id is refused");
    assert!(
        matches!(
            &error,
            EmbedError::Store(StoreError::SessionDeleted { session_id })
                if session_id.as_str() == "create-deleted"
        ),
        "a retired id reports the tombstone, got {error:?}"
    );
    Ok(())
}

/// A host that reuses an enqueue id for a different submission is told so with
/// a typed, terminal error rather than a generic store failure; an identical
/// retry replays the original acceptance (FIG-3544).
#[tokio::test]
async fn reused_enqueue_id_with_changed_input_is_a_typed_identity_conflict() -> Result<()> {
    let double = restate_double(SEED).await;
    let core = explicit_ephemeral_facets(LashCore::standard_builder(
        double.lash_backend(),
        crate::TurnBudget::Unbounded,
    ))
    .provider(mock_provider())
    .model(mock_model_spec())
    .build(crate::testing::runtime_lease_owner())?;
    crate::tests::create_catalog_session(&core, "fig3544-enqueue-conflict").await?;
    let durable = core.session("fig3544-enqueue-conflict").durable().await?;

    let first = durable
        .send(TurnInput::text("original"))
        .id("retry-me")
        .accepted()
        .await?;
    let replay = durable
        .send(TurnInput::text("original"))
        .id("retry-me")
        .accepted()
        .await?;
    assert_eq!(replay, first, "an identical retry replays the acceptance");

    let conflict = durable
        .send(TurnInput::text("changed"))
        .id("retry-me")
        .accepted()
        .await
        .expect_err("a changed submission under a used id is refused");
    let EmbedError::Runtime(error) = &conflict else {
        panic!("expected a typed runtime error, got {conflict:?}");
    };
    assert_eq!(
        error.code,
        lash_core::RuntimeErrorCode::DurableIdentityConflict,
        "the refusal is typed, not a generic store commit failure"
    );
    assert!(conflict.is_terminal() && !conflict.is_retryable());
    Ok(())
}
