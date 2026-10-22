//! Package sidecars, immutable execution registrations and per-use resolution outcomes.
pub const LABELS_TABLE: &str = "package_labels";
pub const EXECUTIONS_TABLE: &str = "package_executions";
pub const RESOLUTIONS_TABLE: &str = "execution_resolutions";
/// Owner and immutable membership, without duplicate execution key.
pub const REGISTRATION_COLUMNS: &str = "actor_key, registration_json";
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

        /// Register an execution once; replay must match its actor and record.
        register_execution = "INSERT INTO package_executions (execution, actor_key, registration_json)
             VALUES (?1, ?2, ?3) ON CONFLICT (execution) DO NOTHING";
        registration = "SELECT actor_key, registration_json FROM package_executions WHERE execution = ?1 AND ended = 0";
        adopt_execution = "UPDATE package_executions SET registration_json = ?3
             WHERE execution = ?1 AND actor_key = ?2 AND ended = 0";
        end_execution = "UPDATE package_executions SET ended = 1 WHERE execution = ?1 AND actor_key = ?2";
        end_actor_executions = "UPDATE package_executions SET ended = 1 WHERE actor_key = ?1";
        /// One snapshot of all live execution debt, tagged by source record kind.
        requirements_rows = "SELECT registration_json AS record_json, 0 AS resolved FROM package_executions
             WHERE actor_key = ?1 AND ended = 0
             UNION ALL
             SELECT r.record_json, 1 AS resolved FROM execution_resolutions AS r
             JOIN package_executions AS e ON e.execution = r.execution
             WHERE e.actor_key = ?1 AND e.ended = 0";
        registration_matches = "SELECT execution FROM package_executions
             WHERE execution = ?1 AND actor_key = ?2 AND registration_json = ?3";
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
