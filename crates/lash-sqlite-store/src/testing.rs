//! SQLite inspection helpers and in-store pauses for external test harnesses.
//!
//! This module only exists behind the crate's `testing` feature. Production
//! factories arm no pause, and production builds do not compile the points.

use lash_sansio::sync::{LockResultExt, MutexExt};
use std::sync::{Arc, Condvar, Mutex};

pub use crate::migration::{SqliteMigrationFault, SqliteMigrationHook, SqliteMigrationStep};

/// Observes each database commit of finalize, including partial-set crash cuts.
#[derive(Clone)]
pub struct SqliteFinalizeHook(Arc<dyn Fn(crate::SqliteDatabase) + Send + Sync>);

impl SqliteFinalizeHook {
    /// Run `committed` immediately after each database commits its epoch.
    pub fn new(committed: impl Fn(crate::SqliteDatabase) + Send + Sync + 'static) -> Self {
        Self(Arc::new(committed))
    }

    pub(crate) fn committed(&self, database: crate::SqliteDatabase) {
        (self.0)(database);
    }
}

impl std::fmt::Debug for SqliteFinalizeHook {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("SqliteFinalizeHook")
    }
}

/// Returns the production trigger-subscription listing SQL for conformance assertions.
///
/// The filter no longer builds the statement; it selects one (FIG-3385). The
/// text is the named statement its shape is served by, which is what the
/// listing actually issues.
pub fn trigger_subscription_list_sql(
    filter: &lash_core_execution::TriggerSubscriptionFilter,
) -> String {
    crate::triggers::subscription_list_sql(filter).to_string()
}

/// The shared-fragment DDL statements provisioning applies to `database`.
/// Fixtures that shadow a schema table with their own declaration apply these
/// to complete the fragment-carried catalog without duplicating DDL text.
pub fn database_fragment_statements(
    database: crate::SqliteDatabase,
) -> impl Iterator<Item = &'static str> {
    database.fragment_statements()
}

/// The full provisioning DDL for `database`: the schema body followed by the
/// shared fragments, in application order.
///
/// Fixtures that shadow one schema table with their own declaration apply
/// this to complete the catalog: `CREATE TABLE IF NOT EXISTS` leaves the
/// shadowed declaration alone while every table declared outside the shared
/// fragments — and the named CHECKs the constraint inspector requires of them —
/// is created from the same text the store provisions.
pub fn database_provisioning_statements(
    database: crate::SqliteDatabase,
) -> impl Iterator<Item = &'static str> {
    database.provisioning_statements()
}

/// The `CREATE TABLE` block for `table` cut out of `database`'s provisioning
/// DDL, schema body and shared fragments alike.
///
/// Fixtures that shadow one table cannot apply the schema body whole — its
/// indexes would name columns the shadow lacks — so they complete the catalog
/// one statement at a time. Extracting from the provisioning text keeps the
/// fixture on the same DDL bytes the store executes rather than a
/// hand-duplicated copy that can drift.
pub fn database_table_ddl(database: crate::SqliteDatabase, table: &str) -> &'static str {
    let marker = format!("CREATE TABLE IF NOT EXISTS {table} (");
    for statement in database.provisioning_statements() {
        let Some(start) = statement.find(&marker) else {
            continue;
        };
        let tail = &statement[start..];
        let end = tail
            .find(';')
            .unwrap_or_else(|| panic!("{table} DDL must end with a semicolon"));
        return &tail[..=end];
    }
    panic!("{database:?} provisioning must declare {table}");
}

/// One row a raw test read returned: each selected column's name and value,
/// in select order. SQLite's `NULL`, integer, real and text map to their JSON
/// kinds; a blob reads as its lowercase hex.
pub type RawRow = Vec<(String, serde_json::Value)>;

