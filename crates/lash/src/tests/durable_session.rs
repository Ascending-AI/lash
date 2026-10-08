//! Durable Session: acquisition, refusals, observation, and coexistence with a
//! running session (FIG-3366, ADR 0119), on the durable substrate over SQLite
//! memory stores (FIG-5307).
//!
//! An input the double's held shift kept pending stays pending here because
//! the core that sends it serves no session actor: nothing claims the row
//! until a serving core over the same stores runs it.

use super::*;
use lash_core::SessionId;

/// Catalog wrapper that counts which seam a caller reached for, and fails
/// the by-id seam once a law arms it.
///
/// A Durable Session must resolve through the non-creating by-id seam and
/// must never reach `admit_session`; these counters are what makes that a
/// test rather than a claim.
struct CountingDeploymentStore {
    inner: Arc<dyn lash_core::DeploymentStore>,
    /// Every call of the creating seam, whatever it answered.
    admissions: AtomicUsize,
    creates: AtomicUsize,
    by_id_opens: AtomicUsize,
    /// The refusal every by-id lookup answers while armed.
    failing: StdMutex<Option<CatalogFailure>>,
}

impl CountingDeploymentStore {
    fn new(inner: Arc<dyn lash_core::DeploymentStore>) -> Self {
        Self {
            inner,
            admissions: AtomicUsize::new(0),
            creates: AtomicUsize::new(0),
            by_id_opens: AtomicUsize::new(0),
            failing: StdMutex::new(None),
        }
    }

    fn reset(&self) {
        self.admissions.store(0, Ordering::SeqCst);
        self.by_id_opens.store(0, Ordering::SeqCst);
    }
}

#[async_trait]
impl lash_core::store::RuntimeStoreDecorator for CountingDeploymentStore {
    type Inner = dyn lash_core::DeploymentStore;

    fn inner(&self) -> &Self::Inner {
        self.inner.as_ref()
    }

    async fn admit_session(
        &self,
        request: &lash_core::SessionStoreCreateRequest,
    ) -> std::result::Result<lash_core::store::SessionAdmission, StoreError> {
        self.admissions.fetch_add(1, Ordering::SeqCst);
        let admission = self.inner.admit_session(request).await?;
        if admission == lash_core::store::SessionAdmission::Created {
            self.creates.fetch_add(1, Ordering::SeqCst);
        }
        Ok(admission)
    }

    async fn lookup_session(
        &self,
        session_id: &SessionId,
    ) -> std::result::Result<lash_core::store::SessionLookup, StoreError> {
        self.by_id_opens.fetch_add(1, Ordering::SeqCst);
        if let Some(failure) = *self.failing.lock_recover() {
            return Err(failure.error());
        }
        self.inner.lookup_session(session_id).await
    }
}

impl lash_core::DeploymentStoreDecorator for CountingDeploymentStore {}

/// A SQLite memory backend whose catalog counts, and its counter.
async fn counting_backend() -> (lash_core::Backend, Arc<CountingDeploymentStore>) {
    let inner = sqlite_memory_store_backend().await;
    let counting = Arc::new(CountingDeploymentStore::new(inner.session_store_factory()));
    let catalog = Arc::clone(&counting);
    let backend = DecoratedBackend::over(inner)
        .session_store_factory(move |_| catalog)
        .into_backend();
    (backend, counting)
}

/// A core over `backend` that serves its sessions when `serve` says so.
fn core_serving(backend: lash_core::Backend, serve: bool) -> LashCore {
    explicit_ephemeral_facets(LashCore::standard_builder(backend))
        .serve_test_llm_profile(mock_provider(), mock_llm_profile_spec())
        .serve_sessions(serve)
        .build(crate::testing::runtime_lease_owner())
        .expect("standard core")
}

fn id(text: &str) -> crate::SessionId {
    crate::SessionId::parse(text).expect("nonblank host identity")
}

fn turn(text: &str) -> crate::TurnId {
    crate::TurnId::parse(text).expect("nonblank host identity")
}

async fn create(core: &LashCore, session: &str) -> crate::DurableSession {
    core.session(id(session))
        .create(crate::SessionCreation::root(
            crate::plugins::SessionToolAccess::ambient(),
            mock_session_spec(),
        ))
        .await
        .expect("create the session")
}

#[derive(Clone, Copy)]
enum CatalogFailure {
    Contended,
    Unsupported,
}

impl CatalogFailure {
    fn error(self) -> StoreError {
        match self {
            Self::Contended => StoreError::Contended,
            Self::Unsupported => StoreError::UnsupportedStoreOperation {
                operation: "lookup_session",
            },
        }
    }

    fn is_preserved(self, error: &EmbedError) -> bool {
        let typed = match (self, error) {
            (Self::Contended, EmbedError::Store(StoreError::Contended)) => true,
            (
                Self::Unsupported,
                EmbedError::Store(StoreError::UnsupportedStoreOperation { operation }),
            ) => *operation == "lookup_session",
            _ => false,
        };
        typed && error.is_retryable() == matches!(self, Self::Contended)
    }
}

