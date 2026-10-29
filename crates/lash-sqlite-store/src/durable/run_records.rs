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
use lash_protocol_types::ToolCallId;
use lash_store_sql::durable::run_records::RunRecordStatements;
use rusqlite::{Connection, OptionalExtension};

use super::{Answer, Committing, corrupt, integer, node_ref};

static SQL: LazyLock<RunRecordStatements> =
    LazyLock::new(|| RunRecordStatements::render(crate::schema_layout::MAIN));

lash_store_sql::statements! {
    struct RunFactBatchStatements @ "durable_run_fact_batch_sqlite" {
        /// Validate the batch's ordinal chain and outcome uniqueness against
        /// both stored rows and its earlier rows before inserting the set.
        append = "WITH incoming AS MATERIALIZED (SELECT CAST(key AS INTEGER) AS pos, value->>0 AS owner_key, value->>1 AS run_seq, value->>2 AS ordinal, value->>3 AS kind, value->>4 AS call_id, unhex(value->>5) AS record_ref, value->>6 AS written_epoch FROM json_each(?1))
             INSERT INTO run_records (owner_key, run_seq, ordinal, kind, call_id, record_ref, written_epoch)
             SELECT owner_key, run_seq, ordinal, kind, call_id, record_ref, written_epoch FROM incoming
             WHERE NOT EXISTS (
                 SELECT 1 FROM incoming AS item WHERE
                   EXISTS (SELECT 1 FROM run_records AS held WHERE held.owner_key = item.owner_key
                     AND held.run_seq = item.run_seq AND held.ordinal = item.ordinal)
                   OR EXISTS (SELECT 1 FROM incoming AS earlier WHERE earlier.pos < item.pos
                     AND earlier.owner_key = item.owner_key AND earlier.run_seq = item.run_seq
                     AND earlier.ordinal = item.ordinal)
                   OR (item.ordinal <> 0 AND NOT EXISTS (
                     SELECT 1 FROM run_records AS held WHERE held.owner_key = item.owner_key
                       AND held.run_seq = item.run_seq AND held.ordinal = item.ordinal - 1)
                     AND NOT EXISTS (SELECT 1 FROM incoming AS earlier WHERE earlier.pos < item.pos
                       AND earlier.owner_key = item.owner_key AND earlier.run_seq = item.run_seq
                       AND earlier.ordinal = item.ordinal - 1))
                   OR (item.kind = 'x_outcome' AND item.call_id IS NOT NULL AND (
                     EXISTS (SELECT 1 FROM run_records AS held WHERE held.owner_key = item.owner_key
                       AND held.run_seq = item.run_seq AND held.kind = 'x_outcome' AND held.call_id = item.call_id)
                     OR EXISTS (SELECT 1 FROM incoming AS earlier WHERE earlier.pos < item.pos
                       AND earlier.owner_key = item.owner_key AND earlier.run_seq = item.run_seq
                       AND earlier.kind = 'x_outcome' AND earlier.call_id = item.call_id)))
             ) ORDER BY pos ON CONFLICT DO NOTHING RETURNING ordinal";
    }
}
static BATCH: LazyLock<RunFactBatchStatements> =
    LazyLock::new(|| RunFactBatchStatements::render(crate::schema_layout::MAIN));

fn append_rows(epoch: Epoch, writes: &[&RunFactWrite]) -> Result<String, DurableError> {
    let mut rows = Vec::with_capacity(writes.len());
    for write in writes {
        let RunFactWrite::Append {
            owner,
            run,
            ordinal,
            kind,
            call,
            record,
        } = write
        else {
            return Err(corrupt("run fact batch", "contains a prune"));
        };
        rows.push(serde_json::json!([
            owner.stored(),
            integer::<i64>(run.0).map_err(super::store_failure)?,
            integer::<i64>(ordinal.0).map_err(super::store_failure)?,
            kind.as_str(),
            call.as_ref().map(ToolCallId::as_str),
            record.to_hex(),
            epoch.0
        ]));
    }
    serde_json::to_string(&rows).map_err(|error| corrupt("run fact batch", &error.to_string()))
}

