//! ADR 0025 §5 crash cuts and ADR 0016 terminal precedence on current stores.

use super::*;

struct Fixture {
    stores: Arc<dyn lash_core::StoreSet>,
    _directory: tempfile::TempDir,
    _database: Option<lash_postgres_store::testing::IsolatedDatabase>,
}

impl Fixture {
    async fn sqlite(file: bool) -> Self {
        let directory = tempfile::tempdir().expect("SQLite fixture directory");
        let stores = if file {
            lash_sqlite_store::SqliteStoreSet::open(directory.path().join("sqlite.db")).await
        } else {
            lash_sqlite_store::SqliteStoreSet::memory().await
        }
        .expect("SQLite store set");
        Self {
            stores: Arc::new(stores),
            _directory: directory,
            _database: None,
        }
    }

    #[expect(
        clippy::disallowed_methods,
        reason = "service fixture reads the isolated PostgreSQL gate configuration"
    )]
    async fn postgres() -> Self {
        let url = std::env::var("LASH_POSTGRES_DATABASE_URL").expect("PostgreSQL gate URL");
        let database = lash_postgres_store::testing::IsolatedDatabase::create(&url).await;
        let storage = lash_postgres_store::PostgresStorage::connect(database.url())
            .await
            .expect("PostgreSQL storage");
        let directory = tempfile::tempdir().expect("attachment directory");
        Self {
            stores: Arc::new(lash_postgres_store::PostgresStoreSet::new(
                &storage,
                Arc::new(lash_core::facade_support::FileAttachmentStore::new(
                    directory.path(),
                )),
            )),
            _directory: directory,
            _database: Some(database),
        }
    }
}

async fn handover_recovers_at_every_cut(fixture: &Fixture) {
    // Handover, Send and Retire reuse the cuts in the build-roll laws.
    // Before cancel-forward isolates the first journaled step after the send.
    for point in [
        CrashPoint::BeforeRunResult {
            name: Some("lash.segment.handover".into()),
        },
        CrashPoint::BeforeFrame {
            ty: MessageType::OneWayCallCommand,
        },
        CrashPoint::BeforeRun {
            name: "lash.segment.cancel-forward".into(),
        },
        CrashPoint::BeforeRunResult {
            name: Some("lash.segment.retire".into()),
        },
    ] {
        let roll = Roll::start_on(seed(), PROGRAM, false, Arc::clone(&fixture.stores), true).await;
        let process_id = roll.register_process().await;
        let awaiter = roll.arm_awaiter(&process_id).await;
        let key = process_segment_workflow_key(&process_id, HANDING_OVER);
        roll.server.crash_on(
            CrashRule::new(point.clone())
                .service(PROCESS_WORKFLOW)
                .handler("run")
                .key(&key),
        );
        roll.send_segment_zero(&process_id).await;
        let expected = process_success(serde_json::json!({ "build": "N" }));
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(60), awaiter)
                .await
                .expect("handover recovers")
                .expect("awaiter task")
                .expect("terminal"),
            expected,
            "{point:?}: one terminal outcome",
        );
        // Let the predecessor finish retirement even if its successor ended first.
        roll.wait_for(|roll| {
            roll.invocations_of(&format!("{PROCESS_WORKFLOW}/{key}/run"))
                .iter()
                .all(|view| view.status == "completed")
        })
        .await;
        roll.settle().await;
        assert_eq!(
            roll.server.stats().crashes,
            1,
            "{point:?}: the requested cut fires"
        );
        assert!(
            roll.server.stats().replays > 0,
            "{point:?}: forced replay runs"
        );
        let successor = format!(
            "{PROCESS_WORKFLOW}/{}/run",
            process_segment_workflow_key(&process_id, SUCCESSOR)
        );
        assert_eq!(
            roll.invocations_of(&successor).len(),
            1,
            "{point:?}: one successor"
        );
        assert_eq!(roll.record(&process_id).await.outcome(), Some(expected));
        assert_eq!(
            roll.invocations_of(&format!(
                "{PROCESS_WORKFLOW}/{process_id}/complete_terminal"
            ))
            .len(),
            1,
            "{point:?}: one terminal delivery"
        );
        {
            let resumptions = roll.runner_n.resumptions.lock_recover();
            for ordinal in 0..SEGMENTS {
                let restored: Vec<_> = resumptions
                    .iter()
                    .filter(|(seen, _)| *seen == ordinal)
                    .collect();
                assert!(
                    !restored.is_empty(),
                    "{point:?}: segment {ordinal} executes"
                );
                let expected = (ordinal > 0).then(|| lash_core::SegmentHandover {
                    reason: lash_core::BoundaryReason::JournalBudget,
                    program_hash: PROGRAM.into(),
                    engine_state: vec![u8::try_from(ordinal - 1).expect("small ordinal")],
                });
                assert!(
                    restored.iter().all(|(_, handover)| *handover == expected),
                    "{point:?}: every replay resumes the exact continuation: {restored:?}"
                );
            }
        }
        assert!(
            roll.continuations
                .get_segment_handover(&process_id, HANDING_OVER)
                .await
                .expect("retired continuation read")
                .is_none()
        );
        let retained = roll
            .continuations
            .get_segment_handover(&process_id, SUCCESSOR)
            .await
            .expect("successor continuation read")
            .expect("retained successor");
        assert_eq!(retained.segment_ordinal, SUCCESSOR);
        assert_eq!(retained.handover.engine_state, vec![1]);
        assert_eq!(retained.handover.program_hash, PROGRAM);
        assert_eq!(retained.route, PROCESS_WORKFLOW);
        assert_eq!(
            roll.continuations
                .latest_segment_handover(&process_id)
                .await
                .expect("latest continuation"),
            Some(retained)
        );
    }
}

