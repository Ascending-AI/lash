//! SQLite adapter for sealed model-call records.
use super::{Answer, Committing};
use lash_core_store::checkpoint_graph::{DecodeLimits, GraphDecoder};
use lash_core_store::model_calls::{ModelCallFact, ModelCallRecord};
use lash_durable::domain::{ModelCallWrite, RunFacts};
use lash_store_sql::durable::model_calls::ModelCallStatements;
use rusqlite::Connection;
use std::sync::LazyLock;

pub(crate) const TABLES: &str = "
CREATE TABLE IF NOT EXISTS model_call_pins (
    session_id TEXT NOT NULL, run TEXT NOT NULL, call TEXT NOT NULL, call_id TEXT NOT NULL,
    attempt INTEGER NOT NULL CHECK (attempt > 0), answered INTEGER NOT NULL CHECK (answered IN (0, 1)),
    enclosing_owner TEXT, enclosing_run INTEGER, enclosing_call TEXT,
    PRIMARY KEY (session_id, run, call), UNIQUE (session_id, run, call_id)
) WITHOUT ROWID;
CREATE TABLE IF NOT EXISTS model_call_records (
    session_id TEXT NOT NULL,
    run TEXT NOT NULL,
    call TEXT NOT NULL,
    call_id TEXT NOT NULL,
    record_ref BLOB NOT NULL REFERENCES blobs(hash) ON DELETE RESTRICT,
    ended_at_ms INTEGER NOT NULL,
    written_epoch INTEGER NOT NULL,
    PRIMARY KEY (session_id, run, call),
    UNIQUE (session_id, run, call_id)
) WITHOUT ROWID;
CREATE INDEX IF NOT EXISTS idx_model_call_records_record_ref ON model_call_records(record_ref);
";
pub(super) static SQL: LazyLock<ModelCallStatements> =
    LazyLock::new(|| ModelCallStatements::render(crate::schema_layout::MAIN));

pub(super) fn apply(
    tx: &Connection,
    commit: &Committing<'_>,
    write: &ModelCallWrite,
) -> Answer<()> {
    let inserted = tx
        .prepare_cached(SQL.append.sql())?
        .execute(rusqlite::params![
            write.key.session.as_str(),
            write.key.call.owner_column(),
            write.key.call.call_column(),
            write.call_id.0,
            write.record.0.as_slice(),
            commit.now.0,
            commit.epoch.0,
        ])?;
    // A successful insert owns this exact record. Only a replay can find
    // an older sealed row, whose immutable identity must then match.
    if inserted == 0 {
        let retained: String = tx.prepare_cached(SQL.root.sql())?.query_row(
            [
                write.key.session.as_str(),
                &write.key.call.owner_column(),
                &write.key.call.call_column(),
            ],
            |row| crate::codec::hash_column(row, 0),
        )?;
        if retained != write.record.to_hex() {
            return Ok(Err(super::corrupt(
                "model call",
                "a sealed call cannot be replaced",
            )));
        }
    }
    tx.prepare_cached(SQL.retire_pin.sql())?.execute([
        write.key.session.as_str(),
        &write.key.call.owner_column(),
        &write.key.call.call_column(),
    ])?;
    Ok(Ok(()))
}

pub(super) fn delete_session(
    tx: &Connection,
    session: &lash_kernel_value::identity::SessionId,
) -> rusqlite::Result<Vec<String>> {
    tx.prepare_cached(SQL.delete_pins.sql())?
        .execute([session.as_str()])?;
    tx.prepare_cached(SQL.delete_session.sql())?
        .query_map([session.as_str()], |row| crate::codec::hash_column(row, 0))?
        .collect()
}

pub(super) fn read(
    tx: &Connection,
    session: &lash_kernel_value::identity::SessionId,
    owner: &str,
    limits: DecodeLimits,
) -> Answer<Vec<ModelCallRecord>> {
    let rows = tx
        .prepare_cached(SQL.read.sql())?
        .query_map([session.as_str(), owner], |row| {
            Ok((
                row.get::<_, String>(0)?,
                crate::codec::hash_column(row, 1)?,
                row.get::<_, i64>(2)?,
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let roots = match rows
        .iter()
        .map(|(_, root, _)| super::node_ref(root))
        .collect::<Result<Vec<_>, _>>()
    {
        Ok(roots) => roots,
        Err(error) => return Ok(Err(error)),
    };
    let nodes = match super::graph::walk_roots(tx, &roots, &limits)? {
        Ok(nodes) => nodes,
        Err(error) => return Ok(Err(error.into())),
    };
    let graph = RunFacts {
        nodes,
        read_limits: limits,
        ..Default::default()
    };
    let mut decoder = GraphDecoder::new(&graph, limits);
    let records = rows
        .into_iter()
        .zip(roots)
        .map(|((id, _, ended), root)| {
            let fact = ModelCallFact::load(&mut decoder, root)?;
            if fact.call_id.0 != id {
                return Err(super::corrupt("model call", "row and fact identity differ"));
            }
            Ok(ModelCallRecord {
                call: fact.call,
                call_id: fact.call_id,
                end: fact.end,
                record: fact.record,
                ended_at_ms: u64::try_from(ended)
                    .map_err(|_| super::corrupt("model call", "negative end time"))?,
            })
        })
        .collect::<Result<Vec<_>, lash_durable::DurableError>>();
    Ok(records.map(|mut records| {
        records.sort_by_key(|record| record.call);
        records
    }))
}

pub(super) fn apply_pin(
    tx: &Connection,
    write: &lash_durable::domain::ModelCallPinWrite,
) -> Answer<()> {
    use lash_durable::domain::ModelCallPinWrite;
    match write {
        ModelCallPinWrite::Reserve {
            key,
            call_id,
            attempt,
            enclosing,
        } => {
            tx.prepare_cached(SQL.reserve.sql())?
                .execute(rusqlite::params![
                    key.session.as_str(),
                    key.call.owner_column(),
                    key.call.call_column(),
                    call_id.0,
                    i64::from(*attempt),
                    enclosing.as_ref().map(|owner| owner.owner.stored()),
                    enclosing
                        .as_ref()
                        .map(|owner| i64::try_from(owner.run.0).unwrap_or(i64::MAX)),
                    enclosing.as_ref().map(|owner| owner.call.as_str()),
                ])?;
        }
        ModelCallPinWrite::Answered { key } => {
            tx.prepare_cached(SQL.answered.sql())?.execute([
                key.session.as_str(),
                &key.call.owner_column(),
                &key.call.call_column(),
            ])?;
        }
    }
    Ok(Ok(()))
}
pub(super) fn pins(
    tx: &Connection,
    session: &lash_kernel_value::identity::SessionId,
    owner: &str,
) -> Answer<Vec<lash_durable::domain::ModelCallPinRow>> {
    let rows = tx
        .prepare_cached(SQL.pins.sql())?
        .query_map([session.as_str(), owner], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, i64>(3)?,
                row.get::<_, bool>(4)?,
                row.get::<_, Option<String>>(5)?,
                row.get::<_, Option<i64>>(6)?,
                row.get::<_, Option<String>>(7)?,
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows
        .into_iter()
        .map(
            |(
                owner,
                call,
                id,
                attempt,
                answered,
                enclosing_owner,
                enclosing_run,
                enclosing_call,
            )| {
                lash_durable::domain::model_calls::pin_row(
                    session,
                    &owner,
                    call,
                    id,
                    attempt,
                    answered,
                    (enclosing_owner, enclosing_run, enclosing_call),
                )
            },
        )
        .collect())
}
