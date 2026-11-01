//! Retained tool-call publications, sharing the existing fenced commit.

use std::sync::LazyLock;

use lash_core_store::checkpoint_graph::{DecodeLimits, NodeId};
use lash_durable::domain::tool_calls::ToolCallRoot;
use lash_durable::domain::{
    OwnerKey, RunFacts, RunSeq, ToolCallAnswer, ToolCallDecision, ToolCallRow, ToolCallSelection,
    ToolCallWrite, ToolCalls,
};
use lash_durable::{DurableError, Epoch};
use lash_store_sql::{Dialect, durable::tool_calls::ToolCallStatements};
use sqlx::PgConnection;

use super::steps::{Entry, Shape, Step, Ty, Value};
use super::{Committing, corrupt, sqlx_failure};

pub(super) static SQL: LazyLock<ToolCallStatements> =
    LazyLock::new(|| ToolCallStatements::render(Dialect::postgres()));
lash_store_sql::statements! {
    struct ToolCallPostgresStatements @ "durable_tool_call_postgres" {
        selected = "SELECT call_id, execution_owner, run_seq, member, decision, decision_ref, answer_outcome, body_ref, admission_seq
            FROM tool_calls WHERE owner_key = ?1 AND call_id = ANY(?2)";
    }
}
static POSTGRES: LazyLock<ToolCallPostgresStatements> =
    LazyLock::new(|| ToolCallPostgresStatements::render(Dialect::postgres()));

pub(super) static ADMIT: Entry = Entry::new(
    || &SQL.admit,
    &[
        Ty::Text,
        Ty::Text,
        Ty::Text,
        Ty::Int,
        Ty::Int,
        Ty::Bool,
        Ty::Text,
        Ty::Text,
        Ty::Bool,
    ],
    Shape::Exec,
);
pub(super) static DECIDE: Entry = Entry::new(
    || &SQL.decide,
    &[Ty::Text, Ty::Text, Ty::Text, Ty::Key],
    Shape::Exec,
);
pub(super) static ANSWER: Entry = Entry::new(
    || &SQL.answer,
    &[Ty::Text, Ty::Text, Ty::Text, Ty::Key, Ty::Bool],
    Shape::Exec,
);
pub(super) static COMPLETE: Entry = Entry::new(
    || &SQL.complete,
    &[Ty::Text, Ty::Text, Ty::Key, Ty::Text, Ty::Key, Ty::Bool],
    Shape::Exec,
);

pub(super) fn steps(_epoch: Epoch, write: &ToolCallWrite) -> Result<Vec<Step>, DurableError> {
    let number =
        |value| i64::try_from(value).map_err(|_| corrupt("tool call ordinal", "out of range"));
    if let ToolCallWrite::Answer { answer, .. } | ToolCallWrite::Complete { answer, .. } = write
        && !answer.valid()
    {
        return Err(corrupt("tool call", "settlement and body disagree"));
    }
    let (entry, values) = match write {
        ToolCallWrite::Expire { .. } => return Ok(Vec::new()),
        ToolCallWrite::Admit {
            counted,
            logical,
            execution,
            run,
            member,
            call,
            cancellable,
        } => {
            let owner = logical.clone();
            let (session, turn) = match &owner {
                lash_durable::domain::CallOwner::Turn(session, turn) => (
                    Some(session.as_str().to_owned()),
                    Some(turn.as_str().to_owned()),
                ),
                lash_durable::domain::CallOwner::Operation(session, _) => {
                    (Some(session.as_str().to_owned()), None)
                }
                _ => (None, None),
            };
            (
                &ADMIT,
                vec![
                    Value::text(owner.stored()),
                    Value::text(call.as_str()),
                    Value::text(execution.stored()),
                    Value::int(number(run.0)?),
                    Value::int(number(*member)?),
                    Value::Bool(*cancellable),
                    Value::Text(session),
                    Value::Text(turn),
                    Value::Bool(*counted),
                ],
            )
        }
        ToolCallWrite::Decide {
            owner,
            call,
            decision,
            fact,
        } => (
            &DECIDE,
            vec![
                Value::text(owner.stored()),
                Value::text(call.as_str()),
                Value::text(match decision {
                    ToolCallDecision::Final => "final",
                    ToolCallDecision::Cancel => "cancel",
                }),
                Value::key(fact.to_hex()),
            ],
        ),
        ToolCallWrite::Complete {
            owner,
            call,
            fact,
            answer,
        } => (
            &COMPLETE,
            vec![
                Value::text(owner.stored()),
                Value::text(call.as_str()),
                Value::key(fact.to_hex()),
                Value::text(
                    serde_json::to_string(&answer.outcome)
                        .map_err(|error| corrupt("tool call outcome", &error.to_string()))?,
                ),
                Value::Key(answer.body.map(|node| node.to_hex())),
                Value::Bool(answer.cancelled()),
            ],
        ),
        ToolCallWrite::Answer {
            owner,
            call,
            answer,
        } => (
            &ANSWER,
            vec![
                Value::text(owner.stored()),
                Value::text(call.as_str()),
                Value::text(
                    serde_json::to_string(&answer.outcome)
                        .map_err(|error| corrupt("tool call outcome", &error.to_string()))?,
                ),
                Value::Key(answer.body.map(|node| node.to_hex())),
                Value::Bool(answer.cancelled()),
            ],
        ),
    };
    Ok(vec![Step::one(entry, values)])
}

