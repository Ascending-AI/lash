use super::*;
use lash_core::runtime::{QueuedDrainPolicy, QueuedDrainRequest, QueuedDrainSelection};

#[derive(Clone, Copy, Debug)]
enum OracleDrainPolicy {
    All,
    SameKeyAndAuthority,
}

impl QueuedDrainPolicy for OracleDrainPolicy {
    fn name(&self) -> &str {
        match self {
            Self::All => "oracle_all",
            Self::SameKeyAndAuthority => "oracle_same_key_and_authority",
        }
    }

    fn select_drain(&self, request: &QueuedDrainRequest<'_>) -> QueuedDrainSelection {
        match self {
            Self::All => QueuedDrainSelection::everything(request),
            Self::SameKeyAndAuthority => {
                let Some(head) = request.candidates().first() else {
                    return QueuedDrainSelection::head_only();
                };
                if head.merge_key.is_none() {
                    return QueuedDrainSelection::head_only();
                }
                QueuedDrainSelection::leading(
                    request
                        .candidates()
                        .iter()
                        .take_while(|row| {
                            row.merge_key == head.merge_key && row.authority == head.authority
                        })
                        .count(),
                )
            }
        }
    }
}

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
        target_session_id: session_id.clone(),
        process_id: process_id(),
        sequence: 1,
        event_type: "process.wake".to_string(),
        process_caused_by: None,
        authority: lash_core::runtime::QueuedWorkAuthority::default(),
        input: row_id.to_string(),
        created_at_ms: 1,
        trace_cause: Default::default(),
    })
}

/// The row id an admitted literal-oracle batch carries in its one wake.
fn oracle_row_id(batch: &lash_core::runtime::QueuedWorkBatch) -> String {
    match &batch.payload {
        lash_core::runtime::QueuedWorkPayload::ProcessWake { wake } => wake.input.clone(),
        other => panic!("literal-oracle batch must carry one process wake, got {other:?}"),
    }
}

#[derive(Clone, Copy)]
struct BatchOracleRow {
    id: &'static str,
    merge_key: Option<&'static str>,
    principal: Option<&'static str>,
    elevation: Option<&'static str>,
}

impl BatchOracleRow {
    const fn new(id: &'static str, merge_key: Option<&'static str>) -> Self {
        Self {
            id,
            merge_key,
            principal: None,
            elevation: None,
        }
    }

    const fn with_authority(
        mut self,
        principal: &'static str,
        elevation: Option<&'static str>,
    ) -> Self {
        self.principal = Some(principal);
        self.elevation = elevation;
        self
    }
}

