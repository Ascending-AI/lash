use super::*;

#[derive(Debug)]
struct AdvancingDifferentialClock(std::sync::atomic::AtomicU64);

impl AdvancingDifferentialClock {
    fn new(timestamp_ms: u64) -> Self {
        Self(std::sync::atomic::AtomicU64::new(timestamp_ms))
    }
}

#[async_trait::async_trait]
impl Clock for AdvancingDifferentialClock {
    fn now(&self) -> std::time::Instant {
        std::time::Instant::now()
    }

    fn timestamp_datetime(&self) -> chrono::DateTime<chrono::Utc> {
        let timestamp_ms = self.0.load(std::sync::atomic::Ordering::SeqCst);
        chrono::DateTime::from(
            std::time::UNIX_EPOCH + std::time::Duration::from_millis(timestamp_ms),
        )
    }

    async fn sleep(&self, duration: Duration) {
        tokio::time::sleep(duration).await;
    }

    async fn sleep_until(&self, deadline: std::time::Instant) {
        tokio::time::sleep_until(tokio::time::Instant::from_std(deadline)).await;
    }
}

#[test]
fn advancing_differential_clock_wall_clock_faces_agree() {
    let clock = AdvancingDifferentialClock::new(1_700_000_000_123);
    let clock: &dyn lash_core::Clock = &clock;
    let milliseconds = clock.timestamp_ms();
    let datetime = clock.timestamp_datetime();
    let text = chrono::DateTime::parse_from_rfc3339(&clock.timestamp_rfc3339())
        .expect("clock emits RFC 3339");
    assert_eq!(datetime.timestamp_millis() as u64, milliseconds);
    assert_eq!(text.timestamp_millis() as u64, milliseconds);
}

/// A literal-oracle row as a durable process wake from its own process, so
/// every row has a distinct `(process, sequence)` source. The row id rides in
/// the wake input, where [`oracle_row_id`] reads it back.
fn oracle_wake_draft(session_id: &SessionId, row_id: &str) -> QueuedWorkBatchDraft {
    let process_id = || lash_core::runtime::ProcessId::fixture(row_id);
    lash_core::runtime::process_wake_batch_draft(lash_core::runtime::ProcessWakeDelivery {
        version: lash_core::runtime::PROCESS_WAKE_DELIVERY_FORMAT_VERSION,
        wake_id: format!("{row_id}-wake-1"),
        target_session_id: session_id.clone(),
        process_id: process_id(),
        sequence: 1,
        event_type: "process.wake".to_string(),
        event_invocation: lash_core::runtime::RuntimeInvocation {
            attribution: lash_core::runtime::RuntimeAttribution::for_session(session_id.clone()),
            subject: lash_core::runtime::RuntimeSubject::ProcessEvent {
                process_id: process_id(),
                sequence: 1,
                event_type: "process.wake".to_string(),
            },
            caused_by: None,
            replay: None,
        },
        process_caused_by: None,
        authority: lash_core::runtime::QueuedWorkAuthority::default(),
        input: row_id.to_string(),
        created_at_ms: 1,
    })
}

/// The row id an admitted literal-oracle batch carries in its one wake.
fn oracle_row_id(batch: &lash_core::runtime::QueuedWorkBatch) -> String {
    match batch.items.as_slice() {
        [
            lash_core::runtime::QueuedWorkItem {
                payload: lash_core::runtime::QueuedWorkPayload::ProcessWake { wake },
                ..
            },
        ] => wake.input.clone(),
        other => panic!("literal-oracle batch must carry one process wake, got {other:?}"),
    }
}

#[derive(Clone, Copy)]
struct BatchOracleRow {
    id: &'static str,
    merge_key: Option<&'static str>,
}

struct BatchOracleFixture {
    name: &'static str,
    max_rows: usize,
    rows: &'static [BatchOracleRow],
}