pub(super) async fn apply(
    tx: &mut PgConnection,
    commit: &Committing<'_>,
    write: &ToolCallWrite,
) -> Result<(), DurableError> {
    if let ToolCallWrite::Expire { owner } = write {
        for root in delete_owner(tx, owner).await.map_err(sqlx_failure)? {
            commit.retired.stored(root);
        }
        return Ok(());
    }
    if super::steps::run(tx, &steps(commit.epoch, write)?, &commit.steps())
        .await?
        .is_some()
    {
        return Err(corrupt(
            "tool call",
            "conflicting identity or immutable publication",
        ));
    }
    Ok(())
}

pub(super) fn root_sql(part: ToolCallRoot) -> &'static str {
    match part {
        ToolCallRoot::Decision => SQL.decision_root.sql(),
        ToolCallRoot::Answer => SQL.answer_root.sql(),
    }
}

type StoredCall = (
    String,
    String,
    i64,
    i64,
    Option<String>,
    Option<crate::keys::Key>,
    Option<String>,
    Option<crate::keys::Key>,
    i64,
);

impl super::PostgresDurableStore {
    pub(super) async fn read_tool_calls(
        &self,
        owner: &lash_durable::domain::CallOwner,
        selection: &ToolCallSelection,
    ) -> Result<ToolCalls, DurableError> {
        let limits: DecodeLimits = self.pools.graph_reads.owner();
        self.read_snapshot(|snapshot| {
            Box::pin(async move {
                let key = owner.stored();
                let rows: Vec<StoredCall> = match selection {
                    ToolCallSelection::Calls(calls) => {
                        crate::observed_sql::query_as(POSTGRES.selected.sql())
                            .bind(key)
                            .bind(calls.iter().map(|call| call.as_str()).collect::<Vec<_>>())
                            .fetch_all(crate::observed_sql::executor(&mut **snapshot))
                            .await
                            .map_err(sqlx_failure)?
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
                        crate::observed_sql::query_as(sql.sql())
                            .bind(key)
                            .bind(sequence)
                            .bind(call)
                            .bind(i64::from(*limit))
                            .fetch_all(crate::observed_sql::executor(&mut **snapshot))
                            .await
                            .map_err(sqlx_failure)?
                    }
                };
                let rows = rows
                    .into_iter()
                    .map(
                        |(
                            call,
                            execution,
                            run,
                            member,
                            decision,
                            decision_ref,
                            outcome,
                            body,
                            sequence,
                        )| {
                            Ok(ToolCallRow {
                                sequence: u64::try_from(sequence)
                                    .map_err(|_| corrupt("tool call order", "negative"))?,
                                owner: owner.clone(),
                                call: lash_protocol_types::ToolCallId::parse(&call)
                                    .map_err(|_| corrupt("tool call id", &call))?,
                                execution: OwnerKey::parse(&execution)
                                    .map_err(|_| corrupt("tool call execution", &execution))?,
                                run: RunSeq(
                                    u64::try_from(run)
                                        .map_err(|_| corrupt("tool call run", "negative"))?,
                                ),
                                member: u64::try_from(member)
                                    .map_err(|_| corrupt("tool call member", "negative"))?,
                                decision: match decision.as_deref() {
                                    None => None,
                                    Some("final") => Some(ToolCallDecision::Final),
                                    Some("cancel") => Some(ToolCallDecision::Cancel),
                                    Some(other) => {
                                        return Err(corrupt("tool call decision", other));
                                    }
                                },
                                decision_fact: decision_ref
                                    .as_ref()
                                    .map(|node| super::node_ref(&node.0))
                                    .transpose()?,
                                answer: outcome
                                    .map(|outcome| {
                                        Ok::<_, DurableError>(ToolCallAnswer {
                                            outcome: serde_json::from_str(&outcome).map_err(
                                                |_| corrupt("tool call outcome", &outcome),
                                            )?,
                                            body: body
                                                .as_ref()
                                                .map(|node| super::node_ref(&node.0))
                                                .transpose()?,
                                        })
                                    })
                                    .transpose()?,
                            })
                        },
                    )
                    .collect::<Result<Vec<_>, DurableError>>()?;
                let roots: Vec<NodeId> = rows
                    .iter()
                    .filter_map(|row| row.answer.as_ref().and_then(|answer| answer.body))
                    .collect();
                let nodes = super::graph::walk_within(snapshot, &roots, &limits, None).await?;
                Ok(ToolCalls {
                    rows,
                    graph: RunFacts {
                        nodes,
                        read_limits: limits,
                        ..Default::default()
                    },
                })
            })
        })
        .await
    }
}