/// Every row `sql` selects from `database` of `stores`, over a fresh
/// read-only connection.
///
/// An inspection hook for simulation checkers that judge a finished run's
/// durable rows (lash-sim's global invariants, FIG-4086). It never writes,
/// and no lash component reads through it.
pub fn read_rows_for_testing(
    stores: &crate::SqliteStoreSet,
    database: crate::SqliteDatabase,
    sql: &str,
) -> Result<Vec<RawRow>, String> {
    use rusqlite::types::ValueRef;
    let target = stores.location().target(database);
    let connection = rusqlite::Connection::open_with_flags(
        target.read_only_uri(),
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY
            | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX
            | rusqlite::OpenFlags::SQLITE_OPEN_URI,
    )
    .map_err(|error| format!("open {database:?} read-only: {error}"))?;
    connection
        .busy_timeout(crate::connection_sql::READ_ONLY_BUSY_TIMEOUT)
        .map_err(|error| format!("set the busy timeout on {database:?}: {error}"))?;
    let mut statement = connection
        .prepare(sql)
        .map_err(|error| format!("prepare `{sql}` on {database:?}: {error}"))?;
    let names = statement
        .column_names()
        .into_iter()
        .map(str::to_owned)
        .collect::<Vec<_>>();
    let mut rows = statement
        .query([])
        .map_err(|error| format!("run `{sql}` on {database:?}: {error}"))?;
    let mut read = Vec::new();
    while let Some(row) = rows
        .next()
        .map_err(|error| format!("read `{sql}` on {database:?}: {error}"))?
    {
        let mut columns = Vec::with_capacity(names.len());
        for (index, name) in names.iter().enumerate() {
            let value = match row
                .get_ref(index)
                .map_err(|error| format!("read column `{name}` of `{sql}`: {error}"))?
            {
                ValueRef::Null => serde_json::Value::Null,
                ValueRef::Integer(value) => serde_json::Value::from(value),
                ValueRef::Real(value) => serde_json::Value::from(value),
                ValueRef::Text(bytes) => {
                    serde_json::Value::String(String::from_utf8_lossy(bytes).into_owned())
                }
                ValueRef::Blob(bytes) => serde_json::Value::String(
                    bytes.iter().map(|byte| format!("{byte:02x}")).collect(),
                ),
            };
            columns.push((name.clone(), value));
        }
        read.push(columns);
    }
    Ok(read)
}

/// One stored value: where it is, its bytes, and every JSON document those
/// bytes decode to as this store writes them.
#[derive(Clone, Debug)]
pub struct StoredCell {
    /// `<database>/<table>.<column>#<row>`, or `<database>/schema/<table>`
    /// for a table's own declaration.
    pub location: String,
    pub bytes: Vec<u8>,
    /// The value as JSON text, as a msgpack record, or as a blob envelope's
    /// (decompressed) content in either encoding; empty for a scalar.
    pub documents: Vec<serde_json::Value>,
}

/// Every table declaration and every non-null cell of every table in
/// `database` of `stores`, with the documents each decodes to.
///
/// An inspection hook for simulation checkers that audit what a finished
/// run persisted (lash-sim's crash-matrix catalog audit, FIG-4179). It never
/// writes, and no lash component reads through it.
pub fn read_stored_cells_for_testing(
    stores: &crate::SqliteStoreSet,
    database: crate::SqliteDatabase,
) -> Result<Vec<StoredCell>, String> {
    let target = stores.location().target(database);
    let connection = rusqlite::Connection::open_with_flags(
        target.read_only_uri(),
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY
            | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX
            | rusqlite::OpenFlags::SQLITE_OPEN_URI,
    )
    .map_err(|error| format!("open {database:?} read-only: {error}"))?;
    connection
        .busy_timeout(crate::connection_sql::READ_ONLY_BUSY_TIMEOUT)
        .map_err(|error| format!("set the busy timeout on {database:?}: {error}"))?;
    let mut cells = Vec::new();
    let tables = {
        let mut statement = connection
            .prepare("SELECT name, sql FROM sqlite_master WHERE type = 'table' ORDER BY name")
            .map_err(|error| format!("list the tables of {database:?}: {error}"))?;
        statement
            .query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, Option<String>>(1)?.unwrap_or_default(),
                ))
            })
            .and_then(Iterator::collect::<Result<Vec<_>, _>>)
            .map_err(|error| format!("list the tables of {database:?}: {error}"))?
    };
    for (table, declaration) in tables {
        cells.push(StoredCell {
            location: format!("{database:?}/schema/{table}"),
            bytes: declaration.into_bytes(),
            documents: Vec::new(),
        });
        let mut statement = connection
            .prepare(&format!("SELECT * FROM \"{table}\""))
            .map_err(|error| format!("read `{table}` of {database:?}: {error}"))?;
        let columns = statement
            .column_names()
            .into_iter()
            .map(str::to_owned)
            .collect::<Vec<_>>();
        let mut rows = statement
            .query([])
            .map_err(|error| format!("read `{table}` of {database:?}: {error}"))?;
        let mut index = 0_usize;
        while let Some(row) = rows
            .next()
            .map_err(|error| format!("read `{table}` of {database:?}: {error}"))?
        {
            for (column_index, column) in columns.iter().enumerate() {
                let bytes = match row
                    .get_ref(column_index)
                    .map_err(|error| format!("read `{table}.{column}`: {error}"))?
                {
                    rusqlite::types::ValueRef::Null => continue,
                    rusqlite::types::ValueRef::Integer(value) => value.to_string().into_bytes(),
                    rusqlite::types::ValueRef::Real(value) => value.to_string().into_bytes(),
                    rusqlite::types::ValueRef::Text(bytes)
                    | rusqlite::types::ValueRef::Blob(bytes) => bytes.to_vec(),
                };
                cells.push(StoredCell {
                    location: format!("{database:?}/{table}.{column}#{index}"),
                    documents: stored_documents(&bytes),
                    bytes,
                });
            }
            index += 1;
        }
    }
    Ok(cells)
}