struct BatchOracleFixture {
    name: &'static str,
    max_rows: usize,
    rows: &'static [BatchOracleRow],
    all: &'static [&'static [&'static str]],
    grouped: &'static [&'static [&'static str]],
}

const BATCH_ORACLE_FIXTURES: &[BatchOracleFixture] = &[
    BatchOracleFixture {
        name: "max_rows_one",
        max_rows: 1,
        rows: &[
            BatchOracleRow::new("max1-a1", Some("a")),
            BatchOracleRow::new("max1-a2", Some("a")),
        ],
        all: &[&["max1-a1"], &["max1-a2"]],
        grouped: &[&["max1-a1"], &["max1-a2"]],
    },
    BatchOracleFixture {
        name: "bound_at_key_change",
        max_rows: 2,
        rows: &[
            BatchOracleRow::new("bound-a1", Some("a")),
            BatchOracleRow::new("bound-a2", Some("a")),
            BatchOracleRow::new("bound-b1", Some("b")),
        ],
        all: &[&["bound-a1", "bound-a2"], &["bound-b1"]],
        grouped: &[&["bound-a1", "bound-a2"], &["bound-b1"]],
    },
    BatchOracleFixture {
        name: "same_key_max_rows",
        max_rows: 2,
        rows: &[
            BatchOracleRow::new("max2-a1", Some("a")),
            BatchOracleRow::new("max2-a2", Some("a")),
            BatchOracleRow::new("max2-a3", Some("a")),
        ],
        all: &[&["max2-a1", "max2-a2"], &["max2-a3"]],
        grouped: &[&["max2-a1", "max2-a2"], &["max2-a3"]],
    },
    BatchOracleFixture {
        name: "never_interleaved",
        max_rows: 64,
        rows: &[
            BatchOracleRow::new("never-a1", Some("a")),
            BatchOracleRow::new("never-n1", None),
            BatchOracleRow::new("never-n2", None),
            BatchOracleRow::new("never-a2", Some("a")),
        ],
        all: &[&["never-a1", "never-n1", "never-n2", "never-a2"]],
        grouped: &[&["never-a1"], &["never-n1"], &["never-n2"], &["never-a2"]],
    },
    BatchOracleFixture {
        name: "physical_a_b_a",
        max_rows: 64,
        rows: &[
            BatchOracleRow::new("aba-a1", Some("a")),
            BatchOracleRow::new("aba-b1", Some("b")),
            BatchOracleRow::new("aba-a2", Some("a")),
        ],
        all: &[&["aba-a1", "aba-b1", "aba-a2"]],
        grouped: &[&["aba-a1"], &["aba-b1"], &["aba-a2"]],
    },
    BatchOracleFixture {
        name: "principal_a_b_a",
        max_rows: 64,
        rows: &[
            BatchOracleRow::new("principal-a1", Some("a")).with_authority("alice", None),
            BatchOracleRow::new("principal-a2", Some("a")).with_authority("alice", None),
            BatchOracleRow::new("principal-b1", Some("a")).with_authority("bob", None),
            BatchOracleRow::new("principal-a3", Some("a")).with_authority("alice", None),
        ],
        all: &[&[
            "principal-a1",
            "principal-a2",
            "principal-b1",
            "principal-a3",
        ]],
        grouped: &[
            &["principal-a1", "principal-a2"],
            &["principal-b1"],
            &["principal-a3"],
        ],
    },
    BatchOracleFixture {
        name: "elevation_a_b_a",
        max_rows: 64,
        rows: &[
            BatchOracleRow::new("elevation-a1", Some("a")).with_authority("alice", None),
            BatchOracleRow::new("elevation-b1", Some("a")).with_authority("alice", Some("admin")),
            BatchOracleRow::new("elevation-a2", Some("a")).with_authority("alice", None),
        ],
        all: &[&["elevation-a1", "elevation-b1", "elevation-a2"]],
        grouped: &[&["elevation-a1"], &["elevation-b1"], &["elevation-a2"]],
    },
];

/// The run the admission-gap oracle interrupts and redrives.
const ADMISSION_GAP_RUN: &str = "admission-gap-run";

/// Admit `run` headed by the session's first open turn-work batch, composed
/// under `max_rows` and the explicit host drain policy; `None` once no turn work is open.
#[expect(
    clippy::expect_used,
    reason = "test support: the literal oracle's store answers each call; a refusal panics the oracle by design"
)]
async fn admit_oracle_run(
    store: &Arc<dyn RuntimeStore>,
    fence: &lash_core::store::ShiftFence,
    run: &str,
    max_rows: usize,
    drain_policy: OracleDrainPolicy,
) -> Option<lash_core::store::RunAdmission> {
    let head = store
        .list_open_queued_work(fence.session())
        .await
        .expect("list open literal-oracle rows")
        .into_iter()
        .filter(|batch| batch.work_class() == lash_core::store::QueuedWorkClass::TurnWork)
        .min_by_key(|batch| batch.enqueue_seq)?;
    let mut request = lash_core::testing::store_fixtures::admit_run_request_for_test(
        fence,
        &lash_core::TurnId::fixture(run),
        lash_core::store::AdmittedHead::Batch(head.batch_id),
    );
    request.policy = lash_core::testing::queued_work_admission_policy(max_rows);
    request.policy.drain_policy = Arc::new(drain_policy);
    Some(
        store
            .admit_run(&request)
            .await
            .expect("admit literal-oracle run")
            .expect("literal-oracle admission reaches its head"),
    )
}

