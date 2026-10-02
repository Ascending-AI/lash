//! Latest source state and retained deletion evidence, coalesced by subscription.

pub const TABLE: &str = "trigger_subscription_changes";
pub const INSERT_COLUMNS: &str = "subscription_id, change_seq, deleted_at_ms, record_json";
/// Only sequence and desired source state are needed to advance a projector.
/// The deletion timestamp is a compaction predicate, not projection data.
pub const PAGE_COLUMNS: &str = "change_seq, record_json";

crate::statements! {
    pub struct SubscriptionChangeStatements @ "trigger_subscription_change" {
        previous = "SELECT record_json FROM trigger_subscription_changes WHERE subscription_id = ?1";
        upsert = "INSERT INTO trigger_subscription_changes (subscription_id, change_seq, deleted_at_ms, record_json) VALUES (?1, ?2, ?3, ?4)
            ON CONFLICT (subscription_id) DO UPDATE SET change_seq = EXCLUDED.change_seq, deleted_at_ms = EXCLUDED.deleted_at_ms, record_json = EXCLUDED.record_json";
        page = "SELECT change_seq, record_json FROM trigger_subscription_changes WHERE change_seq > ?1 ORDER BY change_seq LIMIT ?2";
        compactable = "SELECT MAX(change_seq) FROM trigger_subscription_changes WHERE deleted_at_ms < ?1";
        compact = "DELETE FROM trigger_subscription_changes WHERE deleted_at_ms < ?1";
    }
}
