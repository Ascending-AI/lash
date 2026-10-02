//! Test-only fixtures for running Postgres suites in parallel.
//!
//! The workspace shares one configured Postgres database
//! (`LASH_POSTGRES_DATABASE_URL`). Suites that truncate or enumerate every
//! `lash_*` table therefore observe rows written by any concurrently running
//! suite, which under a process-per-test runner shows up as rotating,
//! irreproducible failures. Suites that hold the shared advisory lock take
//! turns; suites that do not need their own database instead of a turn.
//!
//! [`IsolatedDatabase`] gives a suite a uniquely named, freshly created
//! database derived from the configured URL, and drops it on teardown.
//! Creation applies this build's committed `schema.sql` artifact into the new
//! database — the same provisioning `lash migrate` performs — so a following
//! [`PostgresStorage`](crate::PostgresStorage) open verifies a schema it did
//! not create (FIG-3797).

use sqlx::{Connection, PgConnection};

#[path = "testing/database_url.rs"]
mod database_url;
pub use database_url::required_database_url;

/// Returns the production trigger-subscription listing SQL for conformance assertions.
///
/// The filter no longer builds the statement; it selects one (FIG-3385). The
/// text is the named statement its shape is served by, which is what the
/// listing actually issues.
pub fn trigger_subscription_list_sql(
    filter: &lash_core_execution::TriggerSubscriptionFilter,
) -> String {
    crate::trigger_store::subscription_list_sql(filter).to_string()
}

/// One stored value: where it is, its bytes, and every JSON document those
/// bytes decode to as this store writes them. The twin of
/// `lash_sqlite_store::testing::StoredCell`.
#[derive(Clone, Debug)]
pub struct StoredCell {
    /// `<table>.<column>#<row>`, or `schema/<table>.<column>` for a column's
    /// own declaration.
    pub location: String,
    pub bytes: Vec<u8>,
    /// The value as JSON text (a `json`/`jsonb` column's included) or as a
    /// msgpack record; empty for a scalar.
    pub documents: Vec<serde_json::Value>,
}

/// Every column declaration and every non-null cell of every table in the
/// storage's schema, with the documents each decodes to.
///
/// An inspection hook for simulation checkers that audit what a finished
/// run persisted (lash-sim's crash-matrix catalog audit, FIG-4179). It never
/// writes, and no lash component reads through it.
pub async fn read_stored_cells_for_testing(
    storage: &crate::PostgresStorage,
) -> Result<Vec<StoredCell>, String> {
    use sqlx::Row as _;
    let columns = sqlx::query(
        "SELECT table_name::text AS table_name, column_name::text AS column_name, \
                data_type::text AS data_type \
         FROM information_schema.columns \
         WHERE table_schema = current_schema() \
         ORDER BY table_name, ordinal_position",
    )
    .fetch_all(storage.pool())
    .await
    .map_err(|error| format!("list the columns: {error}"))?;
    let mut cells = Vec::new();
    for column in columns {
        let table: String = column.get("table_name");
        let name: String = column.get("column_name");
        let data_type: String = column.get("data_type");
        cells.push(StoredCell {
            location: format!("schema/{table}.{name}"),
            bytes: format!("{table}.{name}").into_bytes(),
            documents: Vec::new(),
        });
        let values: Vec<Vec<u8>> = if data_type == "bytea" {
            sqlx::query_scalar(&format!(
                "SELECT \"{name}\" FROM \"{table}\" WHERE \"{name}\" IS NOT NULL"
            ))
            .fetch_all(storage.pool())
            .await
        } else {
            sqlx::query_scalar::<_, String>(&format!(
                "SELECT \"{name}\"::text FROM \"{table}\" WHERE \"{name}\" IS NOT NULL"
            ))
            .fetch_all(storage.pool())
            .await
            .map(|values| values.into_iter().map(String::into_bytes).collect())
        }
        .map_err(|error| format!("read `{table}.{name}`: {error}"))?;
        for (index, bytes) in values.into_iter().enumerate() {
            let structured =
                |value: serde_json::Value| (value.is_object() || value.is_array()).then_some(value);
            let documents = serde_json::from_slice(&bytes)
                .ok()
                .and_then(structured)
                .or_else(|| {
                    rmp_serde::from_slice::<serde_json::Value>(&bytes)
                        .ok()
                        .and_then(structured)
                })
                .into_iter()
                .collect();
            cells.push(StoredCell {
                location: format!("{table}.{name}#{index}"),
                bytes,
                documents,
            });
        }
    }
    Ok(cells)
}

