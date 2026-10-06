//! The harness's own test database: SQLite in memory on the virtual clock.

use crate::clock::SimClock;
use lash_durable::{ActorKey, DurableStore};
use lash_sansio::sync::MutexExt as _;
use std::sync::{Arc, Mutex};

pub(crate) const FORMATS: &str = "t-v1";

pub(crate) async fn sqlite(clock: Arc<SimClock>) -> Arc<dyn DurableStore> {
    let stores = lash_sqlite_store::SqliteStoreSet::memory_with_clock(clock)
        .await
        .expect("an in-memory store set opens");
    Arc::new(stores.durable_store())
}

/// A fresh SQLite file store set on `clock`, under a directory that `dirs`
/// keeps until the test ends.
pub(crate) async fn sqlite_file(
    clock: Arc<SimClock>,
    dirs: &Mutex<Vec<tempfile::TempDir>>,
) -> Arc<dyn DurableStore> {
    let dir = tempfile::tempdir().expect("a temporary directory");
    let stores = lash_sqlite_store::SqliteStoreSet::open_with_clock(dir.path(), clock)
        .await
        .expect("a file store set opens");
    dirs.lock_recover().push(dir);
    Arc::new(stores.durable_store())
}

pub(crate) fn actor(id: &str) -> ActorKey {
    ActorKey::session(id).expect("test actor ids are valid")
}
