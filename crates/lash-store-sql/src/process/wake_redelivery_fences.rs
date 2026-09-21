//! `wake_redelivery_fences`: the floor below which a session will not be woken
//! for a process again.
//!
//! The receiving half of [`super::wake_allocation_floors`]: that table records
//! what a sender allocated, this one records what a receiver has consumed. It
//! lives in the session databases rather than the process registry, which is
//! why its insert is written by a session commit and forks the same way the
//! allocation floor's does.

/// The table's unprefixed name.
pub const TABLE: &str = "wake_redelivery_fences";

/// Every column, in insert order.
pub const INSERT_COLUMNS: &str = "session_id, process_id, allocation_floor";

crate::statements! {
    /// `wake_redelivery_fences` statements both backends issue verbatim.
    pub struct WakeRedeliveryFenceStatements @ "wake_redelivery_fence" {
        /// The floor session `?1` has consumed for process `?2`, if any.
        select_floor = "SELECT allocation_floor FROM wake_redelivery_fences
                 WHERE session_id = ?1 AND process_id = ?2";

        /// Drop every fence session `?1` holds, which is going away.
        delete_by_session = "DELETE FROM wake_redelivery_fences WHERE session_id = ?1";
    }
}
