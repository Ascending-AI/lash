use super::*;

#[derive(Debug)]
struct AdvancingDifferentialClock(std::sync::atomic::AtomicU64);

impl AdvancingDifferentialClock {
    fn new(timestamp_ms: u64) -> Self {
        Self(std::sync::atomic::AtomicU64::new(timestamp_ms))
    }

    fn advance(&self, duration_ms: u64) {
        self.0
            .fetch_add(duration_ms, std::sync::atomic::Ordering::SeqCst);
    }
}

#[async_trait::async_trait]
impl Clock for AdvancingDifferentialClock {
    fn now(&self) -> std::time::Instant {
        std::time::Instant::now()
    }

    fn timestamp_ms(&self) -> u64 {
        self.0.load(std::sync::atomic::Ordering::SeqCst)
    }

    fn timestamp_rfc3339(&self) -> String {
        self.timestamp_datetime().to_rfc3339()
    }

    fn timestamp_datetime(&self) -> chrono::DateTime<chrono::Utc> {
        chrono::DateTime::from(
            std::time::UNIX_EPOCH + std::time::Duration::from_millis(self.timestamp_ms()),
        )
    }

    async fn sleep(&self, duration: Duration) {
        tokio::time::sleep(duration).await;
    }

    async fn sleep_until(&self, deadline: std::time::Instant) {
        tokio::time::sleep_until(tokio::time::Instant::from_std(deadline)).await;
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

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "compares three durable backends; requires Postgres (`just push-gate`, or LASH_POSTGRES_DATABASE_URL with `cargo test -- --include-ignored` / `cargo nextest run --run-ignored all`)"]
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
                 required_backends=[in-memory,sqlite,postgres]; \
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
                 required_backends=[in-memory,sqlite,postgres]; \
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
    let postgres = PostgresStorage::connect(&database_url)
        .await
        .expect("connect required Postgres literal-oracle backend");
    let sqlite_root = tempfile::tempdir().expect("create literal-oracle SQLite root");
    let run_nonce = run_nonce();