const BATCH_ORACLE_FIXTURES: &[BatchOracleFixture] = &[
    BatchOracleFixture {
        name: "max_rows_one",
        max_rows: 1,
        rows: &[
            BatchOracleRow {
                id: "max1-a1",
                merge_key: Some("a"),
            },
            BatchOracleRow {
                id: "max1-a2",
                merge_key: Some("a"),
            },
        ],
    },
    BatchOracleFixture {
        name: "bound_at_key_change",
        max_rows: 2,
        rows: &[
            BatchOracleRow {
                id: "bound-a1",
                merge_key: Some("a"),
            },
            BatchOracleRow {
                id: "bound-a2",
                merge_key: Some("a"),
            },
            BatchOracleRow {
                id: "bound-b1",
                merge_key: Some("b"),
            },
        ],
    },
    BatchOracleFixture {
        name: "never_interleaved",
        max_rows: 64,
        rows: &[
            BatchOracleRow {
                id: "never-a1",
                merge_key: Some("a"),
            },
            BatchOracleRow {
                id: "never-n1",
                merge_key: None,
            },
            BatchOracleRow {
                id: "never-a2",
                merge_key: Some("a"),
            },
        ],
    },
    BatchOracleFixture {
        name: "physical_a_b_a",
        max_rows: 64,
        rows: &[
            BatchOracleRow {
                id: "aba-a1",
                merge_key: Some("a"),
            },
            BatchOracleRow {
                id: "aba-b1",
                merge_key: Some("b"),
            },
            BatchOracleRow {
                id: "aba-a2",
                merge_key: Some("a"),
            },
        ],
    },
];

/// The root the admission-gap oracle interrupts and redrives.
const ADMISSION_GAP_ROOT: &str = "admission-gap-root";

/// Admit `root` headed by the session's first open turn-work batch, composed
/// under `max_rows`; `None` once no turn work is open.
#[expect(
    clippy::expect_used,
    reason = "test support: the literal oracle's store answers each call; a refusal panics the oracle by design"
)]
async fn admit_oracle_root(
    store: &Arc<dyn RuntimeStore>,
    fence: &lash_core::store::DriveFence,
    root: &str,
    max_rows: usize,
) -> Option<lash_core::store::RootAdmission> {
    let head = store
        .list_open_queued_work(fence.session())
        .await
        .expect("list open literal-oracle rows")
        .into_iter()
        .filter(|batch| batch.work_class() == lash_core::store::QueuedWorkClass::TurnWork)
        .min_by_key(|batch| batch.enqueue_seq)?;
    let mut request = lash_core::testing::store_fixtures::admit_root_request_for_test(
        fence,
        &lash_core::TurnId::from(root),
        lash_core::store::AdmittedHead::Batch(head.batch_id),
    );
    request.policy = lash_core::testing::queued_work_claim_policy(max_rows);
    Some(
        store
            .admit_root(&request)
            .await
            .expect("admit literal-oracle root")
            .expect("literal-oracle admission reaches its head"),
    )
}

/// The literal row ids a root admission took, in admission order.
fn admitted_row_ids(admission: &lash_core::store::RootAdmission) -> Vec<String> {
    admission
        .queued
        .iter()
        .flat_map(|queued| queued.batches.iter().map(oracle_row_id))
        .collect()
}