#[derive(Debug)]
struct NoIngress;

#[async_trait::async_trait]
impl HttpTransport for NoIngress {
    async fn send(
        &self,
        _request: HttpRequest,
        _timeout: Option<Duration>,
    ) -> Result<HttpResponse, LlmTransportError> {
        panic!("retained outcomes never contact ingress");
    }
}

async fn a_wait_answers_the_record_state_on_every_host(fixture: &Fixture) {
    let faults = Arc::new(lash_core::testing::ProcessRegistryFaults::new(
        fixture.stores.process_registry(),
    ));
    let registry: Arc<dyn ProcessRegistry> = faults.clone();
    let mut record = registry
        .register_process(held_registration())
        .await
        .expect("register");
    let process_id = record.id.clone();
    let watched = lash_core::facade_support::watch_process_registry(Arc::clone(&registry));
    let hosts: Vec<(&str, Box<dyn lash_core::ProcessWorkSubstrate>)> = vec![
        (
            "polling",
            Box::new(lash_core::NoProcessWork::for_registry(Arc::clone(
                &registry,
            ))),
        ),
        ("watched", Box::new(lash_core::NoProcessWork::new(&watched))),
        (
            "restate",
            Box::new(RestateProcessIngressRunner::new(
                RestateConnection::with_transport("https://restate.invalid", Arc::new(NoIngress)),
                registry,
                fixture.stores.process_continuations(),
                lash_core::engine::EngineGeneration::fixed(crate::tests::test_build_generation()),
            )),
        ),
    ];
    let expected = process_success(serde_json::json!({ "retained": true }));
    record.lifecycle = lash_core::ProcessLifecycleState::Terminal {
        outcome: expected.clone().try_into().expect("a terminal outcome"),
    };
    for (host, work) in &hosts {
        faults.set_process_read_override(record.clone());
        let reads = faults.process_point_reads();
        let result = tokio::time::timeout(
            Duration::from_secs(5),
            work.await_process_terminal(&process_id),
        )
        .await
        .expect("wait resolves immediately");
        assert_eq!(
            faults.process_point_reads(),
            reads + 1,
            "{host}: one point read resolves the wait"
        );
        assert_eq!(
            result.expect(host),
            lash_core::ProcessTerminalWait::Terminal(expected.clone()),
            "{host}: a retained outcome resolves the wait"
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn handover_crash_cuts_on_sqlite_memory_with_forced_replay() {
    handover_recovers_at_every_cut(&Fixture::sqlite(false).await).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn releasing_events_during_handover_preserves_replay_and_live_dependencies() {
    let fixture = Fixture::sqlite(false).await;
    let roll = Roll::start_on(seed(), PROGRAM, false, Arc::clone(&fixture.stores), true).await;
    let process_id = roll.register_process().await;
    let awaiter = roll.arm_awaiter(&process_id).await;
    let registry = Arc::clone(&roll.registry);
    let continuations = Arc::clone(&roll.continuations);
    let pid = process_id.clone();
    roll.gated.arm(
        GatePoint::AfterHandoverPut(SUCCESSOR),
        Box::pin(async move {
            let record = registry
                .get_process(&pid)
                .await
                .expect("process read")
                .expect("process");
            let handover = continuations
                .get_segment_handover(&pid, SUCCESSOR)
                .await
                .expect("handover read")
                .expect("the unjournaled handover is stored");
            let wakes = registry
                .list_wake_deliveries(None)
                .await
                .expect("wake read");
            assert!(!record.is_terminal());
            assert!(record.last_event_sequence > 0);
            let released = registry
                .release_process_events(&pid, record.last_event_sequence)
                .await
                .expect("host releases the running process's history");
            assert_eq!(released.released_through, record.last_event_sequence);
            assert!(released.released_events > 0);
            assert_eq!(
                registry.get_process(&pid).await.expect("process read"),
                Some(record)
            );
            assert_eq!(
                continuations
                    .get_segment_handover(&pid, SUCCESSOR)
                    .await
                    .expect("handover read"),
                Some(handover),
                "release preserves the successor's replay authority"
            );
            assert_eq!(
                registry
                    .list_wake_deliveries(None)
                    .await
                    .expect("wake read"),
                wakes,
                "release preserves delivery content and claims"
            );
        }),
    );
    let key = process_segment_workflow_key(&process_id, HANDING_OVER);
    roll.server.crash_on(
        CrashRule::new(CrashPoint::BeforeRunResult {
            name: Some("lash.segment.handover".into()),
        })
        .service(PROCESS_WORKFLOW)
        .handler("run")
        .key(&key),
    );
    roll.send_segment_zero(&process_id).await;
    let expected = process_success(serde_json::json!({ "build": "N" }));
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(60), awaiter)
            .await
            .expect("released-history replay completes")
            .expect("awaiter task")
            .expect("terminal"),
        expected
    );
    roll.wait_for(|roll| {
        roll.invocations_of(&format!("{PROCESS_WORKFLOW}/{key}/run"))
            .iter()
            .all(|view| view.status == "completed")
    })
    .await;
    roll.settle().await;
    assert!(
        !roll.gated.is_armed(),
        "the host release executed at the cut"
    );
    assert_eq!(roll.server.stats().crashes, 1);
    assert!(roll.server.stats().replays > 0);
    assert_eq!(roll.record(&process_id).await.outcome(), Some(expected));
    assert_eq!(
        roll.invocations_of(&format!(
            "{PROCESS_WORKFLOW}/{}/run",
            process_segment_workflow_key(&process_id, SUCCESSOR)
        ))
        .len(),
        1,
        "cleanup and replay schedule one successor"
    );
    let resumed = roll.runner_n.resumptions.lock_recover();
    let expected_handover = lash_core::SegmentHandover {
        reason: lash_core::BoundaryReason::JournalBudget,
        program_hash: PROGRAM.into(),
        engine_state: vec![1],
    };
    let successors: Vec<_> = resumed
        .iter()
        .filter(|(ordinal, _)| *ordinal == SUCCESSOR)
        .collect();
    assert!(!successors.is_empty());
    assert!(
        successors
            .iter()
            .all(|(_, handover)| *handover == Some(expected_handover.clone()))
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn handover_crash_cuts_on_sqlite_file_with_forced_replay() {
    handover_recovers_at_every_cut(&Fixture::sqlite(true).await).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires PostgreSQL; run in kiln gate with pg16"]
async fn postgres_ingress_handover_crash_cuts_with_forced_replay() {
    handover_recovers_at_every_cut(&Fixture::postgres().await).await;
}

#[tokio::test]
async fn a_wait_answers_the_retained_outcome_on_sqlite_memory() {
    a_wait_answers_the_record_state_on_every_host(&Fixture::sqlite(false).await).await;
}

#[tokio::test]
async fn a_wait_answers_the_retained_outcome_on_sqlite_file() {
    a_wait_answers_the_record_state_on_every_host(&Fixture::sqlite(true).await).await;
}

#[tokio::test]
#[ignore = "requires PostgreSQL; run in kiln gate with pg16"]
async fn postgres_ingress_a_wait_answers_the_retained_outcome() {
    a_wait_answers_the_record_state_on_every_host(&Fixture::postgres().await).await;
}
