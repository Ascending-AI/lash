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
    "session_id, turn_id, turn_commit_hash, result_json, committed_at_ms,
                request_identity_hash, requested_node_count, identity_encoding_version";

/// The prior receipt, as the commit planner adjudicates a replay from it.
///
/// `committed_at_ms` is absent: the decision compares the stored commit hash
/// and the stored append-request identity, and never the instant. The ancestor
/// column stays outside deliberately — the request hash already binds it.
pub const RECEIPT_COLUMNS: &str = "turn_commit_hash, result_json,
                        request_identity_hash, identity_encoding_version,
                        requested_node_count";

/// A settled turn's identity and result, as the failure-evidence and
/// turn-input reads fold them.
///
/// `result_json` is unbounded, so this is the narrowest projection that can
/// answer either question: both decode the receipt body and neither needs the
/// commit hash or the identity columns.
pub const SETTLEMENT_COLUMNS: &str = "turn_id, result_json";

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

        /// The retention sweep asks it of a session-free runtime-operation
        /// scope, which has no session id to key by.
        exists_for_operation = "SELECT EXISTS(SELECT 1 FROM runtime_turn_commits WHERE turn_id = ?1)";

        /// The receipt session `?1` recorded for operation key `?2`.
        select_receipt = "SELECT turn_commit_hash, result_json,
                        request_identity_hash, identity_encoding_version,
                        requested_node_count
                 FROM runtime_turn_commits
                 WHERE session_id = ?1 AND turn_id = ?2";

        /// Session `?1`'s receipts that carry failure evidence, oldest first.
        ///
        /// The `LIKE` is a pre-filter over the receipt body, not the decision:
        /// the caller decodes each row and keeps the ones whose evidence is
        /// actually non-empty, so a false positive costs a decode and a false
        /// negative is impossible.
        select_failure_settlements = "SELECT turn_id, result_json
             FROM runtime_turn_commits
             WHERE session_id = ?1
               AND result_json LIKE '%\"failure_evidence\"%'
             ORDER BY committed_at_ms, turn_id";

        /// Every receipt session `?1` recorded.
        select_all_for_session = "SELECT turn_id, result_json FROM runtime_turn_commits WHERE session_id = ?1";

        insert = "INSERT INTO runtime_turn_commits (
                session_id, turn_id, turn_commit_hash, result_json, committed_at_ms,
                request_identity_hash, requested_node_count, identity_encoding_version
             )
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)";

        /// The three append-identity columns are `NULL` by construction: a
        /// marker is not an append request, so it has no request hash, no node
        /// count and no identity encoding.
        insert_marker = "INSERT INTO runtime_turn_commits (
                session_id, turn_id, turn_commit_hash, result_json, committed_at_ms,
                request_identity_hash, requested_node_count, identity_encoding_version
             ) VALUES (?1, ?2, ?3, ?4, ?5, NULL, NULL, NULL)";
    }
}
