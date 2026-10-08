//! One cross-process lock for migrations and finalize.

use std::fs::File;
use std::time::{Duration, Instant};

use lash_core_execution::StoreError;

use crate::SqliteLocation;

/// The lock file's suffix: it sits beside the database file, named after it
/// the way SQLite names its `-wal` and `-shm` files.
pub(crate) const LOCK_SUFFIX: &str = "-migration.lock";

/// The lock file of the database at `path`.
pub(crate) fn lock_path(path: &std::path::Path) -> std::path::PathBuf {
    let mut name = path.as_os_str().to_owned();
    name.push(LOCK_SUFFIX);
    std::path::PathBuf::from(name)
}

pub(crate) async fn exclusive(
    location: &SqliteLocation,
    busy_timeout: Duration,
    poll: Duration,
) -> Result<Option<File>, StoreError> {
    let SqliteLocation::File { path: database } = location else {
        return Ok(None);
    };
    let path = lock_path(database);
    let file = open_lock(&path)?;
    let deadline = Instant::now() + busy_timeout;
    loop {
        match file.try_lock() {
            Ok(()) => return Ok(Some(file)),
            Err(std::fs::TryLockError::WouldBlock) if Instant::now() < deadline => {
                tokio::time::sleep(poll).await;
            }
            Err(std::fs::TryLockError::WouldBlock) => {
                return Err(failure(format!(
                    "another process is migrating or finalizing the store at {}; open it again once that upgrade ends",
                    database.display()
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
    reason = "store upgrades lock a file beside the host-supplied database file"
)]
fn open_lock(path: &std::path::Path) -> Result<File, StoreError> {
    std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(path)
        .map_err(|error| failure(format!("open {}: {error}", path.display())))
}
