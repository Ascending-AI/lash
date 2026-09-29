//! `artifact_referrer_fences`: the permanent end of a referrer (ADR 0113 §1).
//!
//! One row per ended referrer, of any kind, never deleted. A publish or
//! acquire under a fenced referrer is refused `ReferrerEnded`, and a carry
//! into a fenced destination is skipped: the destination's own cleanup owns
//! what it held.

/// The table's unprefixed name.
pub const TABLE: &str = "artifact_referrer_fences";

/// Every column, in insert order.
pub const INSERT_COLUMNS: &str = "referrer_kind, referrer_id, ended_at_ms";

crate::statements! {
    /// `artifact_referrer_fences` statements both backends issue verbatim.
    pub struct ReferrerFenceStatements @ "artifact_referrer_fence" {
        /// Read before every publish, acquire and carry destination.
        select_is_fenced = "SELECT EXISTS (
                 SELECT 1 FROM artifact_referrer_fences
                 WHERE referrer_kind = ?1 AND referrer_id = ?2
             )";

        /// When referrer `?1`/`?2` ended, if it has.
        select_ended_at = "SELECT ended_at_ms FROM artifact_referrer_fences
             WHERE referrer_kind = ?1 AND referrer_id = ?2";

        /// Fence referrer `?1`/`?2` at `?3` for good. Ending twice is the same
        /// fact as ending once, and keeps the first stamp.
        insert_fence = "INSERT INTO artifact_referrer_fences
             (referrer_kind, referrer_id, ended_at_ms)
             VALUES (?1, ?2, ?3)
             ON CONFLICT DO NOTHING";
    }
}
