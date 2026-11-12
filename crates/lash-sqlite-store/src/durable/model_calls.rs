//! SQLite adapter for sealed model-call records.
use super::{Answer, Committing};
use crate::Prepare as _;
use lash_core_store::checkpoint_graph::{DecodeLimits, GraphDecoder};
use lash_core_store::model_calls::{ModelCallFact, ModelCallRecord};
use lash_durable::domain::{ModelCallWrite, RunFacts};
use lash_store_sql::durable::model_calls::ModelCallStatements;
use rusqlite::Connection;
use std::sync::LazyLock;

pub(crate) const TABLES: &str = "
CREATE TABLE IF NOT EXISTS model_call_records (
    session_id TEXT NOT NULL,
    run TEXT NOT NULL,
    call TEXT NOT NULL,
    call_id TEXT NOT NULL,
    attempt INTEGER CHECK (attempt > 0), answered INTEGER NOT NULL DEFAULT 0 CHECK (answered IN (0, 1)),
    enclosing_owner TEXT, enclosing_run INTEGER, enclosing_call TEXT,
    record_ref INTEGER REFERENCES blobs(id),
    ended_at_ms INTEGER,
    written_epoch INTEGER,
    PRIMARY KEY (session_id, call_id),
    UNIQUE (session_id, run, call),
    CONSTRAINT ck_model_call_records_enclosing CHECK ((enclosing_owner IS NULL AND enclosing_run IS NULL AND enclosing_call IS NULL) OR (enclosing_owner IS NOT NULL AND enclosing_run IS NOT NULL AND enclosing_run >= 0 AND enclosing_call IS NOT NULL)),
    CHECK ((record_ref IS NULL) = (ended_at_ms IS NULL)),
    CHECK ((record_ref IS NULL) = (written_epoch IS NULL)),
    CHECK (record_ref IS NOT NULL OR attempt IS NOT NULL),
    CHECK (record_ref IS NULL OR answered)
) WITHOUT ROWID;
CREATE INDEX IF NOT EXISTS idx_model_call_records_record_ref ON model_call_records(record_ref) WHERE record_ref IS NOT NULL;
";
pub(super) static SQL: LazyLock<ModelCallStatements> =
    LazyLock::new(|| ModelCallStatements::render(crate::schema_layout::MAIN));

lash_store_sql::statements! {
    lazy
    struct ModelCallSqliteStatements @ "durable_model_call_sqlite" {
        selected = "SELECT call_id, (SELECT hash FROM blobs WHERE id = record_ref) AS record_ref, ended_at_ms FROM model_call_records
            WHERE session_id = ?1 AND record_ref IS NOT NULL AND run IN (?2, CASE WHEN ?2 LIKE 'turn:%' THEN 'owned:' || SUBSTR(?2, 6) END)
              AND call_id IN (SELECT value FROM json_each(?3)) ORDER BY run, call";
        selected_pins = "SELECT run, call, call_id, attempt, answered, enclosing_owner, enclosing_run, enclosing_call FROM model_call_records
            WHERE session_id = ?1 AND record_ref IS NULL AND run IN (?2, CASE WHEN ?2 LIKE 'turn:%' THEN 'owned:' || SUBSTR(?2, 6) END)
              AND call_id IN (SELECT value FROM json_each(?3)) ORDER BY run, call";
    }
}
static SQLITE: LazyLock<ModelCallSqliteStatements> =
    LazyLock::new(|| ModelCallSqliteStatements::render(crate::schema_layout::MAIN));

