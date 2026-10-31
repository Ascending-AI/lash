//! Version lifetimes, head roots, and the enqueue-only retirement bridge.
pub const OWNERS_TABLE: &str = "environment_owners";
pub const HEADS_TABLE: &str = "environment_heads";
crate::statements! {
    pub struct EnvironmentStatements @ "durable_environment" {
        authority = "SELECT owner_id, version_id, state, actor_key, preparing_epoch
            FROM environment_owners WHERE owner_id = ?1";
        owner = "SELECT owner_id, version_id, state, actor_key, preparing_epoch, body_json
            FROM environment_owners WHERE owner_id = ?1";
        prepare = "INSERT INTO environment_owners
            (owner_id, version_id, state, actor_key, preparing_epoch, body_json, manifest_ref)
            VALUES (?1, ?2, 'preparing', ?3, ?4, ?5, ?6)
            ON CONFLICT (owner_id) DO NOTHING";
        publish = "UPDATE environment_owners SET state = 'published'
            WHERE owner_id = ?1 AND state = 'preparing' AND actor_key = ?2 AND preparing_epoch = ?3";
        retire = "UPDATE environment_owners SET state = 'retired', body_json = NULL
            WHERE owner_id = ?1 AND state <> 'retired'";
        head_put = "INSERT INTO environment_heads (session_id, root_ref, written_epoch)
            VALUES (?1, ?2, ?3) ON CONFLICT (session_id) DO UPDATE
            SET root_ref = excluded.root_ref, written_epoch = excluded.written_epoch";
        head_remove = "DELETE FROM environment_heads WHERE session_id = ?1 RETURNING root_ref";
        head_root = "SELECT root_ref FROM environment_heads WHERE session_id = ?1";
        actor_epoch = "SELECT epoch FROM actors WHERE actor_key = ?1";
        publication_held = "SELECT EXISTS(SELECT 1 FROM artifact_referrer_edges
            WHERE namespace = 'kernel_document' AND artifact_ref = ?2
            AND referrer_kind = 'environment_version' AND referrer_id = ?1)";
        /// Publication may expose an owner only with a rooted graph reader.
        has_reader = "WITH RECURSIVE reachable AS (
            SELECT hash FROM blobs WHERE hash = ?1
            UNION SELECT e.parent_hash FROM blob_edges e JOIN reachable r ON e.child_hash = r.hash
            ) SELECT EXISTS(SELECT 1 FROM reachable JOIN graph_root_catalog roots ON roots.root_ref = reachable.hash)";
        /// Update only the environment guard. No referrer/artifact lock, fence,
        /// or inline reclamation is permitted on the graph peel path.
        queue_retired = "UPDATE artifact_cleanup_obligations
            SET cleanup_json = ?2, obligation_state = 'due', obligation_attempts = 0,
                obligation_due_at_ms = 0, obligation_claim_token = NULL,
                obligation_stall_reason = NULL, obligation_last_error = NULL,
                obligation_last_error_code = NULL, obligation_settled_at_ms = NULL
            WHERE referrer_kind = 'environment_version' AND referrer_id = ?1";
    }
}
