use super::*;

/// A contract step is refused until `F` reaches its epoch, then until
/// every backfill it names is applied, and each refusal names its remedy.
#[test]
fn a_contract_gate_refuses_before_finalize_and_before_its_backfills() {
    let contract = ContractMigration {
        id: "gate-contract",
        after_fleet: 2,
        after_backfills: &["gate-backfill"],
        statements: "",
        min_reader: 2,
    };
    let before_finalize = contract_admitted(&contract, 1, |_| true).unwrap_err();
    assert_eq!(
        before_finalize,
        MigrationRefusal::ContractBeforeFinalize {
            migration: "gate-contract".to_owned(),
            recorded: 1,
            requires: 2,
        }
    );
    assert!(before_finalize.to_string().contains("lashctl finalize"));
    let before_backfills = contract_admitted(&contract, 2, |_| false).unwrap_err();
    assert_eq!(
        before_backfills,
        MigrationRefusal::ContractBeforeBackfills {
            migration: "gate-contract".to_owned(),
            pending: vec!["gate-backfill".to_owned()],
        }
    );
    assert!(
        before_backfills
            .to_string()
            .contains("lashctl migrate --phase backfill")
    );
    contract_admitted(&contract, 2, |backfill| backfill == "gate-backfill")
        .expect("finalized and backfilled");
}

/// The rollout laws of the backfill and contract phases on PostgreSQL
/// (FIG-3817), each on its own isolated database and executed with Phase A's
/// synthetic successor: N is a storage whose writable range is `[1,1]`, N+1
/// one whose range is `[1,2]` (ADR 0115 §2.1). A test-only move of `F`
/// stands for the successor's fleet epoch. The backfill and contract laws
/// need the synthetic catalog, so they run in the `synthetic-next` build of
/// this crate.
mod rollout {
    // This module is test code; ambient env access is sanctioned here (the
    // workspace clippy ban targets production library code).
    #![allow(clippy::disallowed_methods)]

    use lash_core_execution::compat::{CompatRefusal, VersionRange};
    use lash_core_execution::{
        SessionCatalogStore as _, SessionCommitStore as _, SessionId, SessionMeta, SessionRelation,
        StoreError,
    };

    use crate::testing::IsolatedDatabase;
    use crate::{PostgresStorage, PostgresStoreConfig};

    /// N's writable range, and N+1's.
    const N: VersionRange = VersionRange::exactly(1);
    const NEXT: VersionRange = VersionRange::between(1, 2);

    async fn isolated() -> Option<IsolatedDatabase> {
        let Some(database_url) = crate::postgres_test_support::database_url() else {
            eprintln!("skipping rollout law: database URL is not set");
            return None;
        };
        Some(IsolatedDatabase::create(&database_url).await)
    }

    /// A storage over `url` as a build whose writable range is `writable`.
    async fn open_as(url: &str, writable: VersionRange) -> Result<PostgresStorage, StoreError> {
        let pool = sqlx::PgPool::connect(url)
            .await
            .map_err(crate::store_sqlx_error)?;
        PostgresStorage::from_pool_with_fleet_writable_range_for_testing(
            pool,
            PostgresStoreConfig::default(),
            writable,
        )
        .await
    }

    /// N and N+1 over one freshly provisioned store. In the synthetic build N+1
    /// has run its expand first, as `lashctl migrate` of N+1 does before the
    /// roll: the synthetic shape check admits only an expanded catalog.
    async fn fleet(database: &IsolatedDatabase) -> (PostgresStorage, PostgresStorage) {
        #[cfg(feature = "synthetic-next")]
        {
            let pool = sqlx::PgPool::connect(database.url())
                .await
                .expect("connect for the expand");
            crate::migrate::expand_for_testing(
                &pool,
                &crate::guarded_tx::WriterFence::new(
                    NEXT,
                    lash_core_execution::FleetFormat::current(),
                ),
            )
            .await
            .expect("N+1 expands the store");
            pool.close().await;
        }
        let n = open_as(database.url(), N).await.expect("N opens");
        let next = open_as(database.url(), NEXT).await.expect("N+1 opens");
        (n, next)
    }

    fn meta(session: &str) -> SessionMeta {
        SessionMeta {
            owning_process_id: None,
            session_id: SessionId::fixture(session),
            relation: SessionRelation::Root,
            pending_observer_intents: Vec::new(),
        }
    }

    #[cfg(feature = "synthetic-next")]
    async fn count(storage: &PostgresStorage, table: &str) -> i64 {
        sqlx::query_scalar(&format!("SELECT count(*) FROM {table}"))
            .fetch_one(storage.pool())
            .await
            .expect("count rows")
    }

