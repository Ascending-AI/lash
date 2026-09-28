//! `attachment_blobs`: attachment bytes kept in the SQLite session catalog.
//!
//! One row per attachment content id, holding the bytes and the freshness
//! stamp the mark-and-sweep GC ages a blob by. The table exists on SQLite
//! only: a PostgreSQL deployment takes its attachment backend (S3 or a file
//! store) at construction (ADR 0102). The module is here rather than in
//! the backend because a table's name and its column lists are owned in one
//! place whether one backend carries it or two.
//!
//! It is deliberately not the content-addressed `blobs` table beside it. That
//! table is rooted by checkpoints, anchors and artifact pointers, while the
//! attachment GC lists its backend and deletes every entry the attachment
//! manifest does not root. Sharing one table would hand checkpoint bytes to a
//! sweep that cannot see their roots.

/// The table's unprefixed name.
pub const TABLE: &str = "attachment_blobs";

/// Every column, in insert order. The whole row: the content id, the bytes,
/// and when a put last stamped them.
pub const INSERT_COLUMNS: &str = "attachment_id, content, stored_at_ms";
