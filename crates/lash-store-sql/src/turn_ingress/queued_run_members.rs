//! Ordered references retain run composition after source rows settle.
pub const TABLE: &str = "queued_run_members";
/// Source payloads remain in their queue table; admission reads ordered IDs only.
pub const MEMBER_COLUMNS: &str = "collection_kind, ordinal, member_kind, member_id";
pub const INSERT_COLUMNS: &str =
    "session_id, scope_id, collection_kind, ordinal, member_kind, member_id";
