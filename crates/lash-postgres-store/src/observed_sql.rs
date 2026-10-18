//! Noninvasive cost observations around existing PostgreSQL executions.
//!
//! A task-local scope belongs to one labelled durable attempt. Delegated
//! persistence helpers use the same executor decorator, so their statements
//! and returned column bytes belong to that attempt too. Outside a scope the
//! decorator forwards directly to SQLx. It never changes SQL or binds.

#[cfg(test)]
use std::cell::RefCell;
use std::future::Future;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Instant;

use futures_util::future::BoxFuture;
use futures_util::stream::{BoxStream, StreamExt};
use lash_core_ids::operational_metrics::DurableCommitCost;
use lash_core_ids::operational_metrics::StoreObserver;
use lash_core_ids::operational_metrics::sql;
use lash_durable::{CommitLabel, DurableError};
use sqlx::postgres::{PgQueryResult, PgRow, PgStatement, PgTypeInfo};
use sqlx::{Describe, Either, Execute, Executor, Postgres, Row};

#[derive(Default)]
struct State {
    cost: DurableCommitCost,
    transaction_started: Option<Instant>,
}

type Shared = Arc<Mutex<State>>;

tokio::task_local! {
    static CURRENT: Shared;
}

#[cfg(test)]
tokio::task_local! {
    pub(crate) static RECEIPTS: RefCell<Vec<(String, &'static str, DurableCommitCost)>>;
}

#[cfg(test)]
tokio::task_local! {
    /// The round trips a law's task made to the server: every statement it
    /// sent through the observed executor, and every checkout of a pool that
    /// pings each one.
    pub(crate) static ROUND_TRIPS: std::cell::Cell<u64>;
}

#[cfg(test)]
fn round_trip() {
    let _ = ROUND_TRIPS.try_with(|trips| trips.set(trips.get() + 1));
}

/// Check a connection out of `pool`: a pool that pings every checkout
/// pays a round trip for it.
pub(crate) async fn checkout(
    pool: &sqlx::PgPool,
) -> Result<sqlx::pool::PoolConnection<Postgres>, sqlx::Error> {
    #[cfg(test)]
    if pool.options().get_test_before_acquire() {
        round_trip();
    }
    let started = Instant::now();
    #[cfg(feature = "perf-witness")]
    let mut timing = lash_core_ids::perf_witness::queues::Timer::enqueue("postgres.pool_checkout");
    let connection = pool.acquire().await;
    #[cfg(feature = "perf-witness")]
    if let Some(timing) = &mut timing {
        timing.complete();
    }
    acquired(started.elapsed());
    connection
}

/// Check a connection out of `pool` that answers a ping: one the server
/// closed is dropped for the next, through every connection the pool holds
/// and then a fresh one.
pub(crate) async fn verified(
    pool: &sqlx::PgPool,
) -> Result<sqlx::pool::PoolConnection<Postgres>, sqlx::Error> {
    let mut lost = None;
    for _ in 0..=pool.options().get_max_connections() {
        let mut connection = checkout(pool).await?;
        #[cfg(test)]
        round_trip();
        match sqlx::Connection::ping(&mut *connection).await {
            Ok(()) => return Ok(connection),
            Err(error) => {
                connection.close_on_drop();
                lost = Some(error);
            }
        }
    }
    Err(lost.unwrap_or(sqlx::Error::PoolTimedOut))
}

struct Observation<'a> {
    state: Shared,
    observer: &'a StoreObserver,
    label: CommitLabel,
    outcome: &'static str,
}

impl Drop for Observation<'_> {
    fn drop(&mut self) {
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(started) = state.transaction_started {
            state.cost.transaction_duration = started.elapsed();
        }
        self.observer
            .durable_commit(self.label.as_str(), self.outcome, state.cost);
        #[cfg(test)]
        let _ = RECEIPTS.try_with(|receipts| {
            receipts
                .borrow_mut()
                .push((self.label.as_str().to_owned(), self.outcome, state.cost));
        });
    }
}