async fn catalog_failure_matrix(failure: CatalogFailure) {
    let (backend, counts) = counting_backend().await;
    let core = core_serving(backend, false);
    create(&core, "catalog-failure").await;
    let durable = core
        .session(id("catalog-failure"))
        .durable()
        .await
        .expect("a durable handle resolves lazily");
    counts.reset();
    *counts.failing.lock_recover() = Some(failure);
    let results = [
        (
            "live open",
            core.session(id("catalog-failure")).open().await.map(drop),
        ),
        (
            "durable acquisition",
            durable.pending_turn_inputs().await.map(drop),
        ),
        ("tombstone read", durable.was_deleted().await.map(drop)),
        (
            "process session scope",
            core.processes()
                .session_scope(&SessionId::from("catalog-failure"))
                .await
                .map(drop),
        ),
    ];
    let failures = results
        .into_iter()
        .filter_map(|(api, result)| match result {
            Err(error) if failure.is_preserved(&error) => None,
            other => Some(format!("{api}: {other:?}")),
        })
        .collect::<Vec<_>>();
    assert_eq!(counts.admissions.load(Ordering::SeqCst), 0);
    assert!(
        counts.by_id_opens.load(Ordering::SeqCst) >= 4,
        "every existing-session API resolved through the by-id seam"
    );
    assert!(
        failures.is_empty(),
        "catalog failures lost their typed carrier or retryability: {failures:?}"
    );
    core.shutdown().await.expect("shutdown");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn catalog_contention_retains_its_type_and_retryability_across_existing_session_apis() {
    catalog_failure_matrix(CatalogFailure::Contended).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unsupported_catalog_lookup_retains_its_operation_across_existing_session_apis() {
    catalog_failure_matrix(CatalogFailure::Unsupported).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn existing_session_apis_preserve_absence_and_tombstones_without_creating() {
    let (backend, counts) = counting_backend().await;
    let core = core_serving(backend, false);
    create(&core, "catalog-deleted").await;
    lash_core::SessionCatalogStore::delete_session(
        counts.as_ref(),
        &SessionId::from("catalog-deleted"),
    )
    .await
    .expect("delete the catalog session");
    counts.reset();
    for (name, deleted) in [("catalog-absent", false), ("catalog-deleted", true)] {
        let session_id = SessionId::from(name);
        let open_error = core
            .session(id(name))
            .open()
            .await
            .err()
            .expect("open refuses the id");
        let durable = core.session(id(name)).durable().await.expect("handle");
        let acquisition_error = durable
            .pending_turn_inputs()
            .await
            .expect_err("acquisition refuses the id");
        for error in [open_error, acquisition_error] {
            if deleted {
                assert!(
                    matches!(&error, EmbedError::Store(StoreError::SessionDeleted { session_id: found }) if *found == session_id),
                    "{error:?}"
                );
            } else {
                assert!(
                    matches!(&error, EmbedError::UnknownSession { session_id: found } if *found == session_id),
                    "{error:?}"
                );
            }
        }
        assert_eq!(
            durable.was_deleted().await.expect("tombstone read"),
            deleted
        );
        assert!(!durable.exists().await.expect("existence read"));
        assert!(durable.read().await.expect("read").is_none());
        assert!(
            matches!(core.processes().session_scope(&session_id).await, Err(EmbedError::UnknownSession { session_id: found }) if found == session_id)
        );
    }
    assert_eq!(counts.admissions.load(Ordering::SeqCst), 0);
    core.shutdown().await.expect("shutdown");
}

/// A contended lookup fails the one acquisition that met it, typed and
/// retryable; the handle and its clones acquire on the next call, and a
/// session handle that create bound reads without a lookup.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn durable_acquisition_retries_contention_once_for_clones_and_reuses_bound_stores() {
    let (backend, counts) = counting_backend().await;
    let core = core_serving(backend, false);
    let bound = create(&core, "retry-acquisition").await;
    let durable = core
        .session(id("retry-acquisition"))
        .durable()
        .await
        .expect("handle");
    counts.reset();
    *counts.failing.lock_recover() = Some(CatalogFailure::Contended);
    let error = durable
        .pending_turn_inputs()
        .await
        .expect_err("the contended lookup fails the acquisition");
    assert!(CatalogFailure::Contended.is_preserved(&error), "{error:?}");
    *counts.failing.lock_recover() = None;
    let failed_lookups = counts.by_id_opens.load(Ordering::SeqCst);
    bound.pending_turn_inputs().await.expect("bound read");
    let mut tasks = Vec::new();
    for _ in 0..5 {
        let durable = durable.clone();
        tasks.push(tokio::spawn(
            async move { durable.pending_turn_inputs().await },
        ));
    }
    for task in tasks {
        task.await.expect("clone lookup task").expect("clone read");
    }
    durable.queued_work().await.expect("queued work read");
    assert!(
        failed_lookups >= 1,
        "the contended acquisition reached the by-id seam"
    );
    assert_eq!(counts.admissions.load(Ordering::SeqCst), 0);
    core.shutdown().await.expect("shutdown");
}

/// FIG-4112: every verb but `create` resolves an existing session. Opening an
/// id the catalog has never created — live, with a supplied state, or as an
/// observer — is `UnknownSession`, and no catalog row is written.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn open_of_a_missing_id_is_unknown_session_and_writes_no_row() {
    let (backend, counts) = counting_backend().await;
    let core = core_serving(backend, true);
    let missing = SessionId::from("never-opened");
    let is_unknown = |result: Result<crate::LashSession>| match result {
        Err(EmbedError::UnknownSession { session_id }) => session_id == missing,
        _ => false,
    };
    assert!(is_unknown(core.session(id("never-opened")).open().await));
    let state = || {
        let mut state = lash_core::RuntimeSessionState::new(lash_core::SessionPolicy::new(
            crate::TurnBudget::Unbounded,
            crate::MaxToolCalls::new(1024),
            crate::NoProgressBudget::bounded(12),
        ));
        state.session_id = missing.clone();
        state
    };
    assert!(is_unknown(
        core.session(id("never-opened"))
            .open_with_state(state())
            .await
    ));
    assert!(is_unknown(
        core.session(id("never-opened"))
            .observe_with_state(state())
            .await
    ));
    assert_eq!(
        counts.admissions.load(Ordering::SeqCst),
        0,
        "no verb but create reaches the creating seam"
    );
    assert!(
        matches!(
            lash_core::SessionCatalogStore::lookup_session(counts.as_ref(), &missing)
                .await
                .expect("probe the catalog"),
            lash_core::store::SessionLookup::Absent
        ),
        "the refused opens left no session behind"
    );
    core.shutdown().await.expect("shutdown");
}

/// FIG-4112: two creates of one id racing each other produce exactly one
/// `Ok` and one `SessionAlreadyExists`: the store's insert decides, and the
/// loser never adopts the winner's session.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_creates_of_one_id_give_exactly_one_ok() {
    let (backend, counts) = counting_backend().await;
    let core = core_serving(backend, true);
    let barrier = Arc::new(tokio::sync::Barrier::new(2));
    let racers = (0..2)
        .map(|_| {
            let core = core.clone();
            let barrier = Arc::clone(&barrier);
            tokio::spawn(async move {
                barrier.wait().await;
                core.session(id("raced-create"))
                    .create(crate::SessionCreation::root(
                        crate::plugins::SessionToolAccess::ambient(),
                        mock_session_spec(),
                    ))
                    .await
                    .map(drop)
            })
        })
        .collect::<Vec<_>>();
    let (mut created, mut refused) = (0, 0);
    for racer in racers {
        match racer.await.expect("create task") {
            Ok(()) => created += 1,
            Err(EmbedError::SessionAlreadyExists { session_id })
                if session_id.as_str() == "raced-create" =>
            {
                refused += 1;
            }
            Err(error) => panic!("a racing create failed otherwise: {error:?}"),
        }
    }
    assert_eq!((created, refused), (1, 1));
    assert_eq!(
        counts.creates.load(Ordering::SeqCst),
        1,
        "the store created once"
    );
    drop(
        core.session(id("raced-create"))
            .open()
            .await
            .expect("the winner's session opens"),
    );
    core.shutdown().await.expect("shutdown");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn durable_enqueue_to_an_unknown_id_stores_nothing_and_creates_nothing() {
    let (backend, counts) = counting_backend().await;
    let core = core_serving(backend, true);
    let durable = core
        .session(id("never-created"))
        .durable()
        .await
        .expect("handle");
    let error = durable
        .send(crate::TurnInput::text(
            "queued to a session that does not exist",
        ))
        .id(turn("orphan-enqueue"))
        .await
        .err()
        .expect("enqueue to an unknown id is refused");
    assert!(
        matches!(&error, EmbedError::UnknownSession { session_id } if session_id.as_str() == "never-created"),
        "unknown ids get the typed not-found refusal, got {error:?}"
    );
    assert_eq!(
        counts.creates.load(Ordering::SeqCst),
        0,
        "a refused enqueue must not materialise session metadata"
    );
    assert!(
        matches!(
            lash_core::SessionCatalogStore::lookup_session(
                counts.as_ref(),
                &SessionId::from("never-created"),
            )
            .await
            .expect("probe the catalog"),
            lash_core::store::SessionLookup::Absent
        ),
        "the refused enqueue left no session behind"
    );
    assert!(!durable.exists().await.expect("existence"));
    assert!(!durable.was_deleted().await.expect("tombstone"));
    assert!(durable.read().await.expect("read").is_none());
    core.shutdown().await.expect("shutdown");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn durable_operations_on_a_deleted_id_report_the_tombstone() {
    let (backend, counts) = counting_backend().await;
    let core = core_serving(backend, true);
    create(&core, "deleted-durable").await;
    lash_core::SessionCatalogStore::delete_session(
        counts.as_ref(),
        &SessionId::from("deleted-durable"),
    )
    .await
    .expect("delete the session");
    let durable = core
        .session(id("deleted-durable"))
        .durable()
        .await
        .expect("handle");
    let error = durable
        .send(crate::TurnInput::text("queued after deletion"))
        .await
        .err()
        .expect("enqueue to a deleted id is refused");
    assert!(
        matches!(
            &error,
            EmbedError::Store(StoreError::SessionDeleted { session_id })
                if session_id.as_str() == "deleted-durable"
        ),
        "deleted ids get the store's typed deletion error, got {error:?}"
    );
    assert!(durable.was_deleted().await.expect("tombstone"));
    assert!(!durable.exists().await.expect("existence"));
    core.shutdown().await.expect("shutdown");
}

/// ADR 0101 §5.1 through the facade's ingress: an input addressed to a turn
/// the session never ran is refused as an unknown turn address before
/// anything is stored, and the session's queue stays empty.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_input_addressed_to_an_unknown_turn_is_refused_through_the_facade() {
    let core = core_serving(sqlite_memory_store_backend().await, false);
    let durable = create(&core, "unknown-turn-address").await;
    let error = durable
        .send(crate::TurnInput::text("steer a turn that never ran"))
        .ingress(lash_core::TurnInputIngress::active_turn(
            lash_core::TurnId::from("unknown-turn-address-never-ran"),
            lash_core::TurnInputCheckpointBoundary::AfterWork,
        ))
        .await
        .err()
        .expect("an unknown turn address is refused");
    assert!(
        matches!(
            &error,
            EmbedError::Store(StoreError::IngressTurnAddressUnknown { session_id, turn_id })
                if session_id.as_str() == "unknown-turn-address"
                    && turn_id.as_str() == "unknown-turn-address-never-ran"
        ) || matches!(
            &error,
            EmbedError::Runtime(error)
                if error.code == lash_core::RuntimeErrorCode::TurnAddressUnknown
        ),
        "an unknown turn address gets its typed refusal, got {error:?}"
    );
    assert!(
        durable
            .pending_turn_inputs()
            .await
            .expect("queue read")
            .is_empty(),
        "a refused address stores no row"
    );
    core.shutdown().await.expect("shutdown");
}

/// A durable handle acquires an absent id (refused, nothing created), a
/// session that has only its creation metadata, and one with a committed
/// turn; a deleted id reports its tombstone.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sqlite_durable_acquisition_covers_absent_metadata_only_and_checkpointed_ids() {
    let backend = sqlite_memory_store_backend().await;
    let serving = core_serving(backend.clone(), true);
    let client = core_serving(backend.clone(), false);

    let absent = client
        .session(id("sqlite-absent"))
        .durable()
        .await
        .expect("handle");
    assert!(!absent.exists().await.expect("existence"));
    assert!(
        matches!(
            absent
                .send(crate::TurnInput::text("nope"))
                .await
                .err()
                .expect("absent sqlite id is refused"),
            EmbedError::UnknownSession { .. }
        ),
        "an absent sqlite id is refused without creating a store"
    );

    create(&client, "sqlite-metadata-only").await;
    let metadata_only = client
        .session(id("sqlite-metadata-only"))
        .durable()
        .await
        .expect("handle");
    assert!(metadata_only.exists().await.expect("existence"));
    drop(
        metadata_only
            .send(crate::TurnInput::text("queued on sqlite metadata"))
            .id(turn("sqlite-metadata-input"))
            .await
            .expect("accepted"),
    );
    assert_eq!(
        metadata_only
            .pending_turn_inputs()
            .await
            .expect("queue")
            .len(),
        1
    );

    let committed = create(&serving, "sqlite-checkpointed").await;
    committed
        .send(crate::TurnInput::text("commit a turn"))
        .output()
        .await
        .expect("the turn commits");
    let checkpointed = client
        .session(id("sqlite-checkpointed"))
        .durable()
        .await
        .expect("handle");
    assert!(checkpointed.exists().await.expect("existence"));
    assert_eq!(
        checkpointed
            .committed_turns(None, std::num::NonZeroU32::MIN)
            .await
            .expect("committed turns")
            .turns
            .len(),
        1,
        "the checkpointed id reads its committed turn"
    );

    lash_core::SessionCatalogStore::delete_session(
        backend.session_store_factory().as_ref(),
        &SessionId::from("sqlite-metadata-only"),
    )
    .await
    .expect("delete the sqlite session");
    let deleted = client
        .session(id("sqlite-metadata-only"))
        .durable()
        .await
        .expect("handle");
    assert!(deleted.was_deleted().await.expect("tombstone"));
    assert!(
        matches!(
            deleted
                .send(crate::TurnInput::text("nope"))
                .await
                .err()
                .expect("deleted sqlite id is refused"),
            EmbedError::Store(StoreError::SessionDeleted { .. })
        ),
        "a deleted sqlite id reports the tombstone"
    );
    serving.shutdown().await.expect("shutdown");
    client.shutdown().await.expect("shutdown");
}

