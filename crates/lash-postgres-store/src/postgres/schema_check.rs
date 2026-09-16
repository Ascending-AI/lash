// Schema-13 projections derived from the CREATE TABLE statements in schema.rs.
// These check relation and column availability, not types or constraints. The
// host's migration/conformance tests remain responsible for those contracts.
const SCHEMA_PROBES: &[&str] = &[
    "SELECT component, version FROM lash_schema_versions LIMIT 0",
    "SELECT hash, content FROM lash_blobs LIMIT 0",
    "SELECT session_id, head_revision, head_json, checkpoint_ref FROM lash_sessions LIMIT 0",
    "SELECT session_id, seq, node_id, node_json, tombstoned FROM lash_graph_nodes LIMIT 0",
    "SELECT seq, session_id, entry_json FROM lash_usage_deltas LIMIT 0",
    "SELECT session_id, meta_json FROM lash_session_meta LIMIT 0",
    "SELECT session_id, turn_id, turn_commit_hash, result_json, committed_at_ms FROM lash_runtime_turn_commits LIMIT 0",
    "SELECT session_id, lease_owner_id, lease_owner_incarnation_id, lease_owner_liveness_json, lease_token, lease_fencing_token, lease_claimed_at_ms, lease_expires_at_ms FROM lash_session_execution_leases LIMIT 0",
    "SELECT enqueue_seq, batch_id, session_id, source_key, delivery_policy, slot_policy, merge_key_json, available_at_ms, enqueued_at_ms, claim_id, claim_owner_id, claim_owner_incarnation_id, claim_owner_liveness_json, claim_token, claim_fencing_token, claim_session_lease_generation FROM lash_queued_work_batches LIMIT 0",
    "SELECT batch_id, item_index, item_id, payload_json FROM lash_queued_work_items LIMIT 0",
    "SELECT enqueue_seq, input_id, session_id, source_key, ingress_json, state, input_json, enqueued_at_ms, claim_id, claim_owner_id, claim_owner_incarnation_id, claim_owner_liveness_json, claim_token, claim_fencing_token, claim_session_lease_generation FROM lash_pending_turn_inputs LIMIT 0",
    "SELECT attachment_id, session_id, canonical_uri, intent_at_ms, committed_at_ms FROM lash_attachment_manifest LIMIT 0",
    "SELECT singleton, current_seq FROM lash_process_change_clock LIMIT 0",
    "SELECT process_id, registration_hash, owner_scope_id, created_at_ms, updated_at_ms, change_seq, status, record_json FROM lash_processes LIMIT 0",
    "SELECT process_id, sequence, event_type, payload_hash, idempotency_key, occurred_at_ms, event_json FROM lash_process_events LIMIT 0",
    "SELECT process_id, sequence FROM lash_process_wake_acks LIMIT 0",
    "SELECT session_id, scope_id, process_id, descriptor_json FROM lash_process_handle_grants LIMIT 0",
    "SELECT process_id, lease_owner_id, lease_owner_incarnation_id, lease_owner_liveness_json, lease_token, lease_fencing_token, lease_claimed_at_ms, lease_expires_at_ms FROM lash_process_leases LIMIT 0",
    "SELECT process_id, segment_ordinal, handover_json FROM lash_process_segment_handovers LIMIT 0",
    "SELECT scope_id, replay_key, envelope_hash, status, outcome_json, error_json, lease_owner_id, lease_token, lease_expires_at_ms, due_at_ms, created_at_ms, updated_at_ms FROM lash_runtime_effect_replay LIMIT 0",
    "SELECT subscription_id, registrant_scope_id, handle, source_type, source_key, enabled, created_at_ms, updated_at_ms, record_json FROM lash_trigger_subscriptions LIMIT 0",
    "SELECT occurrence_id, idempotency_key, request_hash, source_type, source_key, occurred_at_ms, record_json FROM lash_trigger_occurrences LIMIT 0",
    "SELECT occurrence_id, subscription_id, process_id, created_at_ms FROM lash_trigger_deliveries LIMIT 0",
    "SELECT namespace, artifact_ref, artifact_bytes FROM lash_lashlang_artifacts LIMIT 0",
];

async fn verify_schema_version(connection: &mut sqlx::PgConnection) -> Result<(), StoreError> {
    let existing: Option<i32> = sqlx::query_scalar(
        "SELECT version FROM lash_schema_versions WHERE component = $1",
    )
    .bind(SCHEMA_COMPONENT)
    .fetch_optional(connection)
    .await
    .map_err(|error| {
        StoreError::Backend(format!(
            "cannot read Lash schema version; run host migrations before opening storage: {error}"
        ))
    })?;
    if existing != Some(SCHEMA_VERSION) {
        return Err(StoreError::Backend(format!(
            "Postgres schema component `{SCHEMA_COMPONENT}` has version {existing:?}, expected {SCHEMA_VERSION}; run host migrations before opening storage"
        )));
    }
    Ok(())
}

async fn verify_provisioned_schema(pool: &PgPool) -> Result<(), StoreError> {
    let mut tx = pool.begin().await.map_err(store_sqlx_error)?;
    tx.execute("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ, READ ONLY")
        .await
        .map_err(store_sqlx_error)?;
    verify_schema_version(&mut tx).await?;
    for statement in SCHEMA_PROBES {
        tx.execute(*statement).await.map_err(|error| {
            StoreError::Backend(format!(
                "Lash schema projection failed ({statement}); run host migrations: {error}"
            ))
        })?;
    }
    let clock_present: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM lash_process_change_clock WHERE singleton = TRUE)",
    )
    .fetch_one(&mut *tx)
    .await
    .map_err(store_sqlx_error)?;
    if !clock_present {
        return Err(StoreError::Backend(
            "Lash process change clock seed is missing; run host migrations".to_string(),
        ));
    }
    tx.commit().await.map_err(store_sqlx_error)
}
