//! Run facts on SQLite: the `run_records` domain's statements, apply and read
//! (I0, FIG-5194).
//!
//! Owned by V0 (FIG-5170), then L4 (FIG-5174). The dispatch in `durable/mod.rs` calls these inside the
//! fenced owner commit (or the mailbox commit), after the fence; a refusal
//! rolls the whole commit back.

use std::sync::LazyLock;

use lash_durable::domain::{
    DomainRefusal, Ordinal, OwnerKey, RunFactKind, RunFactRow, RunFactWrite, RunSeq,
};
use lash_durable::{DurableError, Epoch};
use lash_sansio::ToolCallId;
use lash_store_sql::durable::run_records::RunRecordStatements;
use rusqlite::{Connection, OptionalExtension};

use super::{Answer, Committing, corrupt, integer, node_ref};
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
        CHECK (kind IN ('admit', 'x_start', 'x_outcome', 'x_wait', 'decide', 'present', 'retry',
                        'cell_report')),
    call_id TEXT,
    -- The fact's RoundFact node (FIG-5887 §7).
    record_ref TEXT NOT NULL REFERENCES blobs(hash) ON DELETE RESTRICT,
    written_epoch INTEGER NOT NULL,
    PRIMARY KEY (owner_key, run_seq, ordinal)
);
CREATE UNIQUE INDEX IF NOT EXISTS ux_run_records_outcome
    ON run_records (owner_key, run_seq, call_id)
    WHERE kind = 'x_outcome' AND call_id IS NOT NULL;
CREATE INDEX IF NOT EXISTS idx_run_records_record_ref ON run_records (record_ref);
";

pub(super) fn apply(tx: &Connection, commit: &Committing<'_>, write: &RunFactWrite) -> Answer<()> {
    match write {
        RunFactWrite::Append {
            owner,
            run,
            ordinal,
            kind,
            call,
            record,
        } => {
            let owner_key = owner.stored();
            let appended = tx
                .prepare_cached(SQL.append.sql())?
                .query_row(
                    rusqlite::params![
                        owner_key,
                        integer::<i64>(run.0)?,
                        integer::<i64>(ordinal.0)?,
                        kind.as_str(),
                        call.as_ref().map(ToolCallId::as_str),
                        record.to_hex(),
                        commit.epoch.0,
                    ],
                    |_| Ok(()),
                )
                .optional()?;
            if appended.is_some() {
                return Ok(Ok(()));
            }
            let call = call.as_ref().filter(|_| *kind == RunFactKind::XOutcome);
            refused(tx, owner, *run, *ordinal, call)
        }
        RunFactWrite::Prune { owner, before } => {
            cached_execute(
                tx,
                SQL.prune.sql(),
                rusqlite::params![owner.stored(), integer::<i64>(before.0)?],
            )?;
            Ok(Ok(()))
        }
    }
}

/// Whether owner `owner_key`'s run `run` has a fact at `ordinal`.
fn ordinal_taken(
    tx: &Connection,
    owner_key: &str,
    run: RunSeq,
    ordinal: u64,
) -> rusqlite::Result<bool> {
    Ok(tx
        .prepare_cached(SQL.ordinal_taken.sql())?
        .query_row(
            rusqlite::params![owner_key, integer::<i64>(run.0)?, integer::<i64>(ordinal)?],
            |_| Ok(()),
        )
        .optional()?
        .is_some())
}

/// Why the append of `ordinal` of `owner`'s run `run` (for `call`, when
/// an outcome) was refused, read only once it was, in the refusal's order:
/// its ordinal taken, then the fact before it absent, then its call's
/// outcome recorded.
fn refused(
    tx: &Connection,
    owner: &OwnerKey,
    run: RunSeq,
    ordinal: Ordinal,
    call: Option<&ToolCallId>,
) -> Answer<()> {
    let owner_key = owner.stored();
    let refusal = if ordinal_taken(tx, &owner_key, run, ordinal.0)? {
        DomainRefusal::RunOrdinalTaken {
            owner: owner.clone(),
            run,
            ordinal,
        }
    } else if let Some(previous) = ordinal.0.checked_sub(1)
        && !ordinal_taken(tx, &owner_key, run, previous)?
    {
        DomainRefusal::RunOrdinalGap {
            owner: owner.clone(),
            run,
            ordinal,
        }
    } else if let Some(call) = call {
        DomainRefusal::OutcomeExists {
            owner: owner.clone(),
            run,
            call: call.clone(),
        }
    } else {
        return Ok(Err(corrupt(
            "run fact append",
            "refused for no recorded reason",
        )));
    };
    Ok(Err(DurableError::Domain(refusal)))
}

