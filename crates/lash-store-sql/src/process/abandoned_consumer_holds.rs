//! `abandoned_consumer_holds`: the consumer holds whose call was abandoned
//! before it consumed its child (ADR 0116 §3.4).
//!
//! The opener of a group that cancels a parked call drains the call's cancel
//! obligation: it marks the call's hold key abandoned and, in the same
//! transaction, reads the processes held under it. A registration under a
//! marked key is refused in its own transaction, so a launch racing the
//! cancel either registers first and is drained, or meets the mark and never
//! registers. The owning scope's close forgets its marks: from then on the
//! scope's own ledger row refuses a start under it.

/// The table's unprefixed name.
pub const TABLE: &str = "abandoned_consumer_holds";

crate::statements! {
    /// `abandoned_consumer_holds` statements both backends issue verbatim.
    pub struct AbandonedConsumerHoldStatements @ "abandoned_consumer_hold" {
        /// Mark hold `?1`, owned by the scope `(?2, ?3)`, abandoned at `?4`;
        /// a hold already marked keeps its first mark.
        mark = "INSERT INTO abandoned_consumer_holds
                 (hold_key, owner_scope_kind, owner_scope_id, abandoned_at_ms)
             VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT (hold_key) DO NOTHING";

        /// Whether hold `?1` was abandoned.
        exists = "SELECT EXISTS(SELECT 1 FROM abandoned_consumer_holds WHERE hold_key = ?1)";

        /// Forget the marks of every hold the scope `(?1, ?2)` owned.
        forget_owned_by = "DELETE FROM abandoned_consumer_holds
             WHERE owner_scope_kind = ?1 AND owner_scope_id = ?2";
    }
}
