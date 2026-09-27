//! Durable queued-run admission and ordered membership.
pub const TABLE: &str = "queued_runs";
/// Metadata plus redundant scalar guards are decoded together.
pub const ADMISSION_COLUMNS: &str = "admission_json, status, revision";
pub const INSERT_COLUMNS: &str =
    "session_id, scope_id, status, revision, admission_json, admitted_generation";

crate::statements! {
    pub struct QueuedRunStatements @ "queued_run" {
        pending = "SELECT admission_json, status, revision FROM queued_runs WHERE session_id = ?1 AND status = 'pending'";
        by_scope = "SELECT admission_json, status, revision FROM queued_runs WHERE session_id = ?1 AND scope_id = ?2";
        authorized_member = "SELECT EXISTS (
            SELECT 1 FROM queued_run_members
            WHERE session_id = ?1 AND scope_id = ?2 AND member_kind = ?3 AND member_id = ?4
              AND collection_kind IN ('current', 'withheld', 'assigned')
        )";
        pending_member = "SELECT EXISTS (
            SELECT 1 FROM queued_run_members AS member
            WHERE member.session_id = ?1 AND member.member_kind = ?2
              AND member.member_id = ?3
              AND member.scope_id = (
                  SELECT scope_id FROM queued_runs
                  WHERE session_id = ?1 AND status = 'pending'
              )
        )";
        update = "UPDATE queued_runs SET admission_json = ?3, status = ?4, revision = ?5 WHERE session_id = ?1 AND scope_id = ?2";
        members = "SELECT collection_kind, ordinal, member_kind, member_id FROM queued_run_members WHERE session_id = ?1 AND scope_id = ?2 ORDER BY collection_kind, ordinal";
        clear_members = "DELETE FROM queued_run_members WHERE session_id = ?1 AND scope_id = ?2";
        delete_scope = "DELETE FROM queued_runs WHERE session_id = ?1 AND scope_id = ?2";
        insert_member = "INSERT INTO queued_run_members (session_id, scope_id, collection_kind, ordinal, member_kind, member_id) VALUES (?1, ?2, ?3, ?4, ?5, ?6)";
        delete_members = "DELETE FROM queued_run_members WHERE session_id = ?1";
        delete_runs = "DELETE FROM queued_runs WHERE session_id = ?1";
        cancel_inputs = "UPDATE pending_turn_inputs
            SET state = ?3, claim_id = NULL, claim_owner_id = NULL,
                claim_owner_incarnation_id = NULL, claim_token = NULL, claim_session_lease_generation = 0
            WHERE session_id = ?1 AND {{nonterminal_turn_input_state(state)}}
              AND input_id IN (SELECT member_id FROM queued_run_members WHERE session_id = ?1 AND scope_id = ?2 AND member_kind = 'input')
              AND NOT ({{deferred_next_turn_turn_input_state(state)}} AND input_id IN (
                  SELECT member_id FROM queued_run_members
                  WHERE session_id = ?1 AND scope_id = ?2 AND member_kind = 'input' AND collection_kind = 'assigned'
              ))";
        delete_items = "DELETE FROM queued_work_items WHERE batch_id IN
            (SELECT member_id FROM queued_run_members WHERE session_id = ?1 AND scope_id = ?2 AND member_kind = 'batch')";
        delete_batches = "DELETE FROM queued_work_batches WHERE session_id = ?1 AND
            batch_id IN (SELECT member_id FROM queued_run_members WHERE session_id = ?1 AND scope_id = ?2 AND member_kind = 'batch')";
        /// Open a queued run under `?2` in session `?1`: `?3` is the
        /// admission's JSON body and `?4` the drain generation its drive
        /// stamped, projected for the drain's per-generation counts
        /// (FIG-3795 S9).
        insert = "INSERT INTO queued_runs (session_id, scope_id, status, revision, admission_json, admitted_generation) VALUES (?1, ?2, 'pending', 0, ?3, ?4)";
        /// The turns generation `?1` admitted that have not settled: one
        /// pending queued run per session (FIG-3884). Each dialect's
        /// partial index on `admitted_generation` serves the read.
        count_pending_by_admitted_generation = "SELECT COUNT(*) FROM queued_runs
             WHERE status = 'pending' AND admitted_generation = ?1";
    }
}