pub(crate) async fn measure<T>(
    observer: &StoreObserver,
    label: CommitLabel,
    group_commit_members: u64,
    future: impl Future<Output = Result<T, DurableError>>,
) -> Result<T, DurableError> {
    if !observer.is_observed() {
        return future.await;
    }
    let state = Arc::new(Mutex::new(State {
        cost: DurableCommitCost {
            group_commit_members,
            ..DurableCommitCost::default()
        },
        transaction_started: None,
    }));
    let mut observation = Observation {
        state: Arc::clone(&state),
        observer,
        label,
        outcome: "cancelled",
    };
    let result = observer
        .observe_sql(label.as_str(), "postgres", CURRENT.scope(state, future))
        .await;
    observation.outcome = if result.is_ok() { "success" } else { "error" };
    result
}

pub(crate) fn acquired(wait: std::time::Duration) {
    let _ = CURRENT.try_with(|state| {
        state
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .cost
            .acquire_wait += wait;
    });
}

pub(crate) fn transaction_started() {
    let _ = CURRENT.try_with(|state| {
        state
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .transaction_started = Some(Instant::now());
    });
}

/// A statement guard lives until the delegated future/stream finishes or is
/// dropped. Writes, explicit row/table locks and advisory locks contribute
/// their complete elapsed time to the documented lock-wait upper bound.
struct Statement {
    #[cfg(feature = "perf-witness")]
    work: Option<lash_core_ids::perf_witness::sql_shapes::work::Execution>,
    #[cfg(feature = "perf-witness")]
    repeat_qualified: bool,
    state: Option<Shared>,
    window: Option<sql::Window>,
    sql: String,
    started: Instant,
    locking: bool,
    #[cfg(feature = "perf-witness")]
    timing: Option<lash_core_ids::perf_witness::queues::Timer>,
}

impl Statement {
    fn new(sql: &str, site: &impl std::fmt::Display) -> Option<Self> {
        #[cfg(test)]
        round_trip();
        let state = CURRENT.try_with(Arc::clone).ok();
        let window = sql::Window::current();
        #[cfg(feature = "perf-witness")]
        let recording = lash_core_ids::perf_witness::queues::is_recording();
        #[cfg(not(feature = "perf-witness"))]
        let recording = false;
        #[cfg(feature = "perf-witness")]
        let work = {
            use lash_core_ids::perf_witness;
            use lash_core_ids::perf_witness::sql_shapes;
            if perf_witness::collecting() {
                perf_witness::record_sql_statement(sql);
            }
            if sql_shapes::enabled() {
                let relations = lash_store_sql::relation_set(sql);
                let read = perf_witness::SqlVerb::classify(sql) == perf_witness::SqlVerb::Select;
                sql_shapes::work::Execution::start(
                    sql_shapes::work::Backend::Postgres,
                    sql,
                    &site.to_string(),
                    relations,
                    read,
                )
            } else {
                None
            }
        };
        #[cfg(not(feature = "perf-witness"))]
        let _ = site;
        let joined = {
            #[cfg(feature = "perf-witness")]
            {
                work.is_some() || lash_core_ids::perf_witness::collecting()
            }
            #[cfg(not(feature = "perf-witness"))]
            {
                false
            }
        };
        if state.is_none() && window.is_none() && !joined && !recording {
            return None;
        }
        if let Some(window) = &window {
            window.statement(sql);
        }
        let text = sql.to_owned();
        let sql = sql.to_ascii_uppercase();
        let locking = [
            "FOR UPDATE",
            "FOR SHARE",
            "FOR NO KEY UPDATE",
            "FOR KEY SHARE",
            "INSERT ",
            "UPDATE ",
            "DELETE ",
            "LOCK TABLE",
            "PG_ADVISORY",
            "PG_TRY_ADVISORY",
            // The one-call owner commit takes the commit's locks and writes.
            "LASH_OWNER_COMMIT(",
        ]
        .iter()
        .any(|word| sql.contains(word));
        if let Some(state) = &state {
            state
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .cost
                .sql_statements += 1;
        }
        Some(Self {
            #[cfg(feature = "perf-witness")]
            work,
            #[cfg(feature = "perf-witness")]
            repeat_qualified: work.is_some()
                && lash_store_sql::read_statement(&text) == Some(true)
                && lash_store_sql::relation_set(&text).is_some(),
            state,
            window,
            sql: text,
            started: Instant::now(),
            locking,
            #[cfg(feature = "perf-witness")]
            timing: if locking {
                // Complete SQL elapsed time is an upper bound on lock wait,
                // never an exact server wait; checkout is measured separately.
                lash_core_ids::perf_witness::queues::Timer::enqueue(
                    "postgres.lock_statement_upper_bound",
                )
            } else {
                None
            },
        })
    }