/// FIG-4829: a send's trace context is linked to the input by the
/// acceptance that first stores it. A retry of the same `id` under another
/// context is the same submission and keeps the first link.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_send_links_its_input_to_the_context_its_first_acceptance_carried() {
    let core = core_serving(sqlite_memory_store_backend().await, false);
    let durable = create(&core, "traced-send").await;
    let context = |producer: u8| {
        lash_core::TraceCarrier::parse_w3c(&format!("00-{producer:032x}-{producer:016x}-01"), None)
            .expect("a valid trace context")
    };
    let send = |producer: u8| {
        durable
            .send(crate::TurnInput::text("the same words"))
            .id(turn("traced-send-input"))
            .trace_context(context(producer))
    };
    let first = send(1).await.expect("first acceptance");
    let retried = send(2).await.expect("retried acceptance");
    assert_eq!(retried.input_id(), first.input_id());

    // With no telemetry adapter there is no ambient context to capture.
    drop(
        durable
            .send(crate::TurnInput::text("words nobody traced"))
            .id(turn("untraced-send-input"))
            .await
            .expect("untraced acceptance"),
    );

    let causes = durable
        .pending_turn_inputs()
        .await
        .expect("queue")
        .into_iter()
        .map(|read| {
            (
                read.input.source_key.clone(),
                read.input.trace_cause.clone(),
            )
        })
        .collect::<Vec<_>>();
    assert_eq!(
        causes,
        vec![
            (
                Some("traced-send-input".to_string()),
                lash_core::TraceCause::linked_to(Some(context(1))),
            ),
            (
                Some("untraced-send-input".to_string()),
                lash_core::TraceCause::Root,
            ),
        ]
    );
    core.shutdown().await.expect("shutdown");
}

