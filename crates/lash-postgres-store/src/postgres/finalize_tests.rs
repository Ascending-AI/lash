//! The laws of `lashctl finalize` and of the backfill and contract phases on
//! PostgreSQL (FIG-3800 B, FIG-3817), each on its own isolated database and
//! driven with Phase A's synthetic successor: N is a storage whose writable
//! range is `[1,1]`, N+1 one whose range is `[1,2]` (ADR 0115 §2.1). The
//! backfill and contract laws need the synthetic catalog, so they run in the
//! `synthetic-next` build of this crate.

// FIG-2971: this file is test code; ambient env access is sanctioned here
// (the workspace clippy ban targets production library code).
#![allow(clippy::disallowed_methods)]

use std::sync::Mutex;

use lash_core_execution::compat::{CompatRefusal, VersionRange};
use lash_core_execution::engine::BuildGeneration;
use lash_core_execution::store::fleet_finalize::{
    DeploymentRegistry, DeploymentRegistryError, FinalizeError, FinalizeMode, FinalizeRefusal,
    FleetEpochFlip, RetainedDeployment,
};
use lash_core_execution::{
    SessionCommitStore as _, SessionId, SessionMeta, SessionRelation, StoreError,
};

use crate::testing::IsolatedDatabase;
use crate::{PostgresStorage, PostgresStoreConfig};

/// N's writable range, and N+1's.
const N: VersionRange = VersionRange::exactly(1);
const NEXT: VersionRange = VersionRange::between(1, 2);

/// The deployments a test stands up as registered with the engine.
#[derive(Default)]
struct Registry(Mutex<Vec<RetainedDeployment>>);

impl Registry {
    fn retain(&self, id: &str) {
        self.0.lock().expect("registry").push(RetainedDeployment {
            id: id.to_owned(),
            uri: Some(format!("http://127.0.0.1/{id}")),
        });
    }

    fn remove_all(&self) {
        self.0.lock().expect("registry").clear();
    }
}

#[async_trait::async_trait]
impl DeploymentRegistry for Registry {
    async fn deployments_serving(
        &self,
        _generation: &BuildGeneration,
    ) -> Result<Vec<RetainedDeployment>, DeploymentRegistryError> {
        Ok(self.0.lock().expect("registry").clone())
    }
}