    fn finish(&mut self) {
        #[cfg(feature = "perf-witness")]
        if let Some(timing) = &mut self.timing {
            timing.complete();
        }
    }

    fn row(&self, row: &PgRow) {
        let bytes = (0..row.len())
            .filter_map(|index| row.try_get_raw(index).ok())
            .filter_map(|value| value.as_bytes().ok().map(|bytes| bytes.len() as u64))
            .sum::<u64>();
        #[cfg(feature = "perf-witness")]
        {
            lash_core_ids::perf_witness::record_sql_row_read(&self.sql);
            if let Some(work) = self.work {
                work.row(bytes);
            }
        }
        if let Some(window) = &self.window {
            window.row(&self.sql);
            window.payload(&self.sql, 0, 0, bytes);
        }
        if let Some(state) = &self.state {
            state
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .cost
                .returned_bytes += bytes;
        }
    }
}

impl Drop for Statement {
    fn drop(&mut self) {
        #[cfg(feature = "perf-witness")]
        if let Some(work) = self.work.take() {
            work.dropped();
        }
        if self.locking
            && let Some(state) = &self.state
        {
            state
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .cost
                .lock_statement_elapsed += self.started.elapsed();
        }
    }
}

pub(crate) async fn control<T>(sql: &str, future: impl Future<Output = T>) -> T {
    let mut statement = Statement::new(sql, &"postgres.transaction-control");
    #[cfg(feature = "perf-witness")]
    if let Some(work) = statement.as_ref().and_then(|s| s.work) {
        work.bindings(Some(0), Some(sql.as_bytes().to_vec()), "pg_encoded");
    }
    let result = future.await;
    #[cfg(feature = "perf-witness")]
    if let Some(work) = statement.as_mut().and_then(|s| s.work.take()) {
        work.finish();
    }
    if let Some(statement) = &mut statement {
        statement.finish();
    }
    drop(statement);
    result
}

#[derive(Debug)]
pub(crate) struct Observed<E>(E, &'static std::panic::Location<'static>);

#[track_caller]
pub(crate) fn executor<E>(executor: E) -> Observed<E> {
    Observed(executor, std::panic::Location::caller())
}

