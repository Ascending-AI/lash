//! PostgreSQL adapter for sealed model-call records.
use super::steps::{Entry, Shape, Step, Ty, Value};
use super::{Committing, DurableError, sqlx_failure};
use lash_core_store::model_calls::ModelCallRecord;
use lash_durable::domain::ModelCallWrite;
use lash_store_sql::{Dialect, durable::model_calls::ModelCallStatements};
use sqlx::PgConnection;
use std::sync::LazyLock;

pub(super) static SQL: LazyLock<ModelCallStatements> =
    LazyLock::new(|| ModelCallStatements::render(Dialect::postgres()));
lash_store_sql::statements! {
    lazy
    struct ModelCallPostgresStatements @ "durable_model_call_postgres" {
        selected = "SELECT call_id, (SELECT hash FROM blobs WHERE id = record_ref) AS record_ref, ended_at_ms FROM model_call_records
            WHERE session_id = ?1 AND record_ref IS NOT NULL AND run IN (?2, CASE WHEN ?2 LIKE 'turn:%' THEN 'owned:' || SUBSTR(?2, 6) END)
              AND call_id = ANY(?3) ORDER BY run, call";
        selected_pins = "SELECT run, call, call_id, attempt, answered, enclosing_owner, enclosing_run, enclosing_call FROM model_call_records
            WHERE session_id = ?1 AND record_ref IS NULL AND run IN (?2, CASE WHEN ?2 LIKE 'turn:%' THEN 'owned:' || SUBSTR(?2, 6) END)
              AND call_id = ANY(?3) ORDER BY run, call";

    }
}
static POSTGRES: LazyLock<ModelCallPostgresStatements> =
    LazyLock::new(|| ModelCallPostgresStatements::render(Dialect::postgres()));

/// Every call record of a deleted session `?1`.
pub(super) static DELETE_SESSION: Entry =
    Entry::new(|| &SQL.delete_session, &[Ty::Text], Shape::Returning).retiring("record_ref");
pub(super) static APPEND: Entry = Entry::new(
    || &SQL.append,
    &[
        Ty::Text,
        Ty::Text,
        Ty::Text,
        Ty::Text,
        Ty::Key,
        Ty::Now,
        Ty::Int,
    ],
    Shape::Exec,
);
pub(super) static RESERVE: Entry = Entry::new(
    || &SQL.reserve,
    &[
        Ty::Text,
        Ty::Text,
        Ty::Text,
        Ty::Text,
        Ty::Int,
        Ty::Text,
        Ty::Int,
        Ty::Text,
    ],
    Shape::Exec,
);
pub(super) static ANSWERED: Entry = Entry::new(
    || &SQL.answered,
    &[Ty::Text, Ty::Text, Ty::Text],
    Shape::Exec,
);

fn key_values(key: &lash_durable::domain::PromptCallKey) -> Vec<Value> {
    vec![
        Value::text(key.session.as_str()),
        Value::text(key.call.owner_column()),
        Value::text(key.call.call_column()),
    ]
}

/// Seal the lifecycle row without replacing an immutable record.
pub(super) fn steps(epoch: lash_durable::Epoch, write: &ModelCallWrite) -> Vec<Step> {
    let mut append = key_values(&write.key);
    append.extend([
        Value::text(&write.call_id.0),
        Value::key(write.record.to_hex()),
        Value::Now,
        Value::int(epoch.0),
    ]);
    vec![Step::one(&APPEND, append)]
}

pub(super) fn pin_steps(write: &lash_durable::domain::ModelCallPinWrite) -> Vec<Step> {
    use lash_durable::domain::ModelCallPinWrite;
    vec![match write {
        ModelCallPinWrite::Reserve {
            key,
            call_id,
            attempt,
            enclosing,
        } => {
            let mut values = key_values(key);
            values.extend([
                Value::text(&call_id.0),
                Value::int(i64::from(*attempt)),
                Value::Text(enclosing.as_ref().map(|owner| owner.owner.stored())),
                Value::Int(
                    enclosing
                        .as_ref()
                        .map(|owner| i64::try_from(owner.run.0).unwrap_or(i64::MAX)),
                ),
                Value::Text(
                    enclosing
                        .as_ref()
                        .map(|owner| owner.call.as_str().to_owned()),
                ),
            ]);
            Step::one(&RESERVE, values)
        }
        ModelCallPinWrite::Answered { key } => Step::any(&ANSWERED, key_values(key)),
    }]
}

