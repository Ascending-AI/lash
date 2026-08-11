use super::*;

#[derive(Clone, Copy)]
struct BatchOracleRow {
    id: &'static str,
    merge_key: Option<&'static str>,
}

struct BatchOracleFixture {
    name: &'static str,
    max_rows: usize,
    rows: &'static [BatchOracleRow],
    expected_batches: &'static [&'static [&'static str]],
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
        expected_batches: &[&["max1-a1"], &["max1-a2"]],
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
        expected_batches: &[&["bound-a1", "bound-a2"], &["bound-b1"]],
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
        expected_batches: &[&["never-a1"], &["never-n1"], &["never-a2"]],
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
        expected_batches: &[&["aba-a1"], &["aba-b1"], &["aba-a2"]],
    },
];

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
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
                .try_claim_session_execution_lease(&runner.session_id, &owner, SESSION_LEASE_TTL_MS)
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
            let expected = fixture
                .expected_batches
                .iter()
                .map(|batch| batch.iter().map(|id| (*id).to_string()).collect::<Vec<_>>())
                .collect::<Vec<_>>();
            assert_eq!(
                observed, expected,
                "{} backend violated literal batch oracle {}",
                runner.name, fixture.name
            );
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
