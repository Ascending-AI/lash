//! Tool-call publications inside the fenced execution transaction.

use crate::Prepare as _;
use std::sync::LazyLock;

use lash_core_store::checkpoint_graph::DecodeLimits;
use lash_durable::domain::tool_calls::ToolCallRoot;
use lash_durable::domain::{
    OwnerKey, RunFacts, RunSeq, ToolCallAnswer, ToolCallDecision, ToolCallRow, ToolCallSelection,
    ToolCallWrite, ToolCalls,
};
use lash_store_sql::durable::tool_calls::ToolCallStatements;
use rusqlite::{Connection, OptionalExtension};

use super::{Answer, Committing, corrupt, integer, node_ref};

pub(crate) const TABLES: &str = "
CREATE TABLE IF NOT EXISTS tool_calls (
    owner_key TEXT NOT NULL, call_id TEXT NOT NULL,
    execution_owner TEXT NOT NULL, run_seq INTEGER NOT NULL, member INTEGER NOT NULL,
    admission_seq INTEGER NOT NULL,
    counted INTEGER NOT NULL CHECK (counted IN (0, 1)),
    admitted_count INTEGER NOT NULL CHECK (admitted_count >= 0),
    cancellable INTEGER NOT NULL CHECK (cancellable IN (0, 1)),
    session_id TEXT, turn_id TEXT,
    request_id TEXT, origin TEXT, reason TEXT,
    decision TEXT CHECK (decision IN ('final', 'cancel')),
    decision_ref INTEGER REFERENCES blobs(id),
    answer_outcome TEXT,
    body_ref INTEGER REFERENCES blobs(id),
    PRIMARY KEY (owner_key, call_id),
    CHECK ((decision IS NULL) = (decision_ref IS NULL)),
    CHECK (body_ref IS NULL OR answer_outcome IS NOT NULL)
);
CREATE UNIQUE INDEX IF NOT EXISTS ux_tool_calls_execution ON tool_calls(execution_owner, call_id);
CREATE INDEX IF NOT EXISTS idx_tool_calls_order ON tool_calls(owner_key, admission_seq, call_id);
CREATE INDEX IF NOT EXISTS idx_tool_calls_requests ON tool_calls(session_id, turn_id, call_id) WHERE request_id IS NOT NULL AND answer_outcome IS NULL;
CREATE INDEX IF NOT EXISTS idx_tool_calls_reads ON tool_calls(owner_key, answer_outcome, admission_seq, call_id) WHERE NOT cancellable;
CREATE INDEX IF NOT EXISTS idx_tool_calls_body ON tool_calls(body_ref) WHERE body_ref IS NOT NULL;
CREATE INDEX IF NOT EXISTS idx_tool_calls_decision ON tool_calls(decision_ref) WHERE decision_ref IS NOT NULL;
";

pub(super) static SQL: LazyLock<ToolCallStatements> =
    LazyLock::new(|| ToolCallStatements::render(crate::schema_layout::MAIN));
lash_store_sql::statements! {
    lazy
    struct ToolCallSqliteStatements @ "durable_tool_call_sqlite" {
        admit_many = "INSERT INTO tool_calls AS held
            (owner_key, call_id, execution_owner, run_seq, member, cancellable, session_id, turn_id, counted, admission_seq, admitted_count)
            SELECT owner, call, execution, run, member, cancellable, session, turn, counted, admission_seq, admitted_count
            FROM (
                WITH context AS MATERIALIZED (SELECT ?1 AS owner, ?2 AS execution, ?3 AS run, ?4 AS session, ?5 AS turn, ?6 AS calls),
                prior AS MATERIALIZED (SELECT admission_seq, admitted_count FROM tool_calls WHERE owner_key = (SELECT owner FROM context) ORDER BY admission_seq DESC LIMIT 1),
                source AS MATERIALIZED (SELECT CAST(j.key AS INTEGER) AS pos, c.owner, c.execution, c.run, c.session, c.turn,
                    j.value->>0 AS call, j.value->>1 AS member, j.value->>2 AS cancellable, j.value->>3 AS counted
                    FROM context AS c, json_each(c.calls) AS j),
                incoming AS MATERIALIZED (SELECT source.*,
                    CASE WHEN pos = MIN(pos) OVER (PARTITION BY call)
                        AND NOT EXISTS (SELECT 1 FROM tool_calls WHERE owner_key = source.owner AND call_id = source.call)
                        THEN 1 ELSE 0 END AS fresh FROM source)
                SELECT i.*, COALESCE(p.admission_seq, -1) + SUM(i.fresh) OVER (ORDER BY i.pos) AS admission_seq,
                    COALESCE(p.admitted_count, 0) + SUM(i.fresh * i.counted) OVER (ORDER BY i.pos) AS admitted_count
                FROM incoming AS i LEFT JOIN prior AS p ON TRUE
            ) AS batch WHERE TRUE ORDER BY pos
            ON CONFLICT (owner_key, call_id) DO UPDATE SET call_id = excluded.call_id
            WHERE (held.execution_owner, held.run_seq, held.member, held.cancellable, held.counted)
                = (excluded.execution_owner, excluded.run_seq, excluded.member, excluded.cancellable, excluded.counted)";
        selected = "SELECT call_id, execution_owner, run_seq, member, decision,
                answer_outcome, (SELECT hash FROM blobs WHERE id = body_ref) AS body_ref, admission_seq
            FROM tool_calls WHERE owner_key = ?1 AND call_id IN (SELECT value FROM json_each(?2))
            ORDER BY admission_seq, call_id";
    }
}
static SQLITE: LazyLock<ToolCallSqliteStatements> =
    LazyLock::new(|| ToolCallSqliteStatements::render(crate::schema_layout::MAIN));

