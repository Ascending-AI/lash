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
//! # Not yet here
//!
//! The blob reclaim predicates and the preflight session-checkpoint walk read
//! `session_head`/`lash_sessions`, `node_anchors` and `checkpoint_blob_refs`,
//! and PostgreSQL's artifact release reads `lash_lashlang_artifacts`. None of
//! those tables is converted, so the renderer does not know them and a neutral
//! statement cannot name them; FIG-3399 gives a cross-family statement a
//! declared owner and those statements move here then. Until it lands they
//! stay at their call sites, unchanged, and this family is not in the
//! ownership gate's `converted` list.

pub mod blobs;
pub mod owner_retirements;
pub mod owners;
pub mod refs;
