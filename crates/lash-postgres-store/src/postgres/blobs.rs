//! The PostgreSQL owner of `lash_blobs`, the content-addressed byte store.
//!
//! The table holds no module of its own behaviour: its readers are checkpoint
//! publication (`support.rs`), the mark/sweep collector
//! (`runtime_persistence/maintenance.rs`), owner-scoped session reclaim
//! (`session_blob_reclaim.rs`) and the preflight walk. This module is where
//! their statements live, so that "one table, one owner" survives having four
//! callers, and so the lock order the three writers share is written once.

use std::sync::LazyLock;

use lash_store_sql::Dialect;
use lash_store_sql::artifact::blobs::BlobStatements;

lash_store_sql::statements! {
    /// `lash_blobs` statements only PostgreSQL issues.
    pub(crate) struct BlobPostgresStatements @ "blob" {
        /// Store one chunk of (hash, content) pairs, keeping what is already
        /// there.
        ///
        /// The `unnest` array bind is the fork: it writes a whole chunk in one
        /// round trip and stays under the scalar-parameter ceiling, where
        /// SQLite writes a row per statement with `INSERT OR IGNORE`. The
        /// `ORDER BY` is the global blob lock order every writer of this table
        /// takes, and `ON CONFLICT DO NOTHING` is safe because the rows are
        /// content-addressed: a conflict is always the same bytes.
        insert_chunk = "INSERT INTO blobs (hash, content)
             SELECT hash, content
               FROM unnest(?1::text[], ?2::bytea[]) AS blob(hash, content)
              ORDER BY hash
             ON CONFLICT (hash) DO NOTHING";

        /// Take a read lock on the blob at `?1`, reporting whether it exists.
        ///
        /// `FOR KEY SHARE` is the fork and the point: it holds the row against
        /// a concurrent delete for the rest of the publishing transaction.
        /// SQLite holds the whole database under `BEGIN IMMEDIATE` instead.
        lock_one = "SELECT TRUE FROM blobs WHERE hash = ?1 FOR KEY SHARE";

        /// Lock and report which of the content addresses in `?1` exist, in
        /// the global hash order. Same `FOR KEY SHARE` fork as
        /// [`BlobPostgresStatements::lock_one`], over a chunk.
        lock_existing_hashes = "SELECT hash FROM blobs
             WHERE hash = ANY(?1::text[])
             ORDER BY hash
             FOR KEY SHARE";

        /// Lock every reclaim candidate in `?1` for update, in the global hash
        /// order.
        ///
        /// `FOR UPDATE` rather than `FOR KEY SHARE`: this caller is about to
        /// delete the rows it locks, and it takes them in the same ascending
        /// hash order publication does so the two cannot deadlock.
        lock_reclaim_candidates = "SELECT hash FROM blobs
             WHERE hash = ANY(?1::TEXT[])
             ORDER BY hash
             FOR UPDATE";

        /// The stored bytes for every content address in `?1`, in one round
        /// trip. The text-array bind is the fork; SQLite binds a JSON array
        /// and unpacks it with `json_each`.
        select_bodies_by_hash = "SELECT hash, content FROM blobs WHERE hash = ANY(?1::text[])";
    }
}

/// Every `lash_blobs` statement, rendered once.
pub(crate) struct BlobSql {
    /// `blobs` statements both backends issue verbatim.
    pub(crate) shared: BlobStatements,
    /// `blobs` statements only PostgreSQL issues.
    pub(crate) postgres: BlobPostgresStatements,
}

static BLOB_SQL: LazyLock<BlobSql> = LazyLock::new(|| {
    let dialect = Dialect::postgres();
    BlobSql {
        shared: BlobStatements::render(dialect),
        postgres: BlobPostgresStatements::render(dialect),
    }
});

/// The blob statements, rendered once at first use.
pub(crate) fn blob_sql() -> &'static BlobSql {
    &BLOB_SQL
}
