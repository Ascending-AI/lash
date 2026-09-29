//! SQLite under the shared release-stamp law.

#![expect(
    clippy::expect_used,
    reason = "test target: clippy's allow-unwrap-in-tests only exempts #[test] functions, and the setup helpers around them in this target are test code too"
)]

use std::path::PathBuf;

use async_trait::async_trait;
use lash_conformance::{ReleaseStampDeployment, release_stamp_conformance};
use lash_core_execution::compat::{CompatRefusal, VersionRange};
use lash_core_execution::{
    StoreError, StorePreflight, StoreReleaseState, StoreSchemaStatus, StoreSchemaVerdict,
};
use lash_sqlite_store::{SqliteStore, SqliteStorePreflight};

struct SqliteBackend {
    _root: tempfile::TempDir,
    durable_core: PathBuf,
}

#[async_trait]
impl ReleaseStampDeployment for SqliteBackend {
    fn build_release(&self) -> String {
        // The same injection the store crate stamps with: `main` builds carry
        // `0.0.0-dev` and the release workflow stamps the real version, so this
        // tracks whichever the test binary was built from.
        env!("CARGO_PKG_VERSION").to_string()
    }

    async fn open(&self) -> Result<(), StoreError> {
        SqliteStore::open_file_for_testing(&self.durable_core)
            .await
            .map(|_store| ())
            .map_err(|err| StoreError::Backend(err.to_string()))
    }

    async fn preflight(&self) -> Result<StoreSchemaStatus, StoreError> {
        SqliteStorePreflight::for_durable_core(&self.durable_core)
            .schema_status()
            .await
    }

    async fn force_release(&self, release: &str) -> Result<(), StoreError> {
        let conn = rusqlite::Connection::open(&self.durable_core)
            .expect("open the stamped database directly");
        let changed = conn
            .execute(
                "UPDATE release_stamp SET release_version = ?1 WHERE singleton = 1",
                [release],
            )
            .expect("rewrite the stored release");
        assert_eq!(changed, 1, "the store must already carry a stamp to force");
        Ok(())
    }
}

#[tokio::test]
async fn sqlite_release_stamp_conformance() {
    let root = tempfile::tempdir().expect("scratch directory");
    let durable_core = root.path().join("durable-core.db");
    let backend = SqliteBackend {
        _root: root,
        durable_core,
    };
    release_stamp_conformance(&backend).await;
}

/// A refused open leaves the writing release available to preflight.
#[tokio::test]
async fn a_refused_open_names_the_release_that_wrote_the_store() {
    let root = tempfile::tempdir().expect("scratch directory");
    let path = root.path().join("durable-core.db");
    drop(
        SqliteStore::open_file_for_testing(&path)
            .await
            .expect("provision and stamp"),
    );

    let connection = rusqlite::Connection::open(&path).expect("open the stamped database");
    connection
        .execute("UPDATE lash_compat SET version = 2, min_reader = 2", [])
        .expect("raise the reader floor above this build");
    drop(connection);

    let status = SqliteStorePreflight::for_durable_core(&path)
        .schema_status()
        .await
        .expect("inspect refused store");
    assert_eq!(
        status.databases[0].verdict,
        StoreSchemaVerdict::Refused {
            refusal: CompatRefusal::ReaderFloorAbove {
                component: "sqlite-core".to_owned(),
                found: 2,
                min_reader: 2,
                reads: VersionRange::exactly(1),
                writing_release: None,
            },
        }
    );
    assert!(
        matches!(status.release, StoreReleaseState::Stamped(stamp) if stamp.release == env!("CARGO_PKG_VERSION"))
    );
    assert!(SqliteStore::open_file_for_testing(&path).await.is_err());
}

/// A missing release stamp stays missing when compatibility admission refuses.
#[tokio::test]
async fn an_unstamped_store_is_refused_without_inventing_a_release() {
    let root = tempfile::tempdir().expect("scratch directory");
    let path = root.path().join("durable-core.db");
    drop(
        SqliteStore::open_file_for_testing(&path)
            .await
            .expect("provision and stamp"),
    );

    let connection = rusqlite::Connection::open(&path).expect("open the stamped database");
    connection
        .execute("DROP TABLE release_stamp", [])
        .expect("remove the stamp a pre-stamp build never wrote");
    connection
        .execute("UPDATE lash_compat SET version = 2, min_reader = 2", [])
        .expect("raise the reader floor above this build");
    drop(connection);

    let status = SqliteStorePreflight::for_durable_core(&path)
        .schema_status()
        .await
        .expect("inspect refused store");
    assert_eq!(
        status.databases[0].verdict,
        StoreSchemaVerdict::Refused {
            refusal: CompatRefusal::ReaderFloorAbove {
                component: "sqlite-core".to_owned(),
                found: 2,
                min_reader: 2,
                reads: VersionRange::exactly(1),
                writing_release: None,
            },
        }
    );
    assert_eq!(status.release, StoreReleaseState::Unstamped);
    assert!(SqliteStore::open_file_for_testing(&path).await.is_err());
}
