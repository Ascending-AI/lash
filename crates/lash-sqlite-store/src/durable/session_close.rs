//! The session's closing state on SQLite: the `session_close` domain's statements, apply and read
//! (I0, FIG-5194).
//!
//! Owned by L6b (FIG-5176). The dispatch in `durable/mod.rs` calls these inside the
//! fenced owner commit (or the mailbox commit), after the fence; a refusal
//! rolls the whole commit back.

use std::sync::LazyLock;

use lash_durable::domain::{
    DomainRefusal, ScopeKey, SessionCloseRow, SessionCloseStep, SessionCloseWrite,
};
use lash_durable::{DurableError, DurableInstant, Epoch};
use lash_sansio::SessionId;
use lash_store_sql::durable::session_close::SessionCloseStatements;
use rusqlite::{Connection, OptionalExtension};

use super::{Answer, Committing, corrupt};
use crate::conn::cached_execute;

/// `session_close`: L6b's (FIG-5176) closing state, then tombstone, of a
/// session; `session_scope_ends`: its scopes whose cascade is still marking.
pub(crate) const TABLES: &str = "
CREATE TABLE IF NOT EXISTS session_close (
    session_id TEXT PRIMARY KEY,
    done_step TEXT CONSTRAINT ck_session_close_step
        CHECK (done_step IN ('cancel', 'revoke', 'end_scope', 'triggers', 'artifacts', 'tombstone')),
    begun_at_ms INTEGER NOT NULL,
    written_epoch INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS session_scope_ends (
    session_id TEXT NOT NULL,
    scope_key TEXT NOT NULL,
    begun_at_ms INTEGER NOT NULL,
    written_epoch INTEGER NOT NULL,
    PRIMARY KEY (session_id, scope_key)
);
";

static SQL: LazyLock<SessionCloseStatements> =
    LazyLock::new(|| SessionCloseStatements::render(crate::schema_layout::MAIN));

/// The stored row: its last step done, when it began, its writer's epoch.
type Stored = (Option<String>, i64, i64);

fn stored(tx: &Connection, session: &SessionId) -> rusqlite::Result<Option<Stored>> {
    tx.prepare_cached(SQL.row.sql())?
        .query_row([session.as_str()], |row| {
            Ok((row.get(0)?, row.get(1)?, row.get(2)?))
        })
        .optional()
}

fn step(stored: Option<&str>) -> Result<Option<SessionCloseStep>, DurableError> {
    stored
        .map(|step| {
            SessionCloseStep::parse(step).ok_or_else(|| corrupt("session close step", step))
        })
        .transpose()
}

pub(super) fn apply(
    tx: &Connection,
    commit: &Committing<'_>,
    write: &SessionCloseWrite,
) -> Answer<()> {
    match write {
        SessionCloseWrite::Begin { session } => {
            cached_execute(
                tx,
                SQL.begin.sql(),
                rusqlite::params![session.as_str(), commit.now.0, commit.epoch.0],
            )?;
            Ok(Ok(()))
        }
        SessionCloseWrite::Step {
            session,
            step: next,
        } => {
            let Some((done, _, _)) = stored(tx, session)? else {
                return Ok(Err(DurableError::Domain(
                    DomainRefusal::SessionNotClosing {
                        session: session.clone(),
                    },
                )));
            };
            let done = match step(done.as_deref()) {
                Ok(done) => done,
                Err(error) => return Ok(Err(error)),
            };
            let expected = done.map_or(Some(SessionCloseStep::Cancel), SessionCloseStep::next);
            if expected != Some(*next) {
                return Ok(Err(DurableError::Domain(
                    DomainRefusal::SessionCloseOutOfOrder {
                        session: session.clone(),
                        step: *next,
                        done,
                    },
                )));
            }
            cached_execute(
                tx,
                SQL.record_step.sql(),
                rusqlite::params![session.as_str(), next.as_str(), commit.epoch.0],
            )?;
            Ok(Ok(()))
        }
        SessionCloseWrite::ScopeEnded { session, scope } => {
            cached_execute(
                tx,
                SQL.scope_ended.sql(),
                rusqlite::params![session.as_str(), scope.stored()],
            )?;
            Ok(Ok(()))
        }
    }
}

/// Record `scope` of `session` as ending: its closure left a child to mark.
/// Recording a scope already ending changes nothing.
pub(super) fn record_ending(
    tx: &Connection,
    commit: &Committing<'_>,
    session: &SessionId,
    scope: &ScopeKey,
) -> Answer<()> {
    cached_execute(
        tx,
        SQL.scope_ending.sql(),
        rusqlite::params![
            session.as_str(),
            scope.stored(),
            commit.now.0,
            commit.epoch.0
        ],
    )?;
    Ok(Ok(()))
}

pub(super) fn ending_scopes(tx: &Connection, session: &SessionId) -> Answer<Vec<ScopeKey>> {
    let stored = tx
        .prepare_cached(SQL.ending_scopes.sql())?
        .query_map([session.as_str()], |row| row.get::<_, String>(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(stored
        .iter()
        .map(|key| ScopeKey::parse(key).map_err(|_| corrupt("scope key", key)))
        .collect())
}

pub(super) fn read(tx: &Connection, session: &SessionId) -> Answer<Option<SessionCloseRow>> {
    let Some((done, begun_at, written_epoch)) = stored(tx, session)? else {
        return Ok(Ok(None));
    };
    Ok(step(done.as_deref()).map(|done| {
        Some(SessionCloseRow {
            session: session.clone(),
            done,
            begun_at: DurableInstant(begun_at),
            written_epoch: Epoch(written_epoch),
        })
    }))
}
