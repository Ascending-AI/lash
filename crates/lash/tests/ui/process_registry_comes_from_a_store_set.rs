use std::path::Path;
use std::sync::Arc;

// A SQLite process registry comes only from its store set: a standalone one
// has no trigger store attached, so it cannot check a delivery's start against
// the delivery's binding (FIG-4369).
async fn standalone(path: &Path, clock: Arc<dyn lash::runtime::Clock>) {
    let _ = lash_sqlite_store::SqliteProcessRegistry::open(path).await;
    let _ = lash_sqlite_store::SqliteProcessRegistry::open_with_clock(path, clock).await;
}

fn main() {}
