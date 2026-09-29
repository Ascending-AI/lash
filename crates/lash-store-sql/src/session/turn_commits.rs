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
                failure_evidence";

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
        select_failure_settlements = "SELECT committed_at_ms, turn_id, result_json
             FROM runtime_turn_commits
             WHERE session_id = ?1 AND failure_evidence
             ORDER BY committed_at_ms, turn_id LIMIT ?2";

        /// Resume after the last returned receipt, in stable key order.
        select_failure_settlements_after = "SELECT committed_at_ms, turn_id, result_json
             FROM runtime_turn_commits
             WHERE session_id = ?1 AND failure_evidence
               AND (committed_at_ms, turn_id) > (?2, ?3)
             ORDER BY committed_at_ms, turn_id LIMIT ?4";

        /// Every receipt session `?1` recorded.
        select_all_for_session = "SELECT turn_id, result_json, outcome_code FROM runtime_turn_commits WHERE session_id = ?1";

        insert = "INSERT INTO runtime_turn_commits (
                session_id, turn_id, turn_commit_hash, result_json, outcome_code, committed_at_ms,
                request_identity_hash, requested_node_count, identity_encoding_version,
                failure_evidence
             )
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)";

        /// The three append-identity columns are `NULL` by construction: a
        /// marker is not an append request, so it has no request hash, no node
        /// count and no identity encoding.
        insert_marker = "INSERT INTO runtime_turn_commits (
                session_id, turn_id, turn_commit_hash, result_json, outcome_code, committed_at_ms,
                request_identity_hash, requested_node_count, identity_encoding_version,
                failure_evidence
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, NULL, NULL, NULL, ?7)";
    }
}
