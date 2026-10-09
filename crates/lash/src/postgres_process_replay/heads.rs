//! Process heads held under their row locks for one transaction: what a
//! publication, a cleanup run or a trim reads, changes and writes back.

use std::collections::BTreeMap;

use lash_core::ProcessReplayStoreError;
use sqlx::postgres::PgRow;
use sqlx::{Postgres, Row as _, Transaction};

use super::codec::Doorbell;
use super::schema::{Incarnation, Statements, column, db_error, micros, position};

/// One process's head, as locked and as this transaction changes it.
#[derive(Clone, Debug)]
pub(super) struct Head {
    pub(super) tail: u64,
    pub(super) floor: u64,
    pub(super) first_retained: u64,
    pub(super) events: u64,
    pub(super) bytes: u64,
    /// The process's share of the aggregate byte budget: its window is
    /// trimmed to it.
    pub(super) reserved: u64,
    /// Whether the oldest retained event is past the window's age.
    pub(super) expiring: bool,
    /// The first retained position when the transaction locked the head.
    locked_first_retained: u64,
}

impl Head {
    /// The head of a process that had none: its positions start past
    /// `watermark`.
    pub(super) fn created(watermark: u64) -> Self {
        Self {
            tail: watermark,
            floor: watermark,
            first_retained: watermark + 1,
            events: 0,
            bytes: 0,
            reserved: 0,
            expiring: false,
            locked_first_retained: watermark + 1,
        }
    }

    fn from_row(row: &PgRow) -> (String, Self) {
        let first_retained = position(row.get("first_retained"));
        (
            row.get("process_id"),
            Self {
                tail: position(row.get("tail_position")),
                floor: position(row.get("floor_position")),
                first_retained,
                events: position(row.get("retained_events")),
                bytes: position(row.get("retained_bytes")),
                reserved: position(row.get("reserved_bytes")),
                expiring: row.get("expiring"),
                locked_first_retained: first_retained,
            },
        )
    }

    /// Start a new generation: every position so far is below the floor,
    /// and one position is burned so a cursor at the old tail gaps.
    pub(super) fn invalidate(&mut self) {
        self.tail += 1;
        self.floor = self.tail;
        self.first_retained = self.tail + 1;
        self.events = 0;
        self.bytes = 0;
        self.expiring = false;
    }

    /// Whether this transaction dropped events from the window's start.
    pub(super) fn trimmed(&self) -> bool {
        self.first_retained > self.locked_first_retained
    }
}

/// The heads one transaction holds, by process id, and the incarnation
/// their lock read: `Some(None)` when its sentinel is gone.
#[derive(Debug, Default)]
pub(super) struct Heads {
    pub(super) heads: BTreeMap<String, Head>,
    pub(super) incarnation: Option<Option<Incarnation>>,
}

/// Why a transaction must start over.
#[derive(Debug)]
pub(super) enum Retry {
    /// The incarnation's sentinel is gone: rotate, then write.
    Rotate,
    /// A head appeared or vanished between the lock and its use, or the
    /// transaction lost a deadlock or serialization race.
    Race,
}

/// A failed transaction step: retry it, or answer the error.
#[derive(Debug)]
pub(super) enum Attempt {
    Retry(Retry),
    Failed(ProcessReplayStoreError),
}

impl From<ProcessReplayStoreError> for Attempt {
    fn from(error: ProcessReplayStoreError) -> Self {
        Self::Failed(error)
    }
}

/// A database error, retried when PostgreSQL chose this transaction as a
/// deadlock or serialization victim.
pub(super) fn attempt(context: &'static str) -> impl Fn(sqlx::Error) -> Attempt {
    move |error| {
        let race = error
            .as_database_error()
            .and_then(|error| error.code())
            .is_some_and(|code| code == "40P01" || code == "40001");
        if race {
            Attempt::Retry(Retry::Race)
        } else {
            Attempt::Failed(db_error(context)(error))
        }
    }
}

impl Heads {
    /// Lock the existing heads of `processes` (sorted). Every writer locks
    /// in process order, so concurrent transactions over overlapping
    /// processes never deadlock.
    pub(super) async fn lock(
        tx: &mut Transaction<'_, Postgres>,
        sql: &Statements,
        processes: &[String],
        max_age: std::time::Duration,
    ) -> Result<Self, Attempt> {
        let mut heads = Heads::default();
        for row in sqlx::query(&sql.lock_heads)
            .bind(processes)
            .bind(micros(max_age))
            .fetch_all(&mut **tx)
            .await
            .map_err(attempt("lock heads"))?
        {
            heads.incarnation.get_or_insert_with(|| {
                let id: Option<String> = row.get("incarnation_id");
                let sentinel: Option<String> = row.get("sentinel_id");
                id.filter(|id| sentinel.as_ref() == Some(id))
                    .map(|id| Incarnation {
                        id,
                        watermark: position(
                            row.get::<Option<i64>, _>("watermark").unwrap_or_default(),
                        ),
                    })
            });
            let (process, head) = Head::from_row(&row);
            heads.heads.insert(process, head);
        }
        Ok(heads)
    }

    pub(super) fn missing(&self, processes: &[String]) -> Vec<String> {
        processes
            .iter()
            .filter(|process| !self.heads.contains_key(*process))
            .cloned()
            .collect()
    }