/// The JSON documents `bytes` holds in the encodings this store writes.
fn stored_documents(bytes: &[u8]) -> Vec<serde_json::Value> {
    let structured =
        |value: serde_json::Value| (value.is_object() || value.is_array()).then_some(value);
    if let Some(value) = serde_json::from_slice(bytes).ok().and_then(structured) {
        return vec![value];
    }
    if let Ok(content) = crate::codec::decode_artifact_blob(bytes) {
        return stored_documents(&content);
    }
    crate::codec::decode_msgpack::<serde_json::Value>(bytes)
        .and_then(structured)
        .into_iter()
        .collect()
}

/// Finalize the store at `location` as a build whose writable range is
/// `[1, fleet]`, without authorizing cold recovery or checking retirement,
/// for a test that races writers
/// against it or stands in for a build other than the linked one.
pub fn finalize_fleet_format(
    location: &crate::SqliteLocation,
    fleet: u32,
) -> Result<(), lash_core_execution::StoreError> {
    let writable = lash_core_execution::compat::VersionRange::new(1, fleet)
        .map_err(|error| lash_core_execution::StoreError::Backend(error.to_string()))?;
    crate::compat::flip_epoch_for_testing(
        location,
        std::time::Duration::from_millis(u64::from(crate::conn::BUSY_TIMEOUT_MS)),
        writable,
    )
    .map(|_| ())
    .map_err(crate::sqlite_error)
}

#[derive(Debug, Default)]
struct PauseState {
    state: Mutex<PauseProgress>,
    changed: Condvar,
}

#[derive(Debug, Default)]
struct PauseProgress {
    reached: bool,
    released: bool,
}

impl PauseState {
    /// Connection-thread side: mark the point reached and block until the
    /// test releases it.
    fn hold(&self) {
        let mut progress = self.state.lock_recover();
        progress.reached = true;
        self.changed.notify_all();
        while !progress.released {
            progress = self.changed.wait(progress).recover();
        }
    }

    fn release(&self) {
        self.state.lock_recover().released = true;
        self.changed.notify_all();
    }
}

/// One-shot pause of a write transaction right after its writer fence
/// (ADR 0115 §2.2), before the transaction body: the paused writer holds the
/// database's write lock under the epoch its fence read.
///
/// The fence and the body are statements of one transaction on the
/// connection thread, so no trait separates them; this is one of the in-store
/// points ADR 0044 lists.
#[derive(Clone, Debug)]
pub struct SqliteTransactionPause {
    state: Arc<PauseState>,
}

impl SqliteTransactionPause {
    /// Wait until the background SQLite thread reaches the fence.
    /// Panic after ten seconds if no transaction reaches it.
    pub async fn wait_until_reached(&self) {
        self.wait_until_reached_for(std::time::Duration::from_secs(10))
            .await;
    }

    #[expect(
        clippy::expect_used,
        reason = "test-harness helper: a panicked waiter task must abort the test"
    )]
    async fn wait_until_reached_for(&self, timeout: std::time::Duration) {
        let state = Arc::clone(&self.state);
        tokio::task::spawn_blocking(move || {
            let progress = state.state.lock_recover();
            let (mut progress, _) = state
                .changed
                .wait_timeout_while(progress, timeout, |progress| !progress.reached)
                .recover();
            if !progress.reached {
                progress.released = true;
                state.changed.notify_all();
                panic!("armed SQLite transaction pause was not reached within {timeout:?}");
            }
        })
        .await
        .expect("SQLite pause waiter task");
    }

    /// Release the background SQLite transaction to continue to commit.
    pub fn release(&self) {
        self.state.release();
    }
}

