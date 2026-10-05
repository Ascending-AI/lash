//! `tool_intent_submissions`: the replay ledger of host-submitted tool
//! intents, and `tool_intent_retired_owners`: the fence its reclaimed owners
//! leave behind.
//!
//! A ledger row belongs to its owner session. The retained-evidence lever
//! reclaims it once that session is durably deleted and the row is older than
//! the host's bound, and in the same transaction fences the owner so no later
//! submission can claim one of its identities again (FIG-1509, ADR 0067).

/// The table's unprefixed name.
pub const TABLE: &str = "tool_intent_submissions";

/// The reclaimed-owner fence's unprefixed name.
pub const RETIRED_OWNERS_TABLE: &str = "tool_intent_retired_owners";

/// Every column an insert writes.
pub const INSERT_COLUMNS: &str = "replay_key, owner, execution_scope_id, tool_call_id,
     intent_index, payload_hash, submission_json, admitted_at_ms";

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

        /// Whether owner `?1`'s ledger was reclaimed: a fenced owner admits
        /// no submission again.
        select_owner_retired = "SELECT EXISTS(SELECT 1 FROM tool_intent_retired_owners
             WHERE owner = ?1)";

        /// Delete every fenced owner's row admitted strictly before bound
        /// `?1`. Only a durably deleted owner is ever fenced.
        reclaim_retired = "DELETE FROM tool_intent_submissions
             WHERE admitted_at_ms < ?1
               AND owner IN (SELECT owner FROM tool_intent_retired_owners)";
    }
}