    fn fenced<T: std::fmt::Debug>(outcome: Result<T, StoreError>, what: &str) {
        match outcome {
            Err(StoreError::WriterFenced {
                recorded: 2,
                writable,
            }) if writable == N => {}
            other => panic!("{what} must be fenced at F=2: {other:?}"),
        }
    }

    /// Before finalize, N and N+1 serve one store and read what the other
    /// wrote, N reopens it after N+1 expanded it, and nothing in the new
    /// release's shape is written: backfill and contract are refused. After
    /// finalize, N's writes are fenced and N no longer opens the store.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn rollback_is_safe_until_finalize() {
        let Some(database) = isolated().await else {
            return;
        };
        let (n, next) = fleet(&database).await;

        next.store()
            .admit_session(
                &lash_core_execution::testing::store_fixtures::session_request_from_meta_for_test(
                    meta("written-by-next"),
                ),
            )
            .await
            .expect("N+1 writes before finalize");
        n.store()
            .admit_session(
                &lash_core_execution::testing::store_fixtures::session_request_from_meta_for_test(
                    meta("written-by-n"),
                ),
            )
            .await
            .expect("N writes before finalize");
        assert!(
            n.store()
                .load_session_meta(&SessionId::from("written-by-next"))
                .await
                .expect("N reads")
                .is_some(),
            "N reads what N+1 wrote"
        );
        assert!(
            next.store()
                .load_session_meta(&SessionId::from("written-by-n"))
                .await
                .expect("N+1 reads")
                .is_some(),
            "N+1 reads what N wrote"
        );

        #[cfg(feature = "synthetic-next")]
        {
            use crate::migrate::{MigrationRefusal, run_phase};
            use crate::{MigrateError, MigrationPhase};
            match run_phase(next.pool(), MigrationPhase::Backfill, &next.fence, 3).await {
                Err(MigrateError::Refused(MigrationRefusal::BackfillBeforeFinalize {
                    recorded: 1,
                    requires: 2,
                    ..
                })) => {}
                other => panic!("a backfill before finalize must refuse: {other:?}"),
            }
            match run_phase(next.pool(), MigrationPhase::Contract, &next.fence, 3).await {
                Err(MigrateError::Refused(MigrationRefusal::ContractBeforeFinalize { .. })) => {}
                other => panic!("a contract before finalize must refuse: {other:?}"),
            }
            assert_eq!(
                count(
                    &next,
                    "lash_session_head WHERE synthetic_next_note IS NOT NULL"
                )
                .await,
                0,
                "no row is in the new release's shape before finalize"
            );
            assert_eq!(
                count(
                    &next,
                    "lash_migrations WHERE phase IN ('backfill', 'contract')"
                )
                .await,
                0
            );
        }

        // Rollback: N restarts over the expanded store and keeps writing.
        let n_again = open_as(database.url(), N)
            .await
            .expect("N reopens the store before finalize");
        n_again
            .store()
            .admit_session(
                &lash_core_execution::testing::store_fixtures::session_request_from_meta_for_test(
                    meta("n-after-rollback"),
                ),
            )
            .await
            .expect("N writes after its restart");

