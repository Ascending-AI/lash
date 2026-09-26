//! SQLite under the shared fleet-format law, plus the writer-stamp proof the
//! backend carries on its own: a durable writer emits the version the fleet
//! format assigns, not the build constant it would stamp anyway (FIG-3796).

use std::path::PathBuf;

use async_trait::async_trait;
use lash_conformance::{FleetFormatDeployment, fleet_format_conformance};
use lash_core_execution::{
    FLEET_FORMAT_VERSION, FleetFormat, FleetFormatStore, StoreError, StorePreflight,
    StoreSchemaStatus, WriterPin,
};
use lash_sqlite_store::{SqliteStorePreflight, Store};

struct SqliteBackend {
    _root: tempfile::TempDir,
    durable_core: PathBuf,
}

#[async_trait]
impl FleetFormatDeployment for SqliteBackend {
    async fn open(&self) -> Result<FleetFormat, StoreError> {
        Store::open(&self.durable_core)
            .await
            .map(|store| store.fleet_format())
            .map_err(|err| StoreError::Backend(err.to_string()))
    }

    async fn open_admitting(
        &self,
        writable: std::ops::RangeInclusive<u32>,
    ) -> Result<FleetFormat, StoreError> {
        Store::open_with_fleet_writable_range_for_testing(&self.durable_core, writable)
            .await
            .map(|store| store.fleet_format())
    }

    async fn record_fleet_format(&self, version: u32) -> Result<(), StoreError> {
        let conn = rusqlite::Connection::open(&self.durable_core)
            .map_err(|error| StoreError::Backend(error.to_string()))?;
        conn.execute(
            "UPDATE fleet_format SET format_version = ?1 WHERE singleton = 1",
            rusqlite::params![i64::from(version)],
        )
        .map_err(|error| StoreError::Backend(error.to_string()))?;
        Ok(())
    }

    async fn preflight(&self) -> Result<StoreSchemaStatus, StoreError> {
        SqliteStorePreflight::for_durable_core(&self.durable_core)
            .schema_status()
            .await
    }
}

#[tokio::test]
async fn sqlite_fleet_format_conformance() {
    let root = tempfile::tempdir().expect("scratch directory");
    let durable_core = root.path().join("durable-core.db");
    let backend = SqliteBackend {
        _root: root,
        durable_core,
    };
    fleet_format_conformance(&backend).await;
}

/// A durable writer stamps the version `F` assigns: stood up on a fleet
/// format whose pin table holds `CURRENT_SESSION_STATE_VERSION` at 7, the
/// session-meta writer records 7, not the constant it would stamp anyway.
#[tokio::test]
async fn sqlite_session_meta_stamps_the_version_the_fleet_format_selects() {
    let root = tempfile::tempdir().expect("scratch directory");
    let durable_core = root.path().join("durable-core.db");
    let fleet = FleetFormat::current().with_writer_pins(&[WriterPin {
        constant: "CURRENT_SESSION_STATE_VERSION",
        generation: FLEET_FORMAT_VERSION,
        version: 7,
    }]);
    let store = Store::open(&durable_core)
        .await
        .expect("open")
        .with_fleet_format_for_testing(fleet);
    let session_id = lash_core::SessionId::from("fleet-stamped-session");
    store
        .save_session_meta(lash_core::SessionMeta {
            session_id: session_id.clone(),
            relation: lash_core::SessionRelation::Root,
            pending_observer_intents: Vec::new(),
        })
        .await
        .expect("save session meta");
    drop(store);

    let conn = rusqlite::Connection::open(&durable_core).expect("raw read of the stamp");
    let stamped: i64 = conn
        .query_row(
            "SELECT session_state_version FROM session_meta WHERE session_id = ?1",
            rusqlite::params![session_id.as_str()],
            |row| row.get(0),
        )
        .expect("read the stamped session-state version");
    assert_eq!(
        stamped, 7,
        "the writer must stamp the version F assigns, not the build constant"
    );
}
