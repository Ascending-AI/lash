//! The SQLite SQL that names no lash table.
//!
//! Pragmas and the reads of SQLite's own catalog (`sqlite_master`) and clock
//! are statements issued against a production database that no table owns,
//! because they are about the *connection* rather than about any row. Before
//! FIG-3387 they sat at scattered call sites, some written twice.
//!
//! This module is their named home, held
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

/// Whether the database carries the release-stamp table at all.
///
/// A pre-stamp database does not, and that is an absence rather than a read
/// failure: it records no release because no build that stamps has written it.
pub(crate) const SELECT_RELEASE_STAMP_TABLE_EXISTS: &str =
    "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = 'release_stamp'";

/// The database clock, in epoch milliseconds: what the recovery leader lease
/// compares against (ADR 0109 §1.6), so hosts with skewed clocks agree on a
/// lease's expiry.
pub(crate) const SELECT_DATABASE_EPOCH_MS: &str =
    "SELECT CAST(unixepoch('subsec') * 1000 AS INTEGER)";
