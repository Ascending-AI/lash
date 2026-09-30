//! Statements owned by `usage_runs`.
pub const TABLE: &str = "usage_runs";
pub const RECORD_COLUMNS: &str = "effect_key, run_id, execution_scope_key, source, model, admitted_at_ms, state, unknown_reason, conflict_detail, resolved_at_ms";
crate::statements! {
    pub struct UsageRunsStatements @ "usage_runs" {
        find = "SELECT effect_key, run_id, execution_scope_key, source, model, admitted_at_ms, state, unknown_reason, conflict_detail, resolved_at_ms FROM usage_runs WHERE owner_kind = ?1 AND owner_id = ?2 AND effect_key = ?3 AND run_id = ?4";
        resolve = "UPDATE usage_runs SET state = ?5, unknown_reason = ?6, conflict_detail = NULL, resolved_at_ms = ?7 WHERE owner_kind = ?1 AND owner_id = ?2 AND effect_key = ?3 AND run_id = ?4 AND state IN ('open', 'unknown')";
        supersede = "UPDATE usage_runs SET state = 'unknown', unknown_reason = 'superseded_run', resolved_at_ms = ?5 WHERE owner_kind = ?1 AND owner_id = ?2 AND effect_key = ?3 AND run_id <> ?4 AND state = 'open'";
        conflict = "UPDATE usage_runs SET state = 'conflicted', unknown_reason = NULL, conflict_detail = ?5, resolved_at_ms = ?6 WHERE owner_kind = ?1 AND owner_id = ?2 AND effect_key = ?3 AND (run_id = ?4 OR state = 'open')";
        retire_execution = "UPDATE usage_runs SET state = 'unknown', unknown_reason = 'execution_ended', resolved_at_ms = ?4 WHERE owner_kind = ?1 AND owner_id = ?2 AND execution_scope_key = ?3 AND state = 'open'";
        retire_owner = "UPDATE usage_runs SET state = 'unknown', unknown_reason = 'owner_retired', resolved_at_ms = ?3 WHERE owner_kind = ?1 AND owner_id = ?2 AND state = 'open'";
        completeness = "SELECT CAST(COALESCE(SUM(CASE WHEN state = 'open' THEN 1 ELSE 0 END), 0) AS BIGINT), MIN(CASE WHEN state = 'open' THEN admitted_at_ms END), CAST(COALESCE(SUM(CASE WHEN state = 'unknown' THEN 1 ELSE 0 END), 0) AS BIGINT), CAST(COALESCE(SUM(CASE WHEN state = 'conflicted' THEN 1 ELSE 0 END), 0) AS BIGINT) FROM usage_runs WHERE owner_kind = ?1 AND owner_id = ?2";
        page = "SELECT effect_key, run_id, execution_scope_key, source, model, admitted_at_ms, state, unknown_reason, conflict_detail, resolved_at_ms FROM usage_runs WHERE owner_kind = ?1 AND owner_id = ?2 AND (?3 = 'all' OR (?3 = 'open' AND state = 'open') OR (?3 = 'unresolved' AND state IN ('unknown', 'conflicted'))) AND (effect_key > ?4 OR (effect_key = ?4 AND run_id > ?5)) ORDER BY effect_key, run_id LIMIT ?6";
        delete_retired = "DELETE FROM usage_runs AS run WHERE EXISTS (SELECT 1 FROM usage_owner_retirements AS retirement WHERE retirement.owner_kind = run.owner_kind AND retirement.owner_id = run.owner_id AND retirement.retired_at_ms < ?1)";
    }
}
