//! The SQLite SQL that names no lash table.
//!
//! Pragmas and the reads of SQLite's own catalog (`sqlite_master`) are
//! statements issued against a production database that no table owns,
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
// A savepoint's subjournal only restores an uncommitted transaction. Keep
// that transient state in memory instead of spilling it after SQLite's 64 KiB
// threshold. The WAL and its synchronous acknowledgement are independent.
pub(crate) fn open_pragmas(policy: SqliteConnectionPolicy) -> String {
    format!(
        "PRAGMA busy_timeout={};\
         PRAGMA synchronous={};\
         PRAGMA wal_autocheckpoint={};\
         PRAGMA cache_size={};\
         PRAGMA foreign_keys=ON;\
         PRAGMA temp_store=MEMORY;",
        policy.busy_timeout.as_millis(),
        policy.synchronous.as_pragma_value(),
        policy.wal_autocheckpoint_pages,
        policy.cache_size,
    )
}

/// Whether the database carries the release-stamp table at all.
///
/// A pre-stamp database does not, and that is an absence rather than a read
/// failure: it records no release because no build that stamps has written it.
pub(crate) const SELECT_RELEASE_STAMP_TABLE_EXISTS: &str =
    "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = 'release_stamp'";

/// Save the rollback position of one grouped writer member.
pub(crate) const BATCH_MEMBER_SAVEPOINT: &str = "SAVEPOINT lash_batch_member";
/// Undo one grouped writer member before releasing its savepoint.
pub(crate) const BATCH_MEMBER_ROLLBACK: &str = "ROLLBACK TO lash_batch_member";
/// Keep or finish rolling back one grouped writer member.
pub(crate) const BATCH_MEMBER_RELEASE: &str = "RELEASE lash_batch_member";
