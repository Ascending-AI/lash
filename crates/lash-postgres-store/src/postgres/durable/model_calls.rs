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
    /// The immutable record guard, checked after the idempotent insert.
    struct ModelCallPostgresStatements @ "durable_model_call_postgres" {
        root_matches = "SELECT record_ref FROM model_call_records
            WHERE session_id = ?1 AND run = ?2 AND call = ?3 AND record_ref = ?4";
    }
}
static POSTGRES: LazyLock<ModelCallPostgresStatements> =
    LazyLock::new(|| ModelCallPostgresStatements::render(Dialect::postgres()));

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
pub(super) static ROOT_MATCHES: Entry = Entry::new(
    || &POSTGRES.root_matches,
    &[Ty::Text, Ty::Text, Ty::Text, Ty::Key],
    Shape::Select,
);
pub(super) static RETIRE_PIN: Entry = Entry::new(
    || &SQL.retire_pin,
    &[Ty::Text, Ty::Text, Ty::Text],
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

/// The insert, immutable-root check and reservation retirement stay ordered.
pub(super) fn steps(epoch: lash_durable::Epoch, write: &ModelCallWrite) -> Vec<Step> {
    let key = key_values(&write.key);
    let mut append = key.clone();
    append.extend([
        Value::text(&write.call_id.0),
        Value::key(write.record.to_hex()),
        Value::Now,
        Value::int(epoch.0),
    ]);
    let mut guard = key.clone();
    guard.push(Value::key(write.record.to_hex()));
    vec![
        Step::any(&APPEND, append),
        Step::one(&ROOT_MATCHES, guard),
        Step::any(&RETIRE_PIN, key),
    ]
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
            Step::any(&RESERVE, values)
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
    session: &lash_protocol_types::SessionId,
) -> Result<Vec<String>, sqlx::Error> {
    crate::observed_sql::query(SQL.delete_pins.sql())
        .bind(session.as_str())
        .execute(crate::observed_sql::executor(&mut *tx))
        .await?;
    crate::keys::query_scalar(SQL.delete_session.sql())
        .bind(session.as_str())
        .fetch_all(crate::observed_sql::executor(tx))
        .await
}

impl super::PostgresDurableStore {
    pub(super) async fn read_model_calls(
        &self,
        session: &lash_protocol_types::SessionId,
        owner: &str,
    ) -> Result<Vec<ModelCallRecord>, DurableError> {
        use lash_core_store::{checkpoint_graph::GraphDecoder, model_calls::ModelCallFact};
        use lash_durable::domain::RunFacts;
        let limits = self.pools.graph_reads.owner();
        self.read(|connection| {
            Box::pin(async move {
                let mut snapshot = super::snapshot(connection).await?;
                let rows: Vec<(String, crate::keys::Key, i64)> =
                    crate::observed_sql::query_as(SQL.read.sql())
                        .bind(session.as_str())
                        .bind(owner)
                        .fetch_all(crate::observed_sql::executor(&mut *snapshot))
                        .await
                        .map_err(sqlx_failure)?;
                let roots = rows
                    .iter()
                    .map(|(_, root, _)| super::node_ref(root))
                    .collect::<Result<Vec<_>, _>>()?;
                let nodes = super::graph::walk_within(&mut snapshot, &roots, &limits, None).await?;
                snapshot.rollback().await.map_err(sqlx_failure)?;
                let graph = RunFacts {
                    nodes,
                    read_limits: limits,
                    ..Default::default()
                };
                let mut decoder = GraphDecoder::new(&graph, limits);
                let mut records = Vec::with_capacity(rows.len());
                for ((id, _, ended), root) in rows.into_iter().zip(roots) {
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
            })
        })
        .await
    }
}

pub(super) async fn apply_pin(
    tx: &mut PgConnection,
    commit: &Committing<'_>,
    write: &lash_durable::domain::ModelCallPinWrite,
) -> Result<(), DurableError> {
    // These writes accept any row count, exactly as their explicit SQL did.
    super::steps::run(tx, &pin_steps(write), &commit.steps()).await?;
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
        session: &lash_protocol_types::SessionId,
        owner: &str,
    ) -> Result<Vec<lash_durable::domain::ModelCallPinRow>, DurableError> {
        self.read(|tx| {
            Box::pin(async move {
                let rows: Vec<StoredModelCallPin> = crate::observed_sql::query_as(SQL.pins.sql())
                    .bind(session.as_str())
                    .bind(owner)
                    .fetch_all(crate::observed_sql::executor(&mut **tx))
                    .await
                    .map_err(sqlx_failure)?;
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
