//! The PostgreSQL owner of `lash_blobs`, the content-addressed byte store.
//!
//! The table holds no module of its own behaviour: its readers are checkpoint
//! publication (`support.rs`) and the preflight walk, and the checkpoint
//! graph's peel (`durable/graph.rs`) is what deletes from it. This module is
//! where the publication and preflight statements live, so that "one table,
//! one owner" survives having several callers, and so the lock order its
//! writers share is written once.

use std::sync::LazyLock;

use lash_store_sql::Dialect;

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
        /// Every blob it stores is opaque (encoding 0): graph nodes are
        /// installed only by the graph commit.
        insert_chunk = "INSERT INTO blobs (hash, content, encoding)
             SELECT hash, content, 0
               FROM unnest(?1::bytea[], ?2::bytea[]) AS blob(hash, content)
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
             WHERE hash = ANY(?1::bytea[])
             ORDER BY hash
             FOR KEY SHARE";

        /// The stored bytes for every content address in `?1`, in one round
        /// trip. The bytea-array bind is the fork; SQLite binds a JSON array
        /// and unpacks it with `json_each`.
        select_bodies_by_hash = "SELECT hash, content FROM blobs WHERE hash = ANY(?1::bytea[])";
    }
}

/// Every `lash_blobs` statement, rendered once.
pub(crate) struct BlobSql {
    /// `blobs` statements only PostgreSQL issues.
    pub(crate) postgres: BlobPostgresStatements,
}

static BLOB_SQL: LazyLock<BlobSql> = LazyLock::new(|| {
    let dialect = Dialect::postgres();
    BlobSql {
        postgres: BlobPostgresStatements::render(dialect),
    }
});

/// The blob statements, rendered once at first use.
pub(crate) fn blob_sql() -> &'static BlobSql {
    &BLOB_SQL
}