pub(super) async fn delete_owner(
    tx: &mut PgConnection,
    owner: &lash_durable::domain::CallOwner,
) -> Result<Vec<String>, sqlx::Error> {
    let rows: Vec<(Option<crate::keys::Key>, Option<crate::keys::Key>)> =
        crate::observed_sql::query_as(SQL.delete_owner.sql())
            .bind(owner.stored())
            .fetch_all(crate::observed_sql::executor(tx))
            .await?;
    Ok(rows
        .into_iter()
        .flat_map(|(decision, body)| [decision, body])
        .flatten()
        .map(|key| key.0)
        .collect())
}

pub(super) async fn delete_session(
    tx: &mut PgConnection,
    session: &lash_kernel_value::identity::SessionId,
) -> Result<Vec<String>, sqlx::Error> {
    let rows: Vec<(Option<crate::keys::Key>, Option<crate::keys::Key>)> =
        crate::observed_sql::query_as(SQL.delete_session.sql())
            .bind(session.as_str())
            .fetch_all(crate::observed_sql::executor(tx))
            .await?;
    Ok(rows
        .into_iter()
        .flat_map(|(decision, body)| [decision, body])
        .flatten()
        .map(|key| key.0)
        .collect())
}

pub(super) static FINISH_READS: Entry = Entry::new(
    || &SQL.finish_reads,
    &[Ty::Text, Ty::Text, Ty::Text],
    Shape::Exec,
);
pub(super) fn finish_reads_step(
    session: &lash_kernel_value::identity::SessionId,
    run: &lash_vocabulary::identity::TurnId,
) -> Result<Step, DurableError> {
    let outcome = serde_json::to_string(&lash_durable::domain::tool_calls::interrupted_read())
        .map_err(|error| corrupt("read terminal", &error.to_string()))?;
    Ok(Step::any(
        &FINISH_READS,
        vec![
            Value::text(session.as_str()),
            Value::text(run.as_str()),
            Value::text(outcome),
        ],
    ))
}