/// One-shot deterministic pause inside a read, between the statement that
/// selects a parent row and the statement that hydrates its children.
///
/// Nothing is refused and no transaction is abandoned. The read simply waits
/// inside its own snapshot while the test commits a competing write in that
/// window, which is the only way to drive the window without load.
#[derive(Clone, Debug)]
pub struct SqliteReadPause {
    state: Arc<PauseState>,
}

impl SqliteReadPause {
    /// Wait until the background SQLite thread reaches the armed read seam.
    pub async fn wait_until_reached(&self) {
        let state = Arc::clone(&self.state);
        let waited = tokio::task::spawn_blocking(move || {
            let mut progress = state.state.lock_recover();
            while !progress.reached {
                progress = state.changed.wait(progress).recover();
            }
        })
        .await;
        assert!(waited.is_ok(), "SQLite read-pause waiter task");
    }

    /// Release the paused read so it finishes inside its snapshot.
    pub fn release(&self) {
        self.state.release();
    }
}

#[derive(Debug, Default)]
struct ArmedPauses {
    after_fence: Option<Arc<PauseState>>,
    queued_work_hydration: Option<Arc<PauseState>>,
    process_event_page_after_identity: Option<Arc<PauseState>>,
}

/// The pauses a test arms inside one store's connection thread: the in-store
/// points ADR 0044 lists for SQLite. Each holds a transaction or a read
/// snapshot open at a point no trait seam reaches; none refuses a call or
/// injects an error, which a law does with a `Script` over the store.
#[derive(Clone, Debug, Default)]
pub struct SqlitePauses {
    armed: Arc<Mutex<ArmedPauses>>,
}

impl SqlitePauses {
    /// Pause the next write transaction after its writer fence until the
    /// returned handle is released.
    pub fn pause_after_fence(&self) -> SqliteTransactionPause {
        let state = Arc::new(PauseState::default());
        self.armed.lock_recover().after_fence = Some(Arc::clone(&state));
        SqliteTransactionPause { state }
    }

    /// Pause the next queued-work read after fetching rows and before decoding
    /// their payloads, until the returned handle is released.
    pub fn pause_queued_work_hydration(&self) -> SqliteReadPause {
        let state = Arc::new(PauseState::default());
        self.armed.lock_recover().queued_work_hydration = Some(Arc::clone(&state));
        SqliteReadPause { state }
    }

    /// Pause the next process-event page between its identity/retention lookup
    /// and event query.
    pub fn pause_process_event_page_after_identity(&self) -> SqliteReadPause {
        let state = Arc::new(PauseState::default());
        self.armed.lock_recover().process_event_page_after_identity = Some(Arc::clone(&state));
        SqliteReadPause { state }
    }

    /// Reach the writer fence of a write transaction, blocking the connection
    /// thread while a pause armed by `pause_after_fence` is outstanding.
    pub(crate) fn reach_after_fence(&self) {
        let armed = self.armed.lock_recover().after_fence.take();
        if let Some(pause) = armed {
            pause.hold();
        }
    }

    /// Reach the queued-work hydration seam, blocking the connection thread
    /// while a pause armed by `pause_queued_work_hydration` is outstanding.
    pub(crate) fn reach_queued_work_hydration(&self) {
        let armed = self.armed.lock_recover().queued_work_hydration.take();
        if let Some(pause) = armed {
            pause.hold();
        }
    }

    pub(crate) fn reach_process_event_page_after_identity(&self) {
        let armed = self
            .armed
            .lock_recover()
            .process_event_page_after_identity
            .take();
        if let Some(pause) = armed {
            pause.hold();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn an_unreached_transaction_pause_fails_and_releases() {
        let pauses = SqlitePauses::default();
        let pause = pauses.pause_after_fence();
        let waiter = pause.clone();
        let failed = tokio::spawn(async move {
            waiter
                .wait_until_reached_for(std::time::Duration::from_millis(10))
                .await
        })
        .await
        .expect_err("an unreached pause must fail");
        assert!(failed.is_panic());
        assert!(pause.state.state.lock_recover().released);
    }
}
