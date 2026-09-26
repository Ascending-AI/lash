//! `process_wake_deliveries`: one row per wake owed to a session.
//!
//! `state` carries domain vocabulary (`lash_core::WakeDeliveryState`), so
//! every predicate and every assignment of it here is a `{{term(column)}}`
//! token rather than a spelled label — including the single-state ones, which
//! a `SET` clause and a `WHERE` clause spell identically.

/// The table's unprefixed name.
pub const TABLE: &str = "process_wake_deliveries";

/// Every column, in insert order.
pub const INSERT_COLUMNS: &str = "delivery_id, process_id, target_session_id, sequence, state,
                claim_token, attempts, first_attempt_ms, next_attempt_at_ms, expires_at_ms,
                discard_reason, delivery_json";

/// A delivery as its report reads it: every column but the two that place it
/// in a queue (`process_id`, `target_session_id`), both of which the decoded
/// `delivery_json` already carries.
pub const REPORT_COLUMNS: &str = "delivery_id, state, claim_token, attempts, first_attempt_ms,
                    next_attempt_at_ms, expires_at_ms, discard_reason, delivery_json";

/// [`REPORT_COLUMNS`] without the key, for the read that is already keyed by
/// `delivery_id` and would only be decoding a value it bound.
pub const KEYED_REPORT_COLUMNS: &str = "state, claim_token, attempts, first_attempt_ms,
                    next_attempt_at_ms, expires_at_ms, discard_reason, delivery_json";

/// What the preflight walk reports for an undelivered wake.
///
/// Narrow on purpose: the walk is an operator report over a whole table, so it
/// carries the queue position an operator drains by and the payload, and
/// decodes no attempt bookkeeping.
pub const WALK_COLUMNS: &str = "delivery_id, process_id, target_session_id, state, delivery_json";

crate::statements! {
    /// `process_wake_deliveries` statements both backends issue verbatim.
    pub struct WakeDeliveryStatements @ "process_wake_delivery" {
        /// Delivery `?1`'s state, for the caller that only needs to know
        /// whether it is still owed.
        select_state = "SELECT state FROM process_wake_deliveries WHERE delivery_id = ?1";

        reclaim_lapsed_claims = "UPDATE process_wake_deliveries
                         SET {{pending_wake_delivery_state(state)}}, claim_token = NULL
                         WHERE {{enqueuing_wake_delivery_state(state)}} AND next_attempt_at_ms <= ?1";

        /// Claim delivery `?1` for enqueuing under token `?4`, stamping the
        /// first attempt at `?2` and the next at `?3`.
        start_enqueuing = "UPDATE process_wake_deliveries
                             SET {{enqueuing_wake_delivery_state(state)}},
                                 claim_token = ?4,
                                 attempts = attempts + 1,
                                 first_attempt_ms = COALESCE(first_attempt_ms, ?2),
                                 next_attempt_at_ms = ?3
                             WHERE delivery_id = ?1 AND {{pending_wake_delivery_state(state)}}";

        /// Settle claim `?2` on delivery `?1` into state `?3` with discard
        /// reason `?4`.
        settle_claim = "UPDATE process_wake_deliveries
                     SET state = ?3, claim_token = NULL, discard_reason = ?4
                     WHERE delivery_id = ?1 AND {{enqueuing_wake_delivery_state(state)}} AND claim_token = ?2";

        /// Hand claim `?2` on delivery `?1` back, retrying at `?3`.
        release_claim = "UPDATE process_wake_deliveries
                             SET {{pending_wake_delivery_state(state)}}, claim_token = NULL, next_attempt_at_ms = ?3
                             WHERE delivery_id = ?1 AND {{enqueuing_wake_delivery_state(state)}} AND claim_token = ?2";

        /// Put discarded delivery `?1` back in the pool, expiring at `?2` and
        /// retrying at `?3`, with its attempt bookkeeping reset.
        redrive_discarded = "UPDATE process_wake_deliveries
                             SET {{pending_wake_delivery_state(state)}}, attempts = 0, first_attempt_ms = NULL,
                                 claim_token = NULL, next_attempt_at_ms = ?3, expires_at_ms = ?2,
                                 discard_reason = NULL
                             WHERE delivery_id = ?1 AND {{discarded_wake_delivery_state(state)}}";

        /// Discard every pending wake aimed at session `?1`, which is gone.
        discard_target_gone = "UPDATE process_wake_deliveries
                             SET {{discarded_wake_delivery_state(state)}}, discard_reason = 'target_gone'
                             WHERE target_session_id = ?1 AND {{pending_wake_delivery_state(state)}}";

        /// Discard process `?1`'s pending wakes aimed at session `?2`, which
        /// it no longer wakes.
        discard_retargeted = "UPDATE process_wake_deliveries
                             SET {{discarded_wake_delivery_state(state)}}, discard_reason = 'retargeted'
                             WHERE process_id = ?1 AND target_session_id = ?2 AND {{pending_wake_delivery_state(state)}}";
    }
}