async fn isolated() -> Option<IsolatedDatabase> {
    let Some(database_url) = crate::postgres_test_support::database_url() else {
        eprintln!("skipping finalize law: database URL is not set");
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
        crate::migrate::expand_synthetic_next_for_testing(
            &pool,
            &crate::guarded_tx::WriterFence::new(NEXT, lash_core_execution::FleetFormat::current()),
            2,
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
        session_id: SessionId::from(session),
        relation: SessionRelation::Root,
        pending_observer_intents: Vec::new(),
    }
}

async fn recorded_epoch(storage: &PostgresStorage) -> i32 {
    sqlx::query_scalar("SELECT format_version FROM lash_fleet_format WHERE singleton")
        .fetch_one(storage.pool())
        .await
        .expect("read the recorded epoch")
}

async fn count(storage: &PostgresStorage, table: &str) -> i64 {
    sqlx::query_scalar(&format!("SELECT count(*) FROM {table}"))
        .fetch_one(storage.pool())
        .await
        .expect("count rows")
}

/// Mark `generation` draining, with nothing pinned to it: it reads drained.
async fn drained(storage: &PostgresStorage, generation: &BuildGeneration) {
    storage
        .generation_drain()
        .mark_draining(generation, 1)
        .await
        .expect("mark the retired generation draining");
}

async fn finalize(
    storage: &PostgresStorage,
    retired: &BuildGeneration,
    registry: &Registry,
    mode: FinalizeMode,
) -> Result<crate::FinalizeReport, FinalizeError> {
    storage.finalize_with(retired, registry, mode, 10, 3).await
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

/// Finalize refuses, typed and with nothing changed, while the retired
/// generation is not marked draining, while it still holds work, and while
/// the engine still holds a deployment serving it. Rollback stays possible
/// throughout: N keeps writing. Once drained and removed, finalize moves `F`.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn finalize_refuses_while_an_old_generation_is_live() {
    let Some(database) = isolated().await else {
        return;
    };
    let (n, next) = fleet(&database).await;
    let old = BuildGeneration::for_test("finalize-live-old");
    let registry = Registry::default();

    match finalize(&next, &old, &registry, FinalizeMode::Automatic).await {
        Err(FinalizeError::Refused(FinalizeRefusal::GenerationNotDrained { status })) => {
            assert_eq!(status.draining_since_ms, None, "never marked draining");
        }
        other => panic!("an unmarked generation must refuse finalize: {other:?}"),
    }

    drained(&n, &old).await;
    sqlx::query(
        "INSERT INTO lash_turn_parks (session_id, turn_id, park_id, reason_code, reason_json,
             since_ms, last_refused_ms, attempts, park_build_generation)
         VALUES ('parked-on-old', 't1', 1, 'test', '{}', 1, 1, 1, $1)",
    )
    .bind(old.as_str())
    .execute(n.pool())
    .await
    .expect("pin a parked turn to the retired generation");
    match finalize(&next, &old, &registry, FinalizeMode::Automatic).await {
        Err(FinalizeError::Refused(FinalizeRefusal::GenerationNotDrained { status })) => {
            assert!(status.draining_since_ms.is_some());
            assert_eq!(status.parked_turns, 1, "the pinned park holds the drain");
        }
        other => panic!("a generation with parked work must refuse finalize: {other:?}"),
    }
    sqlx::query("DELETE FROM lash_turn_parks WHERE session_id = 'parked-on-old'")
        .execute(n.pool())
        .await
        .expect("settle the park");

    registry.retain("dp_old");
    match finalize(&next, &old, &registry, FinalizeMode::Automatic).await {
        Err(FinalizeError::Refused(FinalizeRefusal::DeploymentsRetained {
            generation,
            deployments,
        })) => {
            assert_eq!(generation, old);
            assert_eq!(deployments[0].id, "dp_old");
        }
        other => panic!("a retained deployment must refuse finalize: {other:?}"),
    }
    assert_eq!(recorded_epoch(&next).await, 1, "every refusal left F alone");
    n.store()
        .save_session_meta(meta("n-while-refused"))
        .await
        .expect("N still writes: the rollback window is open");

    registry.remove_all();
    let report = finalize(&next, &old, &registry, FinalizeMode::Automatic)
        .await
        .expect("a drained and removed generation finalizes");
    assert_eq!(report.flip, FleetEpochFlip::Finalized { from: 1, to: 2 });
    assert!(report.drain.drained());
    assert_eq!(recorded_epoch(&next).await, 2);
}