pub(super) async fn apply(
    tx: &mut PgConnection,
    commit: &Committing<'_>,
    write: &ModelCallWrite,
) -> Result<(), DurableError> {
    if super::steps::run(tx, &steps(commit.epoch, write), &commit.steps())
        .await?
        .is_some()
    {
        return Err(super::corrupt(
            "model call",
            "a sealed call cannot be replaced",
        ));
    }
    Ok(())
}
pub(super) async fn delete_session(
    tx: &mut PgConnection,
    session: &lash_kernel_value::identity::SessionId,
) -> Result<Vec<String>, sqlx::Error> {
    let roots = crate::keys::query_scalar::<Option<String>>(SQL.delete_session.sql())
        .bind(session.as_str())
        .fetch_all(crate::observed_sql::executor(tx))
        .await?;
    Ok(roots.into_iter().flatten().collect())
}

impl super::PostgresDurableStore {
    pub(super) async fn read_model_call_ids(
        &self,
        session: &lash_kernel_value::identity::SessionId,
        owner: &str,
    ) -> Result<Vec<lash_protocol_types::llm::types::LlmCallId>, DurableError> {
        self.read(|tx| {
            Box::pin(async move {
                let all = lash_durable::domain::process_archive::ModelSelection::Records(None);
                // The history discriminator chooses the representation
                // before any row is read.
                let archived = super::process_history::model_calls(tx, session, owner, all).await?;
                let mut ids: Vec<String> = match archived {
                    Some((_, models)) => models.into_iter().map(|model| model.call_id).collect(),
                    None => crate::observed_sql::query_scalar(SQL.ids.sql())
                        .bind(session.as_str())
                        .bind(owner)
                        .fetch_all(crate::observed_sql::executor(&mut **tx))
                        .await
                        .map_err(sqlx_failure)?,
                };
                // Each statement reads its own snapshot, and a pack only ever
                // moves a history from rows to its archive: rows gone since
                // the discriminator was read are answered by the
                // discriminator read again.
                if ids.is_empty()
                    && let Some((_, models)) =
                        super::process_history::model_calls(tx, session, owner, all).await?
                {
                    ids = models.into_iter().map(|model| model.call_id).collect();
                }
                Ok(ids
                    .into_iter()
                    .map(lash_protocol_types::llm::types::LlmCallId)
                    .collect())
            })
        })
        .await
    }
    pub(super) async fn read_model_calls(
        &self,
        session: &lash_kernel_value::identity::SessionId,
        owner: &str,
        ids: Option<&[lash_protocol_types::llm::types::LlmCallId]>,
    ) -> Result<Vec<ModelCallRecord>, DurableError> {
        let limits = self.pools.graph_reads.owner();
        self.read_snapshot(|snapshot| {
            Box::pin(async move {
                // The history discriminator chooses the representation
                // first, in the read's one snapshot.
                if let Some((bytes, models)) = super::process_history::model_calls(
                    snapshot,
                    session,
                    owner,
                    lash_durable::domain::process_archive::ModelSelection::Records(ids),
                )
                .await?
                {
                    return super::process_history::model_records(&bytes, models, limits).await;
                }
                let sql = if ids.is_some() {
                    POSTGRES.selected.sql()
                } else {
                    SQL.read.sql()
                };
                let mut query = crate::observed_sql::query_as(sql)
                    .bind(session.as_str())
                    .bind(owner);
                if let Some(ids) = ids {
                    query = query.bind(ids.iter().map(|id| id.0.as_str()).collect::<Vec<_>>());
                }
                let rows: Vec<(String, crate::keys::Key, i64)> = query
                    .fetch_all(crate::observed_sql::executor(&mut **snapshot))
                    .await
                    .map_err(sqlx_failure)?;
                let rows = rows
                    .into_iter()
                    .map(|(id, root, ended)| Ok((id, super::node_ref(&root)?, ended)))
                    .collect::<Result<Vec<_>, DurableError>>()?;
                let roots: Vec<_> = rows.iter().map(|(_, root, _)| *root).collect();
                let nodes = super::graph::walk_within(snapshot, &roots, &limits, None).await?;
                decode_records(rows, nodes, limits)
            })
        })
        .await
    }
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
    limits: lash_core_store::checkpoint_graph::DecodeLimits,
) -> Result<Vec<ModelCallRecord>, DurableError> {
    use lash_core_store::{checkpoint_graph::GraphDecoder, model_calls::ModelCallFact};
    use lash_durable::domain::RunFacts;
    let graph = RunFacts {
        nodes,
        read_limits: limits,
        ..Default::default()
    };
    let mut decoder = GraphDecoder::new(&graph, limits);
    let mut records = Vec::with_capacity(rows.len());
    for (id, root, ended) in rows {
        let fact = ModelCallFact::load(&mut decoder, root)?;
        if fact.call_id.0 != id {
            return Err(super::corrupt("model call", "row and fact identity differ"));
        }
        records.push(ModelCallRecord {
            call: fact.call,
            call_id: fact.call_id,
            end: fact.end,
            record: fact.record,
            ended_at_ms: u64::try_from(ended)
                .map_err(|_| super::corrupt("model call", "negative end time"))?,
        });
    }
    records.sort_by_key(|record| record.call);
    Ok(records)
}

