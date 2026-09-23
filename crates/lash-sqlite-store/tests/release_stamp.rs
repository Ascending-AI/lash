//! SQLite under the shared release-stamp law.

#![expect(
    clippy::expect_used,
    reason = "test target: clippy's allow-unwrap-in-tests only exempts #[test] functions, and the setup helpers around them in this target are test code too"
)]

use std::path::PathBuf;

use async_trait::async_trait;
use lash_conformance::{ReleaseStampDeployment, release_stamp_conformance};
use lash_core_execution::{StoreError, StorePreflight, StoreSchemaStatus};
use lash_sqlite_store::{SqliteStorePreflight, Store};

struct SqliteDeployment {
    _root: tempfile::TempDir,
    durable_core: PathBuf,
}

#[async_trait]
impl ReleaseStampDeployment for SqliteDeployment {
    fn build_release(&self) -> String {
        // The same injection the store crate stamps with: `main` builds carry
        // `0.0.0-dev` and the release workflow stamps the real version, so this
        // tracks whichever the test binary was built from.
        env!("CARGO_PKG_VERSION").to_string()
    }

    async fn open(&self) -> Result<(), StoreError> {
        Store::open(&self.durable_core)
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
    let deployment = SqliteDeployment {
        _root: root,
        durable_core,
    };
    release_stamp_conformance(&deployment).await;
}

/// The refusal at open names the writing release beside the schema integers.
///
/// This is the whole point of the stamp: before it, a host that upgraded into a
/// refusal learned two integers and had to work backwards to a release. The
/// pinned sentences are unchanged — the release rides after them.
#[tokio::test]
async fn a_refused_open_names_the_release_that_wrote_the_store() {
    let root = tempfile::tempdir().expect("scratch directory");
    let path = root.path().join("durable-core.db");
    drop(Store::open(&path).await.expect("provision and stamp"));

    // Roll the recorded schema version back to a generation this build refuses,
    // leaving the stamp in place: exactly the shape a host upgrading across a
    // bump presents.
    let connection = rusqlite::Connection::open(&path).expect("open the stamped database");
    connection
        .pragma_update(None, "user_version", 41)
        .expect("stamp an unsupported generation");
    drop(connection);

    let error = Store::open(&path)
        .await
        .err()
        .expect("an unsupported generation is refused")
        .to_string();
    assert!(
        error.contains(&format!(
            "This store was last written by lash release {}.",
            env!("CARGO_PKG_VERSION")
        )),
        "the refusal must name the writing release: {error}"
    );
    assert!(
        error.contains("supports schema version")
            && error.contains("see docs/adr/0049-session-ids-are-used-once.md."),
        "the pinned refusal sentences must survive verbatim: {error}"
    );
}

/// A store no stamping build has written is refused with the message unchanged.
#[tokio::test]
async fn an_unstamped_store_is_refused_without_inventing_a_release() {
    let root = tempfile::tempdir().expect("scratch directory");
    let path = root.path().join("durable-core.db");
    drop(Store::open(&path).await.expect("provision and stamp"));

    let connection = rusqlite::Connection::open(&path).expect("open the stamped database");
    connection
        .execute("DROP TABLE release_stamp", [])
        .expect("remove the stamp a pre-stamp build never wrote");
    connection
        .pragma_update(None, "user_version", 41)
        .expect("stamp an unsupported generation");
    drop(connection);

    let error = Store::open(&path)
        .await
        .err()
        .expect("an unsupported generation is refused")
        .to_string();
    assert!(
        !error.contains("last written by lash release"),
        "a store with no stamp must not have a release invented for it: {error}"
    );
}
