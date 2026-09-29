//! The artifact family: immutable artifact bytes and the referrer edges that
//! keep them alive (ADR 0113).
//!
//! Five tables. [`blobs`] is the content-addressed byte store, shared with
//! checkpoint storage — every checkpoint root and component lives there too,
//! which is why a blob delete is always conditional on every other rooting
//! relation. [`refs`] is SQLite's pointer table from a namespaced artifact
//! reference to its blob; PostgreSQL keeps the bytes inline in
//! `lash_lashlang_artifacts` instead, so that table has no PostgreSQL half.
//! [`referrer_edges`] is the exact referrer-edge set — the edge *is* the
//! liveness fact, never a maintained count — [`referrer_fences`] is the
//! permanent end of every referrer kind, and [`cleanup_obligations`] is the
//! cleanup each ended or guarded referrer still owes.
//!
//! [`lashlang_artifacts`] is where PostgreSQL keeps those inline bytes. It has
//! no SQLite half, so it owns its name and its column lists here and every
//! statement over it is declared in the PostgreSQL store with a manifest
//! entry.

pub mod blobs;
pub mod cleanup_obligations;
pub mod lashlang_artifacts;
pub mod referrer_edges;
pub mod referrer_fences;
pub mod refs;
