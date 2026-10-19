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
    let policy = SqliteConnectionPolicy {
        wal_autocheckpoint_pages: 64,
        ..SqliteConnectionPolicy::standard(SqliteSynchronous::Normal)
    };
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

    for _ in 0..40 {
        writer
            .write(|tx| {
                tx.execute("INSERT INTO rows VALUES (?1)", [vec![7_u8; 8192]])
                    .map(drop)
            })
            .await
            .expect("commit a large row");
    }
    // The WAL index header's `mxFrame` (bytes 16..20 of the `-shm` file, in
    // native byte order) counts the frames the WAL holds: the burst wrote
    // well past the threshold; once idle, a checkpoint has brought it under,
    // whether it truncated the WAL or the writer restarted it.
    let mut shm = path.into_os_string();
    shm.push("-shm");
    let shm = std::path::PathBuf::from(shm);
    let frames = || {
        std::fs::read(&shm)
            .ok()
            .and_then(|index| index.get(16..20).map(<[u8]>::to_vec))
            .and_then(|bytes| bytes.try_into().ok())
            .map(u32::from_ne_bytes)
    };
    tokio::time::timeout(Duration::from_secs(3), async {
        while !frames().is_some_and(|frames| frames < 64) {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("a WAL past its threshold is checkpointed without another write");
    assert!(worker.runs.load(std::sync::atomic::Ordering::Relaxed) >= 1);
}
