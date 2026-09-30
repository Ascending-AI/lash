//! One cross-process lock for migrations, finalize and cold finalize recovery.

use std::fs::File;
use std::time::{Duration, Instant};

use lash_core_execution::StoreError;

use crate::SqliteLocation;

/// Keep the migration lock's existing name so every upgrader coordinates.
pub(crate) const LOCK: &str = "lash-migration.lock";

pub(crate) async fn exclusive(
    location: &SqliteLocation,
    busy_timeout: Duration,
) -> Result<Option<File>, StoreError> {
    let SqliteLocation::File { root } = location else {
        return Ok(None);
    };
    let path = root.join(LOCK);
    let file = open_lock(&path)?;
    let deadline = Instant::now() + busy_timeout;
    loop {
        match file.try_lock() {
            Ok(()) => return Ok(Some(file)),
            Err(std::fs::TryLockError::WouldBlock) if Instant::now() < deadline => {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            Err(std::fs::TryLockError::WouldBlock) => {
                return Err(failure(format!(
                    "another process is migrating or finalizing the store at {}; open it again once that upgrade ends",
                    root.display()
                )));
            }
            Err(std::fs::TryLockError::Error(error)) => {
                return Err(failure(format!("lock {}: {error}", path.display())));
            }
        }
    }
}

fn failure(message: String) -> StoreError {
    StoreError::StorageFailure {
        backend: crate::SQLITE_BACKEND,
        message,
    }
}

#[expect(
    clippy::disallowed_methods,
    reason = "store upgrades lock a file under the host-supplied store root"
)]
fn open_lock(path: &std::path::Path) -> Result<File, StoreError> {
    std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(path)
        .map_err(|error| failure(format!("open {}: {error}", path.display())))
}
