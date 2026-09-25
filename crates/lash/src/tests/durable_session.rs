//! Durable Session: acquisition, refusals, observation, and coexistence with a
//! live writer (FIG-3366, ADR 0097).

use super::*;
use lash_sansio::SessionId;

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
        after: lash_core::store::TurnParkFeedCursor,
        limit: std::num::NonZeroUsize,
    ) -> std::result::Result<lash_core::store::TurnParkFeedPage, lash_core::StoreError> {
        self.inner.turn_park_feed(after, limit).await
    }

    async fn root_terminal(
        &self,
        session_id: &lash_core::SessionId,
        root: &lash_core::TurnId,
    ) -> std::result::Result<Option<lash_core::store::RootTerminal>, lash_core::StoreError> {
        self.inner.root_terminal(session_id, root).await
    }

    async fn list_open_control_intents(
        &self,
        after: Option<lash_core::store::ControlIntentId>,
        limit: std::num::NonZeroUsize,
    ) -> std::result::Result<Vec<lash_core::store::ControlIntent>, lash_core::StoreError> {
        self.inner.list_open_control_intents(after, limit).await
    }

    async fn compact_turn_park_feed(
        &self,
        through: lash_core::store::TurnParkFeedCursor,
    ) -> std::result::Result<(), lash_core::StoreError> {
        self.inner.compact_turn_park_feed(through).await
    }
}

/// A counting catalog over a fresh memory backend's own.
async fn counting_factory(
    open_delay_ms: u64,
) -> (Arc<DecoratedBackend>, Arc<CountingSessionStoreFactory>) {
    let inner = memory_backend().await;
    let factory = Arc::new(CountingSessionStoreFactory::new(
        inner.session_store_factory(),
        open_delay_ms,
    ));
    let catalog = Arc::clone(&factory);
    let backend = Arc::new(DecoratedBackend::over(inner).session_store_factory(move |_| catalog));
    (backend, factory)
}

fn counting_core(backend: Arc<DecoratedBackend>) -> Result<LashCore> {
    explicit_ephemeral_facets(LashCore::standard_builder(
        backend,
        crate::TurnBudget::Unbounded,
    ))
    .provider(mock_provider())
    .model(mock_model_spec())
    .without_queued_work()
    .build(crate::testing::runtime_lease_owner())
}

#[tokio::test]
async fn durable_acquisition_is_non_creating_and_happens_once_per_handle() -> Result<()> {
    let (backend, factory) = counting_factory(20).await;
    let creates = Arc::clone(&factory.creates);
    let by_id_opens = Arc::clone(&factory.by_id_opens);
    let core = counting_core(Arc::clone(&backend))?;

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
    let (backend, factory) = counting_factory(0).await;
    let creates = Arc::clone(&factory.creates);
    let core = counting_core(Arc::clone(&backend))?;

    let durable = core.session("never-created").durable().await?;
    let error = durable
        .enqueue(TurnInput::text("queued to a session that does not exist"))
        .id("orphan-enqueue")
        .send()
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
    let (backend, factory) = counting_factory(0).await;
    let core = counting_core(Arc::clone(&backend))?;
    drop(core.session("deleted-durable").open().await?);
    lash_core::SessionStoreFactory::delete_session(
        factory.as_ref(),
        &SessionId::from("deleted-durable"),
    )
    .await
    .expect("delete the session");

    let durable = core.session("deleted-durable").durable().await?;
    let error = durable
        .enqueue(TurnInput::text("queued after deletion"))
        .send()
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
    let (backend, _) = counting_factory(0).await;
    let core = counting_core(Arc::clone(&backend))?;

    // Metadata only: created through the catalog, never committed.
    crate::tests::create_catalog_session(&core, "metadata-only").await?;
    let metadata_only = core.session("metadata-only").durable().await?;
    assert!(metadata_only.exists().await?);
    assert!(metadata_only.pending_turn_inputs().await?.is_empty());
    let accepted = metadata_only
        .enqueue(TurnInput::text("queued against metadata-only"))
        .id("metadata-only-input")
        .send()
        .await?;
    assert_eq!(
        metadata_only
            .pending_turn_inputs()
            .await?
            .iter()
            .map(|read| read.input.input_id.to_string())
            .collect::<Vec<_>>(),
        vec![accepted.input_id.to_string()]
    );

    // Checkpointed: a committed turn behind it.
    let session = core.session("checkpointed").open().await?;
    session.turn(TurnInput::text("commit a turn")).run().await?;
    drop(session);
    let checkpointed = core.session("checkpointed").durable().await?;
    assert!(checkpointed.exists().await?);
    assert!(checkpointed.read().await?.is_some());
    checkpointed
        .enqueue(TurnInput::text("queued against a checkpointed head"))
        .id("checkpointed-input")
        .send()
        .await?;
    assert_eq!(checkpointed.pending_turn_inputs().await?.len(), 1);
    Ok(())
}