/// The literal row ids a run admission took, in admission order.
fn admitted_row_ids(admission: &lash_core::store::RunAdmission) -> Vec<String> {
    admission
        .queued
        .iter()
        .flat_map(|queued| queued.batches.iter().map(oracle_row_id))
        .collect()
}

/// End `run` completing every row `admission` took, so the session's next
/// run may be admitted.
#[expect(
    clippy::expect_used,
    reason = "test support: the literal oracle's store answers each call; a refusal panics the oracle by design"
)]
async fn end_oracle_run(
    store: &Arc<dyn RuntimeStore>,
    fence: &lash_core::store::ShiftFence,
    run: &str,
    admission: &lash_core::store::RunAdmission,
) {
    let run = lash_core::TurnId::fixture(run);
    let mut settlement = lash_core::store::IngressSettlement::new(run.clone());
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
            HydratedSessionCheckpoint::default(),
            Vec::new(),
        ),
        fence,
        settlement,
    );
    let turn = lash_core::store::PhysicalTurn::derive_turn_id(&run, 0);
    // Each run's end is its own commit identity: the run's physical turn,
    // as a executed run's final commit is stamped.
    commit.turn_commit = RuntimeTurnCommitStamp::new(lash_core::store::OperationId::turn(
        fence.session().clone(),
        turn.clone(),
        "literal-oracle-end",
    ));
    commit.run_terminal = Some(Box::new(lash_core::store::RunTerminalWrite {
        commit: lash_core::store::TurnCommitId::new(run.clone(), 0),
        turn,
        run,
        outcome: lash_core::store::RunCommittedOutcome::Finished(
            lash_core::facade_support::TurnFinish::AssistantMessage {
                text: String::new(),
            },
        ),
    }));
    store
        .commit_runtime_state(commit)
        .await
        .expect("end literal-oracle run");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "compares three durable backends; requires Postgres (`just push-gate`, or `LASH_POSTGRES_DATABASE_URL=... just cross-backend-store-soak`)"]
