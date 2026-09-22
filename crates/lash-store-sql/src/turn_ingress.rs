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

crate::statements! {
    /// Statements over more than one of the family's tables, which both
    /// backends issue verbatim.
    pub struct TurnIngressStatements @ "turn_ingress" {
        /// Whether session `?1` has work a runner could pick up at `?2`:
        /// an unfinished queued run, an available queued batch, or an input
        /// already deferred to the next turn.
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
             )";

        /// The earliest unclaimed session command and the earliest deferred
        /// turn input of session `?1`, as of `?2`, with `?3` naming the
        /// control work kind.
        ///
        /// Both halves are read in one statement because the answer is their
        /// *relative* order: two statements could observe the queue either
        /// side of an enqueue and report an ordering that never held.
        /// "Unclaimed" is a join against the lease row, because a claim
        /// pinned to a superseded lease generation is not a live claim
        /// (ADR 0029).
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
                ORDER BY enqueued_at_ms ASC, enqueue_seq ASC
                LIMIT 1
             ), earliest_input AS (
                SELECT enqueued_at_ms, enqueue_seq
                FROM pending_turn_inputs AS input
                WHERE session_id = ?1
                  AND {{deferred_next_turn_turn_input_state(input.state)}}
                  AND (claim_token IS NULL OR NOT EXISTS (
                       SELECT 1 FROM session_execution_leases AS lease
                       WHERE lease.session_id = ?1
                         AND lease.lease_token IS NOT NULL
                         AND lease.lease_expires_at_ms > ?2
                         AND lease.lease_fencing_token
                             = input.claim_session_lease_generation
                  ))
                ORDER BY enqueued_at_ms ASC, enqueue_seq ASC
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