#[tokio::test]
async fn sqlite_durable_acquisition_covers_absent_metadata_only_and_checkpointed_ids() -> Result<()>
{
    let dir = tempfile::tempdir().expect("temp dir");
    let backend = Arc::new(
        lash_sqlite_store::SqliteBackend::open(dir.path().join("sessions.db"))
            .await
            .expect("open the SQLite backend"),
    );
    let factory = backend.session_store_factory();
    let core = explicit_ephemeral_facets(LashCore::standard_builder(
        backend.clone(),
        crate::TurnBudget::Unbounded,
    ))
    .provider(mock_provider())
    .model(mock_model_spec())
    .without_queued_work()
    .build(crate::testing::runtime_lease_owner())?;

    let absent = core.session("sqlite-absent").durable().await?;
    assert!(!absent.exists().await?);
    assert!(
        matches!(
            absent
                .enqueue(TurnInput::text("nope"))
                .send()
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
        .enqueue(TurnInput::text("queued on sqlite metadata"))
        .id("sqlite-metadata-input")
        .send()
        .await?;
    assert_eq!(metadata_only.pending_turn_inputs().await?.len(), 1);

    let session = core.session("sqlite-checkpointed").open().await?;
    session.turn(TurnInput::text("commit a turn")).run().await?;
    drop(session);
    let checkpointed = core.session("sqlite-checkpointed").durable().await?;
    assert!(checkpointed.exists().await?);
    checkpointed
        .enqueue(TurnInput::text("queued on a sqlite checkpoint"))
        .id("sqlite-checkpoint-input")
        .send()
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
                .enqueue(TurnInput::text("nope"))
                .send()
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
        memory_backend().await,
        crate::TurnBudget::Unbounded,
    ))
    .provider(mock_provider())
    .model(mock_model_spec())
    .without_queued_work()
    .build(crate::testing::runtime_lease_owner())?;
    let session = core.session("durable-observation").open().await?;
    let cursor = session.observe().current_observation().cursor;

    // A handle acquired from the core, not from the open session.
    let durable = core.session("durable-observation").durable().await?;
    let pending = durable
        .enqueue(TurnInput::text("queued from a separate handle"))
        .id("separate-handle")
        .send()
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
    let core = explicit_ephemeral_facets(LashCore::standard_builder(
        memory_backend().await,
        crate::TurnBudget::Unbounded,
    ))
    .provider(mock_provider())
    .model(mock_model_spec())
    .without_queued_work()
    .build(crate::testing::runtime_lease_owner())?;
    let session_id = SessionId::from("durable-no-runtime");
    // Create the session, then release every runtime: nothing is live.
    let session = core.session(session_id.clone()).open().await?;
    session.turn(TurnInput::text("commit a turn")).run().await?;
    // A cursor minted while the session was live, at its committed head: the
    // runtime-free publication that follows must be reachable from it.
    let cursor = session.observe().current_observation().cursor;
    Box::pin(session.close()).await?;
    let durable = core.session(session_id.clone()).durable().await?;
    let pending = durable
        .enqueue(TurnInput::text("queued with nothing live"))
        .id("no-runtime")
        .send()
        .await?;

    let reopened = core.session(session_id.clone()).open().await?;
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
        memory_backend().await,
        crate::TurnBudget::Unbounded,
    ))
    .provider(mock_provider())
    .model(mock_model_spec())
    .without_queued_work()
    .build(crate::testing::runtime_lease_owner())?;
    let session_id = SessionId::from("durable-beside-writer");
    let writer = core.session(session_id.clone()).open().await?;

    let recorded_parent_before = writer.parent_session_id().map(ToString::to_string);
    let first = core.session(session_id.clone()).durable().await?;
    let second = core.session(session_id.clone()).durable().await?;

    let a = first
        .enqueue(TurnInput::text("from the first durable handle"))
        .id("beside-a")
        .send()
        .await?;
    let b = second
        .enqueue(TurnInput::text("from the second durable handle"))
        .id("beside-b")
        .send()
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
    writer
        .turn(TurnInput::text("writer keeps its lease"))
        .run()
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