/// `run_records`, created by I0 (FIG-5194); its statements are V0's.
pub(crate) const TABLES: &str = "
CREATE TABLE IF NOT EXISTS run_records (
    owner_key TEXT NOT NULL,
    run_seq INTEGER NOT NULL,
    ordinal INTEGER NOT NULL,
    kind TEXT NOT NULL CONSTRAINT ck_run_records_kind
        CHECK (kind IN ('admit', 'x_start', 'x_outcome', 'x_wait', 'present', 'retry',
                        'cell_report')),
    call_id TEXT,
    -- The fact's RoundFact node (FIG-5887 §7).
    record_ref BLOB NOT NULL REFERENCES blobs(hash) ON DELETE RESTRICT,
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
            record: _,
        } => {
            let rows = match append_rows(commit.epoch, &[write]) {
                Ok(rows) => rows,
                Err(error) => return Ok(Err(error)),
            };
            if append_batch(tx, &rows)? == 1 {
                return Ok(Ok(()));
            }
            let call = call.as_ref().filter(|_| *kind == RunFactKind::XOutcome);
            refused(tx, owner, *run, *ordinal, call)
        }
        RunFactWrite::Prune { owner, before } => {
            let pruned = tx
                .prepare_cached(SQL.prune.sql())?
                .query_map(
                    rusqlite::params![owner.stored(), integer::<i64>(before.0)?],
                    |row| crate::codec::hash_column(row, 0),
                )?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            for record in pruned {
                commit.retired.stored(record);
            }
            Ok(Ok(()))
        }
    }
}

fn append_batch(tx: &Connection, rows: &str) -> rusqlite::Result<usize> {
    Ok(tx
        .prepare_cached(BATCH.append.sql())?
        .query_map([rows], |_| Ok(()))?
        .collect::<rusqlite::Result<Vec<_>>>()?
        .len())
}

