use super::*;

use lash_facade_tests::durable::*;
use lash_vocabulary::identity::SessionId;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn catalog_contention_retains_its_type_and_retryability_across_existing_session_apis() {
    use lash_core::RuntimeEffectControllerError;
    use lash_core::RuntimeError;
    use lash_core::RuntimeErrorCode;
    use lash_core::SessionError;
    use lash_execution_contracts::plugin_error::PluginError;

    catalog_failure_matrix(CatalogFailure::Contended).await;

    for error in [
        EmbedError::Runtime(RuntimeError::new(
            RuntimeErrorCode::StoreCommitContended,
            "test",
        )),
        EmbedError::Plugin(PluginError::RuntimeEffectController(
            RuntimeEffectControllerError::from(StoreError::Contended),
        )),
    ] {
        assert!(error.is_retryable(), "{error}");
        assert!(!error.is_terminal(), "{error}");
    }

    let transient: [fn() -> StoreError; 4] = [
        || StoreError::Contended,
        || StoreError::StorageFailure {
            backend: "facade-law",
            message: "the store is temporarily unavailable".to_string(),
        },
        || StoreError::Backend("the store is temporarily unavailable".to_string()),
        || StoreError::MigrationOpenElsewhere {
            database: "durable core".to_string(),
            location: std::path::PathBuf::from("facade-law"),
        },
    ];
    let store_shapes = |error: fn() -> StoreError| {
        [
            EmbedError::Store(error()),
            EmbedError::Session(SessionError::Store {
                context: "facade law".to_string(),
                source: error(),
            }),
        ]
    };
    for error in transient {
        assert!(error().is_transient(), "{}", error());
        for facade in store_shapes(error) {
            assert!(facade.is_retryable() && !facade.is_terminal(), "{facade:?}");
        }
    }
    let corrupt = || StoreError::StoredDataCorrupt {
        record_kind: "SessionHeadMeta",
        message: "not the record".to_string(),
    };
    assert!(!corrupt().is_transient());
    for facade in store_shapes(corrupt) {
        assert!(facade.is_terminal() && !facade.is_retryable(), "{facade:?}");
    }
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
    let is_unknown = |result: Result<lash::LashSession>| match result {
        Err(EmbedError::UnknownSession { session_id }) => session_id == missing,
        _ => false,
    };
    assert!(is_unknown(core.session(id("never-opened")).open().await));
    let state = || {
        let mut state = lash_core::RuntimeSessionState::fixture(lash_core::SessionPolicy::new(
            lash::TurnBudget::Unbounded,
            lash::MaxToolCalls::new(1024),
            lash::NoProgressBudget::bounded(12),
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
                    .create(lash::SessionCreation::root(mock_session_spec()))
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
        .send(lash::TurnInput::text(
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
        .send(lash::TurnInput::text("queued after deletion"))
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
        .send(lash::TurnInput::text("steer a turn that never ran"))
        .ingress(lash_core::TurnInputIngress::active_turn(
            lash_vocabulary::identity::TurnId::from("unknown-turn-address-never-ran"),
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
                .send(lash::TurnInput::text("nope"))
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
            .send(lash::TurnInput::text("queued on sqlite metadata"))
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
        .send(lash::TurnInput::text("commit a turn"))
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

    let administration = serving.session_administration().await;
    LashCore::delete_session(
        administration
            .delete_context("sqlite-metadata-only")
            .expect("delete context"),
    )
    .await
    .expect("request the sqlite session's close");
    assert_eq!(
        serving
            .await_session_deletion(&SessionId::from("sqlite-metadata-only"))
            .await
            .expect("await deletion"),
        lash::SessionDeleteCompletion::Deleted,
    );
    let deleted = client
        .session(id("sqlite-metadata-only"))
        .durable()
        .await
        .expect("handle");
    assert!(deleted.was_deleted().await.expect("tombstone"));
    assert!(
        matches!(
            deleted
                .send(lash::TurnInput::text("nope"))
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

/// FIG-6298: with no telemetry adapter no send opens a producer, so an
/// input is admitted unobserved: a caller's context never becomes the
/// parent an input retains.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_send_without_an_adapter_admits_its_input_unobserved() {
    let core = core_serving(sqlite_memory_store_backend().await, false);
    let durable = create(&core, "traced-send").await;
    let context = lash_core::TraceCarrier::parse_w3c(&format!("00-{:032x}-{:016x}-01", 1, 1), None)
        .expect("a valid trace context");
    drop(
        durable
            .send(lash::TurnInput::text("the same words"))
            .id(turn("traced-send-input"))
            .trace_context(context)
            .await
            .expect("traced acceptance"),
    );
    drop(
        durable
            .send(lash::TurnInput::text("words nobody traced"))
            .id(turn("untraced-send-input"))
            .await
            .expect("untraced acceptance"),
    );

    let causes = durable
        .pending_turn_inputs()
        .await
        .expect("queue")
        .into_iter()
        .map(|read| read.input.trace_cause)
        .collect::<Vec<_>>();
    assert_eq!(causes, vec![lash_core::TraceCause::None; 2]);
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
        .send(lash::TurnInput::text("queued from a separate handle"))
        .id(turn("separate-handle"))
        .await
        .expect("accepted");
    let cancelled = durable
        .cancel(lash::CancelTarget::Run(
            lash_vocabulary::identity::RunId::parse((turn("separate-handle")).as_str()).unwrap(),
        ))
        .await
        .expect("cancel");
    assert!(
        matches!(cancelled, lash::CancelReceipt::Withdrawn { .. }),
        "{cancelled:?}"
    );

    let lash_core::facade_support::SessionResume::Replayed { events } = session
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
        .send(lash::TurnInput::text("from the first durable handle"))
        .id(turn("beside-a"))
        .await
        .expect("accepted");
    let b = second
        .send(lash::TurnInput::text("from the second durable handle"))
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
        .send(lash::TurnInput::text("writer keeps committing"))
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

/// FIG-3353: a grantless core polls and edits a session's queue without
/// building a runtime, so nothing is materialized and the head is unchanged.
///
/// The law asserts its own precondition — a run on a grantless core *does*
/// materialize plugins — and then counts, on the `durable()` path, every
/// step of a runtime build.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn durable_queue_access_on_a_grantless_core_builds_no_runtime() {
    let session_id = SessionId::from("fig-3353-durable-poll");
    let backend = sqlite_memory_store_backend().await;

    // A core that carries the session's tool source, to record its surface.
    let granting = explicit_ephemeral_facets(LashCore::standard_builder(backend.clone()))
        .serve_test_llm_profile(mock_provider(), mock_llm_profile_spec())
        .tools(lash::tool_provider_key!("tools"), Arc::new(AppTools))
        .build(lash_facade_tests::runtime_lease_owner())
        .expect("granting core");
    let granted = create(&granting, session_id.as_str()).await;
    granted
        .send(lash::TurnInput::text(
            "persist a checkpoint with a recorded surface",
        ))
        .output()
        .await
        .expect("the granted turn commits");
    granting.shutdown().await.expect("shutdown");

    // Precondition: a run on a grantless core really does build a runtime.
    let run_counters = Arc::new(RuntimeBuildCounters::default());
    let grantless_runner = probed_core(backend.clone(), &run_counters, true);
    grantless_runner
        .session(id(session_id.as_str()))
        .durable()
        .await
        .expect("handle")
        .send(lash::TurnInput::text("run without the tool's source"))
        .output()
        .await
        .expect("the grantless turn commits");
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
        .send(lash::TurnInput::text("left pending for the grantless core"))
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
        .cancel(lash::CancelTarget::Run(
            lash_vocabulary::identity::RunId::parse((turn("fig-3353-pending")).as_str()).unwrap(),
        ))
        .await
        .expect("cancel");
    assert!(
        matches!(cancelled, lash::CancelReceipt::Withdrawn { .. }),
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
        .send(lash::TurnInput::text(
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
    let provider = lash::testing::TestProvider::builder()
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
        .send(lash::TurnInput::text("claimed by the drain"))
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
        .build(lash_facade_tests::runtime_lease_owner())
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
            .send(lash::TurnInput::text("too early"))
            .await
            .err()
            .expect("an uncreated id is refused"),
        EmbedError::UnknownSession { .. }
    ));
    let durable = create(&idle, "created-then-queued").await;
    let accepted = durable
        .send(lash::TurnInput::text("queued before the first turn"))
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
        .create(lash::SessionCreation::root(mock_session_spec()))
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
        .send(lash::TurnInput::text("original"))
        .id(turn("retry-me"))
        .await
        .expect("first acceptance");
    let replay = durable
        .send(lash::TurnInput::text("original"))
        .id(turn("retry-me"))
        .await
        .expect("identical retry");
    assert_eq!(
        replay.receipt(),
        first.receipt(),
        "an identical retry replays the acceptance"
    );
    let conflict = durable
        .send(lash::TurnInput::text("changed"))
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

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn transcript_totally_projects_really_committed_nodes_in_source_order() {
    let core = core_serving(sqlite_memory_store_backend().await, true);
    let session = create(&core, "transcript-totality").await;
    for input in ["first question", "second question"] {
        session
            .send(lash::TurnInput::text(input))
            .output()
            .await
            .expect("the turn commits");
    }
    let projection = session.transcript().await.expect("transcript");
    let page = session
        .history(lash::persistence::HistoryAnchor::Head, whole_history())
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
        .filter(|entry| is_role(entry, lash::transcript::TranscriptRole::User))
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
            .all(|entry| is_role(entry, lash::transcript::TranscriptRole::Assistant))
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
            .send(lash::TurnInput::text(input))
            .output()
            .await
            .expect("the turn commits");
        let lash_core::facade_support::SessionResume::Replayed { events } = session
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
        assert!(carried.iter().any(
            |row| is_role(row, lash::transcript::TranscriptRole::User) && text_of(row) == input
        ));
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
    let provider = lash::testing::TestProvider::builder()
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
                        lash::testing::execute_code(
                            if call == 0 {
                                "console.log(\"committed corpus output\");"
                            } else {
                                "await control.finish(\"committed corpus reply\");"
                            }
                            .to_string(),
                        ),
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
        .build(lash_facade_tests::runtime_lease_owner())
        .expect("rlm core");
    let session = create(&core, "transcript-rlm-corpus").await;
    session
        .send(lash::TurnInput::text("committed corpus input"))
        .output()
        .await
        .expect("the rlm turn commits");
    let projection = session.transcript().await.expect("transcript");
    let page = session
        .history(lash::persistence::HistoryAnchor::Head, whole_history())
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
        lash::transcript::TranscriptItem::Message(message)
            if message.blocks.iter().any(|block| matches!(
                block,
                lash::transcript::TranscriptBlock::Reasoning { text }
                    if text == "committed corpus reasoning"
            ))
    )));
    assert!(projection.visible().any(|entry| matches!(
        &entry.item,
        lash::transcript::TranscriptItem::Cell(cell)
            if cell.code == "console.log(\"committed corpus output\");"
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

/// One record per tool call under every protocol (FIG-5521): a code mode cell
/// that calls two host tools reports two tool calls, each under its own
/// `call_id`, and the cell's executed calls, read back from the durable
/// transcript after a reopen, name those records by that `call_id`.
#[cfg(feature = "rlm")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_rlm_cells_executed_calls_name_the_turns_tool_call_records_after_a_reopen() -> Result<()>
{
    fn core(backend: lash_execution_contracts::backend::Backend) -> Result<LashCore> {
        explicit_ephemeral_facets(rlm_core_builder_over(backend))
            .serve_test_llm_profile(
                part_provider(
                    "cell-call-records",
                    typescript_block(
                        "const first = await tools.appLookup({});\n\
                         const second = await tools.appLookup({});\n\
                         await control.finish(\"looked up twice\");",
                    ),
                ),
                mock_llm_profile_spec(),
            )
            .tools(lash::tool_provider_key!("tools"), Arc::new(AppTools))
            .build(lash_facade_tests::runtime_lease_owner())
    }
    let backend = sqlite_memory_store_backend().await;
    let session_id = id("cell-call-records");
    let running = core(backend.clone())?;
    let session = created_session(&running, session_id.clone())
        .await
        .open()
        .await?;
    let events = RecordingEvents::default();
    session
        .send(lash::TurnInput::text("look up twice"))
        .output_into(&events)
        .await?;
    // The turn reports each call once it completes, with the record's own
    // fields: its id, tool, arguments and output.
    let mut record_ids = Vec::new();
    for activity in events.snapshot().await {
        if let lash::TurnEvent::ToolCallCompleted {
            call_id,
            name,
            args,
            output,
            ..
        } = activity.event
        {
            // The cell's last call is its `control.finish`.
            if name != "control.finish" {
                assert_eq!(name, "tools.app_lookup");
                assert_eq!(args, serde_json::json!({}));
            }
            assert!(output.is_success());
            if !record_ids.contains(&Some(call_id.clone())) {
                record_ids.push(Some(call_id));
            }
        }
    }
    assert_eq!(
        record_ids.len(),
        3,
        "one record per call, the `control.finish` among them"
    );
    drop(session);
    running.shutdown().await?;

    let reopened = core(backend)?;
    let transcript = reopened
        .session(session_id.clone())
        .open()
        .await?
        .read_view()
        .transcript()
        .expect("valid committed history");
    let cells = transcript
        .visible()
        .filter_map(|entry| match &entry.item {
            lash::transcript::TranscriptItem::Cell(cell) => Some(cell),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(cells.len(), 1, "the turn ran one cell");
    assert_eq!(
        cells[0]
            .calls
            .iter()
            .map(|call| call.call_id.clone())
            .collect::<Vec<_>>(),
        record_ids,
        "each executed call names its tool call record"
    );
    for call in cells[0]
        .calls
        .iter()
        .filter(|call| call.operation != "control.finish")
    {
        assert_eq!(call.operation, "tools.app_lookup");
        assert_eq!(call.outcome, lash::persistence::ExecutedCallOutcome::Ok {});
    }
    reopened.shutdown().await?;
    Ok(())
}

/// A turn keeps references to the calls its code cells made (FIG-6171),
/// so their archival bodies survive retirement of the cell's execution rows. The settled turn's tool records, read from the
/// store after a reopen, are the records the turn streamed, by `call_id`
/// and in call order.
#[cfg(feature = "rlm")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_settled_turns_cell_tool_records_are_the_streamed_ones_after_a_reopen() -> Result<()> {
    fn core(backend: lash_execution_contracts::backend::Backend) -> Result<LashCore> {
        explicit_ephemeral_facets(rlm_core_builder_over(backend))
            .serve_test_llm_profile(
                part_provider(
                    "cell-tool-records",
                    typescript_block(
                        "const first = await tools.appLookup({});\n\
                         const second = await tools.appLookup({});\n\
                         await control.finish(\"looked up twice\");",
                    ),
                ),
                mock_llm_profile_spec(),
            )
            .tools(lash::tool_provider_key!("tools"), Arc::new(AppTools))
            .build(lash_facade_tests::runtime_lease_owner())
    }
    let backend = sqlite_memory_store_backend().await;
    let session_id = id("cell-tool-records");
    let run = turn("cell-tool-records-run");
    let running = core(backend.clone())?;
    let session = created_session(&running, session_id.clone())
        .await
        .open()
        .await?;
    let events = RecordingEvents::default();
    session
        .send(lash::TurnInput::text("look up twice"))
        .id(run.clone())
        .output_into(&events)
        .await?;
    let mut streamed = Vec::new();
    for activity in events.snapshot().await {
        if let lash::TurnEvent::ToolCallCompleted {
            call_id,
            name,
            args,
            output,
            ..
        } = activity.event
            && !streamed
                .iter()
                .any(|(id, ..): &(lash::ToolCallId, _, _, _)| *id == call_id)
        {
            streamed.push((call_id, name, args, output.value_for_projection()));
        }
    }
    assert_eq!(
        streamed.len(),
        3,
        "the cell made two tool calls and its `control.finish`"
    );
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
    assert_eq!(
        recorded(&session.durable().read_turn_tool_calls(&run).await?),
        streamed
    );
    drop(session);
    running.shutdown().await?;

    let reopened = core(backend)?;
    reopened
        .session(session_id.clone())
        .open()
        .await?
        .attach_id(run.clone())
        .output()
        .await?;
    assert_eq!(
        recorded(
            &reopened
                .session(session_id.clone())
                .durable()
                .await?
                .read_turn_tool_calls(&run)
                .await?
        ),
        streamed
    );
    reopened.shutdown().await?;
    Ok(())
}

/// A turn that stops on a code cell loses none of the calls the cell
/// completed (FIG-5330): its cancel records them from the cell's snapshot
/// and the outcomes it committed since, and the cancelled turn's report
/// lists them after a reopen. The cancel is accepted while the cell's
/// second call runs, and the call answers before the turn's owner looks for
/// the cancel. An `Immediate` cancel stops the cell during that call: the
/// accepted request, not the owner's watch, decides that its late answer is
/// not a completion (FIG-6082), so the first call is reported and the
/// second is not. An `AfterStep` cancel lets the cell answer and ends the
/// turn before the model call whose commit would have recorded it: both
/// calls are reported.
#[cfg(feature = "rlm")]
#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn a_cancelled_turn_reports_the_tool_calls_its_cell_completed_after_a_reopen() -> Result<()> {
    for (mode, completed) in [
        (lash::TurnCancelMode::Immediate, 1),
        (lash::TurnCancelMode::AfterStep, 2),
    ] {
        let entered = Arc::new(tokio::sync::Notify::new());
        let release = Arc::new(tokio::sync::Notify::new());
        let (answering, answered) = std::sync::mpsc::sync_channel(1);
        let (accept, accepted) = std::sync::mpsc::channel();
        let accepted = Arc::new(StdMutex::new(accepted));
        let core = |backend: lash_execution_contracts::backend::Backend| {
            explicit_ephemeral_facets(rlm_core_builder_over(backend))
                .serve_test_llm_profile(
                    part_provider(
                        "cancelled-cell-records",
                        typescript_block(
                            "const first = await tools.appLookup({});\n\
                             const second = await tools.appLookup({});",
                        ),
                    ),
                    mock_llm_profile_spec(),
                )
                .tools(
                    lash::tool_provider_key!("tools"),
                    Arc::new(SecondLookupHeld {
                        calls: std::sync::atomic::AtomicUsize::new(0),
                        entered: Arc::clone(&entered),
                        release: Arc::clone(&release),
                        answering: answering.clone(),
                        accepted: Arc::clone(&accepted),
                    }),
                )
                .build(lash_facade_tests::runtime_lease_owner())
        };
        let backend = sqlite_memory_store_backend().await;
        let session_id = id("cancelled-cell-records");
        let run = turn("cancelled-cell-records-run");
        let running = core(backend.clone())?;
        let session = created_session(&running, session_id.clone())
            .await
            .open()
            .await?;
        let handle = session
            .send(lash::TurnInput::text("look up twice"))
            .id(run.clone())
            .await?;
        entered.notified().await;
        // The cancel is requested off the runtime, once the released call
        // holds its one worker: it is accepted before the call answers, and
        // the turn's owner runs only after the call answered.
        let cancel = handle.cancel().request_id("stop").mode(mode);
        let canceller = std::thread::spawn(move || {
            answered.recv().expect("the held call answers");
            let receipt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("the canceller's runtime")
                .block_on(async move { cancel.await });
            accept.send(()).expect("the held call waits for the cancel");
            receipt
        });
        release.notify_one();
        let ended = tokio::time::timeout(std::time::Duration::from_secs(60), handle.output())
            .await
            .expect("the cancelled turn ends")?
            .outcome;
        canceller.join().expect("the canceller runs")?;
        assert!(ended.cancellation().is_some(), "{mode:?}: {:?}", ended);
        drop(session);
        running.shutdown().await?;

        let reopened = core(backend)?;
        let report = reopened
            .session(session_id.clone())
            .open()
            .await?
            .attach_id(run.clone())
            .output()
            .await?;
        assert!(report.outcome.cancellation().is_some(), "{mode:?}");
        assert_eq!(
            reopened
                .session(session_id.clone())
                .durable()
                .await?
                .read_turn_tool_calls(&run)
                .await?
                .iter()
                .map(|record| (record.tool.as_str(), record.output.is_success()))
                .collect::<Vec<_>>(),
            vec![("tools.app_lookup", true); completed],
            "{mode:?}: the calls the cell completed, and no other"
        );
        reopened.shutdown().await?;
    }
    Ok(())
}

/// The after-turn callbacks see the tool calls of a cell that ends the turn
/// (FIG-5330): `await control.finish(...)` in the cell is followed by no commit but the
/// turn's own, which records the cell's calls, and the report the callbacks
/// read lists them.
#[cfg(feature = "rlm")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_after_turn_report_lists_the_tool_calls_of_the_cell_that_finished_the_turn()
-> Result<()> {
    let seen = Arc::new(StdMutex::new(Vec::new()));
    let plugin = lash::plugins::StaticPluginFactory::new(
        lash::plugins::PluginDeclaration::initial("after-turn-cell-calls"),
        lash::plugins::PluginSpec::new().with_after_turn(lash::hook_key!("calls"), {
            let seen = Arc::clone(&seen);
            Arc::new(move |context| {
                let tools = context
                    .turn
                    .tool_calls
                    .iter()
                    .map(|record| record.tool.clone())
                    .collect::<Vec<_>>();
                seen.lock().expect("the law's lock").push(tools);
                Box::pin(async { Ok(lash::plugins::AfterTurnContributions::default()) })
            })
        }),
    );
    let core =
        explicit_ephemeral_facets(rlm_core_builder_over(sqlite_memory_store_backend().await))
            .serve_test_llm_profile(
                part_provider(
                    "after-turn-cell-calls",
                    typescript_block(
                        "const found = await tools.appLookup({});\n\
                 await control.finish(\"looked up\");",
                    ),
                ),
                mock_llm_profile_spec(),
            )
            .tools(lash::tool_provider_key!("tools"), Arc::new(AppTools))
            .plugin(Arc::new(plugin))
            .build(lash_facade_tests::runtime_lease_owner())?;
    let session = created_session(&core, id("after-turn-cell-calls"))
        .await
        .open()
        .await?;
    let settled = session
        .send(lash::TurnInput::text("look up"))
        .output()
        .await?;
    assert_eq!(
        lash::testing::tool_calls_for_activity(&session.durable(), &settled.activities)
            .await
            .len(),
        2,
        "the cell made one tool call and its `control.finish`"
    );
    assert_eq!(
        *seen.lock().expect("the law's lock"),
        vec![vec![
            "tools.app_lookup".to_owned(),
            "control.finish".to_owned()
        ]],
        "the after-turn callbacks ran once, over the cell's calls"
    );
    drop(session);
    core.shutdown().await?;
    Ok(())
}

/// An executed cell is one typed record, and its assistant context is
/// ordinary history, through native RLM calls (FIG-5527). The reply that ran the
/// cell reads back from the durable transcript after a reopen as an
/// assistant message naming the cell, with the native channel's
/// `execute_code` call beside its prose; the cell reads back as the record
/// its protocol committed, whose prints and result are the values the
/// executor reported in the cell's completion activity. One decoder reads
/// native execution.
#[cfg(feature = "rlm")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_cells_assistant_context_and_record_read_back_from_the_transcript_after_a_reopen()
-> Result<()> {
    use lash::transcript::{CellOutcome, TranscriptBlock, TranscriptItem, TranscriptRole};

    const PROGRAM: &str = "console.log(\"counted\");\nawait control.finish(\"three\");";
    const PROSE: &str = "Counting first.";

    fn native_arguments() -> String {
        serde_json::json!({ "code": PROGRAM }).to_string()
    }
    fn core(backend: lash_execution_contracts::backend::Backend) -> Result<LashCore> {
        let provider = lash::testing::TestProvider::builder()
            .kind("cell-record")
            .complete(move |_request| async move {
                Ok(LlmResponse {
                    parts: vec![
                        LlmOutputPart::Text {
                            text: PROSE.to_string(),
                            response_meta: None,
                        },
                        LlmOutputPart::ToolCall {
                            call_id: "provider-call-1".to_string(),
                            tool_name: "execute_code".to_string(),
                            input_json: native_arguments(),
                            replay: None,
                        },
                    ],
                    ..LlmResponse::default()
                })
            })
            .build()
            .into_handle();
        let factory = lash_protocol_rlm::RlmProtocolPluginFactory::new(
            lash_protocol_rlm::RlmProtocolPluginConfig::builder()
                .instruction_limit(lash_protocol_rlm::InstructionBound::instructions(1_000_000))
                .memory_limit(lash_protocol_rlm::MemoryBound::mebibytes(64))
                .build(),
            lash_protocol_rlm::CellDialect::typescript(),
        )
        .with_worker_service(untimed_fixture_workers());
        explicit_ephemeral_facets(LashCore::rlm_builder(backend, factory))
            .serve_test_llm_profile(provider, mock_llm_profile_spec())
            .build(lash_facade_tests::runtime_lease_owner())
    }

    {
        let name = "cell-record-native";
        let backend = sqlite_memory_store_backend().await;
        let session_id = id(name);
        let running = core(backend.clone())?;
        let session = created_session(&running, session_id.clone())
            .await
            .open()
            .await?;
        let events = RecordingEvents::default();
        session
            .send(lash::TurnInput::text("count"))
            .output_into(&events)
            .await?;
        // What the executor reported when the cell settled.
        let completed = events
            .snapshot()
            .await
            .into_iter()
            .find_map(|activity| match activity.event {
                lash::TurnEvent::CodeBlockCompleted {
                    prints,
                    prints_retained,
                    result,
                    ..
                } => Some((prints, prints_retained, result)),
                _ => None,
            })
            .expect("the cell's completion activity");
        drop(session);
        running.shutdown().await?;

        let reopened = core(backend)?;
        let transcript = reopened
            .session(session_id.clone())
            .open()
            .await?
            .durable()
            .transcript()
            .await?;
        let cells = transcript
            .visible()
            .filter_map(|entry| match &entry.item {
                TranscriptItem::Cell(cell) => Some((entry, cell)),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(cells.len(), 1, "{name}: the turn ran one cell");
        let (cell_entry, cell) = cells[0];
        assert_eq!(cell.code, PROGRAM, "{name}");
        assert_eq!(cell.language, "typescript", "{name}");
        assert_eq!(cell_entry.provenance.cell_id.as_ref(), Some(&cell.id));
        // The record's prints and result are the executor's, unconverted.
        assert_eq!(cell.prints.len(), 1, "{name}: {cell:?}");
        assert_eq!(cell.prints[0].value, serde_json::json!("counted"));
        assert!(
            matches!(
                &cell.result,
                CellOutcome::Controlled {
                    tool_name,
                    control: lash_core::CellControl::Finish { value },
                    ..
                } if tool_name == "control.finish"
                    && *value == lash_core::OutputValue::from(serde_json::json!("three"))
            ),
            "{name}: {:?}",
            cell.result
        );
        assert_eq!(
            (&cell.prints, &cell.prints_retained, &cell.result),
            (
                completed.0.as_ref().expect("the executor reported prints"),
                &completed.1,
                &completed.2
            ),
            "{name}: the record holds what the completion activity reported"
        );

        // The reply that ran the cell is an assistant message naming it.
        let contexts = transcript
            .visible()
            .filter(|entry| entry.provenance.cell_id.as_ref() == Some(&cell.id))
            .filter_map(|entry| match &entry.item {
                TranscriptItem::Message(message) => Some(message),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(contexts.len(), 1, "{name}: one assistant context");
        let context = contexts[0];
        assert_eq!(context.role, TranscriptRole::Assistant, "{name}");
        assert!(
            context.blocks.contains(&TranscriptBlock::Text {
                text: PROSE.to_string()
            }),
            "{name}: {context:?}"
        );
        let calls = context
            .blocks
            .iter()
            .filter_map(|block| match block {
                TranscriptBlock::ToolCall {
                    tool_name,
                    arguments,
                    ..
                } => Some((tool_name.as_str(), arguments.clone())),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(calls, [("execute_code", native_arguments())], "{name}");
        reopened.shutdown().await?;
    }
    Ok(())
}

/// Law 5 (FIG-5781): a cell's control call is not a call `max_tool_calls`
/// counts. A session whose cells may make one tool call makes it, and the
/// cell still ends the turn through `control.finish`.
#[cfg(feature = "rlm")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_cell_at_its_tool_call_limit_can_still_finish() -> Result<()> {
    let core =
        explicit_ephemeral_facets(rlm_core_builder_over(sqlite_memory_store_backend().await))
            .serve_test_llm_profile(
                part_provider(
                    "finish-at-the-limit",
                    typescript_block(
                        "const found = await tools.appLookup({});\n\
                         await control.finish(\"looked up once\");",
                    ),
                ),
                mock_llm_profile_spec(),
            )
            .tools(lash::tool_provider_key!("tools"), Arc::new(AppTools))
            .build(lash_facade_tests::runtime_lease_owner())?;
    let spec = lash::SessionSpec::new(
        mock_llm_profile_spec().wire_model,
        lash::TurnBudget::Unbounded,
        lash::MaxToolCalls::new(1),
        lash::NoProgressBudget::bounded(12),
    );
    core.session(id("finish-at-the-limit"))
        .create(lash::SessionCreation::root(spec))
        .await?;
    let session = core.session(id("finish-at-the-limit")).open().await?;
    let settled = session
        .send(lash::TurnInput::text("look up once"))
        .output()
        .await?;
    assert_eq!(
        settled.finished(),
        Some(("control.finish", &serde_json::json!("looked up once"))),
        "{:?}",
        settled.outcome
    );
    assert!(
        lash::testing::tool_calls_for_activity(&session.durable(), &settled.activities)
            .await
            .iter()
            .all(|call| call.output.is_success()),
        "{:?}",
        lash::testing::tool_calls_for_activity(&session.durable(), &settled.activities).await
    );
    drop(session);
    core.shutdown().await?;
    Ok(())
}