impl<'c, E: Executor<'c, Database = Postgres>> Executor<'c> for Observed<E> {
    type Database = Postgres;

    fn fetch_many<'e, 'q: 'e, Q>(
        self,
        query: Q,
    ) -> BoxStream<'e, Result<Either<PgQueryResult, PgRow>, sqlx::Error>>
    where
        'c: 'e,
        Q: 'q + Execute<'q, Postgres>,
    {
        let statement = Statement::new(query.sql(), self.1);
        #[cfg(feature = "perf-witness")]
        let query = JoinedQuery {
            inner: query,
            work: statement.as_ref().and_then(|s| s.work),
            repeat_qualified: statement.as_ref().is_some_and(|s| s.repeat_qualified),
        };
        let stream = self.0.fetch_many(query);
        let Some(mut statement) = statement else {
            return stream;
        };
        let mut stream = stream;
        Box::pin(futures_util::stream::poll_fn(move |cx| {
            use std::task::Poll;
            let result = match stream.as_mut().poll_next(cx) {
                Poll::Pending => return Poll::Pending,
                Poll::Ready(None) => {
                    #[cfg(feature = "perf-witness")]
                    if let Some(work) = statement.work.take() {
                        work.finish();
                    }
                    return Poll::Ready(None);
                }
                Poll::Ready(Some(result)) => result,
            };
            match &result {
                Ok(Either::Right(row)) => statement.row(row),
                Ok(Either::Left(done)) => {
                    statement.finish();
                    #[cfg(feature = "perf-witness")]
                    if let Some(work) = statement.work {
                        work.affected(done.rows_affected());
                    }
                    if let Some(window) = &statement.window {
                        window.payload(&statement.sql, done.rows_affected(), 0, 0);
                    }
                }
                Err(_) => {
                    statement.finish();
                    #[cfg(feature = "perf-witness")]
                    if let Some(work) = statement.work.take() {
                        work.failed();
                    }
                }
            }
            Poll::Ready(Some(result))
        }))
    }

    fn fetch_optional<'e, 'q: 'e, Q>(
        self,
        query: Q,
    ) -> BoxFuture<'e, Result<Option<PgRow>, sqlx::Error>>
    where
        'c: 'e,
        Q: 'q + Execute<'q, Postgres>,
    {
        let joined = {
            #[cfg(feature = "perf-witness")]
            {
                lash_core_ids::perf_witness::sql_shapes::enabled()
            }
            #[cfg(not(feature = "perf-witness"))]
            {
                false
            }
        };
        if sql::Window::current().is_none() && !joined {
            let mut statement = Statement::new(query.sql(), self.1);
            let future = self.0.fetch_optional(query);
            return Box::pin(async move {
                let result = future.await;
                if let Some(statement) = &mut statement {
                    statement.finish();
                }
                if let (Some(statement), Ok(Some(row))) = (&statement, &result) {
                    statement.row(row);
                }
                result
            });
        }
        // Consume command completion as well as rows. SQLx's optional API
        // hides PgQueryResult, including affected rows of RETURNING writes.
        // SQLx's PostgreSQL optional executor also drains all responses so
        // deferred-constraint errors are visible before returning its first row.
        let mut stream = self.fetch_many(query);
        Box::pin(async move {
            let mut first = None;
            while let Some(result) = stream.next().await {
                if let Either::Right(row) = result?
                    && first.is_none()
                {
                    first = Some(row);
                }
            }
            Ok(first)
        })
    }

    fn prepare_with<'e, 'q: 'e>(
        self,
        sql: &'q str,
        parameters: &'e [PgTypeInfo],
    ) -> BoxFuture<'e, Result<PgStatement<'q>, sqlx::Error>>
    where
        'c: 'e,
    {
        self.0.prepare_with(sql, parameters)
    }

    fn describe<'e, 'q: 'e>(
        self,
        sql: &'q str,
    ) -> BoxFuture<'e, Result<Describe<Postgres>, sqlx::Error>>
    where
        'c: 'e,
    {
        self.0.describe(sql)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    /// PG-QUEUE-SEPARATION: checkout backlog and lock-containing SQL retain
    /// distinct physical populations, including work beyond the raw cap.
    #[cfg(feature = "perf-witness")]
    #[tokio::test]
    async fn checkout_and_lock_statement_windows_are_separate() {
        use lash_core_ids::perf_witness::queues::Collector;
        use std::future::Future;
        let database =
            crate::testing::IsolatedDatabase::create(&crate::testing::required_database_url())
                .await;
        let pool = sqlx::postgres::PgPoolOptions::new()
            .max_connections(1)
            .connect(database.url())
            .await
            .expect("one-connection pool");
        let other_pool = sqlx::postgres::PgPoolOptions::new()
            .max_connections(2)
            .connect(database.url())
            .await
            .expect("unrelated pool");
        let held = pool.acquire().await.expect("hold pool");
        let collector = Collector::for_task();
        collector
            .scope(async {
                let mut waiting = Box::pin(checkout(&pool));
                std::future::poll_fn(|cx| {
                    assert!(waiting.as_mut().poll(cx).is_pending());
                    std::task::Poll::Ready(())
                })
                .await;
                assert_eq!(
                    collector.snapshot().sites["postgres.pool_checkout"].waiting,
                    1
                );
                drop(held);
                let mut connection = waiting.await.expect("checkout");
                control("SELECT pg_advisory_xact_lock(1)", async {
                    sqlx::query("SELECT pg_advisory_xact_lock(1)")
                        .execute(&mut *connection)
                        .await
                        .expect("lock")
                })
                .await;
                // FIG-6763: unrelated tasks can check out another pool and run
                // lock-containing statements while this law's window is open.
                let unrelated = tokio::spawn(async move {
                    let (first, second) =
                        tokio::join!(checkout(&other_pool), checkout(&other_pool));
                    let mut first = first.expect("first unrelated checkout");
                    let second = second.expect("second unrelated checkout");
                    control("SELECT pg_advisory_xact_lock(2)", async {
                        sqlx::query("SELECT pg_advisory_xact_lock(2)")
                            .execute(&mut *first)
                            .await
                            .expect("unrelated lock")
                    })
                    .await;
                    drop((first, second));
                    other_pool.close().await;
                });
                unrelated.await.expect("unrelated task");
                let receipt = collector.snapshot();
                let checkout = &receipt.sites["postgres.pool_checkout"];
                let lock = &receipt.sites["postgres.lock_statement_upper_bound"];
                assert_eq!(
                    (
                        checkout.enqueued,
                        checkout.finished,
                        checkout.completed,
                        checkout.waiting
                    ),
                    (1, 1, 1, 0)
                );
                assert_eq!((lock.enqueued, lock.finished, lock.completed), (1, 1, 1));
                assert_eq!((checkout.service.max, lock.service.max), (0, 0));
                assert!(checkout.last_finished_ns <= lock.first_enqueued_ns);
                drop(connection);
            })
            .await;
        pool.close().await;
    }

    /// SQL-STREAM-DROP: abandoning an executor stream closes its observed
    /// population without fabricating command completion or unseen rows.
    #[cfg(feature = "perf-witness")]
    #[expect(
        clippy::disallowed_methods,
        reason = "isolates the exclusive process-global witness from parallel store laws"
    )]
    #[tokio::test]
    async fn dropped_query_streams_conserve_only_observed_work() {
        const CHILD: &str = "LASH_SQL_DROP_PG_CHILD";
        if std::env::var_os(CHILD).is_none() {
            let output = std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "observed_sql::tests::dropped_query_streams_conserve_only_observed_work",
                    "--nocapture",
                ])
                .env(CHILD, "1")
                .output()
                .unwrap();
            let stdout = String::from_utf8_lossy(&output.stdout);
            eprint!("{}", String::from_utf8_lossy(&output.stderr));
            assert!(output.status.success(), "isolated law failed: {stdout}");
            assert!(stdout.contains("test result: ok. 1 passed; 0 failed;"));
            return;
        }
        use lash_core_ids::perf_witness::{Collector, sql_shapes};
        let database =
            crate::testing::IsolatedDatabase::create(&crate::testing::required_database_url())
                .await;
        let mut connection = <sqlx::PgConnection as sqlx::Connection>::connect(database.url())
            .await
            .unwrap();
        let collector = Collector::install_with_sql_shapes().unwrap();
        sql_shapes::work::set_window(Some(80), "turn");
        let mut stream = query("SELECT generate_series(1, 3)").fetch(executor(&mut connection));
        stream.next().await.unwrap().unwrap();
        drop(stream);
        let work = collector.snapshot().sql_shapes.unwrap().work;
        assert!(
            work.active.is_empty(),
            "dropped stream stays active: {work:?}"
        );
        assert!(work.complete && work.conserves());
        assert_eq!(work.rows.len(), 1);
        let row = &work.rows[0];
        assert_eq!(row.counts.statements, 1);
        assert_eq!(row.counts.rows_returned, 1);
        assert_eq!(row.counts.rows_affected, 0);
        assert_eq!(row.counts.result_bytes, 4);
        assert_eq!(row.key.finished_in.as_ref().unwrap().turn, Some(80));
        let json = serde_json::to_value(row).unwrap();
        assert_eq!(json["disposition"], "dropped");

        // Cancellation before polling also drops the executor guard, but
        // cannot invent parameter extraction, rows, or command completion.
        // The query wrapper itself is lazy: this one never reaches the
        // executor, but must release its pending binding admission.
        drop(query("SELECT 2").fetch_optional(executor(&mut connection)));
        let cancelled = executor(&mut connection).fetch_optional(query("SELECT 1"));
        sql_shapes::work::set_window(Some(81), "turn");
        drop(cancelled);
        assert!(
            query("SELECT 1 / 0")
                .fetch_all(executor(&mut connection))
                .await
                .is_err()
        );
        assert_eq!(
            query("SELECT generate_series(1, 3)")
                .fetch_all(executor(&mut connection))
                .await
                .unwrap()
                .len(),
            3
        );
        let work = collector.snapshot().sql_shapes.unwrap().work;
        assert!(work.complete && work.conserves());
        assert!(work.active.is_empty());
        assert_eq!(work.pending_bindings, 0);
        let totals = &work.totals[&sql_shapes::work::Backend::Postgres];
        assert_eq!(totals.statements, 4);
        assert_eq!(totals.rows_returned, 4);
        assert_eq!(totals.rows_affected, 3);
        assert_eq!(totals.result_bytes, 16);
        use sql_shapes::work::Disposition;
        for (disposition, count) in [
            (Disposition::Dropped, 2),
            (Disposition::Error, 1),
            (Disposition::Finished, 1),
        ] {
            assert_eq!(
                work.rows
                    .iter()
                    .filter(|row| row.key.disposition == Some(disposition))
                    .map(|row| row.counts.statements)
                    .sum::<u64>(),
                count
            );
        }
        let cancelled = work
            .rows
            .iter()
            .find(|row| row.key.shape == "select ?")
            .unwrap();
        assert_eq!(cancelled.key.started_in.turn, Some(80));
        assert_eq!(cancelled.key.finished_in.as_ref().unwrap().turn, Some(81));
        assert_eq!(cancelled.counts.rows_returned, 0);
        assert_eq!(cancelled.counts.rows_affected, 0);
    }

    /// SQL-COST-JOIN: PostgreSQL retains one join invocation, binary bind
    /// bytes and command rows, while exact parameter changes split repeats.
    #[cfg(feature = "perf-witness")]
    #[expect(
        clippy::disallowed_methods,
        reason = "isolates the exclusive process-global witness from parallel store laws"
    )]
    #[tokio::test]
    async fn turn_sql_work_joins_encoded_payloads_and_exact_reads() {
        const CHILD: &str = "LASH_SQL_WORK_PG_CHILD";
        if std::env::var_os(CHILD).is_none() {
            let output = std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "observed_sql::tests::turn_sql_work_joins_encoded_payloads_and_exact_reads",
                    "--nocapture",
                ])
                .env(CHILD, "1")
                .output()
                .unwrap();
            let stdout = String::from_utf8_lossy(&output.stdout);
            eprint!("{}", String::from_utf8_lossy(&output.stderr));
            assert!(output.status.success(), "isolated law failed: {stdout}");
            assert!(
                stdout.contains("test result: ok. 1 passed; 0 failed;"),
                "law did not execute: {stdout}"
            );
            return;
        }

        use lash_core_ids::perf_witness::Collector;
        use lash_core_ids::perf_witness::sql_shapes;
        let database =
            crate::testing::IsolatedDatabase::create(&crate::testing::required_database_url())
                .await;
        let storage = crate::testing::connect(database.url()).await.unwrap();
        let mut connection = storage.pool().acquire().await.unwrap();
        sqlx::raw_sql("CREATE TEMP TABLE join_a(id bigint); CREATE TEMP TABLE join_b(id bigint); INSERT INTO join_a VALUES(1); INSERT INTO join_b VALUES(1)")
            .execute(&mut *connection).await.unwrap();
        let collector = Collector::install_with_sql_shapes().unwrap();
        sql_shapes::work::set_window(Some(1), "turn");
        for value in [1_i64, 1, 2] {
            let rows = query("SELECT join_a.id FROM join_a JOIN join_b ON join_a.id=join_b.id WHERE join_a.id=$1")
                .bind(value).fetch_all(executor(&mut *connection)).await.unwrap();
            assert_eq!(rows.len(), usize::from(value == 1));
        }
        let work = collector.snapshot().sql_shapes.unwrap().work;
        assert!(work.complete && work.conserves());
        assert_eq!(
            work.totals[&lash_core_ids::perf_witness::sql_shapes::work::Backend::Postgres]
                .statements,
            3
        );
        assert_eq!(
            work.totals[&lash_core_ids::perf_witness::sql_shapes::work::Backend::Postgres]
                .rows_returned,
            2
        );
        assert_eq!(
            work.totals[&lash_core_ids::perf_witness::sql_shapes::work::Backend::Postgres]
                .rows_affected,
            2
        ); // PostgreSQL SELECT completion
        assert_eq!(
            work.totals[&lash_core_ids::perf_witness::sql_shapes::work::Backend::Postgres]
                .bound_bytes,
            24
        ); // binary int8, not literal spelling
        assert_eq!(
            work.totals[&lash_core_ids::perf_witness::sql_shapes::work::Backend::Postgres]
                .result_bytes,
            16
        );
        assert_eq!(work.repeated_reads.len(), 1);
        assert_eq!(work.repeated_reads[0].executions, 2);
        assert_eq!(
            work.rows.iter().map(|r| r.counts.statements).sum::<u64>(),
            3
        );
        assert!(
            work.rows
                .iter()
                .all(|r| r.key.relations.as_deref() == Some(&["join_a".into(), "join_b".into()]))
        );
        eprintln!("TURN_SQL_WORK {}", serde_json::to_string(&work).unwrap());
    }

    /// SQL-WORK: one known operation has exactly one call and one shape;
    /// returned rows count stream rows, and server work remains unknown.
    #[tokio::test]
    async fn sql_operation_reports_exact_shapes_and_rows() {
        let database =
            crate::testing::IsolatedDatabase::create(&crate::testing::required_database_url())
                .await;
        let storage = crate::testing::connect(database.url())
            .await
            .expect("store");
        for size in [2_i64, 8] {
            let (answer, receipt) = sql::collect(
                format!("known-select/{size}"),
                "postgres",
                crate::observed_sql::query("SELECT generate_series(1, $1)")
                    .bind(size)
                    .fetch_all(executor(storage.pool())),
            )
            .await;
            assert_eq!(answer.expect("query").len(), size as usize);
            assert_eq!(receipt.statements, 1);
            assert_eq!(receipt.shapes.len(), 1);
            assert_eq!(receipt.rows_returned, size as u64);
            assert_eq!(receipt.rows_affected, size as u64); // server SELECT command completion
            assert_eq!(receipt.bound_bytes, 8);
            assert_eq!(receipt.result_bytes, size as u64 * 8);
            assert_eq!(receipt.shapes["select generate_series(?, ?)"].statements, 1);
            assert_eq!(receipt.sqlite_work, None);
            eprintln!(
                "SQL_WORK {}",
                serde_json::to_string(&receipt).expect("receipt")
            );
        }
        // SQL-PAYLOAD: a RETURNING write must retain command completion even
        // when its caller wants only the first row. Count each parameter's
        // actual encoding once, including UTF-8 text and binary bytea.
        let mut connection = storage.pool().acquire().await.expect("connection");
        sqlx::query("CREATE TEMP TABLE payload_sample (text TEXT, bytes BYTEA, number BIGINT)")
            .execute(&mut *connection)
            .await
            .expect("fixture");
        let (answer, receipt) = sql::collect("known-returning", "postgres",
            query("INSERT INTO payload_sample SELECT $1, $2, $3 UNION ALL SELECT $1, $2, $3 RETURNING text, bytes, number")
                .bind("é'a").bind(vec![0_u8, 1, 2]).bind(42_i64)
                .fetch_optional(executor(&mut *connection))).await;
        assert_eq!(
            answer
                .expect("insert")
                .expect("first row")
                .get::<String, _>(0),
            "é'a"
        );
        assert_eq!(receipt.statements, 1);
        assert_eq!(receipt.rows_affected, 2);
        assert_eq!(receipt.rows_returned, 2);
        assert_eq!(receipt.bound_bytes, 15);
        assert_eq!(receipt.result_bytes, 30);
        eprintln!("SQL_PAYLOAD {}", serde_json::to_string(&receipt).unwrap());
    }
    /// SQL-WORK: every requested operation is observed at each history size.
    #[tokio::test]
    async fn sql_work_receipts_cover_two_history_sizes() {
        let database =
            crate::testing::IsolatedDatabase::create(&crate::testing::required_database_url())
                .await;
        let storage = crate::testing::connect(database.url())
            .await
            .expect("store");
        let stores = crate::PostgresStoreSet::new(
            &storage,
            Arc::new(lash_core_store::attachments::UnavailableAttachmentStore),
        );
        let receipts = lash_core_execution::testing::sql_work::receipts(&stores, "postgres").await;
        assert_eq!(receipts.len(), 10);
        for receipt in receipts {
            assert_eq!(receipt.sqlite_work, None);
            eprintln!(
                "SQL_WORK {}",
                serde_json::to_string(&receipt).expect("receipt")
            );
        }
    }
    /// SQL-OWNER: production summaries carry the retained actor fence and
    /// caller operation identity, independent of logical transition metrics.
    #[tokio::test]
    async fn physical_summary_joins_the_owner_commit_and_caller_window() {
        let captured = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let sink = captured.clone();
        let observer = lash_core_ids::operational_metrics::StoreObserver::default().with_sql_sink(
            move |receipt, outcome| {
                if receipt.operation == "sql-work.owner" {
                    sink.lock().expect("sink").push((receipt.clone(), outcome));
                }
            },
        );
        let database =
            crate::testing::IsolatedDatabase::create(&crate::testing::required_database_url())
                .await;
        let storage = crate::PostgresStorage::connect(
            &crate::PostgresEndpoints::from_url(database.url()).expect("endpoints"),
            &crate::testing::fixture_config(),
            observer,
        )
        .await
        .expect("stores");
        let stores = crate::PostgresStoreSet::new(
            &storage,
            Arc::new(lash_core_store::attachments::UnavailableAttachmentStore),
        );
        let (identity, outer) = lash_core_ids::operational_metrics::sql::collect(
            "caller/session/turn/record",
            "postgres",
            lash_core_execution::testing::sql_work::owner_commit(&stores),
        )
        .await;
        let receipts = captured.lock().expect("receipts");
        assert_eq!(receipts.len(), 1);
        let (receipt, outcome) = &receipts[0];
        assert_eq!(*outcome, "success");
        assert_eq!(receipt.owner.as_ref(), Some(&identity));
        assert_eq!(
            receipt.parent_operation.as_deref(),
            Some(outer.operation.as_str())
        );
        assert!(receipt.statements > 0);
        eprintln!(
            "SQL_WORK_OWNER {}",
            serde_json::to_string(receipt).expect("receipt")
        );
    }
}