pub(super) async fn apply_pin(
    tx: &mut PgConnection,
    commit: &Committing<'_>,
    write: &lash_durable::domain::ModelCallPinWrite,
) -> Result<(), DurableError> {
    if super::steps::run(tx, &pin_steps(write), &commit.steps())
        .await?
        .is_some()
    {
        return Err(super::corrupt(
            "model call reservation",
            "conflicting identity",
        ));
    }
    Ok(())
}
/// SQL reservation row, including the opaque outcome that owns its terminal.
type StoredModelCallPin = (
    String,
    String,
    String,
    i64,
    bool,
    Option<String>,
    Option<i64>,
    Option<String>,
);

impl super::PostgresDurableStore {
    pub(super) async fn read_model_call_pins(
        &self,
        session: &lash_kernel_value::identity::SessionId,
        owner: &str,
        ids: Option<&[lash_protocol_types::llm::types::LlmCallId]>,
        call: Option<&str>,
    ) -> Result<Vec<lash_durable::domain::ModelCallPinRow>, DurableError> {
        self.read(|tx| {
            Box::pin(async move {
                let pinned =
                    lash_durable::domain::process_archive::ModelSelection::Pins { ids, call };
                // The history discriminator chooses the representation
                // before any row is read.
                if let Some((_, models)) =
                    super::process_history::model_calls(tx, session, owner, pinned).await?
                {
                    return models
                        .into_iter()
                        .map(|model| lash_durable::domain::process_archive::pin_row(session, model))
                        .collect();
                }
                let sql = if call.is_some() {
                    SQL.pin_by_key.sql()
                } else if ids.is_some() {
                    POSTGRES.selected_pins.sql()
                } else {
                    SQL.pins.sql()
                };
                let mut query = crate::observed_sql::query_as(sql)
                    .bind(session.as_str())
                    .bind(owner);
                if let Some(call) = call {
                    query = query.bind(call);
                } else if let Some(ids) = ids {
                    query = query.bind(ids.iter().map(|id| id.0.as_str()).collect::<Vec<_>>());
                }
                let rows: Vec<StoredModelCallPin> = query
                    .fetch_all(crate::observed_sql::executor(&mut **tx))
                    .await
                    .map_err(sqlx_failure)?;
                // Each statement reads its own snapshot, and a pack only ever
                // moves a history from rows to its archive: rows gone since
                // the discriminator was read are answered by the
                // discriminator read again.
                if rows.is_empty()
                    && let Some((_, models)) =
                        super::process_history::model_calls(tx, session, owner, pinned).await?
                {
                    return models
                        .into_iter()
                        .map(|model| lash_durable::domain::process_archive::pin_row(session, model))
                        .collect();
                }
                rows.into_iter()
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
                    .collect()
            })
        })
        .await
    }
}
