//! Where a scope-retirement fence lives and how the effect journal reaches it
//! (FIG-2499, ADR 0049).
//!
//! The journal file carries its own `effect_scope_retirements` table: the
//! fence of every runtime-operation scope, and of a process scope retired
//! while no registry is bound, is inserted there in the same transaction
//! that deletes the scope's rows. A bound SQLite process registry carries the
//! same table in its own file, and that copy is the fence of every process
//! scope from then on: registration inserts the process row and deletes the
//! fence row in one single-file commit, and retirement's fence insert into
//! that file is its one commit point, the journal purge that follows being an
//! idempotent cleanup. Admission reads both tables.
//!
//! ## Lock order across attached databases
//!
//! A connection that reaches more than one database takes their locks in one
//! global order: durable core, then effect journal, then process registry.
//! Every transaction or statement that spans two of them runs under
//! `BEGIN IMMEDIATE`, which takes every write lock up front in that order; a
//! read that must see two of them reads each in a statement of its own and
//! never holds one while it waits for the other. A `memdb` reader cannot take
//! a shared lock while any writer holds a database's write lock (ADR 0102),
//! so a read holding the journal while it waits for a registry some writer
//! holds would wait out the busy timeout against that writer's commit.
//!
//! This module is the SQLite owner of `effect_scope_retirements`: its
//! dialect-only statements, and the rendered form of the shared ones, for
//! every schema a SQLite connection addresses the table through.

use std::path::Path;
use std::sync::LazyLock;

use lash_store_sql::effect::scope_retirement::ScopeRetirementStatements;
use lash_store_sql::{Dialect, SchemaTables, TableLayout};
use rusqlite::{OptionalExtension, params};

use crate::conn::SqliteConnection;
use crate::location::DatabaseTarget;

/// A database a SQLite connection in this crate addresses a shared table
/// through.
///
/// A `Schema` *selects a deployment layout* (FIG-3406); it is not a qualifier
/// stapled onto every table in a statement. [`Schema::layout`] is the layout
/// in which this connection's `<schema>` database holds every table
/// `lash-store-sql` owns — which is the truth for a lash SQLite deployment
/// whose catalog and journal are one file, and for the journal file and the
/// registry file, each of which is provisioned with the tables its statements
/// name (`SqliteDatabase` in `schema.rs` is the checked-in list). A layout
/// that reaches *two* databases at once is declared where it is needed: see
/// `attachments.rs`, whose GC probes join `main.attachment_manifest` to
/// `process_registry.processes`.
///
/// Every statement over a converted table is rendered once per layout at
/// startup, so a caller that reaches the journal through an `ATTACH` names the
/// schema and gets finished SQL rather than building a qualified statement.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Schema {
    /// The connection's own database: the journal's own file on a journal
    /// connection, the registry's own file on a registry connection.
    Main,
    /// The effect journal, attached to the session catalog for a retention
    /// sweep.
    EffectJournal,
    /// A bound process registry, attached to the journal connection.
    ProcessRegistry,
}

impl Schema {
    /// Every schema, in [`Schema::index`] order.
    pub(crate) const ALL: [Self; 3] = [Self::Main, Self::EffectJournal, Self::ProcessRegistry];

    /// The schema's SQL qualifier.
    pub(crate) const fn qualifier(self) -> &'static str {
        match self {
            Self::Main => "main",
            Self::EffectJournal => "effect_journal",
            Self::ProcessRegistry => "process_registry",
        }
    }

    /// The deployment layout in which this database holds every table
    /// `lash-store-sql` owns.
    ///
    /// Total rather than partial on purpose: the three databases this enum
    /// names are each provisioned with the tables the statements rendered for
    /// them address, and a single-file deployment reaches all of them through
    /// `main`. A family whose statements span two databases declares its own
    /// layout instead of reusing one of these.
    pub(crate) const fn layout(self) -> TableLayout {
        match self {
            Self::Main => MAIN_LAYOUT,
            Self::EffectJournal => EFFECT_JOURNAL_LAYOUT,
            Self::ProcessRegistry => PROCESS_REGISTRY_LAYOUT,
        }
    }

    /// The render dialect that addresses tables through this schema.
    pub(crate) const fn dialect(self) -> Dialect {
        Dialect::sqlite(self.layout())
    }

    /// This schema's slot in a per-schema statement table.
    pub(crate) const fn index(self) -> usize {
        match self {
            Self::Main => 0,
            Self::EffectJournal => 1,
            Self::ProcessRegistry => 2,
        }
    }
}

