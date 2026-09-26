//! The turn-ingress and claim tables: how work reaches a session and who owns
//! it while it is being done.
//!
//! One family, ten tables, three lifecycles that share a fencing generation:
//!
//! * **ingress** — [`pending_inputs`] holds the turn inputs a caller submitted
//!   and [`queued_batches`]/[`queued_items`] the work batches enqueued against
//!   a session.
//! * **authority** — [`session_execution_leases`] is the lane lease whose
//!   fencing token every claim on the two ingress tables pins itself to
//!   (ADR 0029), so "is this claim still live?" is one question about that
//!   lease rather than a per-row timer.
//! * **cancellation** — [`cancellation_bindings`], [`cancel_requests`],
//!   [`closure_authorizations`], [`closure_participants`] and
//!   [`retired_scopes`] carry the durable cancellation facts a turn's closure
//!   is settled against.
//!
//! [`tool_intent_submissions`] is the process registry's replay ledger for
//! submitted tool intents; it shares this family because it is the fourth
//! `(replay_key) -> payload` ingress ledger and has no other home.

pub mod cancel_affected_inputs;
pub mod cancel_requests;
pub mod cancellation_bindings;
pub mod closure_authorizations;
pub mod closure_participants;
pub mod pending_inputs;
pub mod queued_batches;
pub mod queued_items;
pub mod queued_run_members;
pub mod queued_runs;
pub mod retired_scopes;
pub mod session_execution_leases;
pub mod tool_intent_submissions;
pub mod turn_park_clock;
pub mod turn_park_events;
pub mod turn_parks;

crate::statements! {
    /// Statements over more than one of the family's tables, which both
    /// backends issue verbatim.
    pub struct TurnIngressStatements @ "turn_ingress" {
        /// Clear session `?1`'s park once its turn holds no work: no input
        /// row bound to the parked turn and no pending queued run. The
        /// returning projection names the park the `Cancelled` event logs.
        delete_released_turn_park_returning = "DELETE FROM turn_parks
             WHERE session_id = ?1
               AND NOT EXISTS(
                  SELECT 1 FROM pending_turn_inputs pti
                  WHERE pti.session_id = ?1
                    AND pti.claim_bound_turn_id = turn_parks.turn_id
                    AND {{nonterminal_turn_input_state(pti.state)}}
               )
               AND NOT EXISTS(
                  SELECT 1 FROM queued_runs qr
                  WHERE qr.session_id = ?1 AND qr.status = 'pending'
               )
             RETURNING turn_id, park_id";

        /// The deployment's parked turns, the oldest live park's instant,
        /// and its turns in flight: a session with a pending queued run, a
        /// claimed turn input that is not settled, or a parked turn.
        count_unsettled_turns = "SELECT
                (SELECT COUNT(*) FROM turn_parks) AS parked_turns,
                (SELECT MIN(since_ms) FROM turn_parks) AS oldest_parked_since_ms,
                (SELECT COUNT(*) FROM (
                    SELECT session_id FROM turn_parks
                    UNION
                    SELECT session_id FROM queued_runs WHERE status = 'pending'
                    UNION
                    SELECT session_id FROM pending_turn_inputs
                    WHERE claim_id IS NOT NULL
                      AND {{nonterminal_turn_input_state(state)}}
                ) AS unsettled) AS in_flight_turns";

        /// Live parked turns grouped by their reason's stable code. All four
        /// `ParkReasonCode` cells stay observable: the reader zero-fills.
        count_parks_by_reason = "SELECT reason_code, COUNT(*) AS parks
             FROM turn_parks
             GROUP BY reason_code";

        /// Live retired-generation parks grouped by the generation their
        /// admission recorded (FIG-3571): read off the projected, indexed
        /// `park_executable_generation` column, never the reason payload.
        count_retired_parks_by_executable_generation = "SELECT park_executable_generation, COUNT(*) AS parks
             FROM turn_parks
             WHERE park_executable_generation IS NOT NULL
             GROUP BY park_executable_generation";

        /// Whether session `?1` has work a runner could pick up at `?2`:
        /// an unfinished queued run, an available queued batch, or an input
        /// already deferred to the next turn that no aborted turn is bound to
        /// (FIG-3589).
        ///
        /// One question, so one statement: asking it as two would let a
        /// session go from empty to non-empty between them and report a
        /// bound-worthy session as idle.
        has_claimable_work = "SELECT EXISTS(
                SELECT 1 FROM queued_runs
                WHERE session_id = ?1 AND status = 'pending'
             ) OR EXISTS(
                SELECT 1
                FROM queued_work_batches qwb
                WHERE qwb.session_id = ?1
                  AND qwb.available_at_ms <= ?2
             ) OR EXISTS(
                SELECT 1
                FROM pending_turn_inputs pti
                WHERE pti.session_id = ?1
                  AND {{deferred_next_turn_turn_input_state(pti.state)}}
                  AND pti.claim_bound_turn_id IS NULL
             )";

        /// The earliest unclaimed session command and the earliest deferred
        /// turn input of session `?1`, as of `?2`, with `?3` naming the
        /// control work kind.
        ///
        /// Both lanes are projected from one snapshot, so the command-first
        /// decision and the input position describe the same boundary.
        /// "Unclaimed" is a join against the lease row, because a claim
        /// pinned to a superseded lease generation is not a live claim
        /// (ADR 0029). An input bound to an aborted turn is never unclaimed
        /// (FIG-3589).
        pending_session_work_ordering = "WITH earliest_command AS (
                SELECT enqueued_at_ms, enqueue_seq
                FROM queued_work_batches AS queued
                WHERE session_id = ?1
                  AND work_kind = ?3
                  AND (claim_token IS NULL OR NOT EXISTS (
                       SELECT 1 FROM session_execution_leases AS lease
                       WHERE lease.session_id = ?1
                         AND lease.lease_token IS NOT NULL
                         AND lease.lease_expires_at_ms > ?2
                         AND lease.lease_fencing_token
                             = queued.claim_session_lease_generation
                  ))
                ORDER BY enqueue_seq ASC
                LIMIT 1
             ), earliest_input AS (
                SELECT enqueued_at_ms, enqueue_seq
                FROM pending_turn_inputs AS input
                WHERE session_id = ?1
                  AND {{deferred_next_turn_turn_input_state(input.state)}}
                  AND claim_bound_turn_id IS NULL
                  AND (claim_token IS NULL OR NOT EXISTS (
                       SELECT 1 FROM session_execution_leases AS lease
                       WHERE lease.session_id = ?1
                         AND lease.lease_token IS NOT NULL
                         AND lease.lease_expires_at_ms > ?2
                         AND lease.lease_fencing_token
                             = input.claim_session_lease_generation
                  ))
                ORDER BY enqueue_seq ASC
                LIMIT 1
             )
             SELECT command.enqueued_at_ms, command.enqueue_seq,
                    input.enqueued_at_ms, input.enqueue_seq
             FROM (SELECT 1) AS singleton
             LEFT JOIN earliest_command AS command ON TRUE
             LEFT JOIN earliest_input AS input ON TRUE";
    }
}

#[cfg(test)]
#[path = "turn_ingress/tests.rs"]
mod tests;
