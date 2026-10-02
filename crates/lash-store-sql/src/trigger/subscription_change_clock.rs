//! Transactional publication order and retained deletion horizon.

pub const TABLE: &str = "trigger_subscription_change_clock";
pub const INSERT_COLUMNS: &str = "singleton, current_seq, pruned_through";
/// Publication and compaction positions; the singleton key is fixed by SQL.
pub const STATE_COLUMNS: &str = "current_seq, pruned_through";

crate::statements! {
    pub struct SubscriptionChangeClockStatements @ "trigger_subscription_change_clock" {
        clock = "SELECT current_seq, pruned_through FROM trigger_subscription_change_clock WHERE singleton = TRUE";
        bump = "UPDATE trigger_subscription_change_clock SET current_seq = current_seq + 1 WHERE singleton = TRUE AND current_seq < 9223372036854775807 RETURNING current_seq";
        horizon = "UPDATE trigger_subscription_change_clock SET pruned_through = CASE WHEN pruned_through < ?1 THEN ?1 ELSE pruned_through END WHERE singleton = TRUE";
    }
}
