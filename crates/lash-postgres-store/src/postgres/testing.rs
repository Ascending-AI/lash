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

/// Deterministic PostgreSQL substrate fault injection, the twin of
/// `lash_sqlite_store::testing`'s injector.
#[cfg(feature = "testing")]
pub use crate::fault_injection::{
    PostgresFaultArm, PostgresFaultInjector, PostgresFaultObservation, PostgresFaultPoint,
};

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
    release: tokio::sync::oneshot::Receiver<()>,
}

/// One armed pause: the transaction that takes it waits after its fence
/// until [`Self::release`] (or until this handle is dropped).
#[derive(Debug)]
pub struct FencePause {
    reached: Option<tokio::sync::oneshot::Receiver<u32>>,
    release: Option<tokio::sync::oneshot::Sender<()>>,
}

impl AfterFence {
    pub fn new() -> Self {
        Self::default()
    }

    /// Pause the next guarded transaction that passes its fence.
    pub fn pause_next(&self) -> FencePause {
        let (reached_tx, reached_rx) = tokio::sync::oneshot::channel();
        let (release_tx, release_rx) = tokio::sync::oneshot::channel();
        self.lock_state().armed.push_back(ArmedPause {
            reached: reached_tx,
            release: release_rx,
        });
        FencePause {
            reached: Some(reached_rx),
            release: Some(release_tx),
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
            let _ = armed.release.await;
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
        self.reached
            .take()
            .expect("a pause is awaited once")
            .await
            .expect("the seam holding this pause was dropped before a fence reached it")
    }

    /// Let the paused transaction continue.
    pub fn release(mut self) {
        if let Some(release) = self.release.take() {
            let _ = release.send(());
        }
    }
}

/// Finalize's side of the fence, for tests that race it (ADR 0115 §2.2):
/// read the fleet-format row `FOR UPDATE`, move it to `epoch`, and hold the
/// transaction open until [`HeldFinalize::commit`].
///
/// FIG-3800 B ships the operator's finalize; until then this is the one
/// place that moves `F` the way finalize will.
pub struct HeldFinalize {
    tx: sqlx::Transaction<'static, sqlx::Postgres>,
}

impl HeldFinalize {
    /// Begin finalize: waits behind every writer holding the row `FOR SHARE`.
    pub async fn begin(pool: &sqlx::PgPool, epoch: u32) -> Result<Self, sqlx::Error> {
        let mut tx = pool.begin().await?;
        sqlx::query("SELECT format_version FROM lash_fleet_format WHERE singleton FOR UPDATE")
            .execute(&mut *tx)
            .await?;
        sqlx::query("UPDATE lash_fleet_format SET format_version = $1 WHERE singleton")
            .bind(i32::try_from(epoch).unwrap_or(i32::MAX))
            .execute(&mut *tx)
            .await?;
        Ok(Self { tx })
    }

    /// Commit the move.
    pub async fn commit(self) -> Result<(), sqlx::Error> {
        self.tx.commit().await
    }
}

/// Finalize `F` to `epoch` in one transaction (see [`HeldFinalize`]).
pub async fn finalize_fleet_epoch(pool: &sqlx::PgPool, epoch: u32) -> Result<(), sqlx::Error> {
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
        // refuses to run as a bound-parameter statement.
        sqlx::query(&format!("CREATE DATABASE \"{database_name}\""))
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
