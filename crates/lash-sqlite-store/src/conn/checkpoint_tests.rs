use std::time::Duration;

use crate::conn::tests::installed;
use crate::conn::{SqliteConnectionPolicy, SqliteSynchronous};
use crate::location::DatabaseTarget;

/// Checkpoints follow write volume, not wall time (FIG-6907): commits that
/// leave the WAL under its threshold never wake the worker, however long
/// they sit, and no commit checkpoints on the writer's thread; once a commit
/// leaves the threshold in the WAL, the worker brings it back under the
/// threshold without another write. A worker woken by every commit, or by a
/// timer, checkpoints the small commits.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[expect(
    clippy::disallowed_methods,
    reason = "test reads the WAL index's frame count"
)]
async fn the_wal_is_checkpointed_by_volume_off_the_committing_thread() {
    let dir = tempfile::tempdir().expect("checkpoint volume tempdir");
    let path = dir.path().join("core.db");
    let target = DatabaseTarget::File(path.clone());
    let mut policy = SqliteConnectionPolicy {
        wal_autocheckpoint_pages: 64,
        ..SqliteConnectionPolicy::standard(SqliteSynchronous::Normal)
    };
    policy.operational.wal_retained_bytes = 2 * 1024 * 1024;
    let interval = policy.operational.checkpoint_interval;
    let writer = installed(&target, policy).await;
    let worker = super::state_for_testing(&target).expect("the file's checkpoint worker");
    assert_eq!(worker.threshold_for_testing(), 64);
    writer
        .write(|tx| tx.execute_batch("CREATE TABLE rows (body BLOB NOT NULL)"))
        .await
        .expect("create the table");
    for _ in 0..8 {
        writer
            .write(|tx| tx.execute("INSERT INTO rows VALUES (x'07')", []).map(drop))
            .await
            .expect("commit a small row");
    }
    tokio::time::sleep(interval * 4).await;
    assert_eq!(
        worker.runs.load(std::sync::atomic::Ordering::Relaxed),
        0,
        "commits under the threshold never checkpoint"
    );
    let autocheckpoint: i64 = writer
        .call(|c| c.query_row("PRAGMA wal_autocheckpoint", [], |row| row.get(0)))
        .await
        .expect("read the writer's autocheckpoint");
    assert_eq!(autocheckpoint, 0, "no commit checkpoints on the writer");

    writer
        .write(|tx| {
            tx.execute("INSERT INTO rows VALUES (?1)", [vec![7_u8; 1024 * 1024]])
                .map(drop)
        })
        .await
        .expect("one commit exceeds the checkpoint threshold");
    // The WAL index header's `mxFrame` (bytes 16..20 of the `-shm` file, in
    // native byte order) counts committed frames. nBackfill (96..100)
    // reaches it once every committed frame is checkpointed, even when
    // RESTART retains the allocation and no later write resets mxFrame.
    let mut shm = path.into_os_string();
    shm.push("-shm");
    let shm = std::path::PathBuf::from(shm);
    let checkpointed = || {
        std::fs::read(&shm).ok().is_some_and(|index| {
            let frames = u32::from_ne_bytes(index[16..20].try_into().unwrap());
            let backfilled = u32::from_ne_bytes(index[96..100].try_into().unwrap());
            frames == backfilled
        })
    };
    tokio::time::timeout(Duration::from_secs(3), async {
        while !checkpointed() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("the threshold commit is checkpointed without another write");
    drop(
        worker
            .connection
            .lock()
            .expect("checkpoint attempt completed"),
    );
    let mut wal_name = target.file_path().unwrap().as_os_str().to_os_string();
    wal_name.push("-wal");
    let wal_path = std::path::PathBuf::from(wal_name);
    let retained = std::fs::metadata(&wal_path).unwrap().len();
    assert!(
        retained > 0,
        "WAL-BOUNDED-REUSE retains checkpointed allocation for reuse"
    );
    assert!(retained <= policy.operational.wal_retained_bytes);
    assert!(worker.runs.load(std::sync::atomic::Ordering::Relaxed) >= 1);
    writer
        .write(|tx| {
            tx.execute(
                "INSERT INTO rows VALUES (?1)",
                [vec![8_u8; 4 * 1024 * 1024]],
            )
            .map(drop)
        })
        .await
        .expect("a commit exceeds the retained allocation bound");
    tokio::time::timeout(Duration::from_secs(3), async {
        while !checkpointed() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("the oversized WAL is checkpointed without another write");
    drop(
        worker
            .connection
            .lock()
            .expect("checkpoint reset completed"),
    );
    assert_eq!(
        std::fs::metadata(&wal_path).unwrap().len(),
        0,
        "WAL-BOUNDED-REUSE truncates allocation above the host bound"
    );
}
