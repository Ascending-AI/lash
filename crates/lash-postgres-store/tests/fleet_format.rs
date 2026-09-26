//! PostgreSQL under the shared fleet-format law, plus the proofs the backend
//! carries on its own: a durable writer emits the version the fleet format
//! assigns rather than the build constant (FIG-3796), and a role holding
//! nothing but `SELECT` can read `F` while a write is refused.

use std::str::FromStr;

use async_trait::async_trait;
use lash_conformance::{FleetFormatDeployment, fleet_format_conformance};
use lash_core_execution::{
    FLEET_FORMAT_VERSION, FleetFormat, StoreError, StorePreflight, StoreSchemaStatus, WriterPin,
};
use lash_postgres_store::{
    PostgresStorage, PostgresStoreConfig, PostgresStorePreflight, SchemaCheck,
};
use sqlx::postgres::{PgConnectOptions, PgPool, PgPoolOptions};
use sqlx::{Connection, Executor, PgConnection};

use crate::harness::ScratchSchema;
use crate::support::database_url;

struct PostgresDeployment {
    scratch: ScratchSchema,
}

#[async_trait]
impl FleetFormatDeployment for PostgresDeployment {
    async fn open(&self) -> Result<FleetFormat, StoreError> {
        self.scratch
            .open_host_provisioned(SchemaCheck::Enforce)
            .await
            .map(|storage| storage.fleet_format())
    }

    async fn open_admitting(
        &self,
        writable: std::ops::RangeInclusive<u32>,
    ) -> Result<FleetFormat, StoreError> {
        PostgresStorage::from_pool_with_fleet_writable_range_for_testing(
            self.scratch.pool.clone(),
            PostgresStoreConfig {
                schema_check: SchemaCheck::Enforce,
                ..PostgresStoreConfig::default()
            },
            writable,
        )
        .await
        .map(|storage| storage.fleet_format())
    }

    async fn record_fleet_format(&self, version: u32) -> Result<(), StoreError> {
        sqlx::query("UPDATE lash_fleet_format SET format_version = $1 WHERE singleton = TRUE")
            .bind(i32::try_from(version).unwrap_or(i32::MAX))
            .execute(&self.scratch.pool)
            .await
            .map_err(|error| StoreError::Backend(error.to_string()))?;
        Ok(())
    }

    async fn preflight(&self) -> Result<StoreSchemaStatus, StoreError> {
        PostgresStorePreflight::from_pool(self.scratch.pool.clone())
            .schema_status()
            .await
    }
}

#[tokio::test]
async fn postgres_fleet_format_conformance() {
    let Some(database_url) = database_url() else {
        eprintln!("skipping fleet format conformance: database URL is not set");
        return;
    };
    // The committed DDL creates `lash_fleet_format` and seeds no row, so a
    // host-provisioned schema starts unrecorded — the law's first
    // precondition.
    let deployment = PostgresDeployment {
        scratch: ScratchSchema::provision(&database_url).await,
    };
    fleet_format_conformance(&deployment).await;
}

/// A durable writer stamps the version `F` assigns: stood up on a fleet
/// format whose pin table holds `CURRENT_SESSION_STATE_VERSION` at 7, the
/// session-meta writer records 7, not the constant it would stamp anyway.
#[tokio::test]
async fn postgres_session_meta_stamps_the_version_the_fleet_format_selects() {
    let Some(database_url) = database_url() else {
        eprintln!("skipping fleet format writer proof: database URL is not set");
        return;
    };
    let scratch = ScratchSchema::provision(&database_url).await;
    let storage = scratch
        .open_host_provisioned(SchemaCheck::Enforce)
        .await
        .expect("open the host-provisioned schema");
    let session_id = lash_core::SessionId::from("fleet-stamped-session");
    let fleet = FleetFormat::current().with_writer_pins(&[WriterPin {
        constant: "CURRENT_SESSION_STATE_VERSION",
        generation: FLEET_FORMAT_VERSION,
        version: 7,
    }]);
    let store = storage
        .session_store(session_id.clone())
        .with_fleet_format_for_testing(fleet);
    lash_core::SessionCommitStore::save_session_meta(
        &store,
        lash_core::SessionMeta {
            session_id: session_id.clone(),
            relation: lash_core::SessionRelation::Root,
            pending_observer_intents: Vec::new(),
        },
    )
    .await
    .expect("save session meta");

    let stamped: i32 = sqlx::query_scalar(
        "SELECT session_state_version FROM lash_session_meta WHERE session_id = $1",
    )
    .bind(session_id.as_str())
    .fetch_one(&scratch.pool)
    .await
    .expect("read the stamped session-state version");
    assert_eq!(
        stamped, 7,
        "the writer must stamp the version F assigns, not the build constant"
    );
    scratch.cleanup().await;
}