#[path = "observed_sql/bindings.rs"]
mod bindings;
pub(crate) use bindings::{Query, query, query_as, query_scalar};

/// Carry the execution token to SQLx's argument extraction, wherever its
/// future is polled. No task/thread identity guess joins binds to a statement.
#[cfg(feature = "perf-witness")]
struct JoinedQuery<Q> {
    inner: Q,
    work: Option<lash_core_ids::perf_witness::sql_shapes::work::Execution>,
    repeat_qualified: bool,
}
#[cfg(feature = "perf-witness")]
impl<'q, Q: Execute<'q, Postgres>> Execute<'q, Postgres> for JoinedQuery<Q> {
    fn sql(&self) -> &'q str {
        self.inner.sql()
    }
    fn statement(&self) -> Option<&PgStatement<'q>> {
        self.inner.statement()
    }
    fn persistent(&self) -> bool {
        self.inner.persistent()
    }
    fn take_arguments(
        &mut self,
    ) -> Result<Option<sqlx::postgres::PgArguments>, sqlx::error::BoxDynError> {
        let Some(work) = self.work else {
            return self.inner.take_arguments();
        };
        let sql = self.inner.sql();
        let (arguments, captured) = bindings::capture(|| self.inner.take_arguments());
        let (bytes, key) = match captured {
            Some(value) => value,
            None if arguments.as_ref().is_ok_and(|v| v.is_none()) => (Some(0), Some(Vec::new())),
            None => (None, None),
        };
        let key = key.filter(|_| self.repeat_qualified).and_then(|key| {
            (key.len() + sql.len() + 8
                <= lash_core_ids::perf_witness::sql_shapes::work::VALUE_LIMIT)
                .then(|| {
                    let mut identity = (sql.len() as u64).to_le_bytes().to_vec();
                    identity.extend_from_slice(sql.as_bytes());
                    identity.extend_from_slice(&key);
                    identity
                })
        });
        let class = if !self.repeat_qualified {
            "pg_read_syntax_unqualified"
        } else if key.is_some() {
            "pg_encoded_exact"
        } else {
            "pg_encoding_unqualified"
        };
        work.bindings(bytes, key, class);
        arguments
    }
}