/// Contiguous appends share one insert. A refused set is classified in the
/// original order using the same statement with singleton sets, under the
/// same writer lock; the caller rolls the entire commit back on refusal.
pub(super) fn apply_appends(
    tx: &Connection,
    commit: &Committing<'_>,
    writes: &[lash_durable::domain::DomainWrite],
) -> Answer<()> {
    let facts: Vec<_> = writes
        .iter()
        .filter_map(|write| match write {
            lash_durable::domain::DomainWrite::RunFact(fact) => Some(fact),
            _ => None,
        })
        .collect();
    let rows = match append_rows(commit.epoch, &facts) {
        Ok(rows) => rows,
        Err(error) => return Ok(Err(error)),
    };
    if append_batch(tx, &rows)? == facts.len() {
        return Ok(Ok(()));
    }
    for fact in facts {
        if let Err(error) = apply(tx, commit, fact)? {
            return Ok(Err(error));
        }
    }
    Ok(Err(corrupt(
        "run fact batch",
        "refused without a conflicting row",
    )))
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

/// Delete every fact of the owner stored as `owner_key`, answering the
/// RoundFact nodes they held: the caller's to peel.
pub(super) fn delete_owner(tx: &Connection, owner_key: &str) -> rusqlite::Result<Vec<String>> {
    tx.prepare_cached(SQL.delete_owner.sql())?
        .query_map([owner_key], |row| crate::codec::hash_column(row, 0))?
        .collect()
}

/// Delete every fact whose owner key starts with `prefix`, answering the
/// RoundFact nodes they held: the caller's to peel.
pub(super) fn delete_under(tx: &Connection, prefix: &str) -> rusqlite::Result<Vec<String>> {
    tx.prepare_cached(SQL.delete_under.sql())?
        .query_map([prefix], |row| crate::codec::hash_column(row, 0))?
        .collect()
}

fn read_limited(
    tx: &Connection,
    owner: &OwnerKey,
    run: Option<RunSeq>,
    max_rows: Option<usize>,
) -> Answer<Vec<RunFactRow>> {
    let (sql, mut params) = match run {
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
    let sql = match max_rows {
        None => std::borrow::Cow::Borrowed(sql),
        Some(max_rows) => {
            params.push(integer::<i64>(max_rows.saturating_add(1))?.into());
            std::borrow::Cow::Owned(format!("{sql} LIMIT ?{}", params.len()))
        }
    };
    let mut statement = tx.prepare_cached(&sql)?;
    let rows = statement
        .query_map(rusqlite::params_from_iter(params), |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, i64>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, Option<String>>(3)?,
                crate::codec::hash_column(row, 4)?,
                row.get::<_, i64>(5)?,
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    if let Some(max_rows) = max_rows.filter(|limit| rows.len() > *limit) {
        return Ok(Err(DurableError::Graph(
            lash_core_store::checkpoint_graph::GraphRefusal::DecodeLimit {
                dimension: lash_kernel_value::tokens::DecodeDimension::Nodes,
                actual: rows.len() as u64,
                limit: max_rows as u64,
            },
        )));
    }
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
    limits: lash_core_store::checkpoint_graph::DecodeLimits,
) -> Answer<lash_durable::domain::RunFacts> {
    facts_limited(tx, owner, run, limits, None)
}

pub(super) fn facts_limited(
    tx: &Connection,
    owner: &OwnerKey,
    run: Option<RunSeq>,
    limits: lash_core_store::checkpoint_graph::DecodeLimits,
    max_rows: Option<usize>,
) -> Answer<lash_durable::domain::RunFacts> {
    let rows = match read_limited(tx, owner, run, max_rows)? {
        Ok(rows) => rows,
        Err(error) => return Ok(Err(error)),
    };
    let roots: Vec<_> = rows.iter().map(|row| row.record).collect();
    Ok(super::graph::walk_roots(tx, &roots, &limits)?
        .map(|nodes| lash_durable::domain::RunFacts {
            rows,
            nodes,
            read_limits: limits,
        })
        .map_err(DurableError::from))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// STORE-GRAPH-POLICY: SQLite's hydration refuses the deployment's byte budget.
    #[tokio::test]
    async fn a_sqlite_store_honors_its_deployment_graph_read_policy() {
        use lash_core_store::checkpoint_graph::EncodeBudget;
        use lash_core_store::checkpoint_graph::ExplicitLimit;
        use lash_core_store::checkpoint_graph::GraphReadPolicy;
        use lash_core_store::checkpoint_graph::GraphRefusal;
        use lash_core_store::checkpoint_graph::GraphStaging;
        use lash_core_store::checkpoint_graph::NodeKind;
        use lash_core_store::checkpoint_graph::TypedBody;
        use lash_durable::domain::DurableReads as _;
        use lash_kernel_value::tokens::TokenWriter;
        let mut options = crate::SqliteStoreSetOptions::memory();
        options.graph_reads = GraphReadPolicy::new(
            1,
            2,
            3,
            64,
            lash_kernel_value::data_limits::KernelDataLimits::standard(),
        )
        .unwrap();
        let stores = crate::SqliteStoreSet::memory_with_options_and_clock(
            options,
            std::sync::Arc::new(lash_core_ids::clock::SystemClock),
        )
        .await
        .unwrap();
        let store = stores.durable_store();
        let owner = OwnerKey::Turn("read-policy".into(), "turn".into());
        let stored_owner = owner.stored();
        let mut staging = GraphStaging::new(EncodeBudget::new(ExplicitLimit::new(u64::MAX)));
        let mut body = TokenWriter::new();
        body.text("fact");
        let root = staging
            .encode(NodeKind::ROUND_FACT, TypedBody::logical(body.as_bytes()))
            .unwrap();
        let node = staging.nodes()[&root].bytes.clone();
        store
            .conn
            .call(move |conn| {
                conn.execute(
                    "INSERT INTO blobs (hash, content, encoding) VALUES (?1, ?2, 1)",
                    rusqlite::params![root.0.as_slice(), node.as_ref()],
                )?;
                conn.query_row(
                    "INSERT INTO run_records
                        (owner_key, run_seq, ordinal, kind, call_id, record_ref, written_epoch)
                        VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7) RETURNING ordinal",
                    rusqlite::params![
                        stored_owner,
                        1,
                        0,
                        "admit",
                        Option::<String>::None,
                        root.0.as_slice(),
                        1
                    ],
                    |_| Ok(()),
                )?;
                Ok(())
            })
            .await
            .unwrap();
        assert!(matches!(
            store.run_facts(&owner).await,
            Err(DurableError::Graph(GraphRefusal::DecodeLimit {
                limit: 1,
                ..
            }))
        ));
    }

    /// RUN-READ: round k's recovery selects only its rows and visits each
    /// shared descendant once in one walk, regardless of earlier rounds.
    #[test]
    fn resuming_round_k_reads_only_its_rows_and_nodes_in_one_walk() {
        use lash_core_store::checkpoint_graph::EncodeBudget;
        use lash_core_store::checkpoint_graph::ExplicitLimit;
        use lash_core_store::checkpoint_graph::GraphStaging;
        use lash_core_store::checkpoint_graph::NodeKind;
        use lash_core_store::checkpoint_graph::TypedBody;
        use lash_kernel_value::tokens::TokenWriter;
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
                    rusqlite::params![root.0.as_slice(), staging.nodes()[&root].bytes.as_ref()],
                )
                .expect("root blob");
                conn.query_row(
                    "INSERT INTO run_records
                        (owner_key, run_seq, ordinal, kind, call_id, record_ref, written_epoch)
                        VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7) RETURNING ordinal",
                    rusqlite::params![
                        owner.stored(),
                        run,
                        ordinal,
                        "admit",
                        Option::<String>::None,
                        root.0.as_slice(),
                        1
                    ],
                    |_| Ok(()),
                )
                .expect("fact row");
            }
        }
        for (id, node) in staging.nodes() {
            conn.execute("INSERT INTO blobs (hash, content, encoding) VALUES (?1, ?2, 1) ON CONFLICT DO NOTHING", rusqlite::params![id.0.as_slice(), node.bytes.as_ref()]).expect("blob");
        }
        super::super::graph::WALK_COUNTS.with_borrow_mut(|counts| *counts = Default::default());
        let recovered = facts(
            &conn,
            &owner,
            Some(RunSeq(k as u64)),
            lash_durable::domain::graph::OWNER_READ_LIMITS,
        )
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
                    dimension: lash_kernel_value::tokens::DecodeDimension::Nodes,
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
                    dimension: lash_kernel_value::tokens::DecodeDimension::TotalBytes,
                    ..
                }
            )
        ));
        super::super::graph::WALK_COUNTS.with_borrow_mut(|counts| *counts = Default::default());
        let all = facts(
            &conn,
            &owner,
            None,
            lash_durable::domain::graph::OWNER_READ_LIMITS,
        )
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

    /// GRAPH-READ-BATCH-WORK: N stored nodes at depth D need at most
    /// D + ceil(N / 128) body SELECTs, each returning at most 128 bodies.
    #[tokio::test]
    async fn run_fact_hydration_uses_bounded_frontiers() {
        use lash_core_store::checkpoint_graph::EncodeBudget;
        use lash_core_store::checkpoint_graph::ExplicitLimit;
        use lash_core_store::checkpoint_graph::GraphStaging;
        use lash_core_store::checkpoint_graph::NodeKind;
        use lash_core_store::checkpoint_graph::TypedBody;
        use lash_kernel_value::tokens::TokenWriter;
        for (width, depth, sparse) in [
            (1_usize, 3, false),
            (127, 3, false),
            (128, 3, false),
            (129, 3, false),
            (513, 3, false),
            (257, 64, true),
        ] {
            let mut conn = Connection::open_in_memory().expect("SQLite");
            conn.execute_batch(crate::schema::SCHEMA).expect("schema");
            conn.execute_batch(TABLES).expect("facts");
            crate::observed_sql::enable(&conn);
            let owner = OwnerKey::Turn("frontier-session".into(), "turn".into());
            let mut staging = GraphStaging::new(EncodeBudget::new(ExplicitLimit::new(u64::MAX)));
            for ordinal in 0..width {
                let branch_depth = if sparse && ordinal % 128 != 0 {
                    1
                } else {
                    depth
                };
                let mut child = None;
                for level in 0..branch_depth {
                    let mut body = TokenWriter::new();
                    body.seq(if child.is_some() { 2 } else { 1 });
                    body.text(&format!("frontier-{ordinal}-{level}"));
                    if let Some(child) = child {
                        body.child(child);
                    }
                    let root = staging
                        .encode(NodeKind::ROUND_FACT, TypedBody::logical(body.as_bytes()))
                        .expect("fact node");
                    child = Some(root.child_ref());
                    if level == branch_depth - 1 {
                        conn.execute(
                            "INSERT INTO blobs (hash, content, encoding) VALUES (?1, ?2, 1)",
                            rusqlite::params![
                                root.0.as_slice(),
                                staging.nodes()[&root].bytes.as_ref()
                            ],
                        )
                        .expect("root body");
                        conn.execute(
                            "INSERT INTO run_records VALUES (?1, 1, ?2, 'admit', NULL, ?3, 1)",
                            rusqlite::params![owner.stored(), ordinal, root.0.as_slice()],
                        )
                        .expect("ledger root");
                    }
                }
            }
            for (id, node) in staging.nodes() {
                conn.execute(
                    "INSERT INTO blobs (hash, content, encoding) VALUES (?1, ?2, 1) ON CONFLICT DO NOTHING",
                    rusqlite::params![id.0.as_slice(), node.bytes.as_ref()],
                )
                .expect("body");
            }
            let tx = conn.transaction().expect("read snapshot");
            let (answer, receipt) = lash_core_ids::operational_metrics::sql::collect(
                "graph.frontiers",
                "sqlite",
                async {
                    facts(
                        &tx,
                        &owner,
                        None,
                        lash_durable::domain::graph::OWNER_READ_LIMITS,
                    )
                },
            )
            .await;
            let recovered = answer.expect("read").expect("hydrate");
            assert_eq!(recovered.rows.len(), width);
            assert_eq!(
                recovered.nodes.len(),
                if sparse {
                    width + width.div_ceil(128) * (depth - 1)
                } else {
                    width * depth
                }
            );
            for (id, node) in staging.nodes() {
                assert_eq!(&*recovered.nodes[id].bytes, &*node.bytes);
            }
            let bodies: Vec<_> = receipt
                .shapes
                .iter()
                .filter(|(sql, _)| sql.starts_with("select ") && sql.contains("main.blobs"))
                .map(|(_, shape)| shape)
                .collect();
            let selects = bodies.iter().map(|shape| shape.statements).sum::<u64>();
            let rows = bodies.iter().map(|shape| shape.rows_returned).sum::<u64>();
            assert_eq!(receipt.unretained_shape_statements, 0);
            assert_eq!(rows, recovered.nodes.len() as u64);
            assert!(
                bodies
                    .iter()
                    .all(|shape| shape.rows_returned <= 128 * shape.statements)
            );
            let bound = depth + recovered.nodes.len().div_ceil(128);
            eprintln!(
                "frontiers: nodes={} depth={depth} body_selects={selects} rows={rows} bound={bound}",
                recovered.nodes.len()
            );
            assert!(
                selects <= bound as u64,
                "per-node fetches exceed frontier bound: {receipt:?}"
            );
        }
    }

    /// GRAPH-READ-ORDER: fetching sibling roots together must not let a
    /// later root's corruption precede an earlier root's descendant or budget.
    #[test]
    fn frontier_batches_preserve_depth_first_refusals() {
        use lash_core_store::checkpoint_graph::BlobEncoding;
        use lash_core_store::checkpoint_graph::DecodeLimits;
        use lash_core_store::checkpoint_graph::EncodeBudget;
        use lash_core_store::checkpoint_graph::ExplicitLimit;
        use lash_core_store::checkpoint_graph::GraphRefusal;
        use lash_core_store::checkpoint_graph::GraphStaging;
        use lash_core_store::checkpoint_graph::NodeId;
        use lash_core_store::checkpoint_graph::NodeKind;
        use lash_core_store::checkpoint_graph::NodeSource as _;
        use lash_core_store::checkpoint_graph::TypedBody;
        use lash_kernel_value::tokens::DecodeDimension;
        use lash_kernel_value::tokens::TokenWriter;
        let conn = Connection::open_in_memory().expect("SQLite");
        conn.execute_batch(crate::schema::SCHEMA).expect("schema");
        let mut staging = GraphStaging::new(EncodeBudget::new(ExplicitLimit::new(u64::MAX)));
        let missing = NodeId([47; 32]);
        let mut body = TokenWriter::new();
        body.child(missing.child_ref());
        let first = staging
            .encode(NodeKind::ROUND_FACT, TypedBody::logical(body.as_bytes()))
            .expect("first root");
        let bytes = &staging.nodes()[&first].bytes;
        conn.execute(
            "INSERT INTO blobs VALUES (?1, ?2, 1)",
            rusqlite::params![first.0.as_slice(), bytes.as_ref()],
        )
        .expect("first root");
        let later = NodeId([0; 32]);
        conn.execute_batch("PRAGMA ignore_check_constraints = ON")
            .expect("foreign corruption");
        conn.execute(
            "INSERT INTO blobs VALUES (?1, x'00', 7)",
            [later.0.as_slice()],
        )
        .expect("corrupt later root");
        let roots = [first, later];
        let walk = |limits: &DecodeLimits| {
            super::super::graph::walk_roots(&conn, &roots, limits).expect("read")
        };
        let limits = lash_durable::domain::graph::OWNER_READ_LIMITS;
        assert_eq!(walk(&limits), Err(GraphRefusal::MissingNode(missing)));
        assert_eq!(
            walk(&DecodeLimits { nodes: 0, ..limits }),
            Err(GraphRefusal::DecodeLimit {
                dimension: DecodeDimension::Nodes,
                actual: 1,
                limit: 0
            })
        );
        assert_eq!(
            walk(&DecodeLimits {
                total_bytes: 0,
                ..limits
            }),
            Err(GraphRefusal::DecodeLimit {
                dimension: DecodeDimension::TotalBytes,
                actual: bytes.len() as u64,
                limit: 0
            })
        );
        conn.execute(
            "INSERT INTO blobs VALUES (?1, x'00', 1)",
            [missing.0.as_slice()],
        )
        .expect("corrupt descendant");
        assert_eq!(walk(&limits), Err(GraphRefusal::HashMismatch(missing)));
        assert_eq!(
            walk(&DecodeLimits { nodes: 1, ..limits }),
            Err(GraphRefusal::DecodeLimit {
                dimension: DecodeDimension::Nodes,
                actual: 2,
                limit: 1
            })
        );
        conn.execute(
            "UPDATE blobs SET encoding = 9 WHERE hash = ?1",
            [missing.0.as_slice()],
        )
        .expect("corrupt encoding");
        let expected = GraphRefusal::Backend(lash_durable::StoreFailure {
            code: None,
            kind: lash_durable::StoreFailureKind::Corrupt,
            message: "stored blob encoding `9` does not decode".into(),
        });
        assert_eq!(
            walk(&DecodeLimits { nodes: 1, ..limits }),
            Err(expected.clone())
        );
        // Graph-only readers preserve the same encoding refusal, without
        // asking SQLite to convert a skipped opaque payload to a blob.
        conn.execute(
            "UPDATE blobs SET content = 'not a blob' WHERE hash = ?1",
            [missing.0.as_slice()],
        )
        .expect("foreign payload");
        assert!(
            matches!(super::super::graph::graph_closure(&conn, first, &limits).expect("read"), Err(refusal) if refusal == expected)
        );
        conn.execute(
            "UPDATE blobs SET encoding = 0 WHERE hash = ?1",
            [missing.0.as_slice()],
        )
        .expect("opaque descendant");
        let closure = super::super::graph::graph_closure(&conn, first, &limits)
            .expect("read")
            .expect("graph-only closure");
        assert_eq!(
            closure.fetch(&missing).expect("opaque identity").encoding,
            BlobEncoding::Opaque
        );
        assert!(
            closure
                .fetch(&missing)
                .expect("opaque identity")
                .bytes
                .is_empty()
        );
        assert!(
            super::super::graph::walk_roots(&conn, &[first], &limits).is_err(),
            "a full reader must still convert the opaque payload"
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
            process_views: Default::default(),
            actor: &actor,
            epoch: Epoch(1),
            now: lash_durable::DurableInstant(1),
            fleet: lash_core_store::store::FleetFormat::current(),
            blob_profile: crate::BuiltinBlobProfile::default(),
            retired: super::super::Retired::default(),
            open_roots: Default::default(),
        };
        let record = lash_core_store::checkpoint_graph::NodeId([7; 32]);
        conn.execute(
            "INSERT INTO blobs (hash, content, encoding) VALUES (?1, x'00', 1)",
            [record.0.as_slice()],
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
            rusqlite::params![owner.stored(), record.0.as_slice()],
        )
        .expect("foreign negative identity");
        let answer = read_limited(&conn, &owner, None, None);
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