/// The `AfterFence` seam (ADR 0115 §6): pauses a guarded transaction right
/// after its writer fence, while it holds the fleet-format row `FOR SHARE`.
///
/// Install it on a storage with
/// [`PostgresStorage::with_after_fence_for_testing`](crate::PostgresStorage::with_after_fence_for_testing);
/// every handle that storage hands out passes it. Each [`Self::pause_next`]
/// arms one pause, taken by the next transaction whose fence admits `F`, in
/// arming order. It also counts fences that met lock contention, so a test can
/// tell a retried fence from a first one.
#[derive(Clone, Debug, Default)]
pub struct AfterFence {
    state: std::sync::Arc<std::sync::Mutex<AfterFenceState>>,
}

#[derive(Debug, Default)]
struct AfterFenceState {
    armed: std::collections::VecDeque<ArmedPause>,
    passed: Vec<u32>,
    contended: u64,
}

#[derive(Debug)]
struct ArmedPause {
    reached: tokio::sync::oneshot::Sender<u32>,
    gate: std::sync::Arc<lash_core_execution::testing::Gate>,
}

/// One armed pause: the transaction that takes it waits after its fence
/// until [`Self::release`] (or until this handle is dropped).
#[derive(Debug)]
pub struct FencePause {
    reached: Option<tokio::sync::oneshot::Receiver<u32>>,
    gate: std::sync::Arc<lash_core_execution::testing::Gate>,
}

impl AfterFence {
    pub fn new() -> Self {
        Self::default()
    }

    /// Pause the next guarded transaction that passes its fence.
    pub fn pause_next(&self) -> FencePause {
        let (reached_tx, reached_rx) = tokio::sync::oneshot::channel();
        let gate = std::sync::Arc::new(lash_core_execution::testing::Gate::new(
            "postgres writer fence",
        ));
        self.lock_state().armed.push_back(ArmedPause {
            reached: reached_tx,
            gate: std::sync::Arc::clone(&gate),
        });
        FencePause {
            reached: Some(reached_rx),
            gate,
        }
    }

    /// The epoch each fence that admitted its transaction read, in order.
    pub fn passed(&self) -> Vec<u32> {
        self.lock_state().passed.clone()
    }

    /// How many fences failed on lock contention.
    pub fn contended(&self) -> u64 {
        self.lock_state().contended
    }

    pub(crate) fn record_contended(&self) {
        self.lock_state().contended += 1;
    }

    /// Called by a fence that admitted `recorded`: takes the next armed pause,
    /// if any, and waits for its release.
    pub(crate) async fn pass(&self, recorded: u32) {
        let armed = {
            let mut state = self.lock_state();
            state.passed.push(recorded);
            state.armed.pop_front()
        };
        if let Some(armed) = armed {
            let _ = armed.reached.send(recorded);
            armed.gate.pass().await;
        }
    }

    fn lock_state(&self) -> std::sync::MutexGuard<'_, AfterFenceState> {
        use lash_sansio::sync::MutexExt;
        self.state.lock_recover()
    }
}

impl FencePause {
    /// Wait until a transaction has taken this pause; answers the `F` its
    /// fence read.
    ///
    /// # Panics
    ///
    /// Panics when called twice, or when the seam was dropped unreached.
    #[expect(
        clippy::expect_used,
        reason = "test-harness helper: a pause awaited twice or never reachable is a test-authoring fault"
    )]
    pub async fn reached(&mut self) -> u32 {
        self.gate.reached(1).await;
        self.reached
            .take()
            .expect("a pause is awaited once")
            .await
            .expect("the seam holding this pause was dropped before a fence reached it")
    }

    /// Let the paused transaction continue.
    pub fn release(self) {
        self.gate.open_all();
    }
}

impl Drop for FencePause {
    fn drop(&mut self) {
        self.gate.open_all();
    }
}

/// Finalize's side of the fence, for tests that race it (ADR 0115 §2.2):
/// the production flip — the fleet-format row read `FOR UPDATE` and moved to
/// `epoch` — held open until [`HeldFinalize::commit`]. It skips the drain,
/// the retirement check and the hold, which a race does not exercise; the
/// operator's finalize is [`PostgresStorage::finalize`](crate::PostgresStorage::finalize).
pub struct HeldFinalize {
    flip: crate::finalize::PendingFlip,
    fence: crate::guarded_tx::WriterFence,
}