/// Delete every fact of the owner stored as `owner_key`.
pub(super) fn delete_owner(tx: &Connection, owner_key: &str) -> rusqlite::Result<()> {
    cached_execute(tx, SQL.delete_owner.sql(), [owner_key]).map(drop)
}

/// Delete every fact whose owner key starts with `prefix`.
pub(super) fn delete_under(tx: &Connection, prefix: &str) -> rusqlite::Result<()> {
    cached_execute(tx, SQL.delete_under.sql(), [prefix]).map(drop)
}

/// The RoundFact node of every fact whose owner key starts with `prefix`.
pub(super) fn roots_under(tx: &Connection, prefix: &str) -> rusqlite::Result<Vec<String>> {
    tx.prepare_cached(SQL.roots_under.sql())?
        .query_map([prefix], |row| row.get(0))?
        .collect()
}

pub(super) fn read(
    tx: &Connection,
    owner: &OwnerKey,
    run: Option<RunSeq>,
) -> Answer<Vec<RunFactRow>> {
    let (sql, params) = match run {
        Some(run) => (
            SQL.read_of.sql(),
            vec![
                rusqlite::types::Value::Text(owner.stored()),
                integer::<i64>(run.0)?.into(),
            ],
        ),
        None => (
            SQL.read.sql(),
            vec![rusqlite::types::Value::Text(owner.stored())],
        ),
    };
    let mut statement = tx.prepare_cached(sql)?;
    let rows = statement
        .query_map(rusqlite::params_from_iter(params), |row| {
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
    for (run, ordinal, kind, call, record, epoch) in rows {
        let Some(kind) = RunFactKind::parse(&kind) else {
            return Ok(Err(corrupt("run record kind", &kind)));
        };
        let call = match call {
            None => None,
            Some(call) => match ToolCallId::parse(&call) {
                Ok(call) => Some(call),
                Err(_) => return Ok(Err(corrupt("tool call id", &call))),
            },
        };
        let record = match node_ref(&record) {
            Ok(record) => record,
            Err(error) => return Ok(Err(error)),
        };
        records.push(RunFactRow {
            owner: owner.clone(),
            run: RunSeq(integer::<u64>(run)?),
            ordinal: Ordinal(integer::<u64>(ordinal)?),
            kind,
            call,
            record,
            written_epoch: Epoch(epoch),
        });
    }
    Ok(Ok(records))
}

/// Select and hydrate the owner's facts in the caller's read transaction.
pub(super) fn facts(
    tx: &Connection,
    owner: &OwnerKey,
    run: Option<RunSeq>,
) -> Answer<lash_durable::domain::RunFacts> {
    let rows = match read(tx, owner, run)? {
        Ok(rows) => rows,
        Err(error) => return Ok(Err(error)),
    };
    let roots: Vec<_> = rows.iter().map(|row| row.record).collect();
    Ok(
        super::graph::walk_roots(tx, &roots, &lash_durable::domain::graph::OWNER_READ_LIMITS)?
            .map(|nodes| lash_durable::domain::RunFacts { rows, nodes })
            .map_err(DurableError::from),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// RUN-READ: round k's recovery selects only its rows and visits each
    /// shared descendant once in one walk, regardless of earlier rounds.
    #[test]
    fn resuming_round_k_reads_only_its_rows_and_nodes_in_one_walk() {
        use lash_core_store::checkpoint_graph::{
            EncodeBudget, ExplicitLimit, GraphStaging, NodeKind, TokenWriter, TypedBody,
        };
        let conn = Connection::open_in_memory().expect("SQLite");
        conn.execute_batch(crate::schema::SCHEMA).expect("schema");
        conn.execute_batch(TABLES).expect("facts");
        let owner = OwnerKey::Turn("session".into(), "turn".into());
        let mut staging = GraphStaging::new(EncodeBudget::new(ExplicitLimit::new(u64::MAX)));
        let mut body = TokenWriter::new();
        body.text("shared");
        let shared = staging
            .encode(NodeKind::FROZEN_VALUE, TypedBody::logical(body.as_bytes()))
            .expect("shared node");
        let k = 8;
        for run in 1..=k {
            for ordinal in 0..2 {
                let mut body = TokenWriter::new();
                body.seq(2);
                body.text(&format!("round {run} fact {ordinal}"));
                body.child(shared.child_ref());
                let root = staging
                    .encode(NodeKind::ROUND_FACT, TypedBody::logical(body.as_bytes()))
                    .expect("fact node");
                conn.execute(
                    "INSERT INTO blobs (hash, content, encoding) VALUES (?1, ?2, 1)",
                    rusqlite::params![root.to_hex(), staging.nodes()[&root].bytes.as_ref()],
                )
                .expect("root blob");
                // The append is conditional and answers the ordinal it wrote.
                conn.query_row(
                    SQL.append.sql(),
                    rusqlite::params![
                        owner.stored(),
                        run,
                        ordinal,
                        "admit",
                        Option::<String>::None,
                        root.to_hex(),
                        1
                    ],
                    |_| Ok(()),
                )
                .expect("fact row");
            }
        }
        for (id, node) in staging.nodes() {
            conn.execute("INSERT INTO blobs (hash, content, encoding) VALUES (?1, ?2, 1) ON CONFLICT DO NOTHING", rusqlite::params![id.to_hex(), node.bytes.as_ref()]).expect("blob");
        }
        super::super::graph::WALK_COUNTS.with_borrow_mut(|counts| *counts = Default::default());
        let recovered = facts(&conn, &owner, Some(RunSeq(k as u64)))
            .expect("read")
            .expect("hydrate");
        let selected_rows = recovered.rows.len();
        let (walks, visits) = super::super::graph::WALK_COUNTS.with_borrow(Clone::clone);
        eprintln!(
            "round {k}: selected_rows={selected_rows} nodes={} walks={walks} visits={}",
            recovered.nodes.len(),
            visits.values().sum::<u64>()
        );
        assert_eq!(selected_rows, 2, "recovery must select only round k's rows");
        assert_eq!(
            recovered.nodes.len(),
            3,
            "only round k's two roots and shared node"
        );
        assert_eq!(walks, 1, "all roots share one walk");
        assert_eq!(visits.len(), 3);
        assert!(visits.values().all(|count| *count == 1));
        assert!(recovered.rows.iter().all(|row| row.run == RunSeq(k as u64)));
        let roots: Vec<_> = recovered.rows.iter().map(|row| row.record).collect();
        let bytes: u64 = recovered
            .nodes
            .values()
            .map(|node| node.bytes.len() as u64)
            .sum();
        let mut limits = lash_core_store::checkpoint_graph::DecodeLimits::within_total(bytes);
        limits.nodes = 3;
        assert_eq!(
            super::super::graph::walk_roots(&conn, &roots, &limits)
                .expect("exact budget")
                .expect("closure"),
            recovered.nodes
        );
        limits.nodes = 2;
        assert!(matches!(
            super::super::graph::walk_roots(&conn, &roots, &limits).expect("read"),
            Err(
                lash_core_store::checkpoint_graph::GraphRefusal::DecodeLimit {
                    dimension: lash_core_store::checkpoint_graph::DecodeDimension::Nodes,
                    actual: 3,
                    limit: 2
                }
            )
        ));
        limits.nodes = 3;
        limits.total_bytes = bytes - 1;
        assert!(matches!(
            super::super::graph::walk_roots(&conn, &roots, &limits).expect("read"),
            Err(
                lash_core_store::checkpoint_graph::GraphRefusal::DecodeLimit {
                    dimension: lash_core_store::checkpoint_graph::DecodeDimension::TotalBytes,
                    ..
                }
            )
        ));
        super::super::graph::WALK_COUNTS.with_borrow_mut(|counts| *counts = Default::default());
        let all = facts(&conn, &owner, None)
            .expect("turn read")
            .expect("turn graph");
        let (walks, visits) = super::super::graph::WALK_COUNTS.with_borrow(Clone::clone);
        assert_eq!(all.rows.len(), 16);
        assert_eq!(all.nodes.len(), 17);
        assert_eq!(walks, 1);
        assert!(visits.values().all(|count| *count == 1));
        eprintln!(
            "turn report: selected_rows=16 nodes=17 walks={walks} visits={}",
            visits.values().sum::<u64>()
        );
    }

    #[test]
    fn negative_record_identity_is_corruption() {
        let conn = Connection::open_in_memory().expect("open records");
        conn.execute_batch(crate::schema::SCHEMA)
            .expect("create blobs");
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
        let record = lash_core_store::checkpoint_graph::NodeId([7; 32]);
        conn.execute(
            "INSERT INTO blobs (hash, content, encoding) VALUES (?1, x'00', 1)",
            [record.to_hex()],
        )
        .expect("a record blob");
        let write = RunFactWrite::Append {
            owner: owner.clone(),
            run: RunSeq(u64::MAX),
            ordinal: Ordinal(0),
            kind: RunFactKind::Admit,
            call: None,
            record,
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
            "INSERT INTO run_records
                 (owner_key, run_seq, ordinal, kind, call_id, record_ref, written_epoch)
             VALUES (?1, -1, 0, 'admit', NULL, ?2, 1)",
            [owner.stored(), record.to_hex()],
        )
        .expect("foreign negative identity");
        let answer = read(&conn, &owner, None);
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
