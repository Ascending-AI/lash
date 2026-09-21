//! `turn_cancel_requests`: the durable cancellation intent for one turn.
//!
//! The two backends store the same facts in different shapes and have since
//! before this arc: SQLite keeps the request as one `record_json` document
//! beside its revision, while PostgreSQL spreads it over typed columns and
//! keeps the affected-input receipts in a second table. ADR 0098 freezes the
//! durable encodings, so every statement over this table forks and this module
//! owns the two column lists rather than a shared statement.

/// The table's unprefixed name.
pub const TABLE: &str = "turn_cancel_requests";

/// The columns SQLite's insert writes: one document plus its revision.
pub const INSERT_COLUMNS: &str = "session_id, turn_id, record_json, intent_revision";

/// The columns PostgreSQL's insert writes: the request's fields, typed.
pub const INSERT_COLUMNS_TYPED: &str = "session_id, turn_id, request_id, origin, reason,
     disposition, mode, intent_revision";

/// SQLite's stored request document with its revision: the intent snapshot.
pub const RECORD_WITH_REVISION_COLUMNS: &str = "record_json, intent_revision";

/// PostgreSQL's request fields, without the revision: what a record read
/// rebuilds the request from before attaching its affected-input receipts.
pub const REQUEST_COLUMNS: &str = "request_id, origin, reason, disposition, mode";

/// [`REQUEST_COLUMNS`] with the revision: PostgreSQL's intent snapshot.
///
/// A separate list because the snapshot and the record are different reads with
/// different consumers — the snapshot is a compare-and-swap predicate, the
/// record is evidence — and pinning them to one list would make every
/// revision-free read decode a revision it must not act on.
pub const REQUEST_WITH_REVISION_COLUMNS: &str = "request_id, origin, reason, disposition, mode,
     intent_revision";

crate::statements! {
    /// `turn_cancel_requests` statements both backends issue verbatim.
    pub struct CancelRequestStatements @ "turn_cancel_request" {
        /// Delete session `?1`'s cancellation requests, on session deletion.
        delete_by_session = "DELETE FROM turn_cancel_requests WHERE session_id = ?1";

        /// Advance turn `?2` of session `?1` to intent revision `?3`.
        ///
        /// The revision is the closure compare-and-swap's version: the first
        /// policy acceptor is immutable, and a stronger same-policy request
        /// advances only this column.
        advance_intent_revision = "UPDATE turn_cancel_requests
             SET intent_revision = ?3
             WHERE session_id = ?1 AND turn_id = ?2";
    }
}
