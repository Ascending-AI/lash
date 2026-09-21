//! `tool_intent_submissions`: the replay ledger of submitted tool intents.

/// The table's unprefixed name.
pub const TABLE: &str = "tool_intent_submissions";

/// Every column an insert writes.
pub const INSERT_COLUMNS: &str = "replay_key, session_id, execution_scope_id, tool_call_id,
     intent_index, kind, payload_hash, submission_json";

crate::statements! {
    /// `tool_intent_submissions` statements both backends issue verbatim.
    pub struct ToolIntentSubmissionStatements @ "tool_intent_submission" {
        /// The submission recorded under replay key `?1`.
        select_by_replay_key = "SELECT submission_json FROM tool_intent_submissions
             WHERE replay_key = ?1";

        /// Replace the submission recorded under replay key `?1`.
        update_submission = "UPDATE tool_intent_submissions
             SET submission_json = ?2
             WHERE replay_key = ?1";
    }
}
