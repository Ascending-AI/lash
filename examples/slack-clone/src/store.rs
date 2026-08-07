//! A small async wrapper over a blocking `rusqlite` connection.
//!
//! Both binaries keep durable state in SQLite — the platform its workspace, the
//! bot its event ledger — and both drive it from async request handlers. One
//! serialized connection behind `spawn_blocking` is the honest shape for an
//! example: it is obviously correct, it never blocks a runtime worker, and it
//! leaves no room for a connection-pool bug to be mistaken for a Lash bug.

use std::path::Path;
use std::sync::{Arc, Mutex};

use anyhow::{Context as _, Result};
use rusqlite::Connection;

/// A cloneable handle to one SQLite connection.
#[derive(Clone)]
pub struct SqliteHandle {
    connection: Arc<Mutex<Connection>>,
}

impl SqliteHandle {
    /// Open (creating parent directories) and apply `schema`.
    ///
    /// `schema` must be idempotent — `CREATE TABLE IF NOT EXISTS` throughout —
    /// because it runs on every boot. An example that needs a migration tool to
    /// restart is an example nobody restarts.
    pub fn open(path: &Path, schema: &str) -> Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("create sqlite directory {}", parent.display()))?;
        }
        let connection = Connection::open(path)
            .with_context(|| format!("open sqlite database {}", path.display()))?;
        connection
            .execute_batch(
                "PRAGMA journal_mode = WAL;
                 PRAGMA foreign_keys = ON;
                 PRAGMA busy_timeout = 5000;",
            )
            .context("apply sqlite pragmas")?;
        connection.execute_batch(schema).context("apply schema")?;
        Ok(Self {
            connection: Arc::new(Mutex::new(connection)),
        })
    }

    /// Run `work` against the connection on the blocking pool.
    pub async fn call<T, F>(&self, work: F) -> Result<T>
    where
        T: Send + 'static,
        F: FnOnce(&mut Connection) -> Result<T> + Send + 'static,
    {
        let connection = Arc::clone(&self.connection);
        tokio::task::spawn_blocking(move || {
            let mut guard = connection
                .lock()
                .map_err(|_| anyhow::anyhow!("sqlite connection mutex poisoned"))?;
            work(&mut guard)
        })
        .await
        .context("sqlite blocking task failed")?
    }
}

impl std::fmt::Debug for SqliteHandle {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("SqliteHandle")
            .finish_non_exhaustive()
    }
}
