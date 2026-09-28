//! The PostgreSQL build-generation drain (FIG-3799).
//!
//! The marks, the processes and the parked turns share one catalog; each
//! count is its own indexed statement.

use std::num::NonZeroUsize;
use std::sync::LazyLock;

use lash_core_execution::ProcessId;
use lash_core_execution::engine::BuildGeneration;
use lash_core_execution::store::generation_drain::{
    DrainingGeneration, GenerationDrainStore, GenerationWork,
};
use lash_store_sql::Dialect;
use lash_store_sql::draining_generations::DrainingGenerationStatements;
use sqlx::{PgPool, Row};

use crate::StoreError;
use crate::support::store_sqlx_error;

static SQL: LazyLock<DrainingGenerationStatements> =
    LazyLock::new(|| DrainingGenerationStatements::render(Dialect::postgres()));

fn millis(field: &'static str, value: u64) -> Result<i64, StoreError> {
    i64::try_from(value)
        .map_err(|_| StoreError::Backend(format!("{field} {value} exceeds the stored range")))
}

fn count(value: i64) -> u64 {
    u64::try_from(value).unwrap_or(0)
}

fn corrupt(record_kind: &'static str, error: impl std::fmt::Display) -> StoreError {
    StoreError::StoredDataCorrupt {
        record_kind,
        message: error.to_string(),
    }
}

/// The drain over one PostgreSQL catalog.
#[derive(Clone)]
pub(crate) struct PostgresGenerationDrain {
    pool: PgPool,
}

impl PostgresGenerationDrain {
    pub(crate) fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    async fn count_of(&self, sql: &str, generation: &BuildGeneration) -> Result<u64, StoreError> {
        let counted: i64 = sqlx::query_scalar(sql)
            .bind(generation.as_str())
            .fetch_one(&self.pool)
            .await
            .map_err(store_sqlx_error)?;
        Ok(count(counted))
    }
}

#[async_trait::async_trait]
impl GenerationDrainStore for PostgresGenerationDrain {
    async fn mark_draining(
        &self,
        generation: &BuildGeneration,
        now_ms: u64,
    ) -> Result<bool, StoreError> {
        let marked = sqlx::query(SQL.mark.sql())
            .bind(generation.as_str())
            .bind(millis("drain mark instant", now_ms)?)
            .execute(&self.pool)
            .await
            .map_err(store_sqlx_error)?;
        Ok(marked.rows_affected() == 1)
    }

    async fn clear_draining(&self, generation: &BuildGeneration) -> Result<bool, StoreError> {
        let cleared = sqlx::query(SQL.clear.sql())
            .bind(generation.as_str())
            .execute(&self.pool)
            .await
            .map_err(store_sqlx_error)?;
        Ok(cleared.rows_affected() == 1)
    }

    async fn draining_generations(&self) -> Result<Vec<DrainingGeneration>, StoreError> {
        let rows = sqlx::query(SQL.select_all.sql())
            .fetch_all(&self.pool)
            .await
            .map_err(store_sqlx_error)?;
        rows.iter()
            .map(|row| {
                let generation: String = row.try_get(0).map_err(store_sqlx_error)?;
                let marked_at_ms: i64 = row.try_get(1).map_err(store_sqlx_error)?;
                Ok(DrainingGeneration {
                    generation: BuildGeneration::parse(&generation)
                        .map_err(|error| corrupt("DrainingGeneration", error))?,
                    marked_at_ms: count(marked_at_ms),
                })
            })
            .collect()
    }

    async fn generation_work(
        &self,
        generation: &BuildGeneration,
    ) -> Result<GenerationWork, StoreError> {
        let process = &crate::process_sql::process_sql().process;
        Ok(GenerationWork {
            live_processes: self
                .count_of(process.count_live_by_segment_generation.sql(), generation)
                .await?,
            parked_processes: self
                .count_of(process.count_parked_by_build_generation.sql(), generation)
                .await?,
            parked_turns: self
                .count_of(
                    crate::turn_ingress::turn_ingress_sql()
                        .turn_parks
                        .count_by_build_generation
                        .sql(),
                    generation,
                )
                .await?,
            in_flight_turns: self
                .count_of(
                    crate::session_roots::session_roots_sql()
                        .roots
                        .count_unfinished_by_admitted_generation
                        .sql(),
                    generation,
                )
                .await?,
        })
    }

    async fn live_processes(
        &self,
        generation: &BuildGeneration,
        after: Option<&ProcessId>,
        limit: NonZeroUsize,
    ) -> Result<Vec<ProcessId>, StoreError> {
        let ids: Vec<String> = sqlx::query_scalar(
            crate::process_sql::process_sql()
                .process
                .list_live_by_segment_generation
                .sql(),
        )
        .bind(generation.as_str())
        .bind(after.map(ProcessId::as_str).unwrap_or_default())
        .bind(i64::try_from(limit.get()).unwrap_or(i64::MAX))
        .fetch_all(&self.pool)
        .await
        .map_err(store_sqlx_error)?;
        ids.iter()
            .map(|id| ProcessId::parse(id).map_err(|error| corrupt("ProcessId", error)))
            .collect()
    }
}