/// The connection's own database holds every table this crate's statements
/// name: the layout of a journal connection, of a registry connection, and of
/// any deployment that keeps its catalog and journal in one file.
const MAIN_LAYOUT: TableLayout = TableLayout::new(&[SchemaTables::new(
    Schema::Main.qualifier(),
    lash_store_sql::TABLES,
)]);

/// The effect journal, attached to a session catalog for a retention sweep.
const EFFECT_JOURNAL_LAYOUT: TableLayout = TableLayout::new(&[SchemaTables::new(
    Schema::EffectJournal.qualifier(),
    lash_store_sql::TABLES,
)]);

/// A bound process registry, attached to this connection. Its own copy of the
/// fence table is reached through this layout rather than through a second
/// entry in the journal's: one table name, two files, two layouts (ADR 0049).
const PROCESS_REGISTRY_LAYOUT: TableLayout = TableLayout::new(&[SchemaTables::new(
    Schema::ProcessRegistry.qualifier(),
    lash_store_sql::TABLES,
)]);

lash_store_sql::statements! {
    /// `effect_scope_retirements` statements only SQLite issues.
    pub(crate) struct ScopeRetirementSqliteStatements @ "effect_scope_retirement" {
        /// Write the permanent fence of scope `?1` at `?2`, keeping the first
        /// stamp.
        insert_fence = "INSERT INTO effect_scope_retirements (
                 scope_id, retired_at_ms, artifact_cleanup_completed
             )
             VALUES (?1, ?2, 0)
             ON CONFLICT (scope_id) DO NOTHING";

        /// Every fenced scope in this file.
        select_all_scope_ids = "SELECT scope_id FROM effect_scope_retirements";

        /// Every fenced scope whose artifact cleanup has not run.
        select_pending_artifact_cleanup = "SELECT scope_id FROM effect_scope_retirements
             WHERE artifact_cleanup_completed = 0
             ORDER BY scope_id";

        complete_artifact_cleanup = "UPDATE effect_scope_retirements
             SET artifact_cleanup_completed = 1
             WHERE scope_id = ?1";
    }
}

/// Every `effect_scope_retirements` statement, rendered for one schema.
pub(crate) struct FenceSql {
    /// The statements PostgreSQL issues verbatim too.
    pub(crate) shared: ScopeRetirementStatements,
    /// The statements only SQLite issues.
    pub(crate) sqlite: ScopeRetirementSqliteStatements,
}

impl FenceSql {
    fn render(schema: Schema) -> Self {
        Self {
            shared: ScopeRetirementStatements::render(schema.dialect()),
            sqlite: ScopeRetirementSqliteStatements::render(schema.dialect()),
        }
    }
}

static FENCE_SQL: LazyLock<[FenceSql; 3]> = LazyLock::new(|| Schema::ALL.map(FenceSql::render));

/// The fence-table statements addressed through `schema`, rendered once.
pub(crate) fn fence_sql(schema: Schema) -> &'static FenceSql {
    &FENCE_SQL[schema.index()]
}

/// The fence tables a journal connection consults: its own, and the attached
/// registry's when a registry is bound.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct FenceLocations {
    /// The schema the journal's own fence table is addressed through.
    journal: Schema,
    /// The attached registry's schema, `None` when no registry file is bound
    /// (or the registry is this very file, addressed as `main`).
    registry: Option<Schema>,
}

impl FenceLocations {
    /// The journal alone, addressed as `main`.
    pub(crate) const JOURNAL_ONLY: Self = Self {
        journal: Schema::Main,
        registry: None,
    };

    /// A journal attached under `journal` with no registry beside it.
    pub(crate) const fn journal_only_at(journal: Schema) -> Self {
        Self {
            journal,
            registry: None,
        }
    }

    /// A journal attached under `journal` beside a registry attached as
    /// [`Schema::ProcessRegistry`]: the retention sweep's view.
    pub(crate) const fn attached(journal: Schema) -> Self {
        Self {
            journal,
            registry: Some(Schema::ProcessRegistry),
        }
    }