/// Only consecutive admissions with one context can share that context's bind.
pub(super) fn admission_group_len(writes: &[lash_durable::DomainWrite]) -> usize {
    let Some(lash_durable::DomainWrite::ToolCall(ToolCallWrite::Admit {
        logical,
        execution,
        run,
        ..
    })) = writes.first()
    else {
        return 0;
    };
    writes.iter().take_while(|write| matches!(write,
        lash_durable::DomainWrite::ToolCall(ToolCallWrite::Admit { logical: other_logical, execution: other_execution, run: other_run, .. })
            if other_logical == logical && other_execution == execution && other_run == run)).count()
}

pub(super) fn apply_admissions(
    tx: &Connection,
    writes: &[lash_durable::DomainWrite],
) -> Answer<()> {
    let Some(lash_durable::DomainWrite::ToolCall(ToolCallWrite::Admit {
        logical,
        execution,
        run,
        ..
    })) = writes.first()
    else {
        return Ok(Err(corrupt("tool admissions", "empty or mixed batch")));
    };
    let mut rows = Vec::with_capacity(writes.len());
    for write in writes {
        let lash_durable::DomainWrite::ToolCall(ToolCallWrite::Admit {
            call,
            member,
            cancellable,
            counted,
            ..
        }) = write
        else {
            return Ok(Err(corrupt("tool admissions", "mixed batch")));
        };
        rows.push((
            call.as_str(),
            integer::<i64>(*member)?,
            *cancellable,
            *counted,
        ));
    }
    let (session, turn) = match logical {
        lash_durable::domain::CallOwner::Turn(session, turn) => {
            (Some(session.as_str()), Some(turn.as_str()))
        }
        lash_durable::domain::CallOwner::Operation(session, _) => (Some(session.as_str()), None),
        lash_durable::domain::CallOwner::Process(_) => (None, None),
    };
    let rows = serde_json::to_string(&rows)
        .map_err(|error| rusqlite::Error::ToSqlConversionFailure(Box::new(error)))?;
    let changed = tx
        .statement(SQLITE.admit_many.sql())?
        .execute(rusqlite::params![
            logical.stored(),
            execution.stored(),
            integer::<i64>(run.0)?,
            session,
            turn,
            rows
        ])?;
    Ok(if changed == writes.len() {
        Ok(())
    } else {
        Err(corrupt("tool admissions", "conflicting identity in batch"))
    })
}