    for fixture in BATCH_ORACLE_FIXTURES {
        let fixture_root = sqlite_root.path().join(fixture.name);
        let fixture_nonce = format!("{run_nonce}-{}", fixture.name);
        let mut runners = runners_for_case(
            CaseName::QueuedWorkClaimAndAbandon,
            &fixture_root,
            &postgres,
            &database_url,
            &fixture_nonce,
        )
        .await;
        for runner in &mut runners {
            let store = runner.store();
            for row in fixture.rows {
                let mut draft = QueuedWorkBatchDraft::new(
                    &runner.session_id,
                    DeliveryPolicy::EarliestSafeBoundary,
                    vec![QueuedWorkPayload::agent_frame_task(
                        "literal-oracle-frame",
                        row.id,
                        None,
                    )],
                )
                .with_source_key(row.id);
                if let Some(merge_key) = row.merge_key {
                    draft = draft.with_merge_key(merge_key);
                }
                store
                    .enqueue_queued_work(draft)
                    .await
                    .expect("enqueue literal-oracle row");
            }
            let owner = LeaseOwnerIdentity::opaque(
                format!("literal-oracle-{}", runner.name),
                format!("literal-oracle-{}:incarnation", runner.name),
            );
            let lease = store
                .try_claim_session_execution_lease(
                    &runner.session_id,
                    &owner,
                    "coalesced-batch-oracle-executor",
                    SESSION_LEASE_TTL_MS,
                )
                .await
                .expect("claim literal-oracle session lease")
                .acquired()
                .expect("literal-oracle session lease is free");
            let mut observed = Vec::new();
            while let Some(claim) = store
                .claim_ready_queued_work(
                    &runner.session_id,
                    &lease.fence(),
                    &owner,
                    QueuedWorkClaimBoundary::Idle,
                    lash_core::testing::queued_work_claim_policy(fixture.max_rows),
                )
                .await
                .expect("claim literal-oracle batch")
                .claim()
            {
                observed.push(
                    claim
                        .batches
                        .iter()
                        .map(|batch| {
                            batch
                                .source_key
                                .clone()
                                .expect("literal-oracle row has a source id")
                        })
                        .collect::<Vec<_>>(),
                );
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
                .release_session_execution_lease(&lease.completion())
                .await
                .expect("release literal-oracle session lease");
            runner.close_reopened_postgres_pool().await;
        }
    }

    eprintln!(
        "PASSED literal coalesced-batch oracles; \
         compared_backends=[in-memory,sqlite,postgres]; cases=4"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "compares three durable backends; requires Postgres (`just push-gate`, or LASH_POSTGRES_DATABASE_URL with `cargo test -- --include-ignored` / `cargo nextest run --run-ignored all`)"]
async fn interrupted_claim_identity_crosses_a_newly_ready_physical_gap() {
    let database_url = match std::env::var("LASH_POSTGRES_DATABASE_URL") {
        Ok(database_url) if !database_url.is_empty() => database_url,
        _ => {
            assert_ne!(
                std::env::var("LASH_REQUIRE_POSTGRES").as_deref(),
                Ok("1"),
                "LASH_POSTGRES_DATABASE_URL must be set when LASH_REQUIRE_POSTGRES=1"
            );
            eprintln!(
                "SKIPPED interrupted-claim ready-gap literal oracle; compared_backends=[]; \
                 required_backends=[in-memory,sqlite,postgres]"
            );
            return;
        }
    };
    let mut database_lock = PgConnection::connect(&database_url)
        .await
        .expect("connect Postgres ready-gap advisory lock");
    sqlx::query("SELECT pg_advisory_lock($1)")
        .bind(SHARED_DATABASE_LOCK_KEY)
        .execute(&mut database_lock)
        .await
        .expect("acquire Postgres ready-gap advisory lock");
    let postgres = PostgresStorage::connect(&database_url)
        .await
        .expect("connect required Postgres ready-gap backend");
    let sqlite_root = tempfile::tempdir().expect("create ready-gap SQLite root");
    let now_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock is after Unix epoch")
        .as_millis() as u64;
    let delayed_at_ms = now_ms + 2_000;
    let clock = Arc::new(AdvancingDifferentialClock::new(now_ms));
    let mut runners = runners_for_case_with_clock(
        CaseName::QueuedWorkClaimAndAbandon,
        sqlite_root.path(),
        &postgres,
        &database_url,
        &format!("{}-ready-gap", run_nonce()),
        Arc::clone(&clock) as Arc<dyn Clock>,
    )
    .await;

    for runner in &mut runners {
        let store = runner.store();
        for (source_key, available_at_ms) in
            [("gap-w1", 0), ("gap-w2", delayed_at_ms), ("gap-w3", 0)]
        {
            store
                .enqueue_queued_work(
                    QueuedWorkBatchDraft::new(
                        &runner.session_id,
                        DeliveryPolicy::EarliestSafeBoundary,
                        vec![QueuedWorkPayload::agent_frame_task(
                            "ready-gap-frame",
                            source_key,
                            None,
                        )],
                    )
                    .with_source_key(source_key)
                    .with_merge_key("ready-gap-key")
                    .with_available_at_ms(available_at_ms),
                )
                .await
                .expect("enqueue ready-gap literal row");
        }
        let owner = LeaseOwnerIdentity::opaque(
            format!("ready-gap-a-{}", runner.name),
            format!("ready-gap-a-{}:incarnation", runner.name),
        );
        let lease = store
            .try_claim_session_execution_lease(
                &runner.session_id,
                &owner,
                "coalesced-batch-oracle-executor",
                SESSION_LEASE_TTL_MS,
            )
            .await
            .expect("claim first ready-gap session lease")
            .acquired()
            .expect("first ready-gap session lease is free");
        let claim = store
            .claim_ready_queued_work(
                &runner.session_id,
                &lease.fence(),
                &owner,
                QueuedWorkClaimBoundary::Idle,
                lash_core::testing::queued_work_claim_policy(64),
            )
            .await
            .expect("claim original ready-gap composition")
            .claim()
            .expect("original ready-gap composition exists");
        assert_eq!(
            claim
                .batches
                .iter()
                .map(|batch| batch.source_key.clone().expect("source key is present"))
                .collect::<Vec<_>>(),
            vec!["gap-w1".to_string(), "gap-w3".to_string()],
            "{} backend changed the initial literal ready-gap composition",
            runner.name
        );
        store
            .release_session_execution_lease(&lease.completion())
            .await
            .expect("release first ready-gap session lease");
    }

    clock.advance(4_000);
    let wait_ms = delayed_at_ms.saturating_sub(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock is after Unix epoch")
            .as_millis() as u64,
    );
    tokio::time::sleep(Duration::from_millis(wait_ms + 50)).await;

    for runner in &mut runners {
        let store = runner.store();
        let owner = LeaseOwnerIdentity::opaque(
            format!("ready-gap-b-{}", runner.name),
            format!("ready-gap-b-{}:incarnation", runner.name),
        );
        let lease = store
            .try_claim_session_execution_lease(
                &runner.session_id,
                &owner,
                "coalesced-batch-oracle-executor",
                SESSION_LEASE_TTL_MS,
            )
            .await
            .expect("claim successor ready-gap session lease")
            .acquired()
            .expect("successor ready-gap session lease is free");
        let redriven = store
            .claim_ready_queued_work(
                &runner.session_id,
                &lease.fence(),
                &owner,
                QueuedWorkClaimBoundary::Idle,
                lash_core::testing::queued_work_claim_policy(64),
            )
            .await
            .expect("redrive ready-gap composition")
            .claim()
            .expect("ready-gap composition remains reclaimable");
        assert_eq!(
            redriven
                .batches
                .iter()
                .map(|batch| batch.source_key.clone().expect("source key is present"))
                .collect::<Vec<_>>(),
            vec!["gap-w1".to_string(), "gap-w3".to_string()],
            "{} backend did not recover the literal claim identity",
            runner.name
        );
        let delayed = store
            .claim_ready_queued_work(
                &runner.session_id,
                &lease.fence(),
                &owner,
                QueuedWorkClaimBoundary::Idle,
                lash_core::testing::queued_work_claim_policy(64),
            )
            .await
            .expect("claim delayed ready-gap row")
            .claim()
            .expect("delayed ready-gap row remains separate");
        assert_eq!(
            delayed
                .batches
                .iter()
                .map(|batch| batch.source_key.clone().expect("source key is present"))
                .collect::<Vec<_>>(),
            vec!["gap-w2".to_string()],
            "{} backend did not preserve the literal delayed-row remainder",
            runner.name
        );
        store
            .release_session_execution_lease(&lease.completion())
            .await
            .expect("release successor ready-gap session lease");
        runner.close_reopened_postgres_pool().await;
    }

    eprintln!(
        "PASSED interrupted-claim ready-gap literal oracle; \
         compared_backends=[in-memory,sqlite,postgres]"
    );
}
