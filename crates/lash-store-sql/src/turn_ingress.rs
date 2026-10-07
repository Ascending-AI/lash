//! The turn-ingress tables: how work reaches a session and which run holds
//! it while it is being done.
//!
//! One family, two lifecycles:
//!
//! * **ingress** — [`pending_inputs`] holds the turn inputs a caller submitted
//!   and [`queued_batches`] the work batches enqueued against
//!   a session. A row a run admitted names it (`admitted_run` and
//!   `admitted_by`), written by the session actor's owner commit that
//!   admitted the run, and only that run's commit or terminal write lets go
//!   of it again (FIG-3927).
//! * **cancellation** — [`cancel_requests`] holds the cancel request a turn
//!   accepted, which its session's owner honours.
//!
//! [`tool_intent_submissions`] is the process registry's replay ledger for
//! submitted tool intents; it shares this family because it is the fourth
//! `(replay_key) -> payload` ingress ledger and has no other home.

pub mod cancel_requests;
pub mod pending_inputs;
pub mod queued_batches;
pub mod run_specs;
pub mod tool_intent_submissions;

crate::statements! {
    /// Statements over more than one of the family's tables, which both
    /// backends issue verbatim.
    pub struct TurnIngressStatements @ "turn_ingress" {
        /// Whether turn `?2` of session `?1`, a physical turn of run `?3`,
        /// has ended: its final commit is recorded, or its run has terminal
        /// evidence. Read in the admitting transaction of input addressed to
        /// it (ADR 0101 §5.1).
        turn_address_ended = "SELECT EXISTS(
                SELECT 1 FROM runtime_turn_commits
                WHERE session_id = ?1 AND turn_id = ?2
             ) OR EXISTS(
                SELECT 1 FROM session_runs
                WHERE session_id = ?1 AND run = ?3 AND terminal_kind IS NOT NULL
             )";

        /// Whether run `?2` of session `?1` has terminal evidence. A
        /// teardown of one of its turns then finds no input to dispose of:
        /// the run's terminal write already applied its disposition, and
        /// what it left open is next-turn input (ADR 0101 §5.1).
        run_ended = "SELECT EXISTS(
                SELECT 1 FROM session_runs
                WHERE session_id = ?1 AND run = ?2 AND terminal_kind IS NOT NULL
             )";

        /// The deployment's turns in flight — sessions with an unfinished
        /// run — and those among them whose close is pending: the session's
        /// `close_session` intent holds them until its close retires the run.
        count_unsettled_turns = "SELECT
                (SELECT COUNT(DISTINCT session_id) FROM session_runs
                    WHERE admission_json IS NOT NULL AND terminal_kind IS NULL
                ) AS in_flight_turns,
                (SELECT COUNT(DISTINCT session_id) FROM session_runs
                    WHERE admission_json IS NOT NULL AND terminal_kind IS NULL
                      AND session_id IN (
                          SELECT session_id FROM control_intents
                          WHERE kind = 'close_session' AND state = 'pending'
                      )
                ) AS held_by_stalled_close";

        /// Whether session `?1` has work a runner could pick up: an unfinished
        /// run, an open queued batch, or an open input. With no unfinished
        /// run every open input is next-turn input, whatever turn its
        /// submitted delivery addresses (ADR 0101 §5.1).
        ///
        /// One question, so one statement: asking it as two would let a
        /// session go from empty to non-empty between them and report a
        /// bound-worthy session as idle.
        has_admissible_work = "SELECT EXISTS(
                SELECT 1 FROM session_runs
                WHERE session_id = ?1
                  AND admission_json IS NOT NULL
                  AND terminal_kind IS NULL
             ) OR EXISTS(
                SELECT 1
                FROM queued_work_batches qwb
                WHERE qwb.session_id = ?1
                  AND qwb.admitted_run IS NULL
                  AND qwb.terminal_cause IS NULL
             ) OR EXISTS(
                SELECT 1
                FROM pending_turn_inputs pti
                WHERE pti.session_id = ?1
                  AND {{undelivered_turn_input_state(pti.state)}}
                  AND pti.admitted_run IS NULL
             )";

        /// The earliest open session command and open turn input of session
        /// `?1`, with `?2` naming the control work kind.
        ///
        /// Both lanes are projected from one snapshot, so the command-first
        /// decision and the input position describe the same boundary. A
        /// session command is never admitted, and an input a run admitted is
        /// that run's: the unfinished run is admitted before either lane.
        /// At a boundary every open input is next-turn input, whatever turn
        /// its submitted delivery addresses (ADR 0101 §5.1).
        pending_session_work_ordering = "WITH earliest_command AS (
                SELECT enqueued_at_ms, enqueue_seq
                FROM queued_work_batches AS queued
                WHERE session_id = ?1
                  AND work_kind = ?2
                  AND terminal_cause IS NULL
                ORDER BY enqueue_seq ASC
                LIMIT 1
             ), earliest_input AS (
                SELECT enqueued_at_ms, enqueue_seq
                FROM pending_turn_inputs AS input
                WHERE session_id = ?1
                  AND {{undelivered_turn_input_state(input.state)}}
                  AND input.admitted_run IS NULL
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
