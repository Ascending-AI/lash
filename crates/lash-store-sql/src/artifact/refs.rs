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

/// What the garbage collector reads to build its pointer-table roots.
///
/// Narrow because the collector needs exactly two facts per row: which blob
/// to retain, and which namespace it came from — the namespace is the sole
/// owner of the payload-family label (FIG-1949), so a root cannot inherit a
/// sibling's kind. `artifact_ref` is the ordering key and never decoded.
pub const GC_ROOT_COLUMNS: &str = "namespace, blob_ref";

/// One page of the preflight walk over published module artifacts.
///
/// The projection is alias-qualified because the row spans the pointer and the
/// blob it points at, and the join is `LEFT` on purpose: an inner join would
/// make an artifact whose blob has gone missing simply vanish from the walk,
/// which is the single most alarming finding a preflight can make rendered as
/// "no such artifact". The content column rides along so a page costs one
/// round trip rather than one per pointer.
pub const PREFLIGHT_PAGE_COLUMNS: &str = "refs.artifact_ref, refs.blob_ref, blobs.content";