/// An operator hold refuses the automatic finalize with nothing changed; an
/// operator finalizing by hand overrides it; a cleared hold lets the
/// automatic finalize through. A build the finalize fenced out can no longer
/// move the hold.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn finalize_honours_operator_hold() {
    let Some(database) = isolated().await else {
        return;
    };
    let (n, next) = fleet(&database).await;
    let old = BuildGeneration::for_test("finalize-hold-old");
    let registry = Registry::default();
    drained(&n, &old).await;

    assert_eq!(next.finalize_hold().await.expect("read the hold"), None);
    let hold = n
        .set_finalize_hold("watch N+1 for a day")
        .await
        .expect("an operator holds the finalize");
    assert_eq!(hold.reason, "watch N+1 for a day");
    assert_eq!(
        next.finalize_hold().await.expect("read the hold"),
        Some(hold.clone()),
        "every build reads the one hold on the fleet row"
    );
    match finalize(&next, &old, &registry, FinalizeMode::Automatic).await {
        Err(FinalizeError::Refused(FinalizeRefusal::Held { hold: refused })) => {
            assert_eq!(refused, hold);
        }
        other => panic!("the automatic finalize must honour the hold: {other:?}"),
    }
    assert_eq!(
        recorded_epoch(&next).await,
        1,
        "a held finalize moves nothing"
    );
    assert_eq!(
        count(&next, "lash_migrations WHERE phase = 'backfill'").await,
        0,
        "a held finalize runs no backfill"
    );

    // Clearing the hold lets the automatic finalize through.
    let cleared = next
        .clear_finalize_hold()
        .await
        .expect("clear the hold")
        .expect("the hold stood");
    assert_eq!(cleared, hold);
    next.set_finalize_hold("hold again")
        .await
        .expect("hold again");

    // By hand, the operator overrides the hold; the hold itself stays.
    let report = finalize(&next, &old, &registry, FinalizeMode::OverrideHold)
        .await
        .expect("an operator finalizes by hand under the hold");
    assert_eq!(report.flip, FleetEpochFlip::Finalized { from: 1, to: 2 });
    assert!(next.finalize_hold().await.expect("read").is_some());
    fenced(n.clear_finalize_hold().await, "N clearing the hold");
    next.clear_finalize_hold().await.expect("N+1 clears it");
    let again = finalize(&next, &old, &registry, FinalizeMode::Automatic)
        .await
        .expect("an unheld automatic finalize reruns");
    assert_eq!(again.flip, FleetEpochFlip::AlreadyFinalized { fleet: 2 });

    // On a second store, a hold cleared before the automatic finalize.
    let Some(second) = isolated().await else {
        return;
    };
    let (n, next) = fleet(&second).await;
    drained(&n, &old).await;
    next.set_finalize_hold("brief").await.expect("hold");
    next.clear_finalize_hold().await.expect("clear");
    let report = finalize(&next, &old, &registry, FinalizeMode::Automatic)
        .await
        .expect("the automatic finalize runs once the hold is cleared");
    assert_eq!(report.flip, FleetEpochFlip::Finalized { from: 1, to: 2 });
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
    let old = BuildGeneration::for_test("rollback-old");
    let registry = Registry::default();

    next.store()
        .save_session_meta(meta("written-by-next"))
        .await
        .expect("N+1 writes before finalize");
    n.store()
        .save_session_meta(meta("written-by-n"))
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
            count(&next, "lash_sessions WHERE synthetic_next_note IS NOT NULL").await,
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
        .save_session_meta(meta("n-after-rollback"))
        .await
        .expect("N writes after its restart");

    drained(&next, &old).await;
    finalize(&next, &old, &registry, FinalizeMode::Automatic)
        .await
        .expect("finalize");
    fenced(
        n_again
            .store()
            .save_session_meta(meta("n-after-finalize"))
            .await,
        "N after finalize",
    );
    // N no longer opens the store: its `F` is past N's writable range, and
    // in the synthetic build the backfill finalize ran has already added a
    // constraint N's tolerant shape check refuses.
    match open_as(database.url(), N).await.map(|_| ()) {
        Err(StoreError::Incompatible {
            refusal:
                CompatRefusal::FleetOutsideWritable { recorded: 2, .. }
                | CompatRefusal::ShapeRefused { .. },
        }) => {}
        other => panic!("N must refuse the finalized store at open: {other:?}"),
    }
}