pub(super) fn apply(
    tx: &Connection,
    _commit: &Committing<'_>,
    write: &ToolCallWrite,
) -> Answer<()> {
    let changed = match write {
        ToolCallWrite::Expire { owner } => {
            for root in delete_owner(tx, owner)? {
                _commit.retired.stored(root);
            }
            return Ok(Ok(()));
        }
        ToolCallWrite::Admit {
            counted,
            logical,
            execution,
            run,
            member,
            call,
            cancellable,
        } => {
            let owner = logical;
            let (session, turn) = match &owner {
                lash_durable::domain::CallOwner::Turn(session, turn) => {
                    (Some(session.as_str()), Some(turn.as_str()))
                }
                lash_durable::domain::CallOwner::Operation(session, _) => {
                    (Some(session.as_str()), None)
                }
                _ => (None, None),
            };
            tx.statement(SQL.admit.sql())?.execute(rusqlite::params![
                owner.stored(),
                call.as_str(),
                execution.stored(),
                integer::<i64>(run.0)?,
                integer::<i64>(*member)?,
                cancellable,
                session,
                turn,
                counted
            ])?
        }
        ToolCallWrite::Decide {
            owner,
            call,
            decision,
            fact,
        } => tx.statement(SQL.decide.sql())?.execute(rusqlite::params![
            owner.stored(),
            call.as_str(),
            match decision {
                ToolCallDecision::Final => "final",
                ToolCallDecision::Cancel => "cancel",
            },
            fact.0.as_slice()
        ])?,
        ToolCallWrite::Complete {
            owner,
            call,
            fact,
            answer,
        } => {
            let outcome = answer
                .stored_outcome()
                .map_err(|error| rusqlite::Error::ToSqlConversionFailure(Box::new(error)))?;
            tx.statement(SQL.complete.sql())?
                .execute(rusqlite::params![
                    owner.stored(),
                    call.as_str(),
                    fact.0.as_slice(),
                    outcome,
                    answer.body().map(|node| node.0.to_vec()),
                    answer.cancelled()
                ])?
        }
        ToolCallWrite::Answer {
            owner,
            call,
            answer,
        } => {
            let outcome = answer
                .stored_outcome()
                .map_err(|error| rusqlite::Error::ToSqlConversionFailure(Box::new(error)))?;
            tx.statement(SQL.answer.sql())?.execute(rusqlite::params![
                owner.stored(),
                call.as_str(),
                outcome,
                answer.body().map(|node| node.0.to_vec()),
                answer.cancelled()
            ])?
        }
    };
    Ok(if changed == 1 {
        Ok(())
    } else {
        Err(corrupt(
            "tool call",
            "conflicting identity or immutable publication",
        ))
    })
}

pub(super) fn root(
    tx: &Connection,
    owner: &OwnerKey,
    call: &lash_protocol_types::ToolCallId,
    part: ToolCallRoot,
) -> rusqlite::Result<Option<String>> {
    let sql = match part {
        ToolCallRoot::Decision => &SQL.decision_root,
        ToolCallRoot::Answer => &SQL.answer_root,
    };
    Ok(tx
        .statement(sql.sql())?
        .query_row([owner.stored(), call.as_str().to_owned()], |row| {
            crate::codec::optional_hash_column(row, 0)
        })
        .optional()?
        .flatten())
}

type StoredCall = (
    String,
    String,
    i64,
    i64,
    Option<String>,
    Option<String>,
    Option<String>,
    i64,
);

pub(super) fn read(
    tx: &Connection,
    owner: &lash_durable::domain::CallOwner,
    selection: &ToolCallSelection,
    limits: DecodeLimits,
) -> Answer<ToolCalls> {
    // The history discriminator chooses the representation first.
    match super::process_history::tool_calls(tx, owner, selection, &limits)? {
        Ok(Some(archived)) => return Ok(Ok(archived)),
        Ok(None) => {}
        Err(error) => return Ok(Err(error)),
    }
    let map = |row: &rusqlite::Row<'_>| -> rusqlite::Result<StoredCall> {
        Ok((
            row.get(0)?,
            row.get(1)?,
            row.get(2)?,
            row.get(3)?,
            row.get(4)?,
            row.get(5)?,
            crate::codec::optional_hash_column(row, 6)?,
            row.get(7)?,
        ))
    };
    let key = owner.stored();
    let stored = match selection {
        ToolCallSelection::Calls(calls) => {
            let keys =
                serde_json::Value::from(calls.iter().map(|call| call.as_str()).collect::<Vec<_>>())
                    .to_string();
            tx.statement(SQLITE.selected.sql())?
                .query_map(rusqlite::params![key, keys], map)?
                .collect::<rusqlite::Result<Vec<_>>>()?
        }
        ToolCallSelection::List { after, limit }
        | ToolCallSelection::SavedReads { after, limit } => {
            let (sequence, call) = after.as_ref().map_or((-1, ""), |cursor| {
                (
                    i64::try_from(cursor.sequence).unwrap_or(i64::MAX),
                    cursor.call.as_str(),
                )
            });
            let sql = match selection {
                ToolCallSelection::SavedReads { .. } => &SQL.saved_reads,
                _ => &SQL.page,
            };
            tx.statement(sql.sql())?
                .query_map(rusqlite::params![key, sequence, call, limit], map)?
                .collect::<rusqlite::Result<Vec<_>>>()?
        }
    };
    let rows = match stored
        .into_iter()
        .map(
            |(call, execution, run, member, decision, outcome, body, sequence)| {
                Ok(ToolCallRow {
                    sequence: u64::try_from(sequence)
                        .map_err(|_| corrupt("tool call order", "negative"))?,
                    owner: owner.clone(),
                    call: lash_protocol_types::ToolCallId::parse(&call)
                        .map_err(|_| corrupt("tool call id", &call))?,
                    execution: OwnerKey::parse(&execution)
                        .map_err(|_| corrupt("tool call execution", &execution))?,
                    run: RunSeq(
                        u64::try_from(run).map_err(|_| corrupt("tool call run", "negative"))?,
                    ),
                    member: u64::try_from(member)
                        .map_err(|_| corrupt("tool call member", "negative"))?,
                    decision: match decision.as_deref() {
                        None => None,
                        Some("final") => Some(ToolCallDecision::Final),
                        Some("cancel") => Some(ToolCallDecision::Cancel),
                        Some(other) => return Err(corrupt("tool call decision", other)),
                    },
                    answer: outcome
                        .map(|outcome| {
                            ToolCallAnswer::from_stored(
                                &outcome,
                                body.as_deref().map(node_ref).transpose()?,
                            )
                            .ok_or_else(|| corrupt("tool call answer", &outcome))
                        })
                        .transpose()?,
                })
            },
        )
        .collect::<Result<Vec<_>, lash_durable::DurableError>>()
    {
        Ok(rows) => rows,
        Err(error) => return Ok(Err(error)),
    };
    let roots = rows
        .iter()
        .filter_map(|row| row.answer.as_ref().and_then(ToolCallAnswer::body).copied())
        .collect::<Vec<_>>();
    let nodes = match super::graph::walk_roots(tx, &roots, &limits)? {
        Ok(nodes) => nodes,
        Err(error) => return Ok(Err(error.into())),
    };
    Ok(Ok(ToolCalls {
        rows,
        graph: RunFacts {
            nodes,
            read_limits: limits,
            ..Default::default()
        },
    }))
}

