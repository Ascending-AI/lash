//! `lashlang_artifacts`: the artifact bytes PostgreSQL stores inline.
//!
//! One row per `(namespace, artifact_ref)`, holding the bytes themselves. The
//! table exists on PostgreSQL only: SQLite reaches the same bytes through the
//! content-addressed `blobs` table and the `artifact_refs` pointer table, so
//! every statement over this table is dialect-only and carries a manifest
//! entry. The module is here rather than in the backend because a table's
//! name and its column lists are owned in one place whether one backend
//! carries it or two — which is what stops the next call site inventing a
//! fourth column subset (FIG-3387).
//!
//! `artifact_owners` holds the edges into it, and the two are read under one
//! advisory-lock order; see [`super::owners`].

/// The table's unprefixed name.
pub const TABLE: &str = "lashlang_artifacts";

/// Every column, in insert order. The whole row: the key plus the bytes.
pub const INSERT_COLUMNS: &str = "namespace, artifact_ref, artifact_bytes";

/// One page of a namespace's artifacts, keyed by `artifact_ref`.
///
/// The preflight walk reads this and nothing else: the namespace is the page's
/// parameter, so repeating it per row would be bytes on the wire for a value
/// the caller already has.
pub const PAGE_COLUMNS: &str = "artifact_ref, artifact_bytes";