/// An observer subscribed through one handle sees the queue events a
/// separately acquired durable handle's enqueue and withdrawal publish.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_live_observer_sees_queue_events_from_a_separately_acquired_durable_session() {
    let backend = sqlite_memory_store_backend().await;
    let core = core_serving(backend, false);
    create(&core, "durable-observation").await;
    let session = core
        .session(id("durable-observation"))
        .open()
        .await
        .expect("open");
    let cursor = session
        .observe()
        .snapshot()
        .await
        .expect("durable snapshot")
        .cursor;

    // A handle acquired from the core, not from the open session.
    let durable = core
        .session(id("durable-observation"))
        .durable()
        .await
        .expect("handle");
    let pending = durable
        .send(crate::TurnInput::text("queued from a separate handle"))
        .id(turn("separate-handle"))
        .await
        .expect("accepted");
    let cancelled = durable
        .cancel(crate::CancelTarget::Run(turn("separate-handle")))
        .await
        .expect("cancel");
    assert!(
        matches!(cancelled, crate::CancelReceipt::Withdrawn { .. }),
        "{cancelled:?}"
    );

    let crate::support::SessionResume::Replayed { events } = session
        .observe()
        .resume_from_cursor(&cursor)
        .await
        .expect("resume")
    else {
        panic!("the already-subscribed observer must replay the queue events");
    };
    let queue_event = |wanted: lash_core::SessionQueueEventKind| {
        events.iter().any(|event| {
            matches!(
                &event.payload,
                lash_core::SessionObservationEventPayload::QueueChanged { kind, batch_ids }
                    if *kind == wanted
                        && batch_ids.as_slice() == std::slice::from_ref(pending.input_id())
            )
        })
    };
    assert!(
        queue_event(lash_core::SessionQueueEventKind::Enqueued),
        "the live observer receives Enqueued from the separately acquired handle: {events:?}"
    );
    assert!(
        queue_event(lash_core::SessionQueueEventKind::Cancelled),
        "the live observer receives Cancelled from the separately acquired handle: {events:?}"
    );
    core.shutdown().await.expect("shutdown");
}

/// Two durable handles beside an open session address one queue; once a
/// node serves the session, every durably accepted input runs, and the
/// session keeps the relation it was created with.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn two_durable_handles_operate_beside_an_independently_leased_writer() {
    let backend = sqlite_memory_store_backend().await;
    let client = core_serving(backend.clone(), false);
    create(&client, "durable-beside-writer").await;
    let writer = client
        .session(id("durable-beside-writer"))
        .open()
        .await
        .expect("open");
    let recorded_parent_before = writer.parent_session_id().map(ToString::to_string);
    let first = client
        .session(id("durable-beside-writer"))
        .durable()
        .await
        .expect("handle");
    let second = client
        .session(id("durable-beside-writer"))
        .durable()
        .await
        .expect("handle");
    let a = first
        .send(crate::TurnInput::text("from the first durable handle"))
        .id(turn("beside-a"))
        .await
        .expect("accepted");
    let b = second
        .send(crate::TurnInput::text("from the second durable handle"))
        .id(turn("beside-b"))
        .await
        .expect("accepted");
    let pending_before_turn = second
        .pending_turn_inputs()
        .await
        .expect("queue")
        .iter()
        .map(|read| read.input.input_id.clone())
        .collect::<Vec<_>>();
    assert_eq!(
        pending_before_turn,
        vec![a.input_id().clone(), b.input_id().clone()],
        "both handles wrote into the same durable queue beside the open session"
    );

    let serving = core_serving(backend, true);
    let writes = writer
        .send(crate::TurnInput::text("writer keeps committing"))
        .output()
        .await
        .expect("the writer's turn commits");
    assert!(writes.is_success(), "{writes:?}");
    for handle in [a, b] {
        assert!(
            handle
                .output()
                .await
                .expect("each accepted input runs")
                .is_success(),
            "every durably accepted input settles"
        );
    }
    assert!(first.exists().await.expect("existence"));
    assert!(
        second
            .pending_turn_inputs()
            .await
            .expect("queue")
            .is_empty()
    );
    assert_eq!(
        writer.parent_session_id().map(ToString::to_string),
        recorded_parent_before,
        "durable access beside a writer leaves the recorded Session Relation alone"
    );
    let settled = first
        .read()
        .await
        .expect("read")
        .expect("the committed session reads back through the catalog");
    assert_eq!(settled.session_id(), "durable-beside-writer");
    assert!(
        settled
            .durable_relation()
            .is_none_or(|relation| matches!(relation, lash_core::SessionRelation::Root)),
        "the session is still recorded as the root it was admitted as"
    );
    serving.shutdown().await.expect("shutdown");
    client.shutdown().await.expect("shutdown");
}

/// Counters for everything a runtime build does that durable access must
/// not.
#[derive(Default)]
struct RuntimeBuildCounters {
    plugin_materializations: AtomicUsize,
    session_restored_events: AtomicUsize,
}