pub(super) fn delete_owner(
    tx: &Connection,
    owner: &lash_durable::domain::CallOwner,
) -> rusqlite::Result<Vec<String>> {
    tx.statement(SQL.delete_owner.sql())?
        .query_map([owner.stored()], |row| {
            Ok([
                crate::codec::optional_hash_column(row, 0)?,
                crate::codec::optional_hash_column(row, 1)?,
            ])
        })?
        .collect::<rusqlite::Result<Vec<_>>>()
        .map(|rows| rows.into_iter().flatten().flatten().collect())
}

pub(super) fn delete_session(
    tx: &Connection,
    session: &lash_kernel_value::identity::SessionId,
) -> rusqlite::Result<Vec<String>> {
    tx.statement(SQL.delete_session.sql())?
        .query_map([session.as_str()], |row| {
            Ok([
                crate::codec::optional_hash_column(row, 0)?,
                crate::codec::optional_hash_column(row, 1)?,
            ])
        })?
        .collect::<rusqlite::Result<Vec<_>>>()
        .map(|rows| rows.into_iter().flatten().flatten().collect())
}

pub(super) fn finish_reads(
    tx: &Connection,
    session: &lash_kernel_value::identity::SessionId,
    run: &lash_vocabulary::identity::TurnId,
) -> rusqlite::Result<()> {
    let outcome = lash_durable::domain::tool_calls::interrupted_read()
        .stored_outcome()
        .map_err(|error| rusqlite::Error::ToSqlConversionFailure(Box::new(error)))?;
    tx.statement(SQL.finish_reads.sql())?
        .execute(rusqlite::params![
            OwnerKey::Turn(session.clone(), run.clone()).stored(),
            outcome
        ])?;
    Ok(())
}

/// Avoid serializing retirement work for roots this commit keeps on call rows.
/// Expiry may follow publication in one commit, so leave that case to SQL.
pub(super) fn retain_publications(retired: &super::Retired, writes: &[lash_durable::DomainWrite]) {
    use lash_durable::DomainWrite;
    if writes
        .iter()
        .any(|write| matches!(write, DomainWrite::ToolCall(ToolCallWrite::Expire { .. })))
    {
        return;
    }
    for write in writes {
        match write {
            DomainWrite::ToolCall(ToolCallWrite::Decide { fact, .. }) => retired.retained(fact),
            DomainWrite::ToolCall(ToolCallWrite::Answer { answer, .. }) => {
                if let Some(body) = answer.body() {
                    retired.retained(body);
                }
            }
            DomainWrite::ToolCall(ToolCallWrite::Complete { fact, answer, .. }) => {
                retired.retained(fact);
                if let Some(body) = answer.body() {
                    retired.retained(body);
                }
            }
            _ => {}
        }
    }
}
