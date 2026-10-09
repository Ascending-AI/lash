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
use lash_core_execution::facade_support::{DurableCommitCost, StoreObserver, sql};
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
    /// sent through the observed executor, and every checkout, which pings.
    pub(crate) static ROUND_TRIPS: std::cell::Cell<u64>;
}

#[cfg(test)]
fn round_trip() {
    let _ = ROUND_TRIPS.try_with(|trips| trips.set(trips.get() + 1));
}

/// Check a connection out of `pool`: the pool pings it first, a round trip
/// of its own.
pub(crate) async fn checkout(
    pool: &sqlx::PgPool,
) -> Result<sqlx::pool::PoolConnection<Postgres>, sqlx::Error> {
    #[cfg(test)]
    round_trip();
    let started = Instant::now();
    let connection = pool.acquire().await;
    acquired(started.elapsed());
    connection
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
    state: Option<Shared>,
    window: Option<sql::Window>,
    sql: String,
    started: Instant,
    locking: bool,
}

impl Statement {
    fn new(sql: &str) -> Option<Self> {
        #[cfg(test)]
        round_trip();
        let state = CURRENT.try_with(Arc::clone).ok();
        let window = sql::Window::current();
        if state.is_none() && window.is_none() {
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
            state,
            window,
            sql: text,
            started: Instant::now(),
            locking,
        })
    }

    fn row(&self, row: &PgRow) {
        let bytes = (0..row.len())
            .filter_map(|index| row.try_get_raw(index).ok())
            .filter_map(|value| value.as_bytes().ok().map(|bytes| bytes.len() as u64))
            .sum::<u64>();
        if let Some(window) = &self.window {
            window.row(&self.sql);
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
    let _statement = Statement::new(sql);
    future.await
}

#[derive(Debug)]
pub(crate) struct Observed<E>(E);

pub(crate) fn executor<E>(executor: E) -> Observed<E> {
    Observed(executor)
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
        let statement = Statement::new(query.sql());
        let stream = self.0.fetch_many(query);
        let Some(statement) = statement else {
            return stream;
        };
        Box::pin(stream.map(move |result| {
            if let Ok(Either::Right(row)) = &result {
                statement.row(row);
            }
            result
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
        let statement = Statement::new(query.sql());
        let future = self.0.fetch_optional(query);
        let Some(statement) = statement else {
            return future;
        };
        Box::pin(async move {
            let result = future.await;
            if let Ok(Some(row)) = &result {
                statement.row(row);
            }
            result
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
                sqlx::query("SELECT generate_series(1, $1)")
                    .bind(size)
                    .fetch_all(executor(storage.pool())),
            )
            .await;
            assert_eq!(answer.expect("query").len(), size as usize);
            assert_eq!(receipt.statements, 1);
            assert_eq!(receipt.shapes.len(), 1);
            assert_eq!(receipt.rows_returned, size as u64);
            assert_eq!(receipt.shapes["select generate_series(?, ?)"].statements, 1);
            assert_eq!(receipt.sqlite_work, None);
            eprintln!(
                "SQL_WORK {}",
                serde_json::to_string(&receipt).expect("receipt")
            );
        }
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
            Arc::new(lash_core_execution::attachments::UnavailableAttachmentStore),
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
        let observer = lash_core_execution::facade_support::StoreObserver::default().with_sql_sink(
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
            Arc::new(lash_core_execution::attachments::UnavailableAttachmentStore),
        );
        let (identity, outer) = lash_core_execution::facade_support::sql::collect(
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