    /// Every schema holding a fence table, the journal first.
    pub(crate) fn schemas(self) -> impl Iterator<Item = Schema> {
        std::iter::once(self.journal).chain(self.registry)
    }

    /// The schema a fence for `scope` is written into: the registry's file
    /// for a process scope when a registry is bound, the journal otherwise.
    pub(crate) fn fence_schema_for(self, scope: &lash_core_execution::ExecutionScope) -> Schema {
        match (scope, self.registry) {
            (lash_core_execution::ExecutionScope::Process { .. }, Some(registry)) => registry,
            _ => self.journal,
        }
    }

    /// Whether the fence for `scope` is written outside the journal file, so
    /// its insert commits on its own ahead of the journal purge.
    pub(crate) fn fence_is_in_registry_file(
        self,
        scope: &lash_core_execution::ExecutionScope,
    ) -> bool {
        self.fence_schema_for(scope) != self.journal
    }

    /// Whether `scope_id` is fenced in any location.
    ///
    /// One prepared statement per location, asked journal-first and stopped at
    /// the first `true`, so a journal fence still answers without touching the
    /// registry file. The locations used to be disjoined into a single
    /// `format!`ed statement to save a round trip; on an in-process SQLite
    /// connection inside the caller's transaction that round trip is a
    /// function call, and the statement it saved was rebuilt on every
    /// admission.
    pub(crate) fn is_fenced(
        self,
        connection: &rusqlite::Connection,
        scope_id: &str,
    ) -> rusqlite::Result<bool> {
        for schema in self.schemas() {
            let fenced: bool = connection.query_row(
                fence_sql(schema).shared.exists.sql(),
                params![scope_id],
                |row| row.get(0),
            )?;
            if fenced {
                return Ok(true);
            }
        }
        Ok(false)
    }

    /// The journal's own fence table alone.
    pub(crate) const fn journal_only(self) -> Self {
        Self {
            journal: self.journal,
            registry: None,
        }
    }

    /// Whether the attached registry fences `scope_id`; `false` when no
    /// registry is attached.
    pub(crate) fn is_fenced_in_registry(
        self,
        connection: &rusqlite::Connection,
        scope_id: &str,
    ) -> rusqlite::Result<bool> {
        match self.registry {
            Some(registry) => connection.query_row(
                fence_sql(registry).shared.exists.sql(),
                params![scope_id],
                |row| row.get(0),
            ),
            None => Ok(false),
        }
    }

    /// Delete the fence of `scope_id` everywhere it may be recorded.
    pub(crate) fn lift(
        self,
        connection: &rusqlite::Connection,
        scope_id: &str,
    ) -> rusqlite::Result<()> {
        for schema in self.schemas() {
            connection.execute(
                fence_sql(schema).shared.delete_by_scope.sql(),
                params![scope_id],
            )?;
        }
        Ok(())
    }
}

/// The bound process registry's database, attached to the journal connection
/// on first use so process-scope fences are read from and written to the
/// database whose transaction registers processes.
///
/// `ATTACH` cannot run inside a transaction, so the attach is a separate
/// serialized step ahead of the write that needs it; once attached it stays
/// for the connection's lifetime. A registry that turns out to be the
/// journal file itself is addressed through `main` instead.
#[derive(Default)]
pub(crate) struct RegistryAttachment {
    state: std::sync::Mutex<RegistryAttachmentState>,
    attach: tokio::sync::Mutex<()>,
}

#[derive(Default)]
enum RegistryAttachmentState {
    #[default]
    Unbound,
    Requested(DatabaseTarget),
    Attached {
        registry: DatabaseTarget,
        locations: FenceLocations,
    },
}