/// After finalize a stale writer of N is refused `WriterFenced` with zero
/// rows written, on every path finalize adds (a session write, a drain
/// mark, the hold, a backfill run as N), while N+1 writes on.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_stale_writer_is_fenced_after_finalize() {
    let Some(database) = isolated().await else {
        return;
    };
    let (n, next) = fleet(&database).await;
    let old = BuildGeneration::for_test("stale-writer-old");
    drained(&n, &old).await;
    let report = finalize(&next, &old, &Registry::default(), FinalizeMode::Automatic)
        .await
        .expect("finalize");
    assert_eq!(report.flip.fleet(), 2);

    let sessions = count(&n, "lash_session_meta").await;
    let marks = count(&n, "lash_draining_generations").await;
    fenced(
        n.store()
            .save_session_meta(meta("stale-after-finalize"))
            .await,
        "N's session write",
    );
    fenced(
        n.generation_drain()
            .mark_draining(&BuildGeneration::for_test("stale-mark"), 2)
            .await,
        "N's drain mark",
    );
    fenced(n.set_finalize_hold("stale").await, "N's hold");
    match crate::migrate::run_phase(n.pool(), crate::MigrationPhase::Backfill, &n.fence, 3).await {
        Ok(report) => assert!(
            report.executed.is_empty(),
            "N carries no backfill, so its run writes nothing"
        ),
        Err(crate::MigrateError::Store(StoreError::WriterFenced { recorded: 2, .. })) => {}
        other => panic!("N's backfill run must write nothing: {other:?}"),
    }
    assert_eq!(count(&n, "lash_session_meta").await, sessions);
    assert_eq!(count(&n, "lash_draining_generations").await, marks);
    assert_eq!(next.finalize_hold().await.expect("read the hold"), None);

    next.store()
        .save_session_meta(meta("next-after-finalize"))
        .await
        .expect("N+1 writes under the epoch it finalized");
}

/// Seed `rows` sessions in the old shape: the note the synthetic backfill
/// fills is absent.
#[cfg(feature = "synthetic-next")]
async fn old_sessions(storage: &PostgresStorage, rows: usize) {
    for index in 0..rows {
        sqlx::query("INSERT INTO lash_sessions (session_id, head_json) VALUES ($1, '{}')")
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
    let old = BuildGeneration::for_test("backfill-old");
    drained(&n, &old).await;

    // Finalize's flip alone: its backfill step is what gets interrupted.
    crate::finalize::flip(next.pool(), &next.fence, FinalizeMode::Automatic)
        .await
        .expect("finalize moves F");
    sqlx::query(
        "INSERT INTO lash_sessions (session_id, head_json, synthetic_next_note)
         VALUES ('old-03-next', '{}', 'written by N+1')",
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
            "lash_sessions WHERE synthetic_next_note LIKE 'backfilled:%'"
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
    let notes: Vec<(String, Option<String>)> =
        sqlx::query_as("SELECT session_id, synthetic_next_note FROM lash_sessions ORDER BY 1")
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

    // A rerun, and a rerun of finalize itself, change nothing.
    assert!(
        crate::migrate::run_backfills(next.pool(), &next.fence, 3)
            .await
            .expect("rerun")
            .is_empty()
    );
    let rerun = finalize(&next, &old, &Registry::default(), FinalizeMode::Automatic)
        .await
        .expect("finalize reruns");
    assert_eq!(rerun.flip, FleetEpochFlip::AlreadyFinalized { fleet: 2 });
    assert!(rerun.backfills.is_empty());
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
    let old = BuildGeneration::for_test("contract-old");
    drained(&n, &old).await;

    let before_finalize = run_phase(next.pool(), MigrationPhase::Contract, &next.fence, 2).await;
    assert!(
        matches!(
            before_finalize,
            Err(MigrateError::Refused(
                MigrationRefusal::ContractBeforeFinalize { .. }
            ))
        ),
        "{before_finalize:?}"
    );
    crate::finalize::flip(next.pool(), &next.fence, FinalizeMode::Automatic)
        .await
        .expect("finalize moves F");

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
    let (stamp_version, min_reader): (i32, i32) =
        sqlx::query_as("SELECT version, min_reader FROM lash_schema_versions WHERE component = $1")
            .bind(crate::SCHEMA_COMPONENT)
            .fetch_one(next.pool())
            .await
            .expect("read the stamp");
    assert_eq!(
        (stamp_version, min_reader),
        (2, 1),
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
           AND constraint_row.conname = 'ck_lash_sessions_synthetic_next_note'",
    )
    .bind(crate::SCHEMA_COMPONENT)
    .fetch_one(next.pool())
    .await
    .expect("read the floor and the constraint");
    assert_eq!(min_reader, 2);
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
