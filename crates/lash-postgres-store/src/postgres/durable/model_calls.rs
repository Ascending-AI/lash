//! PostgreSQL adapter for sealed model-call records.
use super::{Committing, DurableError, sqlx_failure};
use lash_core_store::model_calls::ModelCallRecord;
use lash_durable::domain::ModelCallWrite;
use lash_store_sql::{Dialect, durable::model_calls::ModelCallStatements};
use sqlx::PgConnection;
use std::sync::LazyLock;

pub(super) static SQL: LazyLock<ModelCallStatements> =
    LazyLock::new(|| ModelCallStatements::render(Dialect::postgres()));
pub(super) async fn apply(
    tx: &mut PgConnection,
    commit: &Committing<'_>,
    write: &ModelCallWrite,
) -> Result<(), DurableError> {
    crate::observed_sql::query(SQL.append.sql())
        .bind(write.key.session.as_str())
        .bind(write.key.call.owner_column())
        .bind(write.key.call.call_column())
        .bind(&write.call_id.0)
        .bind(write.record.to_hex())
        .bind(commit.now.0)
        .bind(commit.epoch.0)
        .execute(crate::observed_sql::executor(&mut *tx))
        .await
        .map_err(sqlx_failure)?;
    let retained: String = crate::observed_sql::query_scalar(SQL.root.sql())
        .bind(write.key.session.as_str())
        .bind(write.key.call.owner_column())
        .bind(write.key.call.call_column())
        .fetch_one(crate::observed_sql::executor(&mut *tx))
        .await
        .map_err(sqlx_failure)?;
    if retained != write.record.to_hex() {
        return Err(super::corrupt(
            "model call",
            "a sealed call cannot be replaced",
        ));
    }
    crate::observed_sql::query(SQL.retire_pin.sql())
        .bind(write.key.session.as_str())
        .bind(write.key.call.owner_column())
        .bind(write.key.call.call_column())
        .execute(crate::observed_sql::executor(tx))
        .await
        .map_err(sqlx_failure)?;
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
    crate::observed_sql::query_scalar(SQL.delete_session.sql())
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
                let rows: Vec<(String, String, i64)> =
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
    write: &lash_durable::domain::ModelCallPinWrite,
) -> Result<(), DurableError> {
    use lash_durable::domain::ModelCallPinWrite;
    match write {
        ModelCallPinWrite::Reserve {
            key,
            call_id,
            attempt,
            enclosing,
        } => {
            crate::observed_sql::query(SQL.reserve.sql())
                .bind(key.session.as_str())
                .bind(key.call.owner_column())
                .bind(key.call.call_column())
                .bind(&call_id.0)
                .bind(i64::from(*attempt))
                .bind(enclosing.as_ref().map(|owner| owner.owner.stored()))
                .bind(
                    enclosing
                        .as_ref()
                        .map(|owner| i64::try_from(owner.run.0).unwrap_or(i64::MAX)),
                )
                .bind(enclosing.as_ref().map(|owner| owner.call.as_str()))
                .execute(crate::observed_sql::executor(tx))
                .await
                .map_err(sqlx_failure)?;
        }
        ModelCallPinWrite::Answered { key } => {
            crate::observed_sql::query(SQL.answered.sql())
                .bind(key.session.as_str())
                .bind(key.call.owner_column())
                .bind(key.call.call_column())
                .execute(crate::observed_sql::executor(tx))
                .await
                .map_err(sqlx_failure)?;
        }
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
