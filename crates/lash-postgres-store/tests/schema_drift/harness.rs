//! Shared harness for the schema-drift proofs.
//!
//! Split out of `schema_drift.rs` so the proofs themselves stay the file: the
//! scratch-schema lifecycle and the rejection assertion are machinery every case
//! reuses, not evidence any one case carries.

use lash_postgres_store::{
    PostgresStorage, PostgresStoreConfig, SchemaCheck, SchemaFinding, SchemaProvisioning,
};
use sqlx::postgres::{PgPool, PgPoolOptions};
use sqlx::{Connection, Executor, PgConnection};

use crate::support::database_url;

/// A throwaway PostgreSQL schema, provisioned from the committed DDL artifact.
pub struct ScratchSchema {
    pub name: String,
    pub pool: PgPool,
    database_url: String,
}

impl ScratchSchema {
    /// Creates the schema and provisions it exactly as a host would: by applying
    /// [`PostgresStorage::schema_ddl`], not by letting lash open into it.
    pub async fn provision(database_url: &str) -> Self {
        let name = format!("lash_drift_{}", uuid::Uuid::new_v4().simple());
        let mut admin = PgConnection::connect(database_url)
            .await
            .expect("connect scratch admin");
        admin
            .execute(format!("CREATE SCHEMA {name}").as_str())
            .await
            .expect("create scratch schema");
        admin
            .execute(format!("SET search_path TO {name}").as_str())
            .await
            .expect("point admin search_path at the scratch schema");
        sqlx::raw_sql(PostgresStorage::schema_ddl())
            .execute(&mut admin)
            .await
            .expect("apply the committed DDL artifact");
        admin.close().await.expect("close scratch admin");
        let scratch = name.clone();
        let pool = PgPoolOptions::new()
            .max_connections(2)
            .after_connect(move |connection, _meta| {
                let scratch = scratch.clone();
                Box::pin(async move {
                    connection
                        .execute(format!("SET search_path TO {scratch}").as_str())
                        .await?;
                    Ok(())
                })
            })
            .connect(database_url)
            .await
            .expect("build scratch pool");
        Self {
            name,
            pool,
            database_url: database_url.to_string(),
        }
    }

    /// Runs host DDL or DML against the scratch schema.
    pub async fn apply(&self, statements: &str) {
        sqlx::raw_sql(statements)
            .execute(&self.pool)
            .await
            .unwrap_or_else(|error| panic!("apply scratch mutation: {error}"));
    }

    /// Opens the store the way a host with its own migrations does: no DDL, hard
    /// failure on drift.
    pub async fn open_host_provisioned(
        &self,
        check: SchemaCheck,
    ) -> Result<PostgresStorage, lash_core::StoreError> {
        PostgresStorage::from_pool_with(
            self.pool.clone(),
            PostgresStoreConfig {
                schema_provisioning: SchemaProvisioning::HostProvisioned,
                schema_check: check,
                ..PostgresStoreConfig::default()
            },
        )
        .await
    }

    /// Drops the schema. Called explicitly so a failing assertion leaves the
    /// schema behind for inspection.
    pub async fn cleanup(self) {
        let name = self.name;
        self.pool.close().await;
        let mut admin = PgConnection::connect(&self.database_url)
            .await
            .expect("connect scratch cleanup");
        admin
            .execute(format!("DROP SCHEMA {name} CASCADE").as_str())
            .await
            .expect("drop scratch schema");
    }
}

/// Rewinds a freshly provisioned schema past everything the 54 generation
/// introduced, so a fixture stamped 53 or older is the shape that generation's
/// migration is permission to transform.
///
/// The two `lash_runtime_effect_replay` columns matter as much as the table:
/// `matches_source_shape` requires the exact `MissingColumn` and
/// `MissingUniqueGuard` findings the migration declares, so a source that still
/// carries them is refused as a mismatch rather than migrated. Dropping the
/// columns takes the partial unique guard with them, and dropping the group
/// table takes its two indexes.
pub const REWIND_PAST_54_ARTIFACTS: &str = "DROP TABLE lash_runtime_effect_group;
     ALTER TABLE lash_runtime_effect_replay
         DROP COLUMN group_key,
         DROP COLUMN settlement_seq;";

/// Reads `server_version_num`, for the one assertion that needs a PostgreSQL
/// feature not present on every major in the support matrix.
pub async fn postgres_server_version_num() -> i32 {
    let database_url = database_url().expect("configured Postgres URL");
    let mut connection = PgConnection::connect(&database_url)
        .await
        .expect("connect server-version probe");
    sqlx::query_scalar::<_, String>("SELECT current_setting('server_version_num')")
        .fetch_one(&mut connection)
        .await
        .expect("read server_version_num")
        .parse()
        .expect("server_version_num is numeric")
}

/// Builds a pool whose connections use an explicit `search_path`.
pub async fn pool_with_search_path(database_url: &str, search_path: &str) -> PgPool {
    let search_path = search_path.to_string();
    PgPoolOptions::new()
        .max_connections(2)
        .after_connect(move |connection, _meta| {
            let search_path = search_path.clone();
            Box::pin(async move {
                connection
                    .execute(format!("SET search_path TO {search_path}").as_str())
                    .await?;
                Ok(())
            })
        })
        .connect(database_url)
        .await
        .expect("build search-path pool")
}

/// Applies one mutation to a freshly provisioned schema and asserts the check
/// rejects the open, names the drifted object, and reports the expected finding —
/// while `SchemaCheck::WarnOnly` opens the very same database.
pub async fn assert_mutation_is_rejected(
    mutation: &str,
    expected_message_fragments: &[&str],
    finding_matches: impl Fn(&SchemaFinding) -> bool,
) {
    let Some(database_url) = database_url() else {
        eprintln!("skipping schema drift mutation: database URL is not set");
        return;
    };
    let scratch = ScratchSchema::provision(&database_url).await;
    scratch.apply(mutation).await;

    let error = scratch
        .open_host_provisioned(SchemaCheck::Enforce)
        .await
        .err()
        .unwrap_or_else(|| panic!("a drifted schema must not open: {mutation}"));
    let rendered = error.to_string();
    for fragment in expected_message_fragments {
        assert!(
            rendered.contains(fragment),
            "the open error must name the drifted object; missing {fragment:?} after \
             `{mutation}` in:\n{rendered}"
        );
    }

    // The same drift is reachable as structured findings, which is what a host
    // gates its migration CI on.
    let warned = scratch
        .open_host_provisioned(SchemaCheck::WarnOnly)
        .await
        .unwrap_or_else(|error| {
            panic!("SchemaCheck::WarnOnly must open the drifted schema: {error}")
        });
    let report = warned.verify_schema().await.expect("verify drifted schema");
    assert!(
        !report.is_conformant(),
        "verify_schema must report the drift it warned about: {report}"
    );
    assert!(
        report.findings.iter().any(finding_matches),
        "verify_schema must report the expected finding after `{mutation}`, got {:?}",
        report.findings
    );
    scratch.cleanup().await;
}
