//! `session_ingress`: the one session ingress (ADR 0101).
//!
//! One row per admitted item — host input, process wake or session command —
//! with one per-session `enqueue_seq` taken under the session lock, so enqueue
//! order is per-session commit order. Two class-level lanes share the table:
//! order is `(lane, enqueue_seq)`, and every claim and head query carries an
//! open-state predicate so tombstones stay off the claim path.
//!
//! The row is pure SQL shape and its decoder lives with each backend; the
//! column lists below are the only projections a statement may name.

/// The table's unprefixed name.
pub const TABLE: &str = "session_ingress";

/// Column ownership for the session allocation counter.
pub mod sequence;

/// Every column a row decoder reads, in the order both backends index.
///
/// `lane` and the wake-source columns are absent: both are derived from the
/// kind and the payload, and the table's CHECKs pin them to those.
pub const ROW_COLUMNS: &str = "enqueue_seq, item_id, session_id, kind, source_key,
     delivery_scope, delivery_turn_id, delivery_min_boundary, submission_digest,
     payload_json, authority_json, merge_key, state, terminal_cause_json,
     enqueued_at_ms, terminal_at_ms, claim_id, claim_token, claim_admission_id,
     claim_fencing_token, claim_drive_epoch, claim_turn_id";

/// Every column admission writes, in insert order, after the sequence.
pub const INSERT_COLUMNS: &str = "item_id, session_id, lane, kind, source_key,
     delivery_scope, delivery_turn_id, delivery_min_boundary, submission_digest,
     payload_json, authority_json, merge_key, wake_process_id, wake_sequence,
     state, enqueued_at_ms";

/// [`INSERT_COLUMNS`] behind the sequence, for the backend that draws the
/// sequence before its insert.
pub const INSERT_COLUMNS_WITH_SEQ: &str = "enqueue_seq,
     item_id, session_id, lane, kind, source_key,
     delivery_scope, delivery_turn_id, delivery_min_boundary, submission_digest,
     payload_json, authority_json, merge_key, wake_process_id, wake_sequence,
     state, enqueued_at_ms";

