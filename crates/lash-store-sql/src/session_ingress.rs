//! The session ingress's one per-session order (ADR 0101 §5, amended).
//!
//! A session's ingress is its two admission tables, `pending_turn_inputs`
//! and `queued_work_batches`, composed. Every producer of either table draws
//! its row's `enqueue_seq` from the one counter this family owns, under the
//! session lock, so enqueue order across both tables is per-session commit
//! order and the two tables share one comparable sequence.

/// Column ownership for the session allocation counter.
pub mod sequence;

crate::statements! {
    /// `session_ingress_sequence` statements both backends issue verbatim.
    pub struct SessionIngressStatements @ "session_ingress" {
        /// Allocate under the caller's session lock, retained until deletion.
        allocate_sequence = "INSERT INTO session_ingress_sequence (session_id, enqueue_seq)
             VALUES (?1, 1)
             ON CONFLICT (session_id) DO UPDATE
                 SET enqueue_seq = session_ingress_sequence.enqueue_seq + 1
             RETURNING enqueue_seq";

        /// Retire the counter only when its session is deleted.
        delete_sequence = "DELETE FROM session_ingress_sequence WHERE session_id = ?1";
    }
}