impl RegistryAttachment {
    /// Record the registry to attach; a later request naming a different
    /// database than the one already attached is refused with a warning,
    /// because one journal connection reaches one registry.
    pub(crate) fn request(&self, registry: DatabaseTarget) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        match &*state {
            RegistryAttachmentState::Attached {
                registry: attached, ..
            } if *attached == registry => {}
            RegistryAttachmentState::Attached {
                registry: attached, ..
            } => {
                tracing::warn!(
                    attached = %attached,
                    requested = %registry,
                    "effect journal already attached a different process registry; the later \
                     registry's process-scope fences stay in the journal file"
                );
            }
            _ => *state = RegistryAttachmentState::Requested(registry),
        }
    }

    /// Attach the requested registry if not yet attached, repairing the
    /// journal against it once, and answer the fence locations to consult.
    pub(crate) async fn ensure_attached(
        &self,
        conn: &SqliteConnection,
    ) -> rusqlite::Result<FenceLocations> {
        let _serialized = self.attach.lock().await;
        let requested = {
            let state = self
                .state
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            match &*state {
                RegistryAttachmentState::Unbound => return Ok(FenceLocations::JOURNAL_ONLY),
                RegistryAttachmentState::Attached { locations, .. } => return Ok(*locations),
                RegistryAttachmentState::Requested(registry) => registry.clone(),
            }
        };
        let registry_file = requested.file_path().map(Path::to_path_buf);
        let registry_name = requested.open_name();
        let locations = conn
            .call(move |connection| {
                let main_file: Option<String> = connection
                    .query_row(
                        crate::connection_sql::SELECT_MAIN_DATABASE_FILE,
                        [],
                        |row| row.get(0),
                    )
                    .ok()
                    .flatten();
                // A `memdb` main reports no file, and a registry that is
                // not a file is never the journal's own database.
                if main_file
                    .as_deref()
                    .zip(registry_file.as_deref())
                    .is_some_and(|(main, registry)| !main.is_empty() && Path::new(main) == registry)
                {
                    return Ok(FenceLocations::JOURNAL_ONLY);
                }
                // The connection thread finishes a call whose awaiting
                // future was dropped (a timed-out await-event read), so an
                // attach may have landed without this binder recording it,
                // possibly of a registry the request has since moved away
                // from. The binder never handed out locations for it (this
                // method is the only way to them, and the state is still
                // `Requested`), so no fence was read or written through it.
                // It is detached and the requested registry attached, so the
                // connection holds exactly what the binder then records.
                // Replacing is unconditional because an attached `memdb`
                // database reports no file to compare; refusing instead
                // would poison the connection over a cancellation artifact
                // rather than a caller error.
                let stranded = connection
                    .query_row(
                        crate::connection_sql::SELECT_PROCESS_REGISTRY_IS_ATTACHED,
                        [],
                        |_| Ok(()),
                    )
                    .optional()?
                    .is_some();
                if stranded {
                    connection.execute(crate::connection_sql::DETACH_PROCESS_REGISTRY, [])?;
                }
                connection.execute(
                    crate::connection_sql::ATTACH_PROCESS_REGISTRY,
                    params![registry_name],
                )?;
                Ok(FenceLocations::attached(Schema::Main))
            })
            .await?;
        if locations.registry.is_some() {
            // Bind-time repair (ADR 0049): rows a lost journal purge left under
            // a scope the registry file fences are garbage, and a fence this
            // file wrote for a process scope while no registry was bound is
            // stale once the registry says the process is registered.
            conn.write(move |tx| {
                crate::effect_replay::purge_rows_under_fenced_scopes(tx, Schema::Main, locations)?;
                lift_journal_fences_of_registered_processes(tx, locations)?;
                Ok(())
            })
            .await?;
        }
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        *state = RegistryAttachmentState::Attached {
            registry: requested,
            locations,
        };
        Ok(locations)
    }
}