impl RuntimeBuildCounters {
    fn snapshot(&self) -> (usize, usize) {
        (
            self.plugin_materializations.load(Ordering::SeqCst),
            self.session_restored_events.load(Ordering::SeqCst),
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

impl lash_core::plugin::PluginDefinition for RuntimeBuildProbeFactory {
    fn declaration() -> lash_core::plugin::PluginDeclaration {
        lash_core::plugin::PluginDeclaration::initial("durable-session-runtime-probe")
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
        reg.session().on_event(
            crate::hook_key!("session-on-event-1"),
            Arc::new(move |event| {
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
            }),
        )?;
        Ok(())
    }
}

/// A probed core over `backend`, serving its sessions when `serve` says so.
fn probed_core(
    backend: lash_core::Backend,
    counters: &Arc<RuntimeBuildCounters>,
    serve: bool,
) -> LashCore {
    explicit_ephemeral_facets(LashCore::standard_builder(backend))
        .serve_test_llm_profile(mock_provider(), mock_llm_profile_spec())
        .plugin(Arc::new(RuntimeBuildProbeFactory {
            counters: Arc::clone(counters),
        }))
        .serve_sessions(serve)
        .build(crate::testing::runtime_lease_owner())
        .expect("probed core")
}

/// Read the session head's committed `tool_state`.
async fn persisted_tool_state(backend: &lash_core::Backend, session_id: &SessionId) -> String {
    let store = lash_core::runtime::live_session_view(&backend.session_store_factory(), session_id)
        .await
        .expect("session view")
        .expect("the session has a store");
    let head = store
        .load_session_head_meta()
        .await
        .expect("load the head")
        .expect("the session has a head");
    format!("{head:?}")
}

/// FIG-3353: a grantless core polls and edits a session's queue without
/// building a runtime, so nothing is orphaned and nothing is restored.
///
/// The law asserts its own precondition — a run on a grantless core *does*
/// orphan the persisted tool — and then counts, on the `durable()` path,
/// every step of a runtime build.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn durable_queue_access_on_a_grantless_core_builds_no_runtime() {
    let session_id = SessionId::from("fig-3353-durable-poll");
    let app_lookup = lash_core::ToolId::from("tool:app_lookup");
    let backend = sqlite_memory_store_backend().await;

    // A core that carries the session's tool source, to persist tool state.
    let granting = explicit_ephemeral_facets(LashCore::standard_builder(backend.clone()))
        .serve_test_llm_profile(mock_provider(), mock_llm_profile_spec())
        .tools(Arc::new(AppTools))
        .build(crate::testing::runtime_lease_owner())
        .expect("granting core");
    let granted = create(&granting, session_id.as_str()).await;
    granted
        .send(crate::TurnInput::text(
            "persist a checkpoint with tool state",
        ))
        .output()
        .await
        .expect("the granted turn commits");
    granting.shutdown().await.expect("shutdown");

    // Precondition: a run on a grantless core really does orphan the tool.
    let run_counters = Arc::new(RuntimeBuildCounters::default());
    let grantless_runner = probed_core(backend.clone(), &run_counters, true);
    let output = grantless_runner
        .session(id(session_id.as_str()))
        .durable()
        .await
        .expect("handle")
        .send(crate::TurnInput::text("run without the tool's source"))
        .output()
        .await
        .expect("the grantless turn commits");
    assert_eq!(
        output
            .tool_restore_report()
            .map(|report| report.lost_members.clone()),
        Some(vec![app_lookup.clone()]),
        "precondition: a run on a core without the tool's source orphans it"
    );
    let (plugins, _) = run_counters.snapshot();
    assert!(
        plugins > 0,
        "precondition: a run materialises plugins ({plugins})"
    );
    grantless_runner.shutdown().await.expect("shutdown");

    // The measured core: grantless, probed, and serving nothing.
    let counters = Arc::new(RuntimeBuildCounters::default());
    let grantless = probed_core(backend.clone(), &counters, false);
    let baseline = counters.snapshot();
    let state_before = persisted_tool_state(&backend, &session_id).await;
    let durable = grantless
        .session(id(session_id.as_str()))
        .durable()
        .await
        .expect("handle");
    let queued = durable
        .send(crate::TurnInput::text(
            "left pending for the grantless core",
        ))
        .id(turn("fig-3353-pending"))
        .await
        .expect("accepted");
    let pending = durable.pending_turn_inputs().await.expect("queue");
    assert_eq!(
        pending
            .iter()
            .map(|read| read.input.input_id.clone())
            .collect::<Vec<_>>(),
        vec![queued.input_id().clone()],
        "the grantless core lists the pending input it never granted tools for"
    );
    let cancelled = durable
        .cancel(crate::CancelTarget::Run(turn("fig-3353-pending")))
        .await
        .expect("cancel");
    assert!(
        matches!(cancelled, crate::CancelReceipt::Withdrawn { .. }),
        "the grantless core withdraws the pending input, got {cancelled:?}"
    );
    assert!(
        durable
            .pending_turn_inputs()
            .await
            .expect("queue")
            .is_empty()
    );
    assert_eq!(
        counters.snapshot(),
        baseline,
        "durable queue access builds no plugin session and restores nothing"
    );
    assert_eq!(
        persisted_tool_state(&backend, &session_id).await,
        state_before,
        "the persisted session head is unchanged after a durable poll and withdrawal"
    );
    grantless.shutdown().await.expect("shutdown");
}

/// A catalog that creates and deletes but cannot resolve a session by id.
struct NoByIdLookupFactory {
    inner: Arc<dyn lash_core::DeploymentStore>,
}

const NO_BY_ID_LOOKUP_OPERATION: &str = "SessionCatalogStore::lookup_session";

#[async_trait]
impl lash_core::store::RuntimeStoreDecorator for NoByIdLookupFactory {
    type Inner = dyn lash_core::DeploymentStore;

    fn inner(&self) -> &Self::Inner {
        self.inner.as_ref()
    }

    async fn lookup_session(
        &self,
        _session_id: &SessionId,
    ) -> std::result::Result<lash_core::store::SessionLookup, StoreError> {
        Err(StoreError::UnsupportedStoreOperation {
            operation: NO_BY_ID_LOOKUP_OPERATION,
        })
    }
}

impl lash_core::DeploymentStoreDecorator for NoByIdLookupFactory {}

/// A catalog without the by-id seam must not make an existing session look
/// absent: the acquisition names the missing capability.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_catalog_without_the_by_id_seam_names_the_capability_not_a_missing_session() {
    let backend = DecoratedBackend::over(sqlite_memory_store_backend().await)
        .session_store_factory(|inner| Arc::new(NoByIdLookupFactory { inner }))
        .into_backend();
    let core = core_serving(backend, false);
    create(&core, "no-by-id-seam").await;
    let durable = core
        .session(id("no-by-id-seam"))
        .durable()
        .await
        .expect("handle");
    let error = durable
        .send(crate::TurnInput::text(
            "queued through a catalog with no by-id seam",
        ))
        .id(turn("no-by-id-seam-input"))
        .await
        .err()
        .expect("a catalog that cannot resolve by id refuses the acquisition");
    match &error {
        EmbedError::Store(StoreError::UnsupportedStoreOperation { operation }) => {
            assert_eq!(*operation, NO_BY_ID_LOOKUP_OPERATION);
        }
        other => {
            panic!("a missing by-id seam must not be reported as an absent session, got {other:?}")
        }
    }
    core.shutdown().await.expect("shutdown");
}

/// A row a run admitted is still reported, as admitted to that run, by a
/// separately acquired durable handle while the run is in its model call.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_held_input_is_still_listed_held_by_a_separate_durable_handle() {
    let entered = Arc::new(tokio::sync::Semaphore::new(0));
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
                    entered.add_permits(1);
                    release.acquire().await.expect("released").forget();
                    Ok(text_response("drained"))
                }
            }
        })
        .build()
        .into_handle();
    let backend = sqlite_memory_store_backend().await;
    let client = core_serving(backend.clone(), false);
    let session = create(&client, "durable-held-input").await;
    let accepted = session
        .send(crate::TurnInput::text("claimed by the drain"))
        .id(turn("held-input"))
        .await
        .expect("accepted");
    let observer = client
        .session(id("durable-held-input"))
        .durable()
        .await
        .expect("handle");
    assert!(
        matches!(
            observer
                .pending_turn_inputs()
                .await
                .expect("queue")
                .first()
                .map(|read| &read.status),
            Some(lash_core::runtime::PendingTurnInputReadStatus::Open)
        ),
        "before a run admits it, the row reads as open"
    );

    let serving = explicit_ephemeral_facets(LashCore::standard_builder(backend))
        .serve_test_llm_profile(provider, mock_llm_profile_spec())
        .build(crate::testing::runtime_lease_owner())
        .expect("serving core");
    tokio::time::timeout(std::time::Duration::from_secs(60), entered.acquire())
        .await
        .expect("the run reaches the provider with the input admitted")
        .expect("entered")
        .forget();
    let held = observer.pending_turn_inputs().await.expect("queue");
    let row = held
        .iter()
        .find(|read| read.input.input_id == *accepted.input_id())
        .expect("an admitted input is still reported by the Durable Session, not hidden");
    assert!(
        matches!(
            &row.status,
            lash_core::runtime::PendingTurnInputReadStatus::Admitted { run } if run.as_str() == "held-input"
        ),
        "the input the run took reads as admitted to its run, got {:?}",
        row.status
    );
    release.add_permits(1);
    accepted.output().await.expect("the held turn answers");
    serving.shutdown().await.expect("shutdown");
    client.shutdown().await.expect("shutdown");
}

