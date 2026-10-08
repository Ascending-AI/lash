//! `runtime_turn_commits`: one receipt per committed runtime operation.
//!
//! The receipt is what makes a re-issued commit replay instead of re-running,
//! so every read of it is an identity question and every one of them is the
//! same question asked in a slightly different place. The eight SELECTs each
//! backend carried before this module are the five statements below; the
//! existence probe alone had six verbatim copies across the two stores.

/// The table's unprefixed name.
pub const TABLE: &str = "runtime_turn_commits";

/// Every column, in insert order.
pub const INSERT_COLUMNS: &str =
    "session_id, turn_id, turn_commit_hash, result_json, outcome_code, committed_at_ms,
                request_identity_hash, requested_node_count, identity_encoding_version,
                failure_evidence, change_seq, head_revision";

/// A settled turn's identity and result, as the failure-evidence and
/// turn-input reads fold them.
///
/// The page orders by the immutable commit time and turn id. Its predicate
/// reads the indexed flag without decoding receipts outside the page.
pub const SETTLEMENT_COLUMNS: &str = "committed_at_ms, turn_id, result_json";

crate::statements! {
    /// `runtime_turn_commits` statements both backends issue verbatim.
    pub struct TurnCommitStatements @ "turn_commit" {
        /// One name for what were six verbatim copies: the committed-turn
        /// query, the commit path's own replay probe, the closure
        /// settlement's "already final" fence, the queued-work completion
        /// fence, and two turn-input admission fences all ask exactly this.
        exists_for_turn = "SELECT EXISTS(
                 SELECT 1 FROM runtime_turn_commits
                 WHERE session_id = ?1 AND turn_id = ?2
             )";

        /// The receipt session `?1` recorded for operation key `?2`.
        select_receipt = "SELECT turn_commit_hash, result_json, outcome_code,
                        request_identity_hash, identity_encoding_version,
                        requested_node_count
                 FROM runtime_turn_commits
                 WHERE session_id = ?1 AND turn_id = ?2";

        /// The first failure-evidence page, with one extra row for `next`.
        select_failure_settlements = "SELECT committed_at_ms, turn_id, result_json, outcome_code
             FROM runtime_turn_commits
             WHERE session_id = ?1 AND failure_evidence
             ORDER BY committed_at_ms, turn_id LIMIT ?2";

        /// Resume after the last returned receipt, in stable key order.
        select_failure_settlements_after = "SELECT committed_at_ms, turn_id, result_json, outcome_code
             FROM runtime_turn_commits
             WHERE session_id = ?1 AND failure_evidence
               AND (committed_at_ms, turn_id) > (?2, ?3)
             ORDER BY committed_at_ms, turn_id LIMIT ?4";

        /// Every receipt session `?1` recorded.
        select_all_for_session = "SELECT turn_id, result_json, outcome_code FROM runtime_turn_commits WHERE session_id = ?1";

        insert = "INSERT INTO runtime_turn_commits (
                session_id, turn_id, turn_commit_hash, result_json, outcome_code, committed_at_ms,
                request_identity_hash, requested_node_count, identity_encoding_version,
                failure_evidence, change_seq, head_revision
             )
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)";

        /// Session `?1`'s committed turns after head revision `?2`, oldest
        /// first by commit, at most `?3`. A session's commits are serialized,
        /// so a turn that commits late has a revision above every visible one.
        select_committed_turns_after = "SELECT head_revision, turn_id, result_json, outcome_code,
                    committed_at_ms
             FROM runtime_turn_commits
             WHERE session_id = ?1 AND outcome_code IS NOT NULL AND head_revision > ?2
             ORDER BY head_revision LIMIT ?3";

        change_clock = "SELECT current_seq, retention_horizon FROM turn_change_clock WHERE singleton = 1";
        next_change_seq = "UPDATE turn_change_clock SET current_seq = current_seq + 1
             WHERE singleton = 1 AND current_seq < 9223372036854775807 RETURNING current_seq";
        insert_session_terminal = "INSERT INTO session_terminal_changes
             (change_seq, session_id, fault_json, recorded_at_ms) VALUES (?1, ?2, ?3, ?4)";
        changes_after = "SELECT change_seq, session_id, turn_id, payload, outcome_code, recorded_at_ms FROM (
             SELECT change_seq, session_id, turn_id, result_json AS payload, outcome_code,
                    committed_at_ms AS recorded_at_ms
             FROM runtime_turn_commits WHERE change_seq > ?1 AND outcome_code IS NOT NULL
             UNION ALL
             SELECT change_seq, session_id, NULL, fault_json, NULL, recorded_at_ms
             FROM session_terminal_changes WHERE change_seq > ?1
             ) AS changes ORDER BY change_seq LIMIT ?2";
        removed_horizon = "SELECT MAX(change_seq) FROM (
             SELECT change_seq FROM runtime_turn_commits AS receipt
             WHERE receipt.committed_at_ms < ?1 AND receipt.change_seq <= ?2
               AND receipt.outcome_code IS NOT NULL
               AND EXISTS (SELECT 1 FROM deleted_sessions AS deleted WHERE deleted.session_id = receipt.session_id)
             UNION ALL
             SELECT change_seq FROM session_terminal_changes AS terminal
             WHERE terminal.recorded_at_ms < ?1 AND terminal.change_seq <= ?2
               AND EXISTS (SELECT 1 FROM deleted_sessions AS deleted WHERE deleted.session_id = terminal.session_id)
             ) AS removed";
        advance_horizon = "UPDATE turn_change_clock SET retention_horizon = ?1
             WHERE singleton = 1 AND retention_horizon < ?1";
        delete_session_terminals = "DELETE FROM session_terminal_changes AS terminal
             WHERE terminal.recorded_at_ms < ?1 AND terminal.change_seq <= ?2
               AND EXISTS (SELECT 1 FROM deleted_sessions AS deleted WHERE deleted.session_id = terminal.session_id)";
    }
}

/// Tables sharing the terminal records and their transactional clock.
pub const CLOCK_TABLE: &str = "turn_change_clock";
pub const SESSION_TERMINAL_TABLE: &str = "session_terminal_changes";
