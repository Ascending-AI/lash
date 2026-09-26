//! The SQLite SQL that names no lash table.
//!
//! Pragmas, `ATTACH`, and the three reads of SQLite's own catalog
//! (`sqlite_master`/`sqlite_schema`, `pragma_database_list`) are statements
//! issued against a production database that no table owns, because they are
//! about the *connection* rather than about any row. Before FIG-3387 they sat
//! at seven call sites and two of them — `ATTACH DATABASE ?1 AS
//! process_registry` — were the same text written twice.
//!
//! This module is their named home. The ownership gate lists it and holds it
//! to two rules: nothing here may name a table a family owns (that statement
//! belongs to that family), and no two of these literals may be the same text.
//!
//! Nothing here is a `lash_store_sql::statements!` set, and that is the point
//! of the separate home rather than an omission: a pragma takes no bound
//! parameter and no table name, so there is nothing for the renderer to
//! rewrite, and the connection policy's pragmas are built per connection from
//! values SQLite will not accept as parameters at all.

use std::time::Duration;

use crate::conn::SqliteConnectionPolicy;

/// PRAGMAs applied on the connection thread immediately after open.
///
/// WAL is the reason this crate exists: it uses the `-wal`/`-shm` sidecars and
/// supports multi-process readers plus a single writer, which the prior store's
/// single-file mvcc mode did not give us across processes.
///
/// The `journal_mode=WAL` conversion is applied separately (see
/// `crate::conn::set_wal_journal_mode`) because SQLite does *not* invoke the
/// busy handler for `journal_mode` changes, so concurrent first-openers must
/// retry it by hand.
pub(crate) fn open_pragmas(policy: SqliteConnectionPolicy) -> String {
    format!(
        "PRAGMA busy_timeout={};\
         PRAGMA synchronous={};\
         PRAGMA wal_autocheckpoint={};\
         PRAGMA cache_size={};\
         PRAGMA foreign_keys=ON;",
        policy.busy_timeout.as_millis(),
        policy.synchronous.as_pragma_value(),
        policy.wal_autocheckpoint_pages,
        policy.cache_size,
    )
}

/// The busy timeout a read-only probe connection waits with.
pub(crate) const READ_ONLY_BUSY_TIMEOUT: Duration = Duration::from_secs(1);

/// The page cache a read-only probe connection runs with: 500 KiB, small
/// enough that opening one to answer a question costs nothing to hold.
pub(crate) const READ_ONLY_PRAGMAS: &str = "PRAGMA cache_size = -500;";

/// Attach the process registry's database under the qualifier
/// `Schema::ProcessRegistry.qualifier()` names.
///
/// `ATTACH` names a schema rather than qualifying a table, so the renderer has
/// nothing to say about it; both binders — the durable-core open path and the
/// effect journal's fence binder — issue this one text.
pub(crate) const ATTACH_PROCESS_REGISTRY: &str = "ATTACH DATABASE ?1 AS process_registry";

/// Attach the effect journal's database under the qualifier
/// `Schema::EffectJournal.qualifier()` names.
pub(crate) const ATTACH_EFFECT_JOURNAL: &str = "ATTACH DATABASE ?1 AS effect_journal";

/// The schema generation an attached process registry carries.
pub(crate) const SELECT_PROCESS_REGISTRY_USER_VERSION: &str =
    "PRAGMA process_registry.user_version";

/// The schema generation the connection's own database carries.
pub(crate) const SELECT_USER_VERSION: &str = "PRAGMA user_version";

/// The catalog read, not a read of the table itself: a registry mid-creation
/// has the file and the version counter but not yet the rows, and asking the
/// catalog distinguishes "not provisioned yet" from "provisioned and empty".
pub(crate) const SELECT_PROCESS_REGISTRY_IS_PROVISIONED: &str =
    "SELECT 1 FROM process_registry.sqlite_master
     WHERE type = 'table' AND name = 'processes'";

/// Whether the durable-core database carries the release-stamp table at all.
///
/// A pre-stamp database does not, and that is an absence rather than a read
/// failure: it records no release because no build that stamps has written it.
pub(crate) const SELECT_RELEASE_STAMP_TABLE_EXISTS: &str =
    "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = 'release_stamp'";

/// Whether the durable-core database carries the fleet-format table at all.
///
/// A pre-fleet-format database does not, and that is an absence rather than a
/// read failure: it records no fleet format because no build that writes one
/// has opened it.
pub(crate) const SELECT_FLEET_FORMAT_TABLE_EXISTS: &str =
    "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = 'fleet_format'";

/// Every table's declared DDL, which the constraint inspector parses its named
/// `CHECK`s out of. SQLite has no `pg_constraint`; the text is the catalog.
pub(crate) const SELECT_TABLE_DDL: &str = "SELECT name, sql
     FROM sqlite_schema
     WHERE type = 'table' AND sql IS NOT NULL";

/// The file backing the connection's own `main` database, or `NULL` for an
/// in-memory one.
///
/// The fence binder asks this before attaching: a journal whose `main` is
/// already the registry file must not attach it a second time under another
/// name.
pub(crate) const SELECT_MAIN_DATABASE_FILE: &str =
    "SELECT file FROM pragma_database_list WHERE name = 'main'";

/// Whether this connection has a process registry attached: an attach whose
/// caller was dropped still ran to completion on the connection thread, so
/// the fence binder looks before it attaches.
pub(crate) const SELECT_PROCESS_REGISTRY_IS_ATTACHED: &str =
    "SELECT 1 FROM pragma_database_list WHERE name = 'process_registry'";

/// Detach the process registry, so the fence binder can attach the one it
/// was asked for in place of an attach it never recorded.
pub(crate) const DETACH_PROCESS_REGISTRY: &str = "DETACH DATABASE process_registry";
