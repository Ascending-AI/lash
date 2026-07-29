use crate::*;

pub(crate) async fn ensure_schema(pool: &PgPool) -> Result<Vec<u8>, StoreError> {
    let mut tx = pool.begin().await.map_err(store_sqlx_error)?;
    tx.execute("SELECT pg_advisory_xact_lock(715421, 907001)")
        .await
        .map_err(store_sqlx_error)?;
    tx.execute(
        r#"
        CREATE TABLE IF NOT EXISTS lash_schema_versions (
            component TEXT PRIMARY KEY,
            version INTEGER NOT NULL
        );

        CREATE TABLE IF NOT EXISTS lash_blobs (
            hash TEXT PRIMARY KEY,
            content BYTEA NOT NULL
        );

        CREATE TABLE IF NOT EXISTS lash_sessions (
            session_id TEXT PRIMARY KEY,
            head_revision BIGINT NOT NULL DEFAULT 0,
            head_json TEXT NOT NULL,
            checkpoint_ref TEXT,
            leaf_node_id TEXT
        );
        CREATE INDEX IF NOT EXISTS idx_lash_sessions_leaf
            ON lash_sessions(leaf_node_id);

        CREATE TABLE IF NOT EXISTS lash_node_anchors (
            node_id TEXT PRIMARY KEY,
            checkpoint_ref TEXT NOT NULL,
            source_session_id TEXT NOT NULL
        );

        CREATE TABLE IF NOT EXISTS lash_deleted_sessions (
            session_id TEXT PRIMARY KEY
        );

        CREATE TABLE IF NOT EXISTS lash_graph_nodes (
            session_id TEXT NOT NULL,
            seq BIGSERIAL,
            node_id TEXT PRIMARY KEY,
            parent_node_id TEXT,
            node_json TEXT NOT NULL,
            tombstoned BOOLEAN NOT NULL DEFAULT FALSE
        );
        CREATE INDEX IF NOT EXISTS idx_lash_graph_nodes_seq
            ON lash_graph_nodes(session_id, seq);
        CREATE INDEX IF NOT EXISTS idx_lash_graph_nodes_parent
            ON lash_graph_nodes(parent_node_id);

        CREATE TABLE IF NOT EXISTS lash_usage_deltas (
            seq BIGSERIAL PRIMARY KEY,
            session_id TEXT NOT NULL,
            entry_json TEXT NOT NULL
        );

        CREATE TABLE IF NOT EXISTS lash_session_meta (
            session_id TEXT PRIMARY KEY,
            meta_json TEXT NOT NULL
        );

        CREATE TABLE IF NOT EXISTS lash_runtime_turn_commits (
            session_id TEXT NOT NULL,
            turn_id TEXT NOT NULL,
            turn_commit_hash TEXT NOT NULL,
            result_json TEXT NOT NULL,
            committed_at_ms BIGINT NOT NULL,
            PRIMARY KEY (session_id, turn_id)
        );

        CREATE TABLE IF NOT EXISTS lash_session_execution_leases (
            session_id TEXT PRIMARY KEY,
            lease_owner_id TEXT,
            lease_owner_incarnation_id TEXT,
            lease_owner_liveness_json TEXT,
            lease_token TEXT,
            lease_fencing_token BIGINT NOT NULL DEFAULT 0,
            lease_claimed_at_ms BIGINT NOT NULL DEFAULT 0,
            lease_expires_at_ms BIGINT NOT NULL DEFAULT 0
        );

        CREATE TABLE IF NOT EXISTS lash_queued_work_batches (
            enqueue_seq BIGSERIAL PRIMARY KEY,
            batch_id TEXT NOT NULL UNIQUE,
            session_id TEXT NOT NULL,
            source_key TEXT,
            delivery_policy TEXT NOT NULL,
            slot_policy TEXT NOT NULL,
            merge_key_json TEXT NOT NULL,
            available_at_ms BIGINT NOT NULL,
            enqueued_at_ms BIGINT NOT NULL,
            claim_id TEXT,
            claim_owner_id TEXT,
            claim_owner_incarnation_id TEXT,
            claim_owner_liveness_json TEXT,
            claim_token TEXT,
            claim_fencing_token BIGINT NOT NULL DEFAULT 0,
            claim_session_lease_generation BIGINT NOT NULL DEFAULT 0,
            UNIQUE (session_id, source_key)
        );
        CREATE INDEX IF NOT EXISTS idx_lash_queued_work_ready
            ON lash_queued_work_batches(session_id, available_at_ms, enqueue_seq);

        CREATE TABLE IF NOT EXISTS lash_queued_work_items (
            batch_id TEXT NOT NULL REFERENCES lash_queued_work_batches(batch_id) ON DELETE CASCADE,
            item_index INTEGER NOT NULL,
            item_id TEXT NOT NULL,
            payload_json TEXT NOT NULL,
            PRIMARY KEY (batch_id, item_index)
        );

        CREATE TABLE IF NOT EXISTS lash_consumed_wake_source_keys (
            session_id TEXT NOT NULL,
            source_key TEXT NOT NULL,
            consumed_at_ms BIGINT NOT NULL,
            PRIMARY KEY (session_id, source_key)
        );

        CREATE TABLE IF NOT EXISTS lash_pending_turn_inputs (
            enqueue_seq BIGSERIAL PRIMARY KEY,
            input_id TEXT NOT NULL UNIQUE,
            session_id TEXT NOT NULL,
            source_key TEXT,
            ingress_json TEXT NOT NULL,
            state TEXT NOT NULL,
            input_json TEXT NOT NULL,
            enqueued_at_ms BIGINT NOT NULL,
            claim_id TEXT,
            claim_owner_id TEXT,
            claim_owner_incarnation_id TEXT,
            claim_owner_liveness_json TEXT,
            claim_token TEXT,
            claim_fencing_token BIGINT NOT NULL DEFAULT 0,
            claim_session_lease_generation BIGINT NOT NULL DEFAULT 0,
            UNIQUE (session_id, source_key)
        );
        CREATE INDEX IF NOT EXISTS idx_lash_pending_turn_inputs_session
            ON lash_pending_turn_inputs(session_id, state, enqueue_seq);
        CREATE INDEX IF NOT EXISTS idx_lash_pending_turn_inputs_claim
            ON lash_pending_turn_inputs(session_id, claim_id, claim_token);

        CREATE TABLE IF NOT EXISTS lash_attachment_manifest (
            attachment_id TEXT NOT NULL,
            session_id TEXT NOT NULL,
            canonical_uri TEXT NOT NULL,
            intent_at_ms BIGINT NOT NULL,
            committed_at_ms BIGINT,
            owner_kind TEXT CHECK (owner_kind IN ('turn', 'process')),
            owner_id TEXT,
            CHECK ((owner_kind IS NULL) = (owner_id IS NULL)),
            PRIMARY KEY (session_id, attachment_id)
        );
        CREATE INDEX IF NOT EXISTS idx_lash_attachment_manifest_uncommitted
            ON lash_attachment_manifest(committed_at_ms)
            WHERE committed_at_ms IS NULL;
        CREATE INDEX IF NOT EXISTS idx_lash_attachment_manifest_owner
            ON lash_attachment_manifest(session_id, owner_kind, owner_id, committed_at_ms);

        DROP SEQUENCE IF EXISTS lash_process_change_seq;

        CREATE TABLE IF NOT EXISTS lash_process_change_clock (
            singleton BOOLEAN PRIMARY KEY DEFAULT TRUE,
            current_seq BIGINT NOT NULL,
            CHECK (singleton)
        );
        INSERT INTO lash_process_change_clock (singleton, current_seq)
        VALUES (TRUE, 0)
        ON CONFLICT (singleton) DO NOTHING;

        CREATE TABLE IF NOT EXISTS lash_processes (
            process_id TEXT PRIMARY KEY,
            registration_hash TEXT NOT NULL,
            owner_scope_id TEXT NOT NULL,
            created_at_ms BIGINT NOT NULL,
            updated_at_ms BIGINT NOT NULL,
            change_seq BIGINT NOT NULL,
            status TEXT NOT NULL,
            record_json TEXT NOT NULL
        );
        CREATE INDEX IF NOT EXISTS idx_lash_processes_status
            ON lash_processes(status);
        CREATE INDEX IF NOT EXISTS idx_lash_processes_change_seq
            ON lash_processes(change_seq);

        CREATE TABLE IF NOT EXISTS lash_process_events (
            process_id TEXT NOT NULL REFERENCES lash_processes(process_id) ON DELETE CASCADE,
            sequence BIGINT NOT NULL,
            event_type TEXT NOT NULL,
            payload_hash TEXT NOT NULL,
            idempotency_key TEXT,
            occurred_at_ms BIGINT NOT NULL,
            event_json TEXT NOT NULL,
            PRIMARY KEY (process_id, sequence)
        );
        CREATE UNIQUE INDEX IF NOT EXISTS idx_lash_process_events_key
            ON lash_process_events(process_id, idempotency_key)
            WHERE idempotency_key IS NOT NULL;

        CREATE TABLE IF NOT EXISTS lash_process_wake_deliveries (
            delivery_id TEXT PRIMARY KEY,
            process_id TEXT NOT NULL REFERENCES lash_processes(process_id) ON DELETE CASCADE,
            target_session_id TEXT NOT NULL,
            sequence BIGINT NOT NULL,
            state TEXT NOT NULL,
            attempts BIGINT NOT NULL DEFAULT 0,
            first_attempt_ms BIGINT,
            expires_at_ms BIGINT NOT NULL,
            discard_reason TEXT,
            evidence_cleanup_pending BOOLEAN NOT NULL DEFAULT FALSE,
            delivery_json TEXT NOT NULL
        );
        CREATE INDEX IF NOT EXISTS idx_lash_wake_deliveries_pending
            ON lash_process_wake_deliveries(expires_at_ms)
            WHERE state = 'pending';

        CREATE TABLE IF NOT EXISTS lash_process_handle_grants (
            session_id TEXT NOT NULL,
            scope_id TEXT NOT NULL,
            process_id TEXT NOT NULL REFERENCES lash_processes(process_id) ON DELETE CASCADE,
            descriptor_json TEXT NOT NULL,
            PRIMARY KEY (scope_id, process_id)
        );
        CREATE INDEX IF NOT EXISTS idx_lash_process_handle_grants_session
            ON lash_process_handle_grants(session_id);
        CREATE INDEX IF NOT EXISTS idx_lash_process_handle_grants_process
            ON lash_process_handle_grants(process_id);

        CREATE TABLE IF NOT EXISTS lash_process_leases (
            process_id TEXT PRIMARY KEY REFERENCES lash_processes(process_id) ON DELETE CASCADE,
            lease_owner_id TEXT,
            lease_owner_incarnation_id TEXT,
            lease_owner_liveness_json TEXT,
            lease_token TEXT,
            lease_fencing_token BIGINT NOT NULL DEFAULT 0,
            lease_claimed_at_ms BIGINT NOT NULL DEFAULT 0,
            lease_expires_at_ms BIGINT NOT NULL DEFAULT 0
        );

        CREATE TABLE IF NOT EXISTS lash_process_segment_handovers (
            process_id TEXT NOT NULL REFERENCES lash_processes(process_id) ON DELETE CASCADE,
            segment_ordinal BIGINT NOT NULL,
            handover_json TEXT NOT NULL,
            PRIMARY KEY (process_id, segment_ordinal)
        );

        CREATE TABLE IF NOT EXISTS lash_runtime_effect_replay (
            scope_id TEXT NOT NULL,
            session_id TEXT,
            replay_key TEXT NOT NULL,
            envelope_hash TEXT NOT NULL,
            envelope_json TEXT NOT NULL,
            status TEXT NOT NULL,
            outcome_json TEXT,
            error_json TEXT,
            lease_owner_id TEXT,
            lease_token TEXT,
            lease_expires_at_ms BIGINT NOT NULL DEFAULT 0,
            due_at_ms BIGINT,
            created_at_ms BIGINT NOT NULL,
            updated_at_ms BIGINT NOT NULL,
            PRIMARY KEY (scope_id, replay_key)
        );
        CREATE INDEX IF NOT EXISTS idx_lash_runtime_effect_replay_lease
            ON lash_runtime_effect_replay(status, lease_expires_at_ms);
        CREATE INDEX IF NOT EXISTS idx_lash_runtime_effect_replay_session
            ON lash_runtime_effect_replay(session_id);

        CREATE TABLE IF NOT EXISTS lash_await_event_meta (
            singleton BOOLEAN PRIMARY KEY DEFAULT TRUE,
            signing_secret BYTEA NOT NULL,
            CHECK (singleton)
        );

        CREATE TABLE IF NOT EXISTS lash_await_event_waits (
            key_id TEXT PRIMARY KEY,
            scope_json TEXT NOT NULL,
            wait_json TEXT NOT NULL,
            session_id TEXT,
            turn_control BOOLEAN NOT NULL,
            terminal_json TEXT,
            created_at_ms BIGINT NOT NULL,
            resolved_at_ms BIGINT
        );
        CREATE INDEX IF NOT EXISTS idx_lash_await_event_waits_session
            ON lash_await_event_waits(session_id);

        -- Permanent by design: session ids cannot be reused, so revocation
        -- evidence must remain after every retention-pruning pass.
        CREATE TABLE IF NOT EXISTS lash_await_event_revoked_sessions (
            session_id TEXT PRIMARY KEY,
            revoked_at_ms BIGINT NOT NULL
        );

        CREATE TABLE IF NOT EXISTS lash_trigger_subscriptions (
            subscription_id TEXT PRIMARY KEY,
            owner_scope TEXT NOT NULL,
            subscription_key TEXT NOT NULL,
            incarnation TEXT NOT NULL,
            revision BIGINT NOT NULL,
            definition_hash TEXT NOT NULL,
            source_type TEXT NOT NULL,
            source_key TEXT NOT NULL,
            enabled BOOLEAN NOT NULL,
            tombstoned BOOLEAN NOT NULL,
            created_at_ms BIGINT NOT NULL,
            updated_at_ms BIGINT NOT NULL,
            record_json TEXT NOT NULL,
            UNIQUE(owner_scope, subscription_key)
        );
        CREATE INDEX IF NOT EXISTS idx_lash_trigger_subscriptions_registrant
            ON lash_trigger_subscriptions(owner_scope, subscription_key);
        CREATE INDEX IF NOT EXISTS idx_lash_trigger_subscriptions_source
            ON lash_trigger_subscriptions(source_type, source_key, enabled);

        CREATE TABLE IF NOT EXISTS lash_trigger_occurrences (
            occurrence_id TEXT PRIMARY KEY,
            idempotency_key TEXT NOT NULL UNIQUE,
            request_hash TEXT NOT NULL,
            source_type TEXT NOT NULL,
            source_key TEXT NOT NULL,
            occurred_at_ms BIGINT NOT NULL,
            record_json TEXT NOT NULL
        );
        CREATE INDEX IF NOT EXISTS idx_lash_trigger_occurrences_source
            ON lash_trigger_occurrences(source_type, source_key, occurred_at_ms);

        CREATE TABLE IF NOT EXISTS lash_trigger_deliveries (
            occurrence_id TEXT NOT NULL REFERENCES lash_trigger_occurrences(occurrence_id) ON DELETE CASCADE,
            subscription_id TEXT NOT NULL,
            process_id TEXT NOT NULL,
            subscription_incarnation TEXT NOT NULL,
            subscription_revision BIGINT NOT NULL,
            subscription_snapshot_json TEXT NOT NULL,
            created_at_ms BIGINT NOT NULL,
            PRIMARY KEY (occurrence_id, subscription_id)
        );
        CREATE TABLE IF NOT EXISTS lash_trigger_mutation_receipts (
            operation_id TEXT PRIMARY KEY,
            request_hash TEXT NOT NULL,
            result_json TEXT NOT NULL,
            created_at_ms BIGINT NOT NULL
        );
        CREATE INDEX IF NOT EXISTS idx_lash_trigger_deliveries_subscription
            ON lash_trigger_deliveries(subscription_id);
        CREATE INDEX IF NOT EXISTS idx_lash_trigger_deliveries_process
            ON lash_trigger_deliveries(process_id);

        CREATE TABLE IF NOT EXISTS lash_lashlang_artifacts (
            namespace TEXT NOT NULL,
            artifact_ref TEXT NOT NULL,
            artifact_bytes BYTEA NOT NULL,
            PRIMARY KEY (namespace, artifact_ref)
        );
        "#,
    )
    .await
    .map_err(store_sqlx_error)?;

    let existing: Option<i32> =
        sqlx::query_scalar("SELECT version FROM lash_schema_versions WHERE component = $1")
            .bind(SCHEMA_COMPONENT)
            .fetch_optional(&mut *tx)
            .await
            .map_err(store_sqlx_error)?;
    match existing {
        Some(version) if version == SCHEMA_VERSION => {}
        Some(version) => {
            return Err(StoreError::Backend(format!(
                "Postgres schema component `{SCHEMA_COMPONENT}` has version {version}, expected {SCHEMA_VERSION}"
            )));
        }
        None => {
            sqlx::query("INSERT INTO lash_schema_versions (component, version) VALUES ($1, $2)")
                .bind(SCHEMA_COMPONENT)
                .bind(SCHEMA_VERSION)
                .execute(&mut *tx)
                .await
                .map_err(store_sqlx_error)?;
        }
    }
    let mut secret_candidate = Vec::with_capacity(32);
    secret_candidate.extend_from_slice(uuid::Uuid::new_v4().as_bytes());
    secret_candidate.extend_from_slice(uuid::Uuid::new_v4().as_bytes());
    sqlx::query(
        "INSERT INTO lash_await_event_meta (singleton, signing_secret)
         VALUES (TRUE, $1)
         ON CONFLICT (singleton) DO NOTHING",
    )
    .bind(secret_candidate)
    .execute(&mut *tx)
    .await
    .map_err(store_sqlx_error)?;
    let signing_secret: Vec<u8> = sqlx::query_scalar(
        "SELECT signing_secret FROM lash_await_event_meta WHERE singleton = TRUE",
    )
    .fetch_one(&mut *tx)
    .await
    .map_err(store_sqlx_error)?;
    if signing_secret.len() != 32 {
        return Err(StoreError::Backend(format!(
            "Postgres await-event signing secret has {} bytes, expected 32",
            signing_secret.len()
        )));
    }
    tx.commit().await.map_err(store_sqlx_error)?;
    Ok(signing_secret)
}