    /// Delete the events past the window's age of every expiring head.
    pub(super) async fn expire(
        &mut self,
        tx: &mut Transaction<'_, Postgres>,
        sql: &Statements,
        max_age: std::time::Duration,
    ) -> Result<(), Attempt> {
        let expiring = self
            .heads
            .iter()
            .filter(|(_, head)| head.expiring)
            .map(|(process, _)| process.clone())
            .collect::<Vec<_>>();
        if expiring.is_empty() {
            return Ok(());
        }
        let rows = sqlx::query(&sql.expire_processes)
            .bind(&expiring)
            .bind(micros(max_age))
            .fetch_all(&mut **tx)
            .await
            .map_err(attempt("expire events"))?;
        self.deleted(&rows);
        Ok(())
    }

    /// Delete each listed process's events below its position.
    pub(super) async fn delete_below(
        &mut self,
        tx: &mut Transaction<'_, Postgres>,
        sql: &Statements,
        below: &[(String, u64)],
    ) -> Result<(), Attempt> {
        if below.is_empty() {
            return Ok(());
        }
        let (processes, positions): (Vec<String>, Vec<i64>) = below
            .iter()
            .map(|(process, below)| (process.clone(), column(*below)))
            .unzip();
        let rows = sqlx::query(&sql.delete_below)
            .bind(&processes)
            .bind(&positions)
            .fetch_all(&mut **tx)
            .await
            .map_err(attempt("trim events"))?;
        self.deleted(&rows);
        Ok(())
    }

    /// Trim the oldest events of every head holding more bytes than
    /// `limit` gives it.
    pub(super) async fn trim_bytes(
        &mut self,
        tx: &mut Transaction<'_, Postgres>,
        sql: &Statements,
        limit: impl Fn(&Head) -> u64,
    ) -> Result<(), Attempt> {
        let (over, limits): (Vec<String>, Vec<i64>) = self
            .heads
            .iter()
            .filter(|(_, head)| head.bytes > limit(head))
            .map(|(process, head)| (process.clone(), column(limit(head))))
            .unzip();
        if over.is_empty() {
            return Ok(());
        }
        let rows = sqlx::query(&sql.trim_bytes)
            .bind(&over)
            .bind(&limits)
            .fetch_all(&mut **tx)
            .await
            .map_err(attempt("trim bytes"))?;
        self.deleted(&rows);
        Ok(())
    }

    /// Account deleted rows: a row of the current generation leaves the
    /// window's counters and raises its first retained position; a row
    /// below the floor was already uncounted.
    fn deleted(&mut self, rows: &[PgRow]) {
        for row in rows {
            let process: String = row.get("process_id");
            let Some(head) = self.heads.get_mut(&process) else {
                continue;
            };
            let deleted = position(row.get("position"));
            if deleted > head.floor {
                head.events = head.events.saturating_sub(1);
                head.bytes = head.bytes.saturating_sub(position(row.get("bytes")));
                head.first_retained = head.first_retained.max(deleted + 1);
            }
        }
    }

    /// Delete the identities of the events this transaction dropped from
    /// its windows.
    pub(super) async fn trim_dedupe(
        &self,
        tx: &mut Transaction<'_, Postgres>,
        sql: &Statements,
    ) -> Result<(), Attempt> {
        let (processes, below): (Vec<String>, Vec<i64>) = self
            .heads
            .iter()
            .filter(|(_, head)| head.trimmed())
            .map(|(process, head)| (process.clone(), column(head.first_retained)))
            .unzip();
        if processes.is_empty() {
            return Ok(());
        }
        sqlx::query(&sql.trim_dedupe)
            .bind(&processes)
            .bind(&below)
            .execute(&mut **tx)
            .await
            .map_err(attempt("trim identities"))?;
        Ok(())
    }

    /// Every head as the update's columns.
    pub(super) fn columns(&self) -> HeadColumns {
        let mut columns = HeadColumns::default();
        for (process, head) in &self.heads {
            columns.process.push(process.clone());
            columns.tail.push(column(head.tail));
            columns.floor.push(column(head.floor));
            columns.first_retained.push(column(head.first_retained));
            columns.events.push(column(head.events));
            columns.bytes.push(column(head.bytes));
            columns.reserved.push(column(head.reserved));
        }
        columns
    }

    /// Write every head back and ring `doorbell` when the transaction
    /// commits.
    pub(super) async fn write(
        &self,
        tx: &mut Transaction<'_, Postgres>,
        sql: &Statements,
        doorbell: &Doorbell,
    ) -> Result<(), Attempt> {
        self.columns()
            .bind(sqlx::query(&sql.update_heads))
            .bind(&sql.channel)
            .bind(doorbell.pack()?)
            .execute(&mut **tx)
            .await
            .map_err(attempt("write heads"))?;
        Ok(())
    }
}

/// Heads as the column arrays an update unnests.
#[derive(Default)]
pub(super) struct HeadColumns {
    process: Vec<String>,
    tail: Vec<i64>,
    floor: Vec<i64>,
    first_retained: Vec<i64>,
    events: Vec<i64>,
    bytes: Vec<i64>,
    reserved: Vec<i64>,
}

impl HeadColumns {
    /// Bind the columns as a statement's next seven parameters.
    pub(super) fn bind<'q>(
        self,
        query: sqlx::query::Query<'q, Postgres, sqlx::postgres::PgArguments>,
    ) -> sqlx::query::Query<'q, Postgres, sqlx::postgres::PgArguments> {
        query
            .bind(self.process)
            .bind(self.tail)
            .bind(self.floor)
            .bind(self.first_retained)
            .bind(self.events)
            .bind(self.bytes)
            .bind(self.reserved)
    }
}