#[tokio::test]
async fn abandoning_a_claim_a_caller_does_not_hold_moves_nothing() -> Result<()> {
    let core = explicit_ephemeral_facets(LashCore::standard_builder(
        memory_backend().await,
        crate::TurnBudget::Unbounded,
    ))
    .provider(mock_provider())
    .model(mock_model_spec())
    .without_queued_work()
    .build(crate::testing::runtime_lease_owner())?;
    let session_id = SessionId::from("durable-claim-token");
    let session = core.session(session_id.clone()).open().await?;
    let durable = session.durable();
    let accepted = durable
        .enqueue(TurnInput::text("claimed input"))
        .id("claim-token")
        .send()
        .await?;

    let input = durable
        .pending_turn_inputs()
        .await?
        .into_iter()
        .find(|read| read.input.input_id == accepted.input_id)
        .expect("the enqueued input is pending");
    // Release is token-authorised: the store matches `claim_id` *and*
    // `lease_token`, so a claim this caller never held moves nothing. (The
    // backends' matching itself is pinned by the store conformance suite;
    // what this asserts is that the Durable Session hands the claim through
    // unchanged rather than releasing by session id.)
    let forged = lash_core::TurnInputClaim {
        session_id: session_id.clone(),
        claim_id: "forged-claim".to_string(),
        owner: crate::testing::runtime_lease_owner(),
        lease_token: "forged-token".to_string(),
        fencing_token: 1,
        session_lease_generation: 1,
        data: lash_core::runtime::TurnInputClaimData {
            mode: lash_core::runtime::TurnInputClaimMode::NextTurn,
            inputs: vec![input.input.clone()],
            applications: Vec::new(),
        },
    };
    durable
        .abandon_turn_input_claim(&forged)
        .await
        .expect("a non-holder's release is a no-op, not a queue mutation");
    let after = durable.pending_turn_inputs().await?;
    assert_eq!(after.len(), 1, "the queue is untouched by a forged release");
    assert_eq!(
        after[0].input.input_id, accepted.input_id,
        "the pending input is the one that was enqueued"
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
    async fn admit_pending_processes(
        &self,
        _reason: &str,
    ) -> std::result::Result<
        lash_core::facade_support::ProcessAdmissionReport,
        lash_core::PluginError,
    > {
        self.counters
            .process_admissions
            .fetch_add(1, Ordering::SeqCst);
        Ok(lash_core::facade_support::ProcessAdmissionReport::default())
    }

    async fn await_process_terminal(
        &self,
        process_ref: &lash_core::ProcessRef,
    ) -> std::result::Result<lash_core::ProcessTerminalWait, lash_core::PluginError> {
        panic!("unexpected terminal wait for {process_ref}")
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
    let backend = memory_backend().await;
    let factory: Arc<dyn SessionStoreFactory> = backend.session_store_factory();

    // A core that carries the session's tool source, to persist tool state.
    let granting_core = explicit_ephemeral_facets(LashCore::standard_builder(
        backend.clone(),
        crate::TurnBudget::Unbounded,
    ))
    .provider(mock_provider())
    .model(mock_model_spec())
    .tools(Arc::new(AppTools))
    .without_queued_work()
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
        .turn(TurnInput::text("persist a checkpoint with tool state"))
        .run()
        .await?;
    let queued = granted
        .durable()
        .enqueue(TurnInput::text("left pending for the grantless core"))
        .id("fig-3353-pending")
        .send()
        .await?;
    Box::pin(granted.close()).await?;

    let tool_state_before = persisted_tool_state_bytes(factory.as_ref(), &session_id).await?;

    // The grantless core: same store, no tool source, fully instrumented.
    let counters = Arc::new(RuntimeBuildCounters::default());
    let grantless_core = explicit_ephemeral_facets(LashCore::standard_builder(
        Arc::new(DecoratedBackend::over(backend.clone()).process_work({
            let counters = Arc::clone(&counters);
            move |registry| {
                lash_core::ProcessWorkWiring::new(
                    lash_core::facade_support::watch_process_registry(registry),
                    Arc::new(CountingProcessWork { counters }),
                )
            }
        })),
        crate::TurnBudget::Unbounded,
    ))
    .provider(mock_provider())
    .model(mock_model_spec())
    .plugin(Arc::new(RuntimeBuildProbeFactory {
        counters: Arc::clone(&counters),
    }))
    .without_queued_work()
    .build(crate::testing::runtime_lease_owner())?;

    // Precondition: on this core, `open()` really does orphan the tool. A
    // negative test whose premise does not hold proves nothing.
    {
        let opened = grantless_core.session(session_id.clone()).open().await?;
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

    // The FIG-3353 poll, through the Durable Session.
    let durable = grantless_core.session(session_id.clone()).durable().await?;
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
        after: lash_core::store::TurnParkFeedCursor,
        limit: std::num::NonZeroUsize,
    ) -> std::result::Result<lash_core::store::TurnParkFeedPage, lash_core::StoreError> {
        self.inner.turn_park_feed(after, limit).await
    }

    async fn root_terminal(
        &self,
        session_id: &lash_core::SessionId,
        root: &lash_core::TurnId,
    ) -> std::result::Result<Option<lash_core::store::RootTerminal>, lash_core::StoreError> {
        self.inner.root_terminal(session_id, root).await
    }

    async fn list_open_control_intents(
        &self,
        after: Option<lash_core::store::ControlIntentId>,
        limit: std::num::NonZeroUsize,
    ) -> std::result::Result<Vec<lash_core::store::ControlIntent>, lash_core::StoreError> {
        self.inner.list_open_control_intents(after, limit).await
    }

    async fn compact_turn_park_feed(
        &self,
        through: lash_core::store::TurnParkFeedCursor,
    ) -> std::result::Result<(), lash_core::StoreError> {
        self.inner.compact_turn_park_feed(through).await
    }
}

/// A catalog without the by-id seam must not make an existing session look
/// absent. Before the seam was required, its inherited `Ok(None)` did exactly
/// that: every durable operation on a live session reported `UnknownSession`
/// and sent the host hunting for a session that was there.
#[tokio::test]
async fn a_catalog_without_the_by_id_seam_names_the_capability_not_a_missing_session() -> Result<()>
{
    let backend = DecoratedBackend::over(memory_backend().await)
        .session_store_factory(|inner| Arc::new(NoByIdLookupFactory { inner }));
    let core = explicit_ephemeral_facets(LashCore::standard_builder(
        Arc::new(backend),
        crate::TurnBudget::Unbounded,
    ))
    .provider(mock_provider())
    .model(mock_model_spec())
    .without_queued_work()
    .build(crate::testing::runtime_lease_owner())?;

    // The session genuinely exists: this open created it.
    crate::tests::create_catalog_session(&core, "no-by-id-seam").await?;

    let durable = core.session("no-by-id-seam").durable().await?;
    let error = durable
        .enqueue(TurnInput::text(
            "queued through a catalog with no by-id seam",
        ))
        .id("no-by-id-seam-input")
        .send()
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
        memory_backend().await,
        crate::TurnBudget::Unbounded,
    ))
    .provider(provider)
    .model(mock_model_spec())
    .without_queued_work()
    .build(crate::testing::runtime_lease_owner())?;
    let session_id = SessionId::from("durable-held-input");
    let session = core.session(session_id.clone()).open().await?;
    let accepted = session
        .durable()
        .enqueue(TurnInput::text("claimed by the drain"))
        .id("held-input")
        .send()
        .await?;

    let observer = core.session(session_id.clone()).durable().await?;
    assert!(
        matches!(
            observer
                .pending_turn_inputs()
                .await?
                .first()
                .map(|read| &read.status),
            Some(lash_core::runtime::PendingTurnInputReadStatus::Pending)
        ),
        "before the drain claims it, the row reads as pending"
    );

    let drain = tokio::spawn({
        let session = session.clone();
        async move { session.queued_turn().drain_id("held-drain").run().await }
    });
    tokio::time::timeout(std::time::Duration::from_secs(5), entered.notified())
        .await
        .expect("the drain reaches the provider with the input claimed");

    let held = observer.pending_turn_inputs().await?;
    let row = held
        .iter()
        .find(|read| read.input.input_id == accepted.input_id)
        .expect("a held input is still reported by the Durable Session, not hidden");
    match row.status {
        lash_core::runtime::PendingTurnInputReadStatus::Held {
            lease_expires_at_ms,
        } => assert!(
            lease_expires_at_ms > 0,
            "a held row carries the matching live lease's expiry"
        ),
        ref other => panic!("the claimed input must read as held, got {other:?}"),
    }

    release.add_permits(1);
    drain
        .await
        .expect("drain task")?
        .expect("the queued turn runs");
    Ok(())
}

