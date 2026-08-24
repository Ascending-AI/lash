use std::path::{Path, PathBuf};
use std::sync::Arc;

use lash_core::SessionStoreFactory as _;
use lash_sqlite_store::SqliteSessionStoreFactory;

#[tokio::test]
async fn sqlite_session_read_view_satisfies_conformance() {
    let dir = tempfile::tempdir().expect("read-view tempdir");
    lash_core::testing::conformance::session_store_factory_read_session(Arc::new(
        SqliteSessionStoreFactory::new(dir.path()),
    ))
    .await;
}

fn catalog_state(root: &Path) -> std::collections::BTreeMap<String, (u64, std::time::SystemTime)> {
    std::fs::read_dir(root)
        .expect("read catalog directory")
        .map(|entry| {
            let entry = entry.expect("read catalog entry");
            let metadata = entry.metadata().expect("read catalog metadata");
            (
                entry.file_name().to_string_lossy().into_owned(),
                (
                    metadata.len(),
                    metadata.modified().expect("catalog modified time"),
                ),
            )
        })
        .collect()
}

async fn committed_catalog(
    root: &Path,
    session_id: &str,
) -> (
    SqliteSessionStoreFactory,
    Arc<dyn lash_core::RuntimePersistence>,
    lash_core::SessionReadView,
) {
    let factory = SqliteSessionStoreFactory::new(root);
    let request = lash_core::SessionStoreCreateRequest {
        session_id: session_id.to_string(),
        relation: lash_core::SessionRelation::Root,
        policy: lash_core::SessionPolicy::new(lash_core::TurnBudget::Unbounded),
    };
    let writer = factory
        .create_store(&request)
        .await
        .expect("create no-write proof session");
    let mut state = lash_core::RuntimeSessionState {
        session_id: session_id.to_string(),
        ..lash_core::RuntimeSessionState::new(request.policy)
    };
    state.append_active_conversation_messages(&[lash_core::Message {
        id: format!("{session_id}-message"),
        role: lash_core::MessageRole::User,
        parts: vec![lash_core::Part::text(
            format!("{session_id}-message.p0"),
            "read without changing durable state".to_string(),
            None,
        )]
        .into(),
        origin: None,
    }]);
    writer
        .commit_runtime_state(lash_core::RuntimeCommit::persisted_state_for_test(
            &state,
            &[],
        ))
        .await
        .expect("commit no-write proof session");
    let expected = lash_core::store::load_persisted_session_state(writer.as_ref())
        .await
        .expect("reload committed no-write proof session")
        .expect("committed no-write proof session exists")
        .read_view();
    (factory, writer, expected)
}

async fn wait_for_cold_catalog(root: &Path) {
    let database = root.join("durable-core.db");
    let wal = PathBuf::from(format!("{}-wal", database.display()));
    let shm = PathBuf::from(format!("{}-shm", database.display()));
    for _ in 0..100 {
        if !wal.exists() && !shm.exists() {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    panic!("writer close must leave a cold catalog without WAL sidecars");
}

#[tokio::test]
async fn sqlite_session_read_view_does_not_change_live_catalog_files() {
    const SESSION_ID: &str = "sqlite-read-only-no-write";
    let dir = tempfile::tempdir().expect("read-only proof tempdir");
    let (factory, _writer, _expected) = committed_catalog(dir.path(), SESSION_ID).await;

    let before = catalog_state(dir.path());
    let view = factory
        .open_read_only(SESSION_ID)
        .await
        .expect("open mode=ro session")
        .expect("committed session has a read view");
    assert_eq!(view.messages().len(), 1);
    let after = catalog_state(dir.path());
    assert_eq!(
        after, before,
        "mode=ro projection must not modify files while a writer owns the WAL sidecars"
    );
}

#[tokio::test]
async fn sqlite_session_read_view_materializes_sidecars_for_a_cold_wal_catalog() {
    const SESSION_ID: &str = "sqlite-read-only-cold-wal";
    let dir = tempfile::tempdir().expect("cold WAL proof tempdir");
    let (factory, writer, expected) = committed_catalog(dir.path(), SESSION_ID).await;
    drop(writer);
    wait_for_cold_catalog(dir.path()).await;

    let database = factory.catalog_path();
    let wal = PathBuf::from(format!("{}-wal", database.display()));
    let shm = PathBuf::from(format!("{}-shm", database.display()));
    let database_before = std::fs::read(&database).expect("read cold database bytes");
    let modified_before = database
        .metadata()
        .expect("read cold database metadata")
        .modified()
        .expect("read cold database modified time");

    let actual = factory
        .open_read_only(SESSION_ID)
        .await
        .expect("read cold mode=ro catalog")
        .expect("cold catalog contains the committed session");

    assert_eq!(
        serde_json::to_value(actual.to_snapshot()).expect("serialize actual session view"),
        serde_json::to_value(expected.to_snapshot()).expect("serialize expected session view"),
        "read-only projection must not change session-visible state"
    );
    assert_eq!(
        std::fs::read(&database).expect("reread cold database bytes"),
        database_before,
        "read-only projection must not change the database bytes"
    );
    assert_eq!(
        database
            .metadata()
            .expect("reread cold database metadata")
            .modified()
            .expect("reread cold database modified time"),
        modified_before,
        "read-only projection must not change the database mtime"
    );
    assert_eq!(
        wal.metadata().expect("cold read materializes WAL").len(),
        0,
        "cold read leaves an empty WAL sidecar"
    );
    assert_eq!(
        shm.metadata().expect("cold read materializes SHM").len(),
        32 * 1024,
        "cold read materializes one 32 KiB wal-index region"
    );
}

#[cfg(unix)]
struct PermissionRestore {
    path: PathBuf,
    permissions: std::fs::Permissions,
}

#[cfg(unix)]
impl PermissionRestore {
    fn set(path: impl Into<PathBuf>, mode: u32) -> Self {
        use std::os::unix::fs::PermissionsExt as _;

        let path = path.into();
        let permissions = path
            .metadata()
            .expect("read original permissions")
            .permissions();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode))
            .expect("set read-only permissions");
        Self { path, permissions }
    }
}

#[cfg(unix)]
impl Drop for PermissionRestore {
    fn drop(&mut self) {
        std::fs::set_permissions(&self.path, self.permissions.clone())
            .expect("restore catalog permissions");
    }
}

#[cfg(unix)]
#[tokio::test]
async fn sqlite_session_read_view_reports_cold_read_only_media_as_backend_error() {
    const SESSION_ID: &str = "sqlite-read-only-media";
    let dir = tempfile::tempdir().expect("read-only media proof tempdir");
    let (factory, writer, _expected) = committed_catalog(dir.path(), SESSION_ID).await;
    drop(writer);
    wait_for_cold_catalog(dir.path()).await;

    let _database_permissions = PermissionRestore::set(factory.catalog_path(), 0o444);
    let _directory_permissions = PermissionRestore::set(dir.path(), 0o555);
    let error = factory
        .open_read_only(SESSION_ID)
        .await
        .expect_err("cold WAL catalog on read-only media must fail");
    match error {
        lash_core::StoreError::Backend(message) => assert!(
            message.to_ascii_lowercase().contains("readonly"),
            "SQLite read-only-media failure must retain its backend reason: {message}"
        ),
        other => panic!("read-only-media failure must be a backend error, got {other:?}"),
    }
}