/// Delete every journal-file process-scope fence whose process the attached
/// registry has registered: registration is the owner coming back, and a
/// fence written into this file before the registry was bound cannot have
/// been cleared by the registration transaction.
fn lift_journal_fences_of_registered_processes(
    tx: &rusqlite::Transaction<'_>,
    locations: FenceLocations,
) -> rusqlite::Result<usize> {
    if locations.registry.is_none() {
        return Ok(0);
    }
    let journal = fence_sql(locations.journal);
    let fenced: Vec<String> = {
        let mut statement = tx.prepare(journal.sqlite.select_all_scope_ids.sql())?;
        let rows = statement.query_map([], |row| row.get::<_, String>(0))?;
        rows.collect::<rusqlite::Result<_>>()?
    };
    let mut lifted = 0;
    for scope_id in fenced {
        let Some(lash_core_execution::ExecutionScope::Process { process_id }) =
            lash_core_execution::ExecutionScope::from_journal_key(&scope_id)
        else {
            continue;
        };
        // `processes` belongs to the process family, whose statements this
        // connection reaches through the `process_registry` qualifier it has
        // attached — the second rendering the family keeps for exactly this
        // caller.
        let registered: bool = tx.query_row(
            crate::process_registry::sql::attached_process_sql()
                .process
                .exists_by_id
                .sql(),
            params![process_id.as_str()],
            |row| row.get(0),
        )?;
        if registered {
            lifted += tx.execute(journal.shared.delete_by_scope.sql(), params![scope_id])?;
        }
    }
    Ok(lifted)
}

#[cfg(test)]
mod fenced_predicate_tests {
    use super::*;

    fn connection_with_both_fence_tables() -> rusqlite::Connection {
        let connection = rusqlite::Connection::open_in_memory().expect("open in-memory database");
        connection
            .execute_batch(
                "CREATE TABLE main.effect_scope_retirements (scope_id TEXT PRIMARY KEY);
                 ATTACH ':memory:' AS process_registry;
                 CREATE TABLE process_registry.effect_scope_retirements (scope_id TEXT PRIMARY KEY);",
            )
            .expect("create both fence tables");
        connection
    }

    fn fence(connection: &rusqlite::Connection, schema: Schema, scope_id: &str) {
        connection
            .execute(
                &format!(
                    "INSERT INTO {}.effect_scope_retirements (scope_id) VALUES (?1)",
                    schema.qualifier()
                ),
                params![scope_id],
            )
            .expect("insert fence row");
    }

    #[test]
    fn every_location_is_addressed_through_its_own_rendered_statement() {
        assert!(
            fence_sql(Schema::Main)
                .shared
                .exists
                .sql()
                .contains("FROM main.effect_scope_retirements WHERE scope_id = ?1")
        );
        assert!(
            fence_sql(Schema::ProcessRegistry)
                .shared
                .exists
                .sql()
                .contains("FROM process_registry.effect_scope_retirements WHERE scope_id = ?1")
        );
        assert!(
            fence_sql(Schema::EffectJournal)
                .sqlite
                .select_all_scope_ids
                .sql()
                .contains("FROM effect_journal.effect_scope_retirements")
        );
        assert_eq!(
            fence_sql(Schema::Main).shared.exists.name(),
            "effect_scope_retirement.exists"
        );
    }

    #[test]
    fn a_fence_in_either_location_answers_fenced_and_neither_answers_unfenced() {
        let connection = connection_with_both_fence_tables();
        let locations = FenceLocations::attached(Schema::Main);

        assert!(
            !locations
                .is_fenced(&connection, "scope-a")
                .expect("read fence")
        );

        fence(&connection, Schema::Main, "scope-a");
        fence(&connection, Schema::ProcessRegistry, "scope-b");

        assert!(
            locations
                .is_fenced(&connection, "scope-a")
                .expect("read fence")
        );
        assert!(
            locations
                .is_fenced(&connection, "scope-b")
                .expect("read fence")
        );
        assert!(
            !locations
                .is_fenced(&connection, "scope-c")
                .expect("read fence")
        );
    }

    #[test]
    fn a_journal_only_view_does_not_see_a_registry_fence() {
        let connection = connection_with_both_fence_tables();
        fence(&connection, Schema::ProcessRegistry, "scope-b");

        assert!(
            !FenceLocations::JOURNAL_ONLY
                .is_fenced(&connection, "scope-b")
                .expect("read fence")
        );
    }
}

#[cfg(test)]
mod registry_attachment_tests {
    use super::*;
    use crate::{SqliteBackend, SqliteDatabase};

    /// A backend over a directory or in memory, with its directory kept.
    async fn backend(file: bool) -> (SqliteBackend, Option<tempfile::TempDir>) {
        if file {
            let dir = tempfile::tempdir().expect("backend directory");
            let backend = SqliteBackend::open(dir.path())
                .await
                .expect("open a file backend");
            (backend, Some(dir))
        } else {
            let backend = SqliteBackend::memory()
                .await
                .expect("open a memory backend");
            (backend, None)
        }
    }

