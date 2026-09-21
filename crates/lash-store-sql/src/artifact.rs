//! The artifact family: immutable artifact bytes and the owner edges that
//! keep them alive.
//!
//! Four tables. [`blobs`] is the content-addressed byte store, shared with
//! checkpoint storage — every checkpoint root and component lives there too,
//! which is why a blob delete is always conditional on every other rooting
//! relation. [`refs`] is SQLite's pointer table from a namespaced artifact
//! reference to its blob; PostgreSQL keeps the bytes inline in
//! `lash_lashlang_artifacts` instead, so that table has no PostgreSQL half.
//! [`owners`] is the exact owner-edge set — the edge *is* the liveness fact,
//! never a maintained count — and [`owner_retirements`] is the permanent
//! publication fence for execution owners.
//!
//! [`lashlang_artifacts`] is where PostgreSQL keeps those inline bytes. It has
//! no SQLite half, so it owns its name and its column lists here and every
//! statement over it is declared in the PostgreSQL store with a manifest
//! entry.

pub mod blobs;
pub mod lashlang_artifacts;
pub mod owner_retirements;
pub mod owners;
pub mod refs;
