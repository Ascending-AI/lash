//! `lashlang_artifacts`: the artifact bytes PostgreSQL stores inline.
//!
//! One row per `(namespace, artifact_ref)`, holding the bytes themselves. The
//! table exists on PostgreSQL only: SQLite reaches the same bytes through the
//! content-addressed `blobs` table and the `artifact_refs` pointer table. The
//! module is here rather than in the backend because a table's
//! name and its column lists are owned in one place whether one backend
//! carries it or two — which is what stops the next call site inventing a
//! fourth column subset (FIG-3387).
//!
//! `artifact_referrer_edges` holds the edges into it, and the two are read
//! under one advisory-lock order; see [`super::referrer_edges`].

/// The table's unprefixed name.
pub const TABLE: &str = "lashlang_artifacts";

/// Every column, in insert order. The whole row: the key plus the bytes.
pub const INSERT_COLUMNS: &str = "namespace, artifact_ref, artifact_bytes";
