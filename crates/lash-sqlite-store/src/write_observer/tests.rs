use super::*;

/// TRAFFIC-CONSERVATION: two updates to one physical page count twice,
/// checkpoint bytes remain separate, and no incomplete cut can certify.
#[test]
#[expect(
    clippy::disallowed_methods,
    reason = "isolates the process-global write observer from other store laws"
)]
fn traffic_conservation_rewrites_checkpoint_and_incomplete_boundaries() {
    const LAW: &str =
        "write_observer::tests::traffic_conservation_rewrites_checkpoint_and_incomplete_boundaries";
    const CHILD: &str = "LASH_TRAFFIC_CONSERVATION_CHILD";
    if std::env::var_os(CHILD).is_none() {
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", LAW, "--nocapture"])
            .env(CHILD, "1")
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(String::from_utf8_lossy(&output.stdout).contains("1 passed; 0 failed"));
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let observer = Collector::install().unwrap();
    let connection = open(dir.path().join("writes.db"), rusqlite::OpenFlags::default()).unwrap();
    connection.execute_batch("PRAGMA journal_mode=WAL; PRAGMA wal_autocheckpoint=0; CREATE TABLE sample (value TEXT); INSERT INTO sample VALUES ('first'); PRAGMA wal_checkpoint(TRUNCATE);").unwrap();
    let page_size: u64 = connection
        .query_row("PRAGMA page_size", [], |r| r.get(0))
        .unwrap();
    let root: u32 = connection
        .query_row(
            "SELECT rootpage FROM sqlite_schema WHERE name='sample'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    crate::observed_sql::enable(&connection);
    let witness = lash_core_execution::perf_witness::Collector::install().unwrap();
    let (before_work, before) = observer.cut(|| witness.snapshot());
    for (origin, value) in [
        ("sample.update.first", "second"),
        ("sample.update.second", "third"),
    ] {
        scope(origin, || {
            let writes = crate::observed_sql::WriteWitness::start(&connection);
            connection
                .execute("UPDATE sample SET value=?1", [value])
                .unwrap();
            assert!(
                !observer.snapshot().complete,
                "a boundary inside an operation must refuse"
            );
            writes.finish(&connection);
        });
    }
    let (after_work, after) = observer.cut(|| witness.snapshot());
    let updates = after.difference(&before).unwrap();
    assert!(
        updates.conserves(
            (after_work.store_pages_written - before_work.store_pages_written) * page_size
        )
    );
    assert_eq!(updates.bytes(WriteKind::WalBody), 2 * page_size);
    let bodies: Vec<_> = updates
        .rows
        .iter()
        .filter(|r| r.key.kind == WriteKind::WalBody)
        .collect();
    assert_eq!(bodies.len(), 2);
    assert!(bodies.iter().all(|r| r.key.page == Some(root)
        && r.bytes == page_size
        && r.key.origin.starts_with("sample.update.")));
    assert_eq!(updates.bytes(WriteKind::WalHeader), 32);
    assert_eq!(updates.bytes(WriteKind::WalFrameHeader), 48);
    assert_eq!(updates.bytes(WriteKind::Checkpoint), 0);
    scope("sample.checkpoint", || {
        connection
            .execute_batch("PRAGMA wal_checkpoint(TRUNCATE)")
            .unwrap();
    });
    let checkpoint = observer.snapshot().difference(&after).unwrap();
    assert!(checkpoint.conserves(0));
    assert!(checkpoint.bytes(WriteKind::Checkpoint) >= page_size);
    assert!(
        checkpoint
            .rows
            .iter()
            .all(|r| r.key.origin == "sample.checkpoint")
    );
    assert_eq!(
        connection
            .query_row("SELECT value FROM sample", [], |r| r.get::<_, String>(0))
            .unwrap(),
        "third"
    );
    // A bounded collector retains the residual and refuses it, rather than
    // silently dropping a new page/origin after reaching the row capacity.
    let complete = observer.snapshot();
    {
        let mut state = observer.state.lock().unwrap();
        state.limit = state.rows.len();
    }
    scope("sample.overflow", || {
        connection
            .execute("UPDATE sample SET value='fourth'", [])
            .unwrap();
    });
    let overflow = observer.snapshot();
    assert!(overflow.overflow_bytes > 0);
    assert!(!overflow.complete);
    assert!(overflow.difference(&complete).is_none());
    assert_eq!(
        overflow.total_bytes,
        overflow.rows.iter().map(|r| r.bytes).sum::<u64>()
            + overflow.overflow_bytes
            + overflow.unknown_bytes
    );
    let mut unknown = complete.clone();
    unknown.complete = false;
    unknown.unknown_bytes = 1;
    assert!(unknown.difference(&complete).is_none());
    assert!(!unknown.conserves(0));
}