/// A role holding `SELECT` and nothing else opens the store — `F` is a row
/// the fleet reads at open — and a write attempt is refused as a typed
/// `StoreError`, never silently applied.
#[tokio::test]
async fn a_select_only_role_reads_the_fleet_format_and_is_refused_on_write() {
    let Some(database_url) = database_url() else {
        eprintln!("skipping select-only fleet format proof: database URL is not set");
        return;
    };
    let scratch = ScratchSchema::provision(&database_url).await;
    // Record `F` once through an ordinary writable open so the read arm has a
    // row to read.
    scratch
        .open_host_provisioned(SchemaCheck::Enforce)
        .await
        .expect("first open records the fleet format");

    let role = format!(
        "lash_ro_{}",
        &uuid::Uuid::new_v4().simple().to_string()[..24]
    );
    let password = uuid::Uuid::new_v4().to_string();
    let mut admin = PgConnection::connect(&database_url)
        .await
        .expect("connect role admin");
    admin
        .execute(format!("CREATE ROLE \"{role}\" LOGIN PASSWORD '{password}'").as_str())
        .await
        .expect("create the select-only role");
    for grant in [
        format!("GRANT USAGE ON SCHEMA {} TO \"{role}\"", scratch.name),
        format!(
            "GRANT SELECT ON ALL TABLES IN SCHEMA {} TO \"{role}\"",
            scratch.name
        ),
    ] {
        admin
            .execute(grant.as_str())
            .await
            .expect("grant select-only privileges");
    }

    let options = PgConnectOptions::from_str(&database_url)
        .expect("parse the host database URL")
        .username(&role)
        .password(&password);
    let search_path = scratch.name.clone();
    let reader_pool: PgPool = PgPoolOptions::new()
        .max_connections(1)
        .after_connect(move |connection, _meta| {
            let search_path = search_path.clone();
            Box::pin(async move {
                connection
                    .execute(format!("SET search_path TO {search_path}").as_str())
                    .await?;
                Ok(())
            })
        })
        .connect_with(options)
        .await
        .expect("build the select-only pool");

    let storage = PostgresStorage::from_pool_with(
        reader_pool.clone(),
        PostgresStoreConfig {
            schema_check: SchemaCheck::Enforce,
            ..PostgresStoreConfig::default()
        },
    )
    .await
    .expect("a select-only role opens: the fleet format is a row it can read");
    assert_eq!(
        storage.fleet_format(),
        FleetFormat::current(),
        "the select-only open reads the recorded fleet format"
    );

    // A write attempt is refused as a typed `StoreError`: the row-level DML
    // the writer needs is a privilege the role does not hold.
    let session_id = lash_core::SessionId::from("select-only-session");
    let store = storage.session_store(session_id.clone());
    let error = lash_core::SessionCommitStore::save_session_meta(
        &store,
        lash_core::SessionMeta {
            session_id,
            relation: lash_core::SessionRelation::Root,
            pending_observer_intents: Vec::new(),
        },
    )
    .await
    .expect_err("a select-only role must not write session rows");
    assert!(
        matches!(error, StoreError::StorageFailure { .. }),
        "expected the typed storage refusal, got: {error}"
    );
    let rendered = error.to_string();
    assert!(
        rendered.contains("permission denied")
            || rendered.contains("insufficient_privilege")
            || rendered.contains("42501"),
        "the refusal must carry the privilege denial: {rendered}"
    );

    reader_pool.close().await;
    scratch.cleanup().await;
    admin
        .execute(format!("DROP ROLE \"{role}\"").as_str())
        .await
        .expect("drop the select-only role");
    admin.close().await.expect("close role admin");
}