crate::statements! {
    /// `session_ingress` statements both backends issue verbatim.
    pub struct SessionIngressStatements @ "session_ingress" {
        /// Allocate under the caller's session lock, retained until deletion.
        allocate_sequence = "INSERT INTO session_ingress_sequence (session_id, enqueue_seq)
             VALUES (?1, 1)
             ON CONFLICT (session_id) DO UPDATE
                 SET enqueue_seq = session_ingress_sequence.enqueue_seq + 1
             RETURNING enqueue_seq";

        /// Retire the counter only when its session is deleted.
        delete_sequence = "DELETE FROM session_ingress_sequence WHERE session_id = ?1";

        /// Boundary turn claims wait for every command to settle.
        has_commands = "SELECT EXISTS (SELECT 1 FROM session_ingress
             WHERE session_id = ?1 AND lane = 'command' AND state IN ('open', 'accepted'))";

        /// The row session `?1` admitted under source key `?2`, open or
        /// tombstoned: the source-key namespace covers both.
        select_by_source_key = "SELECT enqueue_seq, item_id, session_id, kind, source_key,
                    delivery_scope, delivery_turn_id, delivery_min_boundary, submission_digest,
                    payload_json, authority_json, merge_key, state, terminal_cause_json,
                    enqueued_at_ms, terminal_at_ms, claim_id, claim_token, claim_admission_id,
                    claim_fencing_token, claim_drive_epoch, claim_turn_id
             FROM session_ingress
             WHERE session_id = ?1 AND source_key = ?2";

        /// The row with item id `?1`, in whichever session holds it: item ids
        /// are unique across the store.
        select_by_item_id = "SELECT enqueue_seq, item_id, session_id, kind, source_key,
                    delivery_scope, delivery_turn_id, delivery_min_boundary, submission_digest,
                    payload_json, authority_json, merge_key, state, terminal_cause_json,
                    enqueued_at_ms, terminal_at_ms, claim_id, claim_token, claim_admission_id,
                    claim_fencing_token, claim_drive_epoch, claim_turn_id
             FROM session_ingress
             WHERE item_id = ?1";

        /// Session `?1`'s non-terminal rows in lane `?2`, in order: the one
        /// candidate scan every claim of that lane decides from.
        select_open_in_lane = "SELECT enqueue_seq, item_id, session_id, kind, source_key,
                    delivery_scope, delivery_turn_id, delivery_min_boundary, submission_digest,
                    payload_json, authority_json, merge_key, state, terminal_cause_json,
                    enqueued_at_ms, terminal_at_ms, claim_id, claim_token, claim_admission_id,
                    claim_fencing_token, claim_drive_epoch, claim_turn_id
             FROM session_ingress
             WHERE session_id = ?1 AND lane = ?2 AND state IN ('open', 'accepted')
             ORDER BY enqueue_seq ASC";

        /// Session `?1`'s non-terminal rows, in `(lane, enqueue_seq)` order.
        select_open = "SELECT enqueue_seq, item_id, session_id, kind, source_key,
                    delivery_scope, delivery_turn_id, delivery_min_boundary, submission_digest,
                    payload_json, authority_json, merge_key, state, terminal_cause_json,
                    enqueued_at_ms, terminal_at_ms, claim_id, claim_token, claim_admission_id,
                    claim_fencing_token, claim_drive_epoch, claim_turn_id
             FROM session_ingress
             WHERE session_id = ?1 AND state IN ('open', 'accepted')
             ORDER BY lane ASC, enqueue_seq ASC";

        /// Every row of session `?1`, tombstones included, in
        /// `(lane, enqueue_seq)` order.
        select_all = "SELECT enqueue_seq, item_id, session_id, kind, source_key,
                    delivery_scope, delivery_turn_id, delivery_min_boundary, submission_digest,
                    payload_json, authority_json, merge_key, state, terminal_cause_json,
                    enqueued_at_ms, terminal_at_ms, claim_id, claim_token, claim_admission_id,
                    claim_fencing_token, claim_drive_epoch, claim_turn_id
             FROM session_ingress
             WHERE session_id = ?1
             ORDER BY lane ASC, enqueue_seq ASC";

        /// Session `?1`'s non-terminal rows addressed to turn `?2`, open or
        /// held: what a cancel of that turn applies its by-author disposition
        /// to beside the rows it held, including rows an interrupted claim of
        /// a superseded drive epoch still holds (ADR 0101 §10).
        select_addressed = "SELECT enqueue_seq, item_id, session_id, kind, source_key,
                    delivery_scope, delivery_turn_id, delivery_min_boundary, submission_digest,
                    payload_json, authority_json, merge_key, state, terminal_cause_json,
                    enqueued_at_ms, terminal_at_ms, claim_id, claim_token, claim_admission_id,
                    claim_fencing_token, claim_drive_epoch, claim_turn_id
             FROM session_ingress
             WHERE session_id = ?1 AND delivery_turn_id = ?2 AND state IN ('open', 'accepted')
             ORDER BY enqueue_seq ASC";

        /// Session `?1`'s rows claim `?2` holds: what a settlement of that
        /// claim reads to prove it names every row of it.
        select_claimed = "SELECT enqueue_seq, item_id, session_id, kind, source_key,
                    delivery_scope, delivery_turn_id, delivery_min_boundary, submission_digest,
                    payload_json, authority_json, merge_key, state, terminal_cause_json,
                    enqueued_at_ms, terminal_at_ms, claim_id, claim_token, claim_admission_id,
                    claim_fencing_token, claim_drive_epoch, claim_turn_id
             FROM session_ingress
             WHERE session_id = ?1 AND claim_id = ?2 AND state = 'accepted'
             ORDER BY enqueue_seq ASC";

        /// Session `?1`'s rows in lane `?2` from sequence `?3` on that a
        /// suffix withdrawal answers for: every non-terminal row, and the
        /// anchor itself whatever its state.
        select_suffix = "SELECT enqueue_seq, item_id, session_id, kind, source_key,
                    delivery_scope, delivery_turn_id, delivery_min_boundary, submission_digest,
                    payload_json, authority_json, merge_key, state, terminal_cause_json,
                    enqueued_at_ms, terminal_at_ms, claim_id, claim_token, claim_admission_id,
                    claim_fencing_token, claim_drive_epoch, claim_turn_id
             FROM session_ingress
             WHERE session_id = ?1 AND lane = ?2 AND enqueue_seq >= ?3
               AND (state IN ('open', 'accepted') OR enqueue_seq = ?3)
             ORDER BY enqueue_seq ASC";

        /// Install claim `?3`/`?4` of admission `?5` on session `?1`'s row
        /// `?2`, at fencing token `?6`, drive epoch `?7`, for checkpoint turn
        /// `?8` (null for an idle claim). The observed fencing token `?9` is
        /// the backstop: a row another writer moved since the scan is not
        /// written.
        claim_row = "UPDATE session_ingress
             SET state = 'accepted',
                 claim_id = ?3,
                 claim_token = ?4,
                 claim_admission_id = ?5,
                 claim_fencing_token = ?6,
                 claim_drive_epoch = ?7,
                 claim_turn_id = ?8
             WHERE session_id = ?1 AND item_id = ?2
               AND state IN ('open', 'accepted')
               AND claim_fencing_token = ?9";

        /// Return session `?1`'s row `?2`, held by claim `?3`/`?4`, to `open`
        /// at its own position.
        release_row = "UPDATE session_ingress
             SET state = 'open',
                 claim_id = NULL,
                 claim_token = NULL,
                 claim_admission_id = NULL,
                 claim_drive_epoch = NULL,
                 claim_turn_id = NULL
             WHERE session_id = ?1 AND item_id = ?2
               AND state = 'accepted' AND claim_id = ?3 AND claim_token = ?4";

        /// Return every row of session `?1` claim `?2`/`?3` still holds at
        /// drive epoch `?4` to `open` at its own position.
        release_claim = "UPDATE session_ingress
             SET state = 'open',
                 claim_id = NULL,
                 claim_token = NULL,
                 claim_admission_id = NULL,
                 claim_drive_epoch = NULL,
                 claim_turn_id = NULL
             WHERE session_id = ?1
               AND state = 'accepted' AND claim_id = ?2 AND claim_token = ?3
               AND claim_drive_epoch = ?4";

        /// Tombstone session `?1`'s row `?2`, held by claim `?3`/`?4`, in
        /// terminal state `?5` with cause `?6` at `?7`.
        tombstone_claimed = "UPDATE session_ingress
             SET state = ?5,
                 terminal_cause_json = ?6,
                 terminal_at_ms = ?7,
                 claim_id = NULL,
                 claim_token = NULL,
                 claim_admission_id = NULL,
                 claim_drive_epoch = NULL,
                 claim_turn_id = NULL
             WHERE session_id = ?1 AND item_id = ?2
               AND state = 'accepted' AND claim_id = ?3 AND claim_token = ?4";

        /// Tombstone session `?1`'s non-terminal row `?2` in terminal state
        /// `?3` with cause `?4` at `?5`, whatever claim it carries. Only a
        /// caller that decided under the session lock that no live claim
        /// holds the row issues it.
        tombstone_unclaimed = "UPDATE session_ingress
             SET state = ?3,
                 terminal_cause_json = ?4,
                 terminal_at_ms = ?5,
                 claim_id = NULL,
                 claim_token = NULL,
                 claim_admission_id = NULL,
                 claim_drive_epoch = NULL,
                 claim_turn_id = NULL
             WHERE session_id = ?1 AND item_id = ?2
               AND state IN ('open', 'accepted')";

        /// Delete session `?1`'s tombstones, uniformly across kinds, except a
        /// wake tombstone above its process's redelivery floor.
        ///
        /// The floor join is the invariant, not an optimisation: every wake
        /// terminal raises the floor in its own transaction, so a wake
        /// tombstone above it would be a row whose redelivery nothing else
        /// could absorb.
        vacuum = "DELETE FROM session_ingress AS ingress
             WHERE ingress.session_id = ?1
               AND ingress.state IN ('completed', 'cancelled')
               AND (
                    ingress.kind <> 'process_wake'
                    OR ingress.wake_sequence <= COALESCE((
                         SELECT fence.allocation_floor
                         FROM wake_redelivery_fences AS fence
                         WHERE fence.session_id = ingress.session_id
                           AND fence.process_id = ingress.wake_process_id
                    ), -1)
               )";

        /// Drop every row session `?1` owns, which is going away.
        delete_by_session = "DELETE FROM session_ingress WHERE session_id = ?1";
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// FIG-3607 contract 5: the ingress adds no surface keyed by a runtime
    /// process registration. The wake payload stays an opaque copy of the
    /// process delivery; no column, key or claim names its incarnation or
    /// reference.
    #[test]
    fn no_ingress_column_or_statement_names_a_runtime_process_registration() {
        let texts = [ROW_COLUMNS, INSERT_COLUMNS, INSERT_COLUMNS_WITH_SEQ]
            .into_iter()
            .chain(
                SessionIngressStatements::NEUTRAL
                    .iter()
                    .map(crate::Statement::neutral),
            );
        for text in texts {
            for forbidden in ["process_incarnation", "process_ref"] {
                assert!(
                    !text.contains(forbidden),
                    "`{forbidden}` must not key the session ingress: {text}"
                );
            }
        }
    }
}
