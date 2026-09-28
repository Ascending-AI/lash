//! `artifact_refs`: SQLite's pointer from a namespaced artifact reference to
//! the blob that holds its bytes.
//!
//! The table has no PostgreSQL half and therefore no shared statement: on
//! PostgreSQL the bytes live inline in `lash_lashlang_artifacts` and there is
//! no pointer row to keep. Its name and its column lists still live here,
//! because the column discipline is the same discipline, and because a
//! PostgreSQL reader looking for the pointer table should find the record of
//! why there isn't one.
//!
//! The pointer is *not* content-addressed even though [`super::blobs`] is:
//! without the namespace column a module reference that collided with a
//! process-execution-env reference would rewrite the same row. The composite
//! key is what keeps the namespaces disjoint.

/// The table's unprefixed name.
pub const TABLE: &str = "artifact_refs";

/// Every column, in insert order.
pub const INSERT_COLUMNS: &str = "namespace, artifact_ref, blob_ref";
