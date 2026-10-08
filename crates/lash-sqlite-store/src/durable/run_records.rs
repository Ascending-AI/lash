//! Run records on SQLite: the `run_records` domain's statements, apply and read
//! (I0, FIG-5194).
//!
//! Owned by V0 (FIG-5170), then L4 (FIG-5174). The dispatch in `durable/mod.rs` calls these inside the
//! fenced owner commit (or the mailbox commit), after the fence; a refusal
//! rolls the whole commit back.

use std::sync::LazyLock;

use lash_durable::domain::{
    DomainRefusal, Ordinal, OwnerKey, RunRecordKind, RunRecordRow, RunRecordWrite, RunSeq,
};
use lash_durable::{DurableError, Epoch};
use lash_sansio::ToolCallId;
use lash_store_sql::durable::run_records::RunRecordStatements;
use rusqlite::{Connection, OptionalExtension};

use super::{Answer, Committing, corrupt, integer};
use crate::conn::cached_execute;

static SQL: LazyLock<RunRecordStatements> =
    LazyLock::new(|| RunRecordStatements::render(crate::schema_layout::MAIN));

/// `run_records`, created by I0 (FIG-5194); its statements are V0's.
pub(crate) const TABLES: &str = "
CREATE TABLE IF NOT EXISTS run_records (
    owner_key TEXT NOT NULL,
    run_seq INTEGER NOT NULL,
    ordinal INTEGER NOT NULL,
    kind TEXT NOT NULL CONSTRAINT ck_run_records_kind
        CHECK (kind IN ('admit', 'x_start', 'x_outcome', 'x_wait', 'decide', 'present', 'retry')),
    call_id TEXT,
    record_json TEXT NOT NULL,
    written_epoch INTEGER NOT NULL,
    PRIMARY KEY (owner_key, run_seq, ordinal)
);
CREATE UNIQUE INDEX IF NOT EXISTS ux_run_records_outcome
    ON run_records (owner_key, run_seq, call_id)
    WHERE kind = 'x_outcome' AND call_id IS NOT NULL;
";

pub(super) fn apply(
    tx: &Connection,
    commit: &Committing<'_>,
    write: &RunRecordWrite,
) -> Answer<()> {
    match write {
        RunRecordWrite::Append {
            owner,
            run,
            ordinal,
            kind,
            call,
            record_json,
        } => {
            let owner_key = owner.stored();
            let taken = tx
                .prepare_cached(SQL.ordinal_taken.sql())?
                .query_row(
                    rusqlite::params![
                        owner_key,
                        integer::<i64>(run.0)?,
                        integer::<i64>(ordinal.0)?
                    ],
                    |_| Ok(()),
                )
                .optional()?;
            if taken.is_some() {
                return Ok(Err(DurableError::Domain(DomainRefusal::RunOrdinalTaken {
                    owner: owner.clone(),
                    run: *run,
                    ordinal: *ordinal,
                })));
            }
            if let Some(previous) = ordinal.0.checked_sub(1) {
                let follows = tx
                    .prepare_cached(SQL.ordinal_taken.sql())?
                    .query_row(
                        rusqlite::params![
                            owner_key,
                            integer::<i64>(run.0)?,
                            integer::<i64>(previous)?
                        ],
                        |_| Ok(()),
                    )
                    .optional()?;
                if follows.is_none() {
                    return Ok(Err(DurableError::Domain(DomainRefusal::RunOrdinalGap {
                        owner: owner.clone(),
                        run: *run,
                        ordinal: *ordinal,
                    })));
                }
            }
            if *kind == RunRecordKind::XOutcome
                && let Some(call) = call
            {
                let exists = tx
                    .prepare_cached(SQL.outcome_exists.sql())?
                    .query_row(
                        rusqlite::params![owner_key, integer::<i64>(run.0)?, call.as_str()],
                        |_| Ok(()),
                    )
                    .optional()?;
                if exists.is_some() {
                    return Ok(Err(DurableError::Domain(DomainRefusal::OutcomeExists {
                        owner: owner.clone(),
                        run: *run,
                        call: call.clone(),
                    })));
                }
            }
            cached_execute(
                tx,
                SQL.append.sql(),
                rusqlite::params![
                    owner_key,
                    integer::<i64>(run.0)?,
                    integer::<i64>(ordinal.0)?,
                    kind.as_str(),
                    call.as_ref().map(ToolCallId::as_str),
                    record_json,
                    commit.epoch.0,
                ],
            )?;
            Ok(Ok(()))
        }
        RunRecordWrite::Prune { owner, before } => {
            cached_execute(
                tx,
                SQL.prune.sql(),
                rusqlite::params![owner.stored(), integer::<i64>(before.0)?],
            )?;
            Ok(Ok(()))
        }
    }
}

pub(super) fn read(tx: &Connection, owner: &OwnerKey) -> Answer<Vec<RunRecordRow>> {
    let rows = tx
        .prepare_cached(SQL.read.sql())?
        .query_map([owner.stored()], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, i64>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, Option<String>>(3)?,
                row.get::<_, String>(4)?,
                row.get::<_, i64>(5)?,
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let mut records = Vec::with_capacity(rows.len());
    for (run, ordinal, kind, call, record_json, epoch) in rows {
        let Some(kind) = RunRecordKind::parse(&kind) else {
            return Ok(Err(corrupt("run record kind", &kind)));
        };
        let call = match call {
            None => None,
            Some(call) => match ToolCallId::parse(&call) {
                Ok(call) => Some(call),
                Err(_) => return Ok(Err(corrupt("tool call id", &call))),
            },
        };
        records.push(RunRecordRow {
            owner: owner.clone(),
            run: RunSeq(integer::<u64>(run)?),
            ordinal: Ordinal(integer::<u64>(ordinal)?),
            kind,
            call,
            record_json,
            written_epoch: Epoch(epoch),
        });
    }
    Ok(Ok(records))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn negative_record_identity_is_corruption() {
        let conn = Connection::open_in_memory().expect("open records");
        conn.execute_batch(TABLES).expect("create records");
        let owner = OwnerKey::Turn("session".into(), "turn".into());
        let actor = lash_durable::ActorKey::session("session").expect("actor");
        let commit = Committing {
            actor: &actor,
            epoch: Epoch(1),
            now: lash_durable::DurableInstant(1),
            fleet: lash_core_execution::FleetFormat::current(),
            blob_profile: crate::BuiltinBlobProfile::default(),
        };
        let write = RunRecordWrite::Append {
            owner: owner.clone(),
            run: RunSeq(u64::MAX),
            ordinal: Ordinal(0),
            kind: RunRecordKind::Admit,
            call: None,
            record_json: "{}".into(),
        };
        assert!(
            !matches!(apply(&conn, &commit, &write), Ok(Ok(()))),
            "an oversized identity must refuse its write"
        );
        assert_eq!(
            conn.query_row("SELECT count(*) FROM run_records", [], |row| row
                .get::<_, i64>(0))
                .expect("count"),
            0
        );

        conn.execute(
            "INSERT INTO run_records VALUES (?1, -1, 0, 'admit', NULL, '{}', 1)",
            [owner.stored()],
        )
        .expect("foreign negative identity");
        let answer = read(&conn, &owner);
        assert!(
            matches!(
                answer,
                Ok(Err(DurableError::Store(lash_durable::StoreFailure {
                    kind: lash_durable::StoreFailureKind::Corrupt,
                    ..
                })))
            ) || matches!(answer, Err(rusqlite::Error::FromSqlConversionFailure(..))),
            "negative identities must report corruption: {answer:?}"
        );
    }
}