pub(super) fn ids(
    tx: &Connection,
    session: &lash_kernel_value::identity::SessionId,
    owner: &str,
) -> Answer<Vec<lash_protocol_types::llm::types::LlmCallId>> {
    // The history discriminator chooses the representation first.
    let read = lash_durable::domain::process_archive::ModelIds { owner, like: true };
    match super::process_history::model_calls(tx, session, &read)? {
        Ok(Some((_, ids))) => {
            return Ok(Ok(ids
                .into_iter()
                .map(lash_protocol_types::llm::types::LlmCallId)
                .collect()));
        }
        Ok(None) => {}
        Err(error) => return Ok(Err(error)),
    }
    let ids = tx
        .statement(SQL.ids.sql())?
        .query_map([session.as_str(), owner], |row| {
            row.get::<_, String>(0)
                .map(lash_protocol_types::llm::types::LlmCallId)
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(Ok(ids))
}

pub(super) fn apply(
    tx: &Connection,
    commit: &Committing<'_>,
    write: &ModelCallWrite,
) -> Answer<()> {
    let inserted = tx.statement(SQL.append.sql())?.execute(rusqlite::params![
        write.key.session.as_str(),
        write.key.call.owner_column(),
        write.key.call.call_column(),
        write.call_id.0,
        write.record.0.as_slice(),
        commit.now.0,
        commit.epoch.0,
    ])?;
    if inserted != 1 {
        return Ok(Err(super::corrupt(
            "model call",
            "a sealed call cannot be replaced",
        )));
    }
    Ok(Ok(()))
}

pub(super) fn delete_session(
    tx: &Connection,
    session: &lash_kernel_value::identity::SessionId,
) -> rusqlite::Result<Vec<String>> {
    let roots = tx
        .statement(SQL.delete_session.sql())?
        .query_map([session.as_str()], |row| {
            crate::codec::optional_hash_column(row, 0)
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(roots.into_iter().flatten().collect())
}

pub(super) fn read(
    tx: &Connection,
    session: &lash_kernel_value::identity::SessionId,
    owner: &str,
    ids: Option<&[lash_protocol_types::llm::types::LlmCallId]>,
    limits: DecodeLimits,
) -> Answer<Vec<ModelCallRecord>> {
    // The history discriminator chooses the representation first.
    let read = lash_durable::domain::process_archive::ModelRecords {
        owner,
        ids,
        like: true,
    };
    match super::process_history::model_calls(tx, session, &read)? {
        Ok(Some((bytes, models))) => {
            return Ok(super::process_history::model_records(
                &bytes, models, limits,
            ));
        }
        Ok(None) => {}
        Err(error) => return Ok(Err(error)),
    }
    let keys = ids.map(|ids| {
        serde_json::Value::from(ids.iter().map(|id| id.0.as_str()).collect::<Vec<_>>()).to_string()
    });
    let sql = if keys.is_some() {
        SQLITE.selected.sql()
    } else {
        SQL.read.sql()
    };
    let mut params = vec![session.as_str(), owner];
    if let Some(keys) = &keys {
        params.push(keys);
    }
    let rows = tx
        .statement(sql)?
        .query_map(crate::statement::params_from_iter(params), |row| {
            Ok((
                row.get::<_, String>(0)?,
                crate::codec::hash_column(row, 1)?,
                row.get::<_, i64>(2)?,
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let rows = match rows
        .into_iter()
        .map(|(id, root, ended)| Ok((id, super::node_ref(&root)?, ended)))
        .collect::<Result<Vec<_>, lash_durable::DurableError>>()
    {
        Ok(rows) => rows,
        Err(error) => return Ok(Err(error)),
    };
    let roots: Vec<_> = rows.iter().map(|(_, root, _)| *root).collect();
    let nodes = match super::graph::walk_roots(tx, &roots, &limits)? {
        Ok(nodes) => nodes,
        Err(error) => return Ok(Err(error.into())),
    };
    Ok(decode_records(rows, nodes, limits))
}

/// Sealed records from their rows' ids, roots and end instants and the
/// nodes those roots reach: the record read's decoding, whichever source
/// answered it.
pub(super) fn decode_records(
    rows: Vec<(String, lash_core_store::checkpoint_graph::NodeId, i64)>,
    nodes: std::collections::BTreeMap<
        lash_core_store::checkpoint_graph::NodeId,
        lash_durable::domain::HydratedNode,
    >,
    limits: DecodeLimits,
) -> Result<Vec<ModelCallRecord>, lash_durable::DurableError> {
    let graph = RunFacts {
        nodes,
        read_limits: limits,
        ..Default::default()
    };
    let mut decoder = GraphDecoder::new(&graph, limits);
    let mut records = rows
        .into_iter()
        .map(|(id, root, ended)| {
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
        .collect::<Result<Vec<_>, lash_durable::DurableError>>()?;
    records.sort_by_key(|record| record.call);
    Ok(records)
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
            let changed = tx.statement(SQL.reserve.sql())?.execute(rusqlite::params![
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
            if changed != 1 {
                return Ok(Err(super::corrupt(
                    "model call reservation",
                    "conflicting identity",
                )));
            }
        }
        ModelCallPinWrite::Answered { key } => {
            tx.statement(SQL.answered.sql())?.execute([
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
    ids: Option<&[lash_protocol_types::llm::types::LlmCallId]>,
    call: Option<&str>,
) -> Answer<Vec<lash_durable::domain::ModelCallPinRow>> {
    // The history discriminator chooses the representation first.
    let read = lash_durable::domain::process_archive::ModelPins {
        owner,
        ids,
        call,
        like: true,
    };
    match super::process_history::model_calls(tx, session, &read)? {
        Ok(Some((_, models))) => {
            return Ok(models
                .into_iter()
                .map(|model| lash_durable::domain::process_archive::pin_row(session, model))
                .collect());
        }
        Ok(None) => {}
        Err(error) => return Ok(Err(error)),
    }
    let keys = ids.map(|ids| {
        serde_json::Value::from(ids.iter().map(|id| id.0.as_str()).collect::<Vec<_>>()).to_string()
    });
    let sql = if call.is_some() {
        SQL.pin_by_key.sql()
    } else if keys.is_some() {
        SQLITE.selected_pins.sql()
    } else {
        SQL.pins.sql()
    };
    let mut params = vec![session.as_str(), owner];
    if let Some(selected) = call.or(keys.as_deref()) {
        params.push(selected);
    }
    let rows = tx
        .statement(sql)?
        .query_map(crate::statement::params_from_iter(params), |row| {
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