async fn coalesced_batches_match_literal_oracles_on_every_backend() {
    let database_url = lash_postgres_store::testing::required_database_url();
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
        for (policy, expected) in [
            (OracleDrainPolicy::All, fixture.all),
            (OracleDrainPolicy::SameKeyAndAuthority, fixture.grouped),
        ] {
            let fixture_root = sqlite_root.path().join(fixture.name).join(policy.name());
            let fixture_nonce = format!("{run_nonce}-{}-{}", fixture.name, policy.name());
            let mut runners = runners_for_case(
                CaseName::QueuedWorkAdmissionReleased,
                &fixture_root,
                &postgres,
                &database_url,
                &fixture_nonce,
            )
            .await;
            let mut baseline = None;
            for runner in &mut runners {
                let store = runner.store();
                for row in fixture.rows {
                    let mut draft = oracle_wake_draft(&runner.session_id, row.id);
                    draft.merge_key = row.merge_key.map(str::to_string);
                    draft.authority = QueuedWorkAuthority {
                        principal: row.principal.map(str::to_string),
                        elevation: row.elevation.map(str::to_string),
                    };
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
                    .seal_shift_epoch_for_test(
                        &runner.session_id,
                        &owner,
                        "coalesced-batch-oracle-executor",
                        SESSION_LEASE_TTL_MS,
                    )
                    .await
                    .expect("seal literal-oracle shift")
                    .acquired()
                    .expect("literal-oracle shift is free");
                let mut observed = Vec::new();
                for index in 0.. {
                    let run = format!("literal-oracle-run-{index}");
                    let Some(admission) =
                        admit_oracle_run(&store, &fence, &run, fixture.max_rows, policy).await
                    else {
                        break;
                    };
                    observed.push(admitted_row_ids(&admission));
                    end_oracle_run(&store, &fence, &run, &admission).await;
                }
                assert_eq!(
                    observed,
                    expected,
                    "{} backend violated literal batch oracle {} under {}",
                    runner.name,
                    fixture.name,
                    policy.name()
                );
                if let Some(baseline) = &baseline {
                    assert_eq!(
                        &observed,
                        baseline,
                        "{} backend diverged for {} under {}",
                        runner.name,
                        fixture.name,
                        policy.name()
                    );
                } else {
                    baseline = Some(observed);
                }
                store
                    .supersede_shift_epoch_for_test(&fence)
                    .await
                    .expect("release literal-oracle shift");
                runner.close_reopened_postgres_pool().await;
            }
        }
    }

    eprintln!(
        "PASSED literal coalesced-batch oracles; \
         compared_backends=[sqlite-memory,sqlite,postgres]; fixtures={}; policies=2; cases={}",
        BATCH_ORACLE_FIXTURES.len(),
        BATCH_ORACLE_FIXTURES.len() * 2
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "compares three durable backends; requires Postgres (`just push-gate`, or `LASH_POSTGRES_DATABASE_URL=... just cross-backend-store-soak`)"]
async fn interrupted_admission_identity_stands_over_a_later_row() {
    let database_url = lash_postgres_store::testing::required_database_url();
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
            .seal_shift_epoch_for_test(
                &runner.session_id,
                &owner,
                "coalesced-batch-oracle-executor",
                SESSION_LEASE_TTL_MS,
            )
            .await
            .expect("seal first admission-gap shift")
            .acquired()
            .expect("first admission-gap shift is free");
        let admission = admit_oracle_run(
            &store,
            &fence,
            ADMISSION_GAP_RUN,
            64,
            OracleDrainPolicy::All,
        )
        .await
        .expect("original admission-gap composition exists");
        assert_eq!(
            admitted_row_ids(&admission),
            vec!["gap-w1".to_string(), "gap-w3".to_string()],
            "{} backend changed the initial literal admission-gap composition",
            runner.name
        );
        store
            .supersede_shift_epoch_for_test(&fence)
            .await
            .expect("supersede first admission-gap shift");
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
            .seal_shift_epoch_for_test(
                &runner.session_id,
                &owner,
                "coalesced-batch-oracle-executor",
                SESSION_LEASE_TTL_MS,
            )
            .await
            .expect("seal successor admission-gap shift")
            .acquired()
            .expect("successor admission-gap shift is free");
        let mut request = lash_core::testing::store_fixtures::admit_run_request_for_test(
            &fence,
            &lash_core::TurnId::from(ADMISSION_GAP_RUN),
            lash_core::store::AdmittedHead::Batch(lash_core::BatchId::from("unused-on-replay")),
        );
        request.policy = lash_core::testing::queued_work_admission_policy(64);
        let redriven = store
            .admit_run(&request)
            .await
            .expect("redrive admission-gap run")
            .expect("the recorded admission-gap admission answers the redrive");
        assert_eq!(
            admitted_row_ids(&redriven),
            vec!["gap-w1".to_string(), "gap-w3".to_string()],
            "{} backend did not recover the literal admission identity",
            runner.name
        );
        end_oracle_run(&store, &fence, ADMISSION_GAP_RUN, &redriven).await;
        let later = admit_oracle_run(
            &store,
            &fence,
            "admission-gap-later-run",
            64,
            OracleDrainPolicy::All,
        )
        .await
        .expect("later admission-gap row remains separate");
        assert_eq!(
            admitted_row_ids(&later),
            vec!["gap-w2".to_string()],
            "{} backend did not preserve the literal later-row remainder",
            runner.name
        );
        store
            .supersede_shift_epoch_for_test(&fence)
            .await
            .expect("release successor admission-gap shift");
        runner.close_reopened_postgres_pool().await;
    }

    eprintln!(
        "PASSED interrupted-admission later-row literal oracle; \
         compared_backends=[sqlite-memory,sqlite,postgres]"
    );
}