/// End `root` completing every row `admission` took, so the session's next
/// root may be admitted.
#[expect(
    clippy::expect_used,
    reason = "test support: the literal oracle's store answers each call; a refusal panics the oracle by design"
)]
async fn end_oracle_root(
    store: &Arc<dyn RuntimeStore>,
    fence: &lash_core::store::DriveFence,
    root: &str,
    admission: &lash_core::store::RootAdmission,
) {
    let root = lash_core::TurnId::from(root);
    let mut settlement = lash_core::store::IngressSettlement::new(root.clone());
    if let Some(queued) = &admission.queued {
        settlement.completed_batches.push(queued.completion());
    }
    let revision = store
        .load_session_head_meta(fence.session())
        .await
        .expect("load literal-oracle head")
        .map_or(0, |head| head.head_revision);
    let mut commit = lash_core::testing::store_fixtures::settling_commit_for_test(
        runtime_commit(
            fence.session(),
            revision,
            &append(Vec::new(), None),
            None,
            None,
            HydratedSessionCheckpoint::default(),
            Vec::new(),
            Vec::new(),
        ),
        fence,
        settlement,
    );
    let turn = lash_core::store::PhysicalTurn::derive_turn_id(&root, 0);
    // Each root's end is its own commit identity: the root's physical turn,
    // as a driven root's final commit is stamped.
    commit.turn_commit = RuntimeTurnCommitStamp::new(lash_core::store::OperationId::turn(
        fence.session().clone(),
        turn.clone(),
        "literal-oracle-end",
    ));
    commit.root_terminal = Some(Box::new(lash_core::store::RootTerminalWrite {
        commit: lash_core::store::TurnCommitId::new(root.clone(), 0),
        turn,
        root,
        stop: None,
    }));
    store
        .commit_runtime_state(commit)
        .await
        .expect("end literal-oracle root");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "compares three durable backends; requires Postgres (`just push-gate`, or LASH_POSTGRES_DATABASE_URL with `kiln run //crates/lash-sim:cross_backend_store_differential__test -- --include-ignored`)"]
