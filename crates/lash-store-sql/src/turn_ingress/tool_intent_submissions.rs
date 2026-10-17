//! `tool_intent_submissions`: the replay ledger of host-submitted tool
//! intents, and `tool_intent_retired_owners`: the fence its reclaimed owners
//! leave behind.
//!
//! A ledger row belongs to its owner session. The retained-evidence lever
//! reclaims it once that session is durably deleted and the row is older than
//! the host's bound, and in the same transaction fences the owner so no later
//! submission can claim one of its identities again (FIG-1509, ADR 0067).
//!
//! A row keeps its replay envelope (`submission_json`) and first settlement
//! (`settlement_json`) while it lives. Its first payload (`intent_json`)
//! follows the revision its owning run (`owner_session_id`, `owner_run`)
//! published (FIG-6612): once the submission settled and that revision is no
//! longer retained, the payload is expired. Expiry is logical and immediate
//! (every read answers it at once); the bytes drain in bounded batches as
//! revisions are released.

/// The table's unprefixed name.
pub const TABLE: &str = "tool_intent_submissions";

/// The reclaimed-owner fence's unprefixed name.
pub const RETIRED_OWNERS_TABLE: &str = "tool_intent_retired_owners";

/// Every column an insert writes.
pub const INSERT_COLUMNS: &str = "replay_key, owner, execution_scope_id, tool_call_id,
     intent_index, payload_hash, submission_json, intent_json, settlement_json,
     owner_session_id, owner_run, admitted_at_ms";

crate::statements! {
    /// `tool_intent_submissions` statements both backends issue verbatim.
    pub struct ToolIntentSubmissionStatements @ "tool_intent_submission" {
        /// The submission recorded under replay key `?1`: its envelope, its
        /// payload (NULL once expired), its settlement and its owning run.
        /// A settled payload whose owning run published a revision its
        /// session no longer retains reads as expired whether or not its
        /// bytes have drained yet.
        select_by_replay_key = "SELECT submission.submission_json,
                    CASE WHEN submission.settlement_json IS NOT NULL AND EXISTS (
                        SELECT 1 FROM session_runs AS ended
                        WHERE ended.session_id = submission.owner_session_id
                          AND ended.run = submission.owner_run
                          AND ended.terminal_head_revision IS NOT NULL
                          AND NOT EXISTS (
                              SELECT 1 FROM session_revisions AS revision
                              WHERE revision.session_id = ended.session_id
                                AND revision.head_revision = ended.terminal_head_revision
                          )
                    ) THEN NULL ELSE submission.intent_json END,
                    submission.settlement_json, submission.owner_session_id,
                    submission.owner_run
             FROM tool_intent_submissions AS submission
             WHERE submission.replay_key = ?1";

        /// Keep settlement `?2` as the first settlement of the submission
        /// recorded under replay key `?1`. No row when one is kept already.
        settle = "UPDATE tool_intent_submissions
             SET settlement_json = ?2
             WHERE replay_key = ?1 AND settlement_json IS NULL";

        /// Drain up to `?2` settled payloads of session `?1` (of every
        /// session when `?1` is NULL) whose owning run published a revision
        /// its session no longer retains. Runs in the transaction that
        /// released revisions; what a batch leaves drains on the next.
        release_settled_payloads = "WITH scoped AS (
                 SELECT replay_key, owner_session_id, owner_run FROM tool_intent_submissions
                 WHERE owner_session_id = ?1
                   AND intent_json IS NOT NULL AND settlement_json IS NOT NULL
                 UNION ALL
                 SELECT replay_key, owner_session_id, owner_run FROM tool_intent_submissions
                 WHERE ?1 IS NULL AND owner_session_id IS NOT NULL
                   AND intent_json IS NOT NULL AND settlement_json IS NOT NULL
             ), released AS (
                 SELECT scoped.replay_key FROM scoped
                 JOIN session_runs AS ended
                   ON ended.session_id = scoped.owner_session_id
                  AND ended.run = scoped.owner_run
                 WHERE ended.terminal_head_revision IS NOT NULL
                   AND NOT EXISTS (
                       SELECT 1 FROM session_revisions AS revision
                       WHERE revision.session_id = ended.session_id
                         AND revision.head_revision = ended.terminal_head_revision
                   )
                 ORDER BY scoped.replay_key
                 LIMIT ?2
             ) UPDATE tool_intent_submissions SET intent_json = NULL
             WHERE replay_key IN (SELECT replay_key FROM released)";

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