impl HeldFinalize {
    /// Begin finalize as a build whose writable range is `[1, epoch]`: waits
    /// behind every writer holding the row `FOR SHARE`.
    pub async fn begin(
        pool: &sqlx::PgPool,
        epoch: u32,
    ) -> Result<Self, lash_core_execution::StoreError> {
        use lash_core_execution::StoreError;
        use lash_core_execution::store::fleet_finalize::{FinalizeError, FinalizeMode};
        let writable = lash_core_execution::compat::VersionRange::new(1, epoch)
            .map_err(|error| StoreError::Backend(error.to_string()))?;
        let fence = crate::guarded_tx::WriterFence::new(
            writable,
            lash_core_execution::FleetFormat::from_version(epoch),
        );
        let flip =
            crate::finalize::begin_flip(pool, &fence, epoch, FinalizeMode::OverrideHold, &[])
                .await
                .map_err(|error| match error {
                    FinalizeError::Store(error) => error,
                    other => StoreError::Backend(other.to_string()),
                })?;
        Ok(Self { flip, fence })
    }

    /// Commit the move.
    pub async fn commit(self) -> Result<(), lash_core_execution::StoreError> {
        self.flip.commit(&self.fence).await.map(|_| ())
    }
}

/// Finalize `F` to `epoch` in one transaction (see [`HeldFinalize`]).
pub async fn finalize_fleet_epoch(
    pool: &sqlx::PgPool,
    epoch: u32,
) -> Result<(), lash_core_execution::StoreError> {
    HeldFinalize::begin(pool, epoch).await?.commit().await
}

/// A throwaway Postgres database, created for one test and dropped with it.
///
/// Construction connects to the maintenance database named in the base URL,
/// issues `CREATE DATABASE`, and hands back a URL pointing at the new
/// database. `Drop` issues `DROP DATABASE ... WITH (FORCE)`, so a test that
/// leaves pooled connections open — or panics — still cleans up.
#[derive(Debug)]
pub struct IsolatedDatabase {
    maintenance_url: String,
    database_name: String,
    url: String,
}

impl IsolatedDatabase {
    /// # Panics
    ///
    /// Panics when the base URL cannot be parsed or the database cannot be
    /// created; both are test-configuration faults with no useful recovery.
    #[expect(
        clippy::expect_used,
        reason = "test-harness helper: a base URL that will not parse or a database that will not create is a test-configuration fault with no useful recovery, as the doc comment above states"
    )]
    pub async fn create(base_url: &str) -> Self {
        let database_name = format!("lash_test_{}", uuid::Uuid::new_v4().simple());
        let url = replace_database_name(base_url, &database_name);
        let mut connection = PgConnection::connect(base_url)
            .await
            .expect("connect Postgres maintenance database for test isolation");
        // Identifiers are generated here, never caller-supplied, so the quoted
        // interpolation cannot carry an injection; `CREATE DATABASE` also
        // refuses to run as a bound-parameter statement. On PostgreSQL 15+ the
        // FILE_COPY strategy copies template1's files instead of WAL-logging
        // every copied block; the harness only ever clones template1, so the
        // semantics a test observes are unchanged (FIG-4721).
        let server_version_num: i32 =
            sqlx::query_scalar("SELECT current_setting('server_version_num')::int")
                .fetch_one(&mut connection)
                .await
                .expect("read the test server's version");
        let strategy = if server_version_num >= 150000 {
            " WITH STRATEGY = FILE_COPY"
        } else {
            ""
        };
        sqlx::query(&format!("CREATE DATABASE \"{database_name}\"{strategy}"))
            .execute(&mut connection)
            .await
            .unwrap_or_else(|error| {
                panic!("create isolated test database {database_name}: {error}")
            });
        connection
            .close()
            .await
            .expect("close Postgres maintenance connection");
        let isolated = Self {
            maintenance_url: base_url.to_string(),
            database_name,
            url,
        };
        // Open never provisions: worker startup runs no DDL (FIG-3797), so the
        // harness applies the committed artifact itself — the same job `lash
        // migrate` does for a deployment.
        let mut connection = PgConnection::connect(&isolated.url)
            .await
            .expect("connect isolated database for provisioning");
        sqlx::raw_sql(crate::schema::SCHEMA_DDL)
            .execute(&mut connection)
            .await
            .expect("provision isolated test database from schema.sql");
        connection
            .close()
            .await
            .expect("close isolated provisioning connection");
        isolated
    }

    /// The connection URL for the isolated database.
    pub fn url(&self) -> &str {
        &self.url
    }

    /// The generated database name, for diagnostics.
    pub fn database_name(&self) -> &str {
        &self.database_name
    }
}