/// `create()` is the one verb that creates, and it creates nothing else: no
/// runtime and no lifecycle event; the created session is ordinary, and a
/// node runs the input that was waiting.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn create_admits_an_absent_id_and_builds_no_runtime() {
    let counters = Arc::new(RuntimeBuildCounters::default());
    let backend = sqlite_memory_store_backend().await;
    let idle = probed_core(backend.clone(), &counters, false);
    let baseline = counters.snapshot();
    assert!(matches!(
        idle.session(id("created-then-queued"))
            .durable()
            .await
            .expect("handle")
            .send(crate::TurnInput::text("too early"))
            .await
            .err()
            .expect("an uncreated id is refused"),
        EmbedError::UnknownSession { .. }
    ));
    let durable = create(&idle, "created-then-queued").await;
    let accepted = durable
        .send(crate::TurnInput::text("queued before the first turn"))
        .id(turn("created-then-queued-input"))
        .await
        .expect("accepted");
    assert_eq!(durable.pending_turn_inputs().await.expect("queue").len(), 1);
    assert!(durable.exists().await.expect("existence"));
    assert_eq!(
        counters.snapshot(),
        baseline,
        "create() materialises no plugin session and restores nothing"
    );

    let serving = core_serving(backend, true);
    let drained = accepted.output().await.expect("the waiting input runs");
    assert_eq!(
        drained.assistant_message(),
        Some("echo: queued before the first turn")
    );
    assert!(
        durable
            .pending_turn_inputs()
            .await
            .expect("queue")
            .is_empty()
    );
    assert_eq!(
        durable
            .turn_input_applications()
            .await
            .expect("applications")
            .iter()
            .map(|application| application.input_id.clone())
            .collect::<Vec<_>>(),
        vec![drained_input(&durable, "created-then-queued-input")],
        "the created session's queued input settles as a durable application"
    );
    serving.shutdown().await.expect("shutdown");
    idle.shutdown().await.expect("shutdown");
}

fn drained_input(durable: &crate::DurableSession, id: &str) -> lash_core::InputId {
    durable.input_id(&turn(id))
}

/// Session ids are single-use, so `create()` refuses a tombstoned one.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn create_on_a_deleted_id_is_refused_with_the_tombstone() {
    let backend = sqlite_memory_store_backend().await;
    let core = core_serving(backend.clone(), true);
    create(&core, "create-deleted").await;
    lash_core::SessionCatalogStore::delete_session(
        backend.session_store_factory().as_ref(),
        &SessionId::from("create-deleted"),
    )
    .await
    .expect("delete the session");
    let error = core
        .session(id("create-deleted"))
        .create(crate::SessionCreation::root(
            crate::plugins::SessionToolAccess::ambient(),
            mock_session_spec(),
        ))
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
    core.shutdown().await.expect("shutdown");
}

/// A host that reuses an enqueue id for a different submission is told so with
/// a typed, terminal error rather than a generic store failure; an identical
/// retry replays the original acceptance (FIG-3544).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn reused_enqueue_id_with_changed_input_is_a_typed_identity_conflict() {
    let core = core_serving(sqlite_memory_store_backend().await, false);
    let durable = create(&core, "fig3544-enqueue-conflict").await;
    let first = durable
        .send(crate::TurnInput::text("original"))
        .id(turn("retry-me"))
        .await
        .expect("first acceptance");
    let replay = durable
        .send(crate::TurnInput::text("original"))
        .id(turn("retry-me"))
        .await
        .expect("identical retry");
    assert_eq!(
        replay.receipt(),
        first.receipt(),
        "an identical retry replays the acceptance"
    );
    let conflict = durable
        .send(crate::TurnInput::text("changed"))
        .id(turn("retry-me"))
        .await
        .err()
        .expect("a changed submission under a used id is refused");
    let EmbedError::Runtime(error) = &conflict else {
        panic!("expected a typed runtime error, got {conflict:?}");
    };
    assert_eq!(
        error.code,
        lash_core::RuntimeErrorCode::DurableIdentityConflict,
        "the refusal is typed, not a generic store commit failure"
    );
    assert!(conflict.is_terminal() && !conflict.is_retryable());
    core.shutdown().await.expect("shutdown");
}

