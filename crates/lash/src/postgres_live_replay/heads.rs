//! Session heads held under their row locks for one transaction: what a
//! publication, a cleanup run or a trim reads, changes and writes back.

use std::collections::BTreeMap;

use lash_core::LiveReplayStoreError;
use sqlx::postgres::PgRow;
use sqlx::{Postgres, Row as _, Transaction};

use super::codec::Doorbell;
use super::schema::{Incarnation, Statements, column, db_error, micros, position};

/// One session's head, as locked and as this transaction changes it.
#[derive(Clone, Debug)]
pub(super) struct Head {
    pub(super) tail: u64,
    pub(super) floor: u64,
    pub(super) first_retained: u64,
    pub(super) events: u64,
    pub(super) bytes: u64,
    /// Whether the oldest retained event is past the window's age.
    pub(super) expiring: bool,
    /// The first retained position when the transaction locked the head.
    locked_first_retained: u64,
}

impl Head {
    fn from_row(row: &PgRow) -> (String, Self) {
        let first_retained = position(row.get("first_retained"));
        (
            row.get("session_id"),
            Self {
                tail: position(row.get("tail_position")),
                floor: position(row.get("floor_position")),
                first_retained,
                events: position(row.get("retained_events")),
                bytes: position(row.get("retained_bytes")),
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
}

/// The heads one transaction holds, by session id, and the incarnation
/// their lock read: `None` when its sentinel is gone.
#[derive(Debug, Default)]
pub(super) struct Heads {
    pub(super) heads: BTreeMap<String, Head>,
    pub(super) incarnation: Option<Option<Incarnation>>,
}

/// The incarnation a head lock's row read, or `None` when it must rotate.
fn incarnation(row: &PgRow) -> Option<Incarnation> {
    row.get::<bool, _>("valid").then(|| Incarnation {
        id: row.get("incarnation_id"),
        watermark: position(row.get("watermark")),
    })
}

/// Why a transaction must start over.
#[derive(Debug)]
pub(super) enum Retry {
    /// The incarnation's sentinel is gone: rotate, then write.
    Rotate,
    /// A head vanished between creation and lock, or the transaction lost
    /// a deadlock or serialization race.
    Race,
}

/// A failed transaction step: retry it, or answer the error.
#[derive(Debug)]
pub(super) enum Attempt {
    Retry(Retry),
    Failed(LiveReplayStoreError),
}

impl From<LiveReplayStoreError> for Attempt {
    fn from(error: LiveReplayStoreError) -> Self {
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
    /// Lock the heads of `sessions` (sorted), creating the missing ones at
    /// the watermark. Every writer locks in session order, so concurrent
    /// transactions over overlapping sessions never deadlock.
    pub(super) async fn lock(
        tx: &mut Transaction<'_, Postgres>,
        sql: &Statements,
        sessions: &[String],
        max_age: std::time::Duration,
        create: bool,
    ) -> Result<Self, Attempt> {
        let mut heads = Heads::default();
        heads.lock_existing(tx, sql, sessions, max_age).await?;
        if !create {
            return Ok(heads);
        }
        let missing = heads.missing(sessions);
        if missing.is_empty() {
            return Ok(heads);
        }
        // Created rows are locked by their insertion; one a racing writer
        // created first is locked by a second pass.
        for row in sqlx::query(&sql.create_heads)
            .bind(&missing)
            .fetch_all(&mut **tx)
            .await
            .map_err(attempt("create heads"))?
        {
            heads.incarnation.get_or_insert_with(|| incarnation(&row));
            let (session, head) = Head::from_row(&row);
            heads.heads.insert(session, head);
        }
        let raced = heads.missing(sessions);
        if !raced.is_empty() {
            heads.lock_existing(tx, sql, &raced, max_age).await?;
            if !heads.missing(sessions).is_empty() {
                return Err(Attempt::Retry(Retry::Race));
            }
        }
        Ok(heads)
    }

    async fn lock_existing(
        &mut self,
        tx: &mut Transaction<'_, Postgres>,
        sql: &Statements,
        sessions: &[String],
        max_age: std::time::Duration,
    ) -> Result<(), Attempt> {
        for row in sqlx::query(&sql.lock_heads)
            .bind(sessions)
            .bind(micros(max_age))
            .fetch_all(&mut **tx)
            .await
            .map_err(attempt("lock heads"))?
        {
            self.incarnation.get_or_insert_with(|| incarnation(&row));
            let (session, head) = Head::from_row(&row);
            self.heads.insert(session, head);
        }
        Ok(())
    }

    fn missing(&self, sessions: &[String]) -> Vec<String> {
        sessions
            .iter()
            .filter(|session| !self.heads.contains_key(*session))
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
            .map(|(session, _)| session.clone())
            .collect::<Vec<_>>();
        if expiring.is_empty() {
            return Ok(());
        }
        let rows = sqlx::query(&sql.expire_sessions)
            .bind(&expiring)
            .bind(micros(max_age))
            .fetch_all(&mut **tx)
            .await
            .map_err(attempt("expire events"))?;
        self.deleted(&rows);
        Ok(())
    }

    /// Delete each listed session's events below its position.
    pub(super) async fn delete_below(
        &mut self,
        tx: &mut Transaction<'_, Postgres>,
        sql: &Statements,
        below: &[(String, u64)],
    ) -> Result<(), Attempt> {
        if below.is_empty() {
            return Ok(());
        }
        let (sessions, positions): (Vec<String>, Vec<i64>) = below
            .iter()
            .map(|(session, below)| (session.clone(), column(*below)))
            .unzip();
        let rows = sqlx::query(&sql.delete_below)
            .bind(&sessions)
            .bind(&positions)
            .fetch_all(&mut **tx)
            .await
            .map_err(attempt("trim events"))?;
        self.deleted(&rows);
        Ok(())
    }

    /// Trim the oldest events of every head over `max_bytes`.
    pub(super) async fn trim_bytes(
        &mut self,
        tx: &mut Transaction<'_, Postgres>,
        sql: &Statements,
        max_bytes: u64,
    ) -> Result<(), Attempt> {
        let over = self
            .heads
            .iter()
            .filter(|(_, head)| head.bytes > max_bytes)
            .map(|(session, _)| session.clone())
            .collect::<Vec<_>>();
        if over.is_empty() {
            return Ok(());
        }
        let rows = sqlx::query(&sql.trim_bytes)
            .bind(&over)
            .bind(column(max_bytes))
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
            let session: String = row.get("session_id");
            let Some(head) = self.heads.get_mut(&session) else {
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

    /// The trim doorbells of every head whose window start moved.
    pub(super) fn trimmed(&self, doorbells: &mut Vec<Doorbell>) {
        for (session, head) in &self.heads {
            if head.first_retained > head.locked_first_retained
                && head.first_retained > head.floor + 1
            {
                doorbells.push(Doorbell::Trimmed {
                    session: session.clone(),
                    first_retained: head.first_retained,
                });
            }
        }
    }

    /// Every head as the update's columns.
    pub(super) fn columns(&self) -> HeadColumns {
        let mut columns = HeadColumns::default();
        for (session, head) in &self.heads {
            columns.session.push(session.clone());
            columns.tail.push(column(head.tail));
            columns.floor.push(column(head.floor));
            columns.first_retained.push(column(head.first_retained));
            columns.events.push(column(head.events));
            columns.bytes.push(column(head.bytes));
        }
        columns
    }

    /// Write every head back and ring `doorbells` when the transaction
    /// commits.
    pub(super) async fn write(
        &self,
        tx: &mut Transaction<'_, Postgres>,
        sql: &Statements,
        doorbells: &[Doorbell],
    ) -> Result<(), Attempt> {
        let columns = self.columns();
        sqlx::query(&sql.update_heads)
            .bind(&columns.session)
            .bind(&columns.tail)
            .bind(&columns.floor)
            .bind(&columns.first_retained)
            .bind(&columns.events)
            .bind(&columns.bytes)
            .bind(&sql.channel)
            .bind(super::codec::pack_doorbells(doorbells)?)
            .execute(&mut **tx)
            .await
            .map_err(attempt("write heads"))?;
        Ok(())
    }
}

/// Heads as the column arrays an update unnests.
#[derive(Default)]
pub(super) struct HeadColumns {
    pub(super) session: Vec<String>,
    pub(super) tail: Vec<i64>,
    pub(super) floor: Vec<i64>,
    pub(super) first_retained: Vec<i64>,
    pub(super) events: Vec<i64>,
    pub(super) bytes: Vec<i64>,
}