    /// A fresh connection to `backend`'s effect journal, which no binder has
    /// touched.
    async fn journal_connection(backend: &SqliteBackend) -> SqliteConnection {
        SqliteConnection::open(&backend.location().target(SqliteDatabase::EffectReplay))
            .await
            .expect("open the effect journal")
    }

    /// Tag `registry` with a marker table, so a test can tell which registry
    /// a connection has attached: an attached `memdb` database reports no
    /// file.
    fn mark(registry: &DatabaseTarget, marker: &str) {
        rusqlite::Connection::open(registry.open_name())
            .expect("open the registry")
            .execute_batch(&format!("CREATE TABLE {marker} (x INTEGER)"))
            .expect("mark the registry");
    }

    /// Attach `registry` by hand, as an attach whose caller was dropped
    /// leaves the connection: attached, and unrecorded by the binder.
    async fn attach_by_hand(conn: &SqliteConnection, registry: &DatabaseTarget) {
        let name = registry.open_name();
        conn.call(move |connection| {
            connection.execute(
                crate::connection_sql::ATTACH_PROCESS_REGISTRY,
                params![name],
            )
        })
        .await
        .expect("attach the registry by hand");
    }

    /// The markers of the registry `conn` has attached.
    async fn attached_markers(conn: &SqliteConnection) -> Vec<String> {
        conn.call(|connection| {
            let mut statement = connection.prepare(
                "SELECT name FROM process_registry.sqlite_master \
                 WHERE name LIKE '%_marker' ORDER BY name",
            )?;
            let rows = statement.query_map([], |row| row.get::<_, String>(0))?;
            rows.collect::<rusqlite::Result<Vec<_>>>()
        })
        .await
        .expect("read the attached registry's markers")
    }

    async fn adopts_an_unrecorded_attach_of_the_requested_registry(file: bool) {
        let (backend, _dir) = backend(file).await;
        let registry = backend.location().target(SqliteDatabase::ProcessRegistry);
        mark(&registry, "requested_marker");
        let conn = journal_connection(&backend).await;
        let binder = RegistryAttachment::default();
        binder.request(registry.clone());
        attach_by_hand(&conn, &registry).await;

        let locations = binder
            .ensure_attached(&conn)
            .await
            .expect("an attach that already landed must not make the binder fail");

        assert!(locations.registry.is_some());
        assert_eq!(attached_markers(&conn).await, ["requested_marker"]);
    }

    async fn replaces_an_unrecorded_attach_of_another_registry(file: bool) {
        let (backend, _dir) = self::backend(file).await;
        let (stranded, _stranded_dir) = self::backend(file).await;
        let registry = backend.location().target(SqliteDatabase::ProcessRegistry);
        let other = stranded.location().target(SqliteDatabase::ProcessRegistry);
        mark(&registry, "requested_marker");
        mark(&other, "stranded_marker");
        let conn = journal_connection(&backend).await;
        let binder = RegistryAttachment::default();
        // The stranded attach named a registry the request has since moved
        // away from.
        attach_by_hand(&conn, &other).await;
        binder.request(registry.clone());

        binder
            .ensure_attached(&conn)
            .await
            .expect("the requested registry replaces the stranded attach");

        assert_eq!(
            attached_markers(&conn).await,
            ["requested_marker"],
            "fences must reach the requested registry, not the stranded one"
        );
    }

    #[tokio::test]
    async fn an_unrecorded_attach_of_the_requested_memory_registry_is_adopted() {
        adopts_an_unrecorded_attach_of_the_requested_registry(false).await;
    }

    #[tokio::test]
    async fn an_unrecorded_attach_of_the_requested_file_registry_is_adopted() {
        adopts_an_unrecorded_attach_of_the_requested_registry(true).await;
    }

    #[tokio::test]
    async fn an_unrecorded_attach_of_another_memory_registry_is_replaced() {
        replaces_an_unrecorded_attach_of_another_registry(false).await;
    }

    #[tokio::test]
    async fn an_unrecorded_attach_of_another_file_registry_is_replaced() {
        replaces_an_unrecorded_attach_of_another_registry(true).await;
    }
}
