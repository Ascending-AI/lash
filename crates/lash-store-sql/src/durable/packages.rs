//! Package sidecars, current execution uses and immutable resolution outcomes.
pub const LABELS_TABLE: &str = "package_labels";
pub const EXECUTIONS_TABLE: &str = "package_executions";
pub const USES_TABLE: &str = "execution_uses";
pub const RESOLUTIONS_TABLE: &str = "execution_resolutions";
pub const DEBTS_TABLE: &str = "execution_debts";
/// Owner and metadata, without duplicate execution or use membership.
pub const REGISTRATION_COLUMNS: &str = "actor_key, referrer_json, requirements_json, ended = 0";
/// The actor's fence, revision and debt inspected before a resolution write.
pub const OWNER_COLUMNS: &str = "epoch, state, state_revision, base_requires";

crate::statements! {
    /// Package statements both backends issue verbatim.
    pub struct PackageStatements @ "durable_packages" {
        insert_labels = "INSERT INTO package_labels (namespace, package_ref, label_digest, labels_json)
             VALUES (?1, ?2, ?3, ?4) ON CONFLICT (namespace, package_ref, label_digest) DO NOTHING";
        labels = "SELECT labels_json FROM package_labels
             WHERE namespace = ?1 AND package_ref = ?2 ORDER BY label_digest";
        labels_held = "SELECT artifact_ref FROM artifact_referrer_edges
             WHERE namespace = ?1 AND artifact_ref = ?2 AND referrer_kind = ?3 AND referrer_id = ?4";
        /// Every node whose lease is live at `?1`, with its decoded format
        /// sets as a JSON array and its capabilities as JSON, by node.
        live_nodes = "SELECT n.node_id, n.formats_json, n.capabilities_json
             FROM nodes AS n
             WHERE n.heartbeat_expires_at_ms >= ?1
             ORDER BY n.node_id";

        /// Up to `?2` actors that have not ended and carry claim
        /// requirements, by key after `?1`, each with its requirements.
        actor_requirements = "SELECT actor_key, requires FROM actors
             WHERE state <> 'terminal' AND requires IS NOT NULL AND actor_key > ?1
             ORDER BY actor_key
             LIMIT ?2";

        /// Register metadata once; replay compares the reconstructed current record.
        register_execution = "INSERT INTO package_executions
                 (execution, actor_key, referrer_json, requirements_json, referrer_kind, referrer_id)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6) ON CONFLICT (execution) DO NOTHING";
        registration = "SELECT actor_key, referrer_json, requirements_json, ended = 0
             FROM package_executions WHERE execution = ?1";
        current_uses = "SELECT link, slot, use_json FROM execution_uses
             WHERE execution = ?1 ORDER BY link, slot";
        registration_use = "SELECT e.actor_key, e.referrer_json, u.use_json
             FROM package_executions e JOIN execution_uses u ON u.execution = e.execution
             WHERE e.execution = ?1 AND e.ended = 0 AND u.link = ?2 AND u.slot = ?3";
        clear_uses = "DELETE FROM execution_uses WHERE execution = ?1";
        adopt_execution = "UPDATE package_executions SET requirements_json = ?3
             WHERE execution = ?1 AND actor_key = ?2 AND ended = 0";
        end_execution = "UPDATE package_executions SET ended = 1 WHERE execution = ?1 AND actor_key = ?2";
        end_actor_executions = "UPDATE package_executions SET ended = 1 WHERE actor_key = ?1 AND ended = 0";
        /// Execution `?1`'s debt, when it is actor `?2`'s, deleted as the
        /// execution ends: every debt reader requires a live execution.
        end_debts = "DELETE FROM execution_debts
             WHERE execution = ?1
               AND EXISTS (SELECT 1 FROM package_executions
                           WHERE execution = ?1 AND actor_key = ?2)";
        /// The debt of actor `?1`'s live executions, deleted as they end.
        end_actor_debts = "DELETE FROM execution_debts
             WHERE execution IN (SELECT execution FROM package_executions
                                 WHERE actor_key = ?1 AND ended = 0)";
        /// Execution `?1`'s stable and discovery debt, which an adoption
        /// replaces; the target debt of its winners stays with them.
        clear_debts = "DELETE FROM execution_debts
             WHERE execution = ?1 AND source IN ('stable', 'discovery')";
        /// Actor `?1`'s merged debt, one token a row (FIG-7576): the stable
        /// debt of each live execution, and for each current use its
        /// winner's target debt, or its discovery debt while it has no
        /// winner. Whole tokens merge by union, a helper archive by its
        /// highest generation, and a helper release into one token naming
        /// every function reached, as `ClaimRequirements::extend` merges.
        debt_tokens = "WITH atoms AS (
                 SELECT d.family, d.token, d.detail
                 FROM package_executions e
                 JOIN execution_debts d ON d.execution = e.execution
                 WHERE e.actor_key = ?1 AND e.ended = 0 AND d.source = 'stable'
                 UNION ALL
                 SELECT d.family, d.token, d.detail
                 FROM package_executions e
                 JOIN execution_uses u ON u.execution = e.execution
                 JOIN execution_debts d
                     ON d.execution = u.execution AND d.link = u.link AND d.slot = u.slot
                 LEFT JOIN execution_resolutions r
                     ON r.execution = u.execution AND r.link = u.link AND r.slot = u.slot
                 WHERE e.actor_key = ?1 AND e.ended = 0
                   AND ((d.source = 'discovery' AND r.execution IS NULL)
                        OR (d.source = 'target' AND r.execution IS NOT NULL))
             )
             SELECT token FROM atoms WHERE family = 'token'
             UNION
             SELECT token || ':' || max(detail) FROM atoms WHERE family = 'archive' GROUP BY token
             UNION
             SELECT token || ':' || COALESCE(
                     string_agg(detail, ',' ORDER BY detail) FILTER (WHERE detail <> ''), '')
             FROM (SELECT DISTINCT token, detail FROM atoms WHERE family = 'outside') AS reached
             GROUP BY token";
        load_resolution = "SELECT record_json FROM execution_resolutions
             WHERE execution = ?1 AND link = ?2 AND slot = ?3";
        insert_resolution = "INSERT INTO execution_resolutions (execution, link, slot, record_json)
             VALUES (?1, ?2, ?3, ?4)";
        resolution_fence = "UPDATE actors SET state_revision = state_revision + 1
             WHERE actor_key = ?1 AND epoch = ?2 AND state = 'owned' AND state_revision = ?3
             RETURNING state_revision";
        owner_revision = "SELECT epoch, state, state_revision, base_requires FROM actors WHERE actor_key = ?1";
    }
}