        crate::testing::finalize_fleet_epoch(next.pool(), 2)
            .await
            .expect("N+1 moves F");
        fenced(
        n_again
            .store()
            .admit_session(
                &lash_core_execution::testing::store_fixtures::session_request_from_meta_for_test(
                    meta("n-after-finalize"),
                ),
            )
            .await,
        "N after finalize",
    );
        // N no longer opens the store: its `F` is past N's writable range.
        match open_as(database.url(), N).await.map(|_| ()) {
            Err(StoreError::Incompatible {
                refusal:
                    CompatRefusal::FleetOutsideWritable { recorded: 2, .. }
                    | CompatRefusal::ShapeRefused { .. },
            }) => {}
            other => panic!("N must refuse the finalized store at open: {other:?}"),
        }
    }

    /// Seed `rows` sessions in the old shape: the note the synthetic backfill
    /// fills is absent.
    #[cfg(feature = "synthetic-next")]
    async fn old_sessions(storage: &PostgresStorage, rows: usize) {
        for index in 0..rows {
            sqlx::query("WITH recorded AS (INSERT INTO lash_session_revisions (session_id, head_revision, head_json) VALUES ($1, 0, '{}') RETURNING session_id, head_revision) INSERT INTO lash_session_head (session_id, head_revision) SELECT session_id, head_revision FROM recorded")
            .bind(format!("old-{index:02}"))
            .execute(storage.pool())
            .await
            .expect("seed an old session");
        }
    }

    #[cfg(feature = "synthetic-next")]
    async fn backfill_ledger(storage: &PostgresStorage) -> (String, Option<String>, i64) {
        sqlx::query_as(
            "SELECT state, backfill_cursor, backfill_rows FROM lash_migrations
         WHERE phase = 'backfill' AND migration = 'synthetic-next-session-note'",
        )
        .fetch_one(storage.pool())
        .await
        .expect("read the backfill ledger row")
    }

    /// A backfill interrupted mid-batch resumes from the last committed cursor:
    /// the batch in flight rolls back whole, a resumed run and a concurrent one
    /// rewrite every old row exactly once, a row N+1 wrote after finalize keeps
    /// its value, and a rerun changes nothing.
    #[cfg(feature = "synthetic-next")]
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn interrupted_backfill_resumes_idempotently() {
        let Some(database) = isolated().await else {
            return;
        };
        let (n, next) = fleet(&database).await;
        old_sessions(&n, 7).await;
        crate::testing::finalize_fleet_epoch(next.pool(), 2)
            .await
            .expect("N+1 moves F");
        sqlx::query(
        "WITH recorded AS (INSERT INTO lash_session_revisions (session_id, head_revision, head_json) VALUES ('old-03-next', 0, '{}') RETURNING session_id, head_revision) INSERT INTO lash_session_head (session_id, head_revision, synthetic_next_note)
         SELECT session_id, head_revision, 'written by N+1' FROM recorded",
    )
    .execute(next.pool())
    .await
    .expect("N+1 writes a row in the new shape after finalize");

        let backfill = &crate::migrate::BACKFILL_MIGRATIONS[0];
        crate::migrate::start_backfill(next.pool(), &next.fence, backfill)
            .await
            .expect("the backfill starts");
        crate::migrate::backfill_batch(next.pool(), &next.fence, backfill, 3)
            .await
            .expect("one batch commits");
        assert_eq!(
            backfill_ledger(&next).await,
            ("running".to_owned(), Some("old-02".to_owned()), 3)
        );

        // The next batch dies before its commit.
        {
            let mut tx = crate::guarded_tx::begin_guarded(next.pool(), &next.fence)
                .await
                .expect("begin the doomed batch");
            crate::migrate::backfill_batch_in(&mut tx, backfill, 3)
                .await
                .expect("the doomed batch rewrites its rows");
            drop(tx);
        }
        assert_eq!(
            backfill_ledger(&next).await,
            ("running".to_owned(), Some("old-02".to_owned()), 3),
            "the lost batch moved no cursor"
        );
        assert_eq!(
            count(
                &next,
                "lash_session_head WHERE synthetic_next_note LIKE 'backfilled:%'"
            )
            .await,
            3,
            "the lost batch rewrote nothing"
        );

        // Two runs resume at once; the ledger row serializes them.
        let (left, right) = tokio::join!(
            crate::migrate::run_backfills(next.pool(), &next.fence, 3),
            crate::migrate::run_backfills(next.pool(), &next.fence, 3),
        );
        left.expect("a resumed run completes");
        right.expect("a concurrent resumed run completes");
        let (state, _, rows) = backfill_ledger(&next).await;
        assert_eq!(state, "applied");
        assert_eq!(rows, 7, "every old row rewritten exactly once");
        let notes: Vec<(String, Option<String>)> = sqlx::query_as(
            "SELECT session_id, synthetic_next_note FROM lash_session_head ORDER BY 1",
        )
        .fetch_all(next.pool())
        .await
        .expect("read the notes");
        for (session, note) in &notes {
            let expected = if session == "old-03-next" {
                "written by N+1".to_owned()
            } else {
                format!("backfilled:{session}")
            };
            assert_eq!(note.as_deref(), Some(expected.as_str()), "{session}");
        }

        // A rerun changes nothing.
        assert!(
            crate::migrate::run_backfills(next.pool(), &next.fence, 3)
                .await
                .expect("rerun")
                .is_empty()
        );
        assert_eq!(backfill_ledger(&next).await.2, 7);
    }

    /// Contract is refused before finalize, then until the ledger shows every
    /// backfill it names applied — including while one is part-way — and then
    /// validates the tightened constraint and raises the reader floor, so N
    /// refuses the store at open. A rerun is a no-op.
    #[cfg(feature = "synthetic-next")]
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn contract_refuses_until_backfills_complete() {
        use crate::migrate::{MigrationRefusal, plan_on, run_phase};
        use crate::{MigrateError, MigrationPhase};

        let Some(database) = isolated().await else {
            return;
        };
        let (n, next) = fleet(&database).await;
        old_sessions(&n, 5).await;

        let before_finalize =
            run_phase(next.pool(), MigrationPhase::Contract, &next.fence, 2).await;
        assert!(
            matches!(
                before_finalize,
                Err(MigrateError::Refused(
                    MigrationRefusal::ContractBeforeFinalize { .. }
                ))
            ),
            "{before_finalize:?}"
        );
        crate::testing::finalize_fleet_epoch(next.pool(), 2)
            .await
            .expect("N+1 moves F");

        let pending = vec!["synthetic-next-session-note".to_owned()];
        let expected = MigrationRefusal::ContractBeforeBackfills {
            migration: "synthetic-next-contract".to_owned(),
            pending: pending.clone(),
        };
        for outcome in [
            run_phase(next.pool(), MigrationPhase::Contract, &next.fence, 2).await,
            plan_on(next.pool(), MigrationPhase::Contract).await,
        ] {
            match outcome {
                Err(MigrateError::Refused(refusal)) => assert_eq!(refusal, expected),
                other => panic!("contract before its backfill must refuse: {other:?}"),
            }
        }

        // Part-way through the backfill, contract still waits.
        let backfill = &crate::migrate::BACKFILL_MIGRATIONS[0];
        crate::migrate::start_backfill(next.pool(), &next.fence, backfill)
            .await
            .expect("the backfill starts");
        crate::migrate::backfill_batch(next.pool(), &next.fence, backfill, 2)
            .await
            .expect("one batch");
        match run_phase(next.pool(), MigrationPhase::Contract, &next.fence, 2).await {
            Err(MigrateError::Refused(refusal)) => assert_eq!(refusal, expected),
            other => panic!("contract during the backfill must refuse: {other:?}"),
        }
        let (stamp_version, min_reader): (i32, i32) = sqlx::query_as(
            "SELECT version, min_reader FROM lash_schema_versions WHERE component = $1",
        )
        .bind(crate::SCHEMA_COMPONENT)
        .fetch_one(next.pool())
        .await
        .expect("read the stamp");
        assert_eq!(
            (stamp_version, min_reader),
            (crate::SCHEMA_VERSION + 1, crate::SCHEMA_VERSION),
            "a refused contract moved no floor"
        );

        let backfilled = run_phase(next.pool(), MigrationPhase::Backfill, &next.fence, 2)
            .await
            .expect("the backfill completes");
        assert_eq!(backfilled.executed.len(), 1);
        assert_eq!(backfilled.executed[0].state, "applied");
        let contracted = run_phase(next.pool(), MigrationPhase::Contract, &next.fence, 2)
            .await
            .expect("contract runs once its backfill is applied");
        assert_eq!(contracted.executed.len(), 1);
        assert_eq!(contracted.executed[0].migration, "synthetic-next-contract");
        let (min_reader, validated): (i32, bool) = sqlx::query_as(
            "SELECT versions.min_reader, constraint_row.convalidated
         FROM lash_schema_versions AS versions, pg_catalog.pg_constraint AS constraint_row
         WHERE versions.component = $1
           AND constraint_row.conname = 'ck_lash_session_head_synthetic_next_note'",
        )
        .bind(crate::SCHEMA_COMPONENT)
        .fetch_one(next.pool())
        .await
        .expect("read the floor and the constraint");
        assert_eq!(min_reader, crate::SCHEMA_VERSION + 1);
        assert!(
            validated,
            "contract validated the constraint the backfill added"
        );
        assert!(
            run_phase(next.pool(), MigrationPhase::Contract, &next.fence, 2)
                .await
                .expect("rerun")
                .executed
                .is_empty()
        );
        // This synthetic build links N+1's descriptor, so the raised reader
        // floor admits it; a fence of N's writable range `[1,1]` stands for N
        // and is refused at the fence. A real N binary's `ReaderFloorAbove`
        // refusal of the contracted store is proved by the rolling upgrade's
        // N probe.
        match open_as(database.url(), N).await.map(|_| ()) {
            Err(StoreError::Incompatible {
                refusal: CompatRefusal::FleetOutsideWritable { recorded: 2, .. },
            }) => {}
            other => panic!("N must refuse the contracted store: {other:?}"),
        }
    }
}