impl Drop for IsolatedDatabase {
    fn drop(&mut self) {
        let maintenance_url = self.maintenance_url.clone();
        let database_name = self.database_name.clone();
        // Teardown is synchronous so the database is gone before the test
        // process exits, and runs on its own thread + runtime so it works from
        // inside any async context, including a current-thread runtime where
        // blocking on the ambient runtime would panic.
        let dropped = std::thread::spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .map_err(|error| error.to_string())?;
            runtime.block_on(async move {
                let mut connection = PgConnection::connect(&maintenance_url)
                    .await
                    .map_err(|error| error.to_string())?;
                sqlx::query(&format!(
                    "DROP DATABASE IF EXISTS \"{database_name}\" WITH (FORCE)"
                ))
                .execute(&mut connection)
                .await
                .map(|_| ())
                .map_err(|error| error.to_string())
            })
        })
        .join();
        match dropped {
            Ok(Ok(())) => {}
            Ok(Err(error)) => {
                eprintln!(
                    "warning: could not drop isolated test database {}: {error}",
                    self.database_name
                );
            }
            Err(_) => {
                eprintln!(
                    "warning: teardown thread panicked dropping isolated test database {}",
                    self.database_name
                );
            }
        }
    }
}

/// Rewrites the database component of a Postgres connection URL.
///
/// Kept string-level rather than URL-parsing so query parameters, credentials,
/// and non-standard hosts survive verbatim.
fn replace_database_name(base_url: &str, database_name: &str) -> String {
    let (before_query, query) = match base_url.find('?') {
        Some(index) => (&base_url[..index], &base_url[index..]),
        None => (base_url, ""),
    };
    let scheme_end = before_query
        .find("://")
        .map(|index| index + 3)
        .unwrap_or_else(|| panic!("Postgres URL {base_url} has no scheme"));
    let authority_end = before_query[scheme_end..]
        .find('/')
        .map(|index| scheme_end + index)
        .unwrap_or(before_query.len());
    format!("{}/{database_name}{query}", &before_query[..authority_end])
}

#[cfg(test)]
mod tests {
    use super::replace_database_name;

    #[test]
    fn replaces_database_component_and_keeps_credentials_and_query() {
        assert_eq!(
            replace_database_name("postgres://lash:lash@localhost:5432/lash", "lash_test_1"),
            "postgres://lash:lash@localhost:5432/lash_test_1"
        );
        assert_eq!(
            replace_database_name(
                "postgres://lash:lash@localhost:5432/lash?sslmode=disable",
                "lash_test_1"
            ),
            "postgres://lash:lash@localhost:5432/lash_test_1?sslmode=disable"
        );
        assert_eq!(
            replace_database_name("postgres://localhost", "lash_test_1"),
            "postgres://localhost/lash_test_1"
        );
    }
}

#[cfg(test)]
mod fence_pause_tests {
    use super::AfterFence;
    use std::time::Duration;

    #[tokio::test(start_paused = true)]
    #[should_panic(expected = "gate `postgres writer fence`: 0 of 1 arrivals within 10s")]
    async fn an_unreached_writer_pause_fails_with_its_label() {
        let seam = AfterFence::new();
        let mut pause = seam.pause_next();
        tokio::time::timeout(Duration::from_secs(11), pause.reached())
            .await
            .expect("the writer pause left its wait unbounded");
    }

    #[tokio::test(start_paused = true)]
    async fn a_writer_pause_keeps_its_epoch_and_drop_releases_the_writer() {
        let seam = AfterFence::new();
        let mut pause = seam.pause_next();
        let mut writer = std::pin::pin!(seam.pass(7));
        tokio::select! {
            biased;
            () = &mut writer => panic!("the writer passed its closed pause"),
            epoch = pause.reached() => assert_eq!(epoch, 7),
        }
        assert!(
            tokio::time::timeout(Duration::from_secs(1), &mut writer)
                .await
                .is_err()
        );
        drop(pause);
        tokio::time::timeout(Duration::from_secs(1), writer)
            .await
            .expect("dropping the pause releases its writer");
        assert_eq!(seam.passed(), vec![7]);
    }
}