/// Whether `entry` is a message spoken in `role`.
fn is_role(
    entry: &crate::transcript::TranscriptEntry,
    role: crate::transcript::TranscriptRole,
) -> bool {
    matches!(&entry.item, crate::transcript::TranscriptItem::Message(message) if message.role == role)
}

/// A message entry's text blocks, one per line.
fn text_of(entry: &crate::transcript::TranscriptEntry) -> String {
    let crate::transcript::TranscriptItem::Message(message) = &entry.item else {
        return String::new();
    };
    message
        .blocks
        .iter()
        .filter_map(|block| match block {
            crate::transcript::TranscriptBlock::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn whole_history() -> crate::persistence::HistoryBudget {
    crate::persistence::HistoryBudget {
        max_nodes: std::num::NonZeroU32::MIN.saturating_add(127),
        max_bytes: std::num::NonZeroU64::MIN.saturating_add(32 * 1024 * 1024 - 1),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn transcript_totally_projects_really_committed_nodes_in_source_order() {
    let core = core_serving(sqlite_memory_store_backend().await, true);
    let session = create(&core, "transcript-totality").await;
    for input in ["first question", "second question"] {
        session
            .send(crate::TurnInput::text(input))
            .output()
            .await
            .expect("the turn commits");
    }
    let projection = session.transcript().await.expect("transcript");
    let page = session
        .history(crate::persistence::HistoryAnchor::Head, whole_history())
        .await
        .expect("history");
    assert!(page.next.is_none());
    assert_eq!(projection.entries().len(), page.nodes.len());
    for (entry, node) in projection.entries().iter().zip(page.nodes.iter().rev()) {
        assert_eq!(entry.timestamp, node.record.timestamp);
        assert_eq!(
            serde_json::to_value(&entry.entry_id).expect("entry id"),
            serde_json::to_value(&node.record.node_id).expect("node id")
        );
    }
    let users = projection
        .visible()
        .filter(|entry| is_role(entry, crate::transcript::TranscriptRole::User))
        .map(text_of)
        .collect::<Vec<_>>();
    assert_eq!(users, ["first question", "second question"]);
    let replies = projection
        .visible()
        .filter(|entry| entry.provenance.is_turn_reply)
        .collect::<Vec<_>>();
    assert_eq!(replies.len(), 2);
    assert!(
        replies
            .iter()
            .all(|entry| is_role(entry, crate::transcript::TranscriptRole::Assistant))
    );
    assert_ne!(replies[0].provenance.turn_id, replies[1].provenance.turn_id);
    assert_eq!(
        projection.visible().count()
            + projection
                .entries()
                .iter()
                .filter(|entry| entry.is_suppressed())
                .count(),
        page.nodes.len()
    );
    core.shutdown().await.expect("shutdown");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "FIG-5387: under load a resume from a snapshot's cursor replays a commit the snapshot already holds"]
async fn committed_row_deltas_transport_each_new_node_once() {
    let core = core_serving(sqlite_memory_store_backend().await, true);
    create(&core, "transcript-deltas").await;
    let session = core
        .session(id("transcript-deltas"))
        .open()
        .await
        .expect("open");
    let mut seen = session
        .read_view()
        .transcript()
        .expect("valid committed history")
        .into_entries()
        .into_iter()
        .map(|row| row.entry_id)
        .collect::<std::collections::HashSet<_>>();
    for input in ["first delta question", "second delta question"] {
        let cursor = session
            .observe()
            .snapshot()
            .await
            .expect("durable snapshot")
            .cursor;
        session
            .send(crate::TurnInput::text(input))
            .output()
            .await
            .expect("the turn commits");
        let crate::support::SessionResume::Replayed { events } = session
            .observe()
            .resume_from_cursor(&cursor)
            .await
            .expect("resume")
        else {
            panic!("the fresh observation cursor must replay");
        };
        let mut carried = Vec::new();
        for event in events {
            let lash_core::SessionObservationEventPayload::Committed { entries: rows, .. } =
                &event.payload
            else {
                continue;
            };
            for row in rows {
                assert!(
                    seen.insert(row.entry_id.clone()),
                    "a commit repeated an earlier node"
                );
                carried.push(row.clone());
            }
        }
        let session = core
            .session(id("transcript-deltas"))
            .open()
            .await
            .expect("reopen");
        let canonical = session
            .read_view()
            .transcript()
            .expect("valid committed history")
            .into_entries();
        assert_eq!(
            seen.len(),
            canonical.len(),
            "every committed node must be carried, including named suppressions"
        );
        assert!(
            carried
                .iter()
                .any(|row| is_role(row, crate::transcript::TranscriptRole::User)
                    && text_of(row) == input)
        );
        for row in carried {
            assert_eq!(
                canonical
                    .iter()
                    .find(|candidate| candidate.entry_id == row.entry_id),
                Some(&row)
            );
        }
    }
    core.shutdown().await.expect("shutdown");
}

#[cfg(feature = "rlm")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn transcript_totally_projects_a_really_committed_rlm_trajectory() {
    let calls = Arc::new(AtomicUsize::new(0));
    let provider = crate::testing::TestProvider::builder()
        .kind("transcript-corpus")
        .complete(move |_| {
            let call = calls.fetch_add(1, Ordering::SeqCst);
            async move {
                Ok(lash_core::LlmResponse {
                    parts: vec![
                        lash_core::LlmOutputPart::Reasoning {
                            text: "committed corpus reasoning".into(),
                            replay: None,
                        },
                        lash_core::LlmOutputPart::Text {
                            text: if call == 0 {
                                "<typescript>print(\"committed corpus output\");</typescript>"
                            } else {
                                "<typescript>finish(\"committed corpus reply\");</typescript>"
                            }
                            .into(),
                            response_meta: None,
                        },
                    ],
                    ..Default::default()
                })
            }
        })
        .build()
        .into_handle();
    let backend = sqlite_memory_store_backend().await;
    let core = explicit_ephemeral_facets(rlm_core_builder_over(backend))
        .serve_test_llm_profile(provider, mock_llm_profile_spec())
        .build(crate::testing::runtime_lease_owner())
        .expect("rlm core");
    let session = create(&core, "transcript-rlm-corpus").await;
    session
        .send(crate::TurnInput::text("committed corpus input"))
        .output()
        .await
        .expect("the rlm turn commits");
    let projection = session.transcript().await.expect("transcript");
    let page = session
        .history(crate::persistence::HistoryAnchor::Head, whole_history())
        .await
        .expect("history");
    assert!(page.next.is_none());
    assert_eq!(projection.entries().len(), page.nodes.len());
    for (entry, node) in projection.entries().iter().zip(page.nodes.iter().rev()) {
        assert_eq!(
            serde_json::to_value(&entry.entry_id).expect("entry id"),
            serde_json::to_value(&node.record.node_id).expect("node id")
        );
        assert_eq!(entry.timestamp, node.record.timestamp);
    }
    assert_eq!(
        projection
            .visible()
            .filter(|entry| entry.provenance.is_turn_reply)
            .map(text_of)
            .collect::<Vec<_>>(),
        ["committed corpus reply"]
    );
    assert!(projection.visible().any(|entry| matches!(
        &entry.item,
        crate::transcript::TranscriptItem::Message(message)
            if message.blocks.iter().any(|block| matches!(
                block,
                crate::transcript::TranscriptBlock::Reasoning { text }
                    if text == "committed corpus reasoning"
            ))
    )));
    assert!(projection.visible().any(|entry| matches!(
        &entry.item,
        crate::transcript::TranscriptItem::Cell(cell)
            if cell.code == "print(\"committed corpus output\");"
                && cell.prints.iter().map(|print| print.text.as_str()).eq(["committed corpus output"])
    )));
    assert!(
        projection
            .entries()
            .iter()
            .any(|entry| entry.is_suppressed())
    );
    core.shutdown().await.expect("shutdown");
}

/// One record per tool call under every protocol (FIG-5521): an RLM cell
/// that calls two host tools reports two tool calls, each under its own
/// `call_id`, and the cell's executed calls, read back from the durable
/// transcript after a reopen, name those records by that `call_id`.
#[cfg(feature = "rlm")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_rlm_cells_executed_calls_name_the_turns_tool_call_records_after_a_reopen() -> Result<()>
{
    fn core(backend: lash_core::Backend) -> Result<LashCore> {
        explicit_ephemeral_facets(rlm_core_builder_over(backend))
            .serve_test_llm_profile(
                text_provider(
                    "cell-call-records",
                    typescript_block(
                        "const first = await tools.app_lookup({});\n\
                         const second = await tools.app_lookup({});\n\
                         finish(\"looked up twice\");",
                    ),
                ),
                mock_llm_profile_spec(),
            )
            .tools(Arc::new(AppTools))
            .build(crate::testing::runtime_lease_owner())
    }
    let backend = sqlite_memory_store_backend().await;
    let session_id = id("cell-call-records");
    let running = core(backend.clone())?;
    let session = running
        .session(session_id.clone())
        .created()
        .await
        .open()
        .await?;
    let events = RecordingEvents::default();
    session
        .send(crate::TurnInput::text("look up twice"))
        .output_into(&events)
        .await?;
    // The turn reports each call once it completes, with the record's own
    // fields: its id, tool, arguments and output.
    let mut record_ids = Vec::new();
    for activity in events.snapshot().await {
        if let crate::TurnEvent::ToolCallCompleted {
            call_id,
            name,
            args,
            output,
            ..
        } = activity.event
        {
            assert_eq!(name, "app_lookup");
            assert_eq!(args, serde_json::json!({}));
            assert!(output.is_success());
            if !record_ids.contains(&Some(call_id.clone())) {
                record_ids.push(Some(call_id));
            }
        }
    }
    assert_eq!(record_ids.len(), 2, "one record per host tool call");
    drop(session);
    running.shutdown().await?;

    let reopened = core(backend)?;
    let transcript = reopened
        .session(session_id)
        .open()
        .await?
        .read_view()
        .transcript()
        .expect("valid committed history");
    let cells = transcript
        .visible()
        .filter_map(|entry| match &entry.item {
            crate::transcript::TranscriptItem::Cell(cell) => Some(cell),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(cells.len(), 1, "the turn ran one cell");
    assert_eq!(cells[0].calls_omitted, 0);
    assert_eq!(
        cells[0]
            .calls
            .iter()
            .map(|call| call.call_id.clone())
            .collect::<Vec<_>>(),
        record_ids,
        "each executed call names its tool call record"
    );
    for call in &cells[0].calls {
        assert_eq!(call.operation, "tools.app_lookup");
        assert_eq!(call.outcome, crate::persistence::ExecutedCallOutcome::Ok);
    }
    reopened.shutdown().await?;
    Ok(())
}

/// A turn keeps the tool calls its code cells made (FIG-5330): the cell's
/// own records of them are pruned as it advances, so the turn records them
/// with its next commit. The settled turn's tool records, read from the
/// store after a reopen, are the records the turn streamed, by `call_id`
/// and in call order.
#[cfg(feature = "rlm")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_settled_turns_cell_tool_records_are_the_streamed_ones_after_a_reopen() -> Result<()> {
    fn core(backend: lash_core::Backend) -> Result<LashCore> {
        explicit_ephemeral_facets(rlm_core_builder_over(backend))
            .serve_test_llm_profile(
                text_provider(
                    "cell-tool-records",
                    typescript_block(
                        "const first = await tools.app_lookup({});\n\
                         const second = await tools.app_lookup({});\n\
                         finish(\"looked up twice\");",
                    ),
                ),
                mock_llm_profile_spec(),
            )
            .tools(Arc::new(AppTools))
            .build(crate::testing::runtime_lease_owner())
    }
    let backend = sqlite_memory_store_backend().await;
    let session_id = id("cell-tool-records");
    let run = turn("cell-tool-records-run");
    let running = core(backend.clone())?;
    let session = running
        .session(session_id.clone())
        .created()
        .await
        .open()
        .await?;
    let events = RecordingEvents::default();
    let settled = session
        .send(crate::TurnInput::text("look up twice"))
        .id(run.clone())
        .output_into(&events)
        .await?;
    let mut streamed = Vec::new();
    for activity in events.snapshot().await {
        if let crate::TurnEvent::ToolCallCompleted {
            call_id,
            name,
            args,
            output,
            ..
        } = activity.event
            && !streamed
                .iter()
                .any(|(id, ..): &(crate::ToolCallId, _, _, _)| *id == call_id)
        {
            streamed.push((call_id, name, args, output.value_for_projection()));
        }
    }
    assert_eq!(streamed.len(), 2, "the cell made two tool calls");
    let recorded = |calls: &[lash_core::ToolCallRecord]| {
        calls
            .iter()
            .map(|record| {
                (
                    record.call_id.clone(),
                    record.tool.clone(),
                    record.args.clone(),
                    record.output.value_for_projection(),
                )
            })
            .collect::<Vec<_>>()
    };
    assert_eq!(recorded(&settled.tool_calls), streamed);
    assert!(settled.omitted.is_none());
    drop(session);
    running.shutdown().await?;

    let reopened = core(backend)?;
    let report = reopened
        .session(session_id)
        .open()
        .await?
        .attach_id(run)
        .output()
        .await?
        .result;
    assert_eq!(report.source, crate::ReportSource::Durable);
    assert_eq!(recorded(&report.tool_calls), streamed);
    assert!(report.omitted.is_none());
    reopened.shutdown().await?;
    Ok(())
}
