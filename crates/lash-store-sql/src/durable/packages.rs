//! Package sidecars, current execution uses and immutable resolution outcomes.
pub const LABELS_TABLE: &str = "package_labels";
pub const EXECUTIONS_TABLE: &str = "package_executions";
pub const USES_TABLE: &str = "execution_uses";
pub const RESOLUTIONS_TABLE: &str = "execution_resolutions";
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
        register_execution = "INSERT INTO package_executions (execution, actor_key, referrer_json, requirements_json)
             VALUES (?1, ?2, ?3, ?4) ON CONFLICT (execution) DO NOTHING";
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
        /// Stable debt once per live execution, then current declarations joined
        /// only to their exact immutable winners, all in one snapshot.
        requirements_rows = "SELECT requirements_json AS stable, NULL AS execution, NULL AS link,
                    NULL AS slot, NULL AS use_json, NULL AS record_json
             FROM package_executions WHERE actor_key = ?1 AND ended = 0
             UNION ALL
             SELECT NULL, u.execution, u.link, u.slot, u.use_json, r.record_json
             FROM package_executions e JOIN execution_uses u ON u.execution = e.execution
             LEFT JOIN execution_resolutions r
                 ON r.execution = u.execution AND r.link = u.link AND r.slot = u.slot
             WHERE e.actor_key = ?1 AND e.ended = 0";
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
