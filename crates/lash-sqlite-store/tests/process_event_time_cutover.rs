use lash_sqlite_store::SqliteProcessRegistry;

const RETAINED_PRIOR_PROCESS_GENERATION: i32 = 27;
const CURRENT_PROCESS_GENERATION: i32 = 28;

#[tokio::test]
async fn sqlite_process_registry_refuses_the_immediate_predecessor_at_open() {
    assert_eq!(
        RETAINED_PRIOR_PROCESS_GENERATION + 1,
        CURRENT_PROCESS_GENERATION
    );
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("pre-epoch-ms-processes.db");
    drop(
        SqliteProcessRegistry::open(&path, dir.path().join("sessions"))
            .await
            .expect("create current process registry"),
    );
    rusqlite::Connection::open(&path)
        .expect("open process registry for rewind")
        .pragma_update(None, "user_version", RETAINED_PRIOR_PROCESS_GENERATION)
        .expect("stamp retained process-registry predecessor");

    let error = SqliteProcessRegistry::open(&path, dir.path().join("sessions"))
        .await
        .err()
        .expect("the retained process-registry predecessor must be refused")
        .to_string();
    assert!(
        error.contains(&format!(
            "supports schema version {CURRENT_PROCESS_GENERATION}"
        )) && error.contains(&format!(
            "database reports version {RETAINED_PRIOR_PROCESS_GENERATION}"
        )),
        "the predecessor refusal must identify expected and found versions: {error}"
    );
}
