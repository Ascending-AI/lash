use super::*;

impl RawDurableReader {
    pub(super) fn detach_store(&mut self) {
        match self {
            Self::InMemory { .. } => {}
            Self::Sqlite { store, .. } | Self::Postgres { store, .. } => {
                store.take();
            }
        }
    }

    pub(super) async fn observe(&self) -> RawDurableState {
        match self {
            Self::InMemory {
                store,
                factory,
                session_id,
            } => {
                let durable_nodes = store
                    .raw_graph_nodes_for_testing()
                    .into_iter()
                    .enumerate()
                    .map(|(ordinal, node)| DurableNode {
                        ordinal,
                        node_id: node.node_id.clone(),
                        parent_node_id: node.parent_node_id.clone(),
                        bytes: normalized_in_memory_node_json(&node),
                    })
                    .collect();
                let pending_turn_inputs = store
                    .raw_pending_turn_inputs_for_testing()
                    .into_iter()
                    .map(|(input_id, state, claim_session_lease_generation)| {
                        PendingTurnInputObservation {
                            input_id,
                            state,
                            claim_session_lease_generation,
                        }
                    })
                    .collect();
                let queued_work = store
                    .raw_queued_work_for_testing()
                    .into_iter()
                    .enumerate()
                    .map(
                        |(
                            ordinal,
                            (
                                batch,
                                claim_id_present,
                                claim_owner,
                                claim_token_present,
                                claim_fencing_token,
                                claim_session_lease_generation,
                            ),
                        )| {
                            queued_work_observation(
                                ordinal,
                                batch,
                                claim_id_present,
                                claim_owner,
                                claim_token_present,
                                claim_fencing_token,
                                claim_session_lease_generation,
                            )
                        },
                    )
                    .collect();
                let session_owned_artifact_refs = session_owned_artifact_ref_observations(
                    store
                        .raw_session_owned_artifact_refs_for_testing(session_id)
                        .await
                        .expect("read in-memory session-owned artifact refs"),
                );
                let checkpoint = store.raw_checkpoint_for_testing().map(|checkpoint| {
                    checkpoint_observation(store.raw_checkpoint_ref_for_testing(), checkpoint)
                });
                let runtime_turn_commits = store
                    .raw_runtime_turn_commits_for_testing()
                    .into_iter()
                    .map(
                        |(operation, turn_commit_hash, result)| RuntimeTurnCommitObservation {
                            operation,
                            turn_commit_hash,
                            result: serde_json::to_value(result)
                                .expect("encode in-memory turn-commit result"),
                        },
                    )
                    .collect();
                let attachment_manifest = store
                    .raw_attachment_manifest_for_testing()
                    .into_iter()
                    .map(attachment_manifest_observation)
                    .collect();
                let node_anchors = factory
                    .raw_node_anchors_for_testing()
                    .into_iter()
                    .map(
                        |(node_id, checkpoint_ref, source_session_id)| NodeAnchorObservation {
                            node_id,
                            checkpoint_ref,
                            source_session_id,
                        },
                    )
                    .collect();
                let usage_deltas = store
                    .raw_usage_deltas_for_testing()
                    .into_iter()
                    .map(usage_delta_observation)
                    .collect();
                let session_meta = store
                    .raw_session_meta_for_testing()
                    .map(session_meta_observation);
                let session_execution_leases = store
                    .raw_session_execution_leases_for_testing()
                    .into_iter()
                    .map(
                        |(
                            _session_id,
                            owner,
                            lease_token_present,
                            fencing_token,
                            claimed_at_epoch_ms,
                            expires_at_epoch_ms,
                        )| SessionExecutionLeaseObservation {
                            owner,
                            lease_token_present,
                            fencing_token,
                            claimed: claimed_at_epoch_ms != 0,
                            ttl_ms: (claimed_at_epoch_ms != 0)
                                .then_some(expires_at_epoch_ms - claimed_at_epoch_ms),
                        },
                    )
                    .collect();
                RawDurableState {
                    head_revision: store.raw_head_revision_for_testing(),
                    leaf_node_id: store.raw_leaf_node_id_for_testing(),
                    checkpoint,
                    durable_nodes,
                    runtime_turn_commits,
                    attachment_manifest,
                    node_anchors,
                    usage_deltas,
                    session_meta,
                    session_execution_leases,
                    pending_turn_inputs,
                    queued_work,
                    session_owned_artifact_refs,
                }
            }
            Self::Sqlite {
                path,
                session_id,
                store,
            } => {
                read_sqlite_durable_state(
                    path,
                    session_id,
                    store
                        .as_ref()
                        .expect("SQLite reader is attached to a store"),
                )
                .await
            }
            Self::Postgres {
                pool,
                session_id,
                store,
            } => {
                let store = store
                    .as_ref()
                    .expect("Postgres reader is attached to a store");
                let head: Option<(i64, Option<String>, Option<String>)> = sqlx::query_as(
                    "SELECT head_revision, leaf_node_id, checkpoint_ref
                     FROM lash_sessions
                     WHERE session_id = $1",
                )
                .bind(session_id)
                .fetch_optional(pool)
                .await
                .expect("read Postgres durable head");
                let (head_revision, leaf_node_id, checkpoint_ref) = head.map_or(
                    (None, None, None),
                    |(revision, leaf_node_id, checkpoint_ref)| {
                        (
                            Some(revision as u64),
                            leaf_node_id,
                            checkpoint_ref.map(BlobRef),
                        )
                    },
                );
                let checkpoint = read_postgres_checkpoint_observation(pool, checkpoint_ref).await;
                let rows: Vec<(i64, String, Option<String>, String)> = sqlx::query_as(
                    "SELECT seq, node_id, parent_node_id, node_json
                     FROM lash_graph_nodes
                     WHERE session_id = $1 AND tombstoned = FALSE
                     ORDER BY seq ASC",
                )
                .bind(session_id)
                .fetch_all(pool)
                .await
                .expect("read Postgres durable nodes");
                let durable_nodes = rows
                    .into_iter()
                    .enumerate()
                    .map(
                        |(ordinal, (_seq, node_id, parent_node_id, node_json))| DurableNode {
                            ordinal,
                            node_id,
                            parent_node_id,
                            bytes: normalized_sql_node_json(&node_json),
                        },
                    )
                    .collect();
                let receipt_rows: Vec<(String, String, String)> = sqlx::query_as(
                    "SELECT turn_id, turn_commit_hash, result_json
                     FROM lash_runtime_turn_commits
                     WHERE session_id = $1
                     ORDER BY turn_id ASC",
                )
                .bind(session_id)
                .fetch_all(pool)
                .await
                .expect("read Postgres turn-commit receipts");
                let runtime_turn_commits = receipt_rows
                    .into_iter()
                    .map(|(operation, turn_commit_hash, result_json)| {
                        RuntimeTurnCommitObservation {
                            operation,
                            turn_commit_hash,
                            result: serde_json::from_str(&result_json)
                                .expect("decode Postgres turn-commit result"),
                        }
                    })
                    .collect();
                let attachment_rows: Vec<AttachmentRow> = sqlx::query_as(
                    "SELECT attachment_id, canonical_uri, intent_at_ms, committed_at_ms,
                            owner_kind, owner_id
                     FROM lash_attachment_manifest
                     WHERE session_id = $1
                     ORDER BY attachment_id ASC",
                )
                .bind(session_id)
                .fetch_all(pool)
                .await
                .expect("read Postgres attachment manifest");
                let attachment_manifest = attachment_rows
                    .into_iter()
                    .map(
                        |(
                            attachment_id,
                            canonical_uri,
                            intent_at_epoch_ms,
                            committed_at_epoch_ms,
                            owner_kind,
                            owner_id,
                        )| AttachmentManifestObservation {
                            attachment_id: AttachmentId::new(attachment_id),
                            canonical_uri,
                            intent_at_epoch_ms: intent_at_epoch_ms as u64,
                            committed: committed_at_epoch_ms.is_some(),
                            owner_kind: decode_attachment_owner_kind(owner_kind.as_deref()),
                            owner_id,
                        },
                    )
                    .collect();
                let anchor_rows: Vec<(String, String, String)> = sqlx::query_as(
                    "SELECT node_id, checkpoint_ref, source_session_id
                     FROM lash_node_anchors
                     WHERE source_session_id = $1
                     ORDER BY node_id ASC",
                )
                .bind(session_id)
                .fetch_all(pool)
                .await
                .expect("read Postgres node anchors");
                let node_anchors = anchor_rows
                    .into_iter()
                    .map(
                        |(node_id, checkpoint_ref, source_session_id)| NodeAnchorObservation {
                            node_id,
                            checkpoint_ref: BlobRef(checkpoint_ref),
                            source_session_id,
                        },
                    )
                    .collect();
                let usage_rows: Vec<String> = sqlx::query_scalar(
                    "SELECT entry_json
                     FROM lash_usage_deltas
                     WHERE session_id = $1
                     ORDER BY seq ASC",
                )
                .bind(session_id)
                .fetch_all(pool)
                .await
                .expect("read Postgres usage deltas");
                let usage_deltas = usage_rows
                    .into_iter()
                    .map(|entry_json| {
                        usage_delta_observation(
                            serde_json::from_str(&entry_json).expect("decode Postgres usage delta"),
                        )
                    })
                    .collect();
                let session_meta = read_postgres_session_meta_observation(pool, session_id).await;
                let lease_rows: Vec<LeaseRow> = sqlx::query_as(
                    "SELECT lease_owner_id, lease_owner_incarnation_id,
                            lease_owner_liveness_json, lease_token,
                            lease_fencing_token, lease_claimed_at_ms, lease_expires_at_ms
                     FROM lash_session_execution_leases
                     WHERE session_id = $1",
                )
                .bind(session_id)
                .fetch_all(pool)
                .await
                .expect("read Postgres session-execution lease");
                let session_execution_leases = lease_rows
                    .into_iter()
                    .map(
                        |(
                            owner_id,
                            incarnation_id,
                            liveness_json,
                            lease_token,
                            fencing_token,
                            claimed_at_epoch_ms,
                            expires_at_epoch_ms,
                        )| SessionExecutionLeaseObservation {
                            owner: decode_lease_owner(owner_id, incarnation_id, liveness_json),
                            lease_token_present: lease_token.is_some(),
                            fencing_token: fencing_token as u64,
                            claimed: claimed_at_epoch_ms != 0,
                            ttl_ms: (claimed_at_epoch_ms != 0)
                                .then_some((expires_at_epoch_ms - claimed_at_epoch_ms) as u64),
                        },
                    )
                    .collect();
                let pending_rows: Vec<(String, String, Option<i64>)> = sqlx::query_as(
                    "SELECT input_id, state,
                            CASE WHEN claim_token IS NULL
                                 THEN NULL
                                 ELSE claim_session_lease_generation
                            END
                     FROM lash_pending_turn_inputs
                     WHERE session_id = $1
                     ORDER BY enqueue_seq ASC",
                )
                .bind(session_id)
                .fetch_all(pool)
                .await
                .expect("read Postgres pending turn inputs");
                let pending_turn_inputs = pending_rows
                    .into_iter()
                    .map(|(input_id, state, claim_session_lease_generation)| {
                        PendingTurnInputObservation {
                            input_id,
                            state: TurnInputState::from_wire_str(&state)
                                .expect("decode Postgres pending-input state"),
                            claim_session_lease_generation: claim_session_lease_generation
                                .map(|generation| generation as u64),
                        }
                    })
                    .collect();
                let queued_work_batches: Vec<QueuedWorkBatchRow> = sqlx::query_as(
                    "SELECT enqueue_seq, batch_id, source_key, delivery_policy, slot_policy,
                            merge_key_json, available_at_ms, claim_id, claim_owner_id,
                            claim_owner_incarnation_id, claim_owner_liveness_json, claim_token,
                            claim_fencing_token, claim_session_lease_generation
                     FROM lash_queued_work_batches
                     WHERE session_id = $1
                     ORDER BY enqueue_seq ASC",
                )
                .bind(session_id)
                .fetch_all(pool)
                .await
                .expect("read Postgres queued-work batches");
                let queued_work_items: Vec<QueuedWorkItemRow> = sqlx::query_as(
                    "SELECT item.batch_id, item.item_index::BIGINT, item.payload_json
                     FROM lash_queued_work_items AS item
                     JOIN lash_queued_work_batches AS batch
                       ON batch.batch_id = item.batch_id
                     WHERE batch.session_id = $1
                     ORDER BY batch.enqueue_seq ASC, item.item_index ASC",
                )
                .bind(session_id)
                .fetch_all(pool)
                .await
                .expect("read Postgres queued-work items");
                let queued_work =
                    queued_work_observations_from_sql_rows(queued_work_batches, queued_work_items);
                let session_owned_artifact_refs = session_owned_artifact_ref_observations(
                    store
                        .raw_session_owned_artifact_refs_for_testing(session_id)
                        .await
                        .expect("read Postgres session-owned artifact refs"),
                );
                RawDurableState {
                    head_revision,
                    leaf_node_id,
                    checkpoint,
                    durable_nodes,
                    runtime_turn_commits,
                    attachment_manifest,
                    node_anchors,
                    usage_deltas,
                    session_meta,
                    session_execution_leases,
                    pending_turn_inputs,
                    queued_work,
                    session_owned_artifact_refs,
                }
            }
        }
    }
}