async fn coalesced_batches_match_literal_oracles_on_every_backend() {
    let database_url = match std::env::var("LASH_POSTGRES_DATABASE_URL") {
        Ok(database_url) if !database_url.is_empty() => database_url,
        Ok(_) => {
            assert_ne!(
                std::env::var("LASH_REQUIRE_POSTGRES").as_deref(),
                Ok("1"),
                "LASH_POSTGRES_DATABASE_URL must be non-empty when LASH_REQUIRE_POSTGRES=1"
            );
            eprintln!(
                "SKIPPED literal coalesced-batch oracles; compared_backends=[]; \
                 required_backends=[sqlite-memory,sqlite,postgres]; \
                 reason=LASH_POSTGRES_DATABASE_URL is not set"
            );
            return;
        }
        Err(error) => {
            assert_ne!(
                std::env::var("LASH_REQUIRE_POSTGRES").as_deref(),
                Ok("1"),
                "LASH_POSTGRES_DATABASE_URL must be set when LASH_REQUIRE_POSTGRES=1: {error}"
            );
            eprintln!(
                "SKIPPED literal coalesced-batch oracles; compared_backends=[]; \
                 required_backends=[sqlite-memory,sqlite,postgres]; \
                 reason=LASH_POSTGRES_DATABASE_URL is not set"
            );
            return;
        }
    };
    let mut database_lock = PgConnection::connect(&database_url)
        .await
        .expect("connect Postgres literal-oracle advisory lock");
    sqlx::query("SELECT pg_advisory_lock($1)")
        .bind(SHARED_DATABASE_LOCK_KEY)
        .execute(&mut database_lock)
        .await
        .expect("acquire Postgres literal-oracle advisory lock");
    // Worker open never provisions (FIG-3797): apply the committed artifact,
    // the same step `lash migrate` performs, before opening.
    sqlx::raw_sql(PostgresStorage::schema_ddl())
        .execute(&mut database_lock)
        .await
        .expect("provision the shared Postgres database from schema.sql");
    let postgres = PostgresStorage::connect(&database_url)
        .await
        .expect("connect required Postgres literal-oracle backend");
    let sqlite_root = tempfile::tempdir().expect("create literal-oracle SQLite root");
    let run_nonce = run_nonce();

    for fixture in BATCH_ORACLE_FIXTURES {
        let fixture_root = sqlite_root.path().join(fixture.name);
        let fixture_nonce = format!("{run_nonce}-{}", fixture.name);
        let mut runners = runners_for_case(
            CaseName::QueuedWorkAdmissionReleased,
            &fixture_root,
            &postgres,
            &database_url,
            &fixture_nonce,
        )
        .await;
        for runner in &mut runners {
            let store = runner.store();
            for row in fixture.rows {
                let mut draft = oracle_wake_draft(&runner.session_id, row.id);
                draft.merge_key = row.merge_key.map(str::to_string);
                store
                    .enqueue_queued_work(draft)
                    .await
                    .expect("enqueue literal-oracle row");
            }
            let owner = LeaseOwnerIdentity::opaque(
                format!("literal-oracle-{}", runner.name),
                format!("literal-oracle-{}:incarnation", runner.name),
            );
            let fence = store
                .seal_drive_epoch_for_test(
                    &runner.session_id,
                    &owner,
                    "coalesced-batch-oracle-executor",
                    SESSION_LEASE_TTL_MS,
                )
                .await
                .expect("seal literal-oracle drive")
                .acquired()
                .expect("literal-oracle drive is free");
            let mut observed = Vec::new();
            for index in 0.. {
                let root = format!("literal-oracle-root-{index}");
                let Some(admission) =
                    admit_oracle_root(&store, &fence, &root, fixture.max_rows).await
                else {
                    break;
                };
                observed.push(admitted_row_ids(&admission));
                end_oracle_root(&store, &fence, &root, &admission).await;
            }
            match fixture.name {
                "max_rows_one" => assert_eq!(
                    observed,
                    vec![vec!["max1-a1".to_string()], vec!["max1-a2".to_string()]],
                    "{} backend violated literal batch oracle max_rows_one",
                    runner.name
                ),
                "bound_at_key_change" => assert_eq!(
                    observed,
                    vec![
                        vec!["bound-a1".to_string(), "bound-a2".to_string()],
                        vec!["bound-b1".to_string()],
                    ],
                    "{} backend violated literal batch oracle bound_at_key_change",
                    runner.name
                ),
                "never_interleaved" => assert_eq!(
                    observed,
                    vec![
                        vec!["never-a1".to_string()],
                        vec!["never-n1".to_string()],
                        vec!["never-a2".to_string()],
                    ],
                    "{} backend violated literal batch oracle never_interleaved",
                    runner.name
                ),
                "physical_a_b_a" => assert_eq!(
                    observed,
                    vec![
                        vec!["aba-a1".to_string()],
                        vec!["aba-b1".to_string()],
                        vec!["aba-a2".to_string()],
                    ],
                    "{} backend violated literal batch oracle physical_a_b_a",
                    runner.name
                ),
                other => panic!("missing literal assertion for batch oracle {other}"),
            }
            store
                .supersede_drive_epoch_for_test(&fence)
                .await
                .expect("release literal-oracle drive");
            runner.close_reopened_postgres_pool().await;
        }
    }

    eprintln!(
        "PASSED literal coalesced-batch oracles; \
         compared_backends=[sqlite-memory,sqlite,postgres]; cases=4"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "compares three durable backends; requires Postgres (`just push-gate`, or LASH_POSTGRES_DATABASE_URL with `kiln run //crates/lash-sim:cross_backend_store_differential__test -- --include-ignored`)"]
async fn interrupted_admission_identity_stands_over_a_later_row() {
    let database_url = match std::env::var("LASH_POSTGRES_DATABASE_URL") {
        Ok(database_url) if !database_url.is_empty() => database_url,
        _ => {
            assert_ne!(
                std::env::var("LASH_REQUIRE_POSTGRES").as_deref(),
                Ok("1"),
                "LASH_POSTGRES_DATABASE_URL must be set when LASH_REQUIRE_POSTGRES=1"
            );
            eprintln!(
                "SKIPPED interrupted-admission later-row literal oracle; compared_backends=[]; \
                 required_backends=[sqlite-memory,sqlite,postgres]"
            );
            return;
        }
    };
    let mut database_lock = PgConnection::connect(&database_url)
        .await
        .expect("connect Postgres later-row advisory lock");
    sqlx::query("SELECT pg_advisory_lock($1)")
        .bind(SHARED_DATABASE_LOCK_KEY)
        .execute(&mut database_lock)
        .await
        .expect("acquire Postgres later-row advisory lock");
    // Worker open never provisions (FIG-3797): apply the committed artifact,
    // the same step `lash migrate` performs, before opening.
    sqlx::raw_sql(PostgresStorage::schema_ddl())
        .execute(&mut database_lock)
        .await
        .expect("provision the shared Postgres database from schema.sql");
    let postgres = PostgresStorage::connect(&database_url)
        .await
        .expect("connect required Postgres later-row backend");
    let sqlite_root = tempfile::tempdir().expect("create later-row SQLite root");
    let mut runners = runners_for_case(
        CaseName::QueuedWorkAdmissionReleased,
        sqlite_root.path(),
        &postgres,
        &database_url,
        &format!("{}-admission-gap", run_nonce()),
    )
    .await;

    for runner in &mut runners {
        let store = runner.store();
        for source_key in ["gap-w1", "gap-w3"] {
            store
                .enqueue_queued_work(
                    oracle_wake_draft(&runner.session_id, source_key)
                        .with_merge_key("admission-gap-key"),
                )
                .await
                .expect("enqueue admission-gap literal row");
        }
        let owner = LeaseOwnerIdentity::opaque(
            format!("admission-gap-a-{}", runner.name),
            format!("admission-gap-a-{}:incarnation", runner.name),
        );
        let fence = store
            .seal_drive_epoch_for_test(
                &runner.session_id,
                &owner,
                "coalesced-batch-oracle-executor",
                SESSION_LEASE_TTL_MS,
            )
            .await
            .expect("seal first admission-gap drive")
            .acquired()
            .expect("first admission-gap drive is free");
        let admission = admit_oracle_root(&store, &fence, ADMISSION_GAP_ROOT, 64)
            .await
            .expect("original admission-gap composition exists");
        assert_eq!(
            admitted_row_ids(&admission),
            vec!["gap-w1".to_string(), "gap-w3".to_string()],
            "{} backend changed the initial literal admission-gap composition",
            runner.name
        );
        store
            .supersede_drive_epoch_for_test(&fence)
            .await
            .expect("supersede first admission-gap drive");
        // The gap row arrives only after the interrupted admission exists, so
        // a redrive must answer the admission's own members, never a
        // re-merge.
        store
            .enqueue_queued_work(
                oracle_wake_draft(&runner.session_id, "gap-w2").with_merge_key("admission-gap-key"),
            )
            .await
            .expect("enqueue later admission-gap literal row");
    }

    for runner in &mut runners {
        let store = runner.store();
        let owner = LeaseOwnerIdentity::opaque(
            format!("admission-gap-b-{}", runner.name),
            format!("admission-gap-b-{}:incarnation", runner.name),
        );
        let fence = store
            .seal_drive_epoch_for_test(
                &runner.session_id,
                &owner,
                "coalesced-batch-oracle-executor",
                SESSION_LEASE_TTL_MS,
            )
            .await
            .expect("seal successor admission-gap drive")
            .acquired()
            .expect("successor admission-gap drive is free");
        let mut request = lash_core::testing::store_fixtures::admit_root_request_for_test(
            &fence,
            &lash_core::TurnId::from(ADMISSION_GAP_ROOT),
            lash_core::store::AdmittedHead::Batch(lash_core::BatchId::from("unused-on-replay")),
        );
        request.policy = lash_core::testing::queued_work_claim_policy(64);
        let redriven = store
            .admit_root(&request)
            .await
            .expect("redrive admission-gap root")
            .expect("the recorded admission-gap admission answers the redrive");
        assert_eq!(
            admitted_row_ids(&redriven),
            vec!["gap-w1".to_string(), "gap-w3".to_string()],
            "{} backend did not recover the literal admission identity",
            runner.name
        );
        end_oracle_root(&store, &fence, ADMISSION_GAP_ROOT, &redriven).await;
        let later = admit_oracle_root(&store, &fence, "admission-gap-later-root", 64)
            .await
            .expect("later admission-gap row remains separate");
        assert_eq!(
            admitted_row_ids(&later),
            vec!["gap-w2".to_string()],
            "{} backend did not preserve the literal later-row remainder",
            runner.name
        );
        store
            .supersede_drive_epoch_for_test(&fence)
            .await
            .expect("release successor admission-gap drive");
        runner.close_reopened_postgres_pool().await;
    }

    eprintln!(
        "PASSED interrupted-admission later-row literal oracle; \
         compared_backends=[sqlite-memory,sqlite,postgres]"
    );
}