/// `create()` is the one verb that creates, and it creates nothing else: no
/// runtime, no lease, no lifecycle event.
#[tokio::test]
async fn create_admits_an_absent_id_and_builds_no_runtime() -> Result<()> {
    let counters = Arc::new(RuntimeBuildCounters::default());
    let core = explicit_ephemeral_facets(LashCore::standard_builder(
        Arc::new(
            DecoratedBackend::over(memory_backend().await).process_work({
                let counters = Arc::clone(&counters);
                move |registry| {
                    lash_core::ProcessWorkWiring::new(
                        lash_core::facade_support::watch_process_registry(registry),
                        Arc::new(CountingProcessWork { counters }),
                    )
                }
            }),
        ),
        crate::TurnBudget::Unbounded,
    ))
    .provider(mock_provider())
    .model(mock_model_spec())
    .plugin(Arc::new(RuntimeBuildProbeFactory {
        counters: Arc::clone(&counters),
    }))
    .without_queued_work()
    .build(crate::testing::runtime_lease_owner())?;

    // Building the core itself materialises its plugin host once; that is the
    // baseline `create()` must not move.
    let baseline = counters.snapshot();

    // Enqueue to an id nobody created is refused...
    assert!(matches!(
        core.session("created-then-queued")
            .durable()
            .await?
            .enqueue(TurnInput::text("too early"))
            .send()
            .await
            .expect_err("an uncreated id is refused"),
        EmbedError::UnknownSession { .. }
    ));

    // ...and `create()` is the explicit two-step's first half.
    let durable = core.session("created-then-queued").create().await?;
    let accepted = durable
        .enqueue(TurnInput::text("queued before the first turn"))
        .id("created-then-queued-input")
        .send()
        .await?;
    assert_eq!(durable.pending_turn_inputs().await?.len(), 1);
    assert!(durable.exists().await?);

    assert_eq!(
        counters.snapshot(),
        baseline,
        "create() materialises no plugin session, restores nothing, admits no process"
    );

    // The session a host creates this way is an ordinary session: opening it
    // runs the input that was waiting.
    let session = core.session("created-then-queued").open().await?;
    let drained = session
        .queued_turn()
        .run()
        .await?
        .expect("the pending input becomes a turn");
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
    let core = explicit_ephemeral_facets(LashCore::standard_builder(
        memory_backend().await,
        crate::TurnBudget::Unbounded,
    ))
    .provider(mock_provider())
    .model(mock_model_spec())
    .without_queued_work()
    .build(crate::testing::runtime_lease_owner())?;
    drop(core.session("create-parent").create().await?);

    let first = core
        .session("create-idempotent")
        .parent("create-parent")
        .create()
        .await?;
    let accepted = first
        .enqueue(TurnInput::text("survives the second create"))
        .id("idempotent-input")
        .send()
        .await?;

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
    let reopened = core.session("create-idempotent").open().await?;
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
    let backend = memory_backend().await;
    let factory = backend.session_store_factory();
    let core = explicit_ephemeral_facets(LashCore::standard_builder(
        backend.clone(),
        crate::TurnBudget::Unbounded,
    ))
    .provider(mock_provider())
    .model(mock_model_spec())
    .without_queued_work()
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
    let core = explicit_ephemeral_facets(LashCore::standard_builder(
        memory_backend().await,
        crate::TurnBudget::Unbounded,
    ))
    .provider(mock_provider())
    .model(mock_model_spec())
    .without_queued_work()
    .build(crate::testing::runtime_lease_owner())?;
    crate::tests::create_catalog_session(&core, "fig3544-enqueue-conflict").await?;
    let durable = core.session("fig3544-enqueue-conflict").durable().await?;

    let first = durable
        .enqueue(TurnInput::text("original"))
        .id("retry-me")
        .send()
        .await?;
    let replay = durable
        .enqueue(TurnInput::text("original"))
        .id("retry-me")
        .send()
        .await?;
    assert_eq!(replay, first, "an identical retry replays the acceptance");

    let conflict = durable
        .enqueue(TurnInput::text("changed"))
        .id("retry-me")
        .send()
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
