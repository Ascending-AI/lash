//! The SQLite build-generation drain (FIG-3799).
//!
//! The marks live in the process registry file beside the processes they
//! move; the parked turns a generation still holds are counted in the durable
//! core. Each read is its own statement: a work count is a status read, not
//! one snapshot across the two files.

use std::num::NonZeroUsize;
use std::sync::LazyLock;

use lash_core_execution::ProcessId;
use lash_core_execution::engine::BuildGeneration;
use lash_core_execution::store::generation_drain::{
    DrainingGeneration, GenerationDrainStore, GenerationWork,
};
use lash_store_sql::draining_generations::DrainingGenerationStatements;

use crate::conn::SqliteConnection;
use crate::schema_layout::Schema;
use crate::{StoreError, sqlite_error, stored_data_corrupt};

static SQL: LazyLock<DrainingGenerationStatements> =
    LazyLock::new(|| DrainingGenerationStatements::render(Schema::Main.dialect()));

fn millis(field: &'static str, value: u64) -> Result<i64, StoreError> {
    i64::try_from(value)
        .map_err(|_| StoreError::Backend(format!("{field} {value} exceeds the stored range")))
}

fn count(value: i64) -> u64 {
    u64::try_from(value).unwrap_or(0)
}

/// The drain over one store set: its process registry and its durable core.
#[derive(Clone)]
pub(crate) struct SqliteGenerationDrain {
    registry: SqliteConnection,
    core: SqliteConnection,
}

impl SqliteGenerationDrain {
    pub(crate) fn new(registry: SqliteConnection, core: SqliteConnection) -> Self {
        Self { registry, core }
    }
}

#[async_trait::async_trait]
impl GenerationDrainStore for SqliteGenerationDrain {
    async fn mark_draining(
        &self,
        generation: &BuildGeneration,
        now_ms: u64,
    ) -> Result<bool, StoreError> {
        let generation = generation.as_str().to_owned();
        let now = millis("drain mark instant", now_ms)?;
        self.registry
            .write(move |tx| {
                Ok(tx.execute(SQL.mark.sql(), rusqlite::params![generation, now])? == 1)
            })
            .await
            .map_err(sqlite_error)
    }

    async fn clear_draining(&self, generation: &BuildGeneration) -> Result<bool, StoreError> {
        let generation = generation.as_str().to_owned();
        self.registry
            .write(move |tx| Ok(tx.execute(SQL.clear.sql(), rusqlite::params![generation])? == 1))
            .await
            .map_err(sqlite_error)
    }

    async fn draining_generations(&self) -> Result<Vec<DrainingGeneration>, StoreError> {
        let rows = self
            .registry
            .call(|conn| {
                let mut statement = conn.prepare_cached(SQL.select_all.sql())?;
                statement
                    .query_map([], |row| {
                        Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
                    })?
                    .collect::<rusqlite::Result<Vec<_>>>()
            })
            .await
            .map_err(sqlite_error)?;
        rows.into_iter()
            .map(|(generation, marked_at_ms)| {
                Ok(DrainingGeneration {
                    generation: BuildGeneration::parse(&generation)
                        .map_err(|error| stored_data_corrupt("DrainingGeneration", error))?,
                    marked_at_ms: count(marked_at_ms),
                })
            })
            .collect()
    }

    async fn generation_work(
        &self,
        generation: &BuildGeneration,
    ) -> Result<GenerationWork, StoreError> {
        let stamp = generation.as_str().to_owned();
        let (live_processes, parked_processes) = self
            .registry
            .read(move |tx| {
                let process = &crate::process_registry::sql::process_sql().process;
                let live: i64 = tx.query_row(
                    process.count_live_by_segment_generation.sql(),
                    rusqlite::params![stamp],
                    |row| row.get(0),
                )?;
                let parked: i64 = tx.query_row(
                    process.count_parked_by_build_generation.sql(),
                    rusqlite::params![stamp],
                    |row| row.get(0),
                )?;
                Ok((live, parked))
            })
            .await
            .map_err(sqlite_error)?;
        let stamp = generation.as_str().to_owned();
        let parked_turns: i64 = self
            .core
            .call(move |conn| {
                conn.query_row(
                    crate::turn_ingress::turn_ingress_sql()
                        .turn_parks
                        .count_by_build_generation
                        .sql(),
                    rusqlite::params![stamp],
                    |row| row.get(0),
                )
            })
            .await
            .map_err(sqlite_error)?;
        Ok(GenerationWork {
            live_processes: count(live_processes),
            parked_processes: count(parked_processes),
            parked_turns: count(parked_turns),
        })
    }

    async fn live_processes(
        &self,
        generation: &BuildGeneration,
        after: Option<&ProcessId>,
        limit: NonZeroUsize,
    ) -> Result<Vec<ProcessId>, StoreError> {
        let stamp = generation.as_str().to_owned();
        let after = after.map(ProcessId::to_string).unwrap_or_default();
        let limit = i64::try_from(limit.get()).unwrap_or(i64::MAX);
        let ids = self
            .registry
            .call(move |conn| {
                let mut statement = conn.prepare_cached(
                    crate::process_registry::sql::process_sql()
                        .process
                        .list_live_by_segment_generation
                        .sql(),
                )?;
                statement
                    .query_map(rusqlite::params![stamp, after, limit], |row| {
                        row.get::<_, String>(0)
                    })?
                    .collect::<rusqlite::Result<Vec<_>>>()
            })
            .await
            .map_err(sqlite_error)?;
        ids.into_iter()
            .map(|id| {
                ProcessId::parse(&id).map_err(|error| stored_data_corrupt("ProcessId", error))
            })
            .collect()
    }
}
