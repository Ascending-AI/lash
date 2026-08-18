//! Backend adapters that let the shared conformance suites drive raw
//! PostgreSQL state: lineage forcing and fence-integrity injection.

use super::*;

pub(crate) struct PostgresLineageConformanceInjector {
    pub(crate) storage: Arc<PostgresStorage>,
}

#[async_trait::async_trait]
impl LineageConformanceInjector for PostgresLineageConformanceInjector {
    async fn force_lineage(&self, session_id: &str, ancestor_node_id: &str) {
        sqlx::query(
            "INSERT INTO lash_fork_lineage
             (session_id, ancestor_session_id, fork_node_id, fork_generation)
             SELECT $1, session_id, node_id, generation
             FROM lash_graph_nodes WHERE node_id = $2
             ON CONFLICT (session_id, ancestor_session_id) DO UPDATE SET
                 fork_node_id = EXCLUDED.fork_node_id,
                 fork_generation = EXCLUDED.fork_generation",
        )
        .bind(session_id)
        .bind(ancestor_node_id)
        .execute(self.storage.pool())
        .await
        .expect("inject false Postgres lineage");
    }

    async fn tombstone_node(&self, node_id: &str) {
        let result =
            sqlx::query("UPDATE lash_graph_nodes SET tombstoned = TRUE WHERE node_id = $1")
                .bind(node_id)
                .execute(self.storage.pool())
                .await
                .expect("tombstone intermediate Postgres node");
        assert_eq!(result.rows_affected(), 1);
    }

    async fn lineage_ancestors(
        &self,
        session_id: &str,
    ) -> Vec<lash_core::store::ForkLineageAncestor> {
        sqlx::query_as::<_, (String, String, i64)>(
            "SELECT ancestor_session_id, fork_node_id, fork_generation
             FROM lash_fork_lineage
             WHERE session_id = $1 ORDER BY ancestor_session_id",
        )
        .bind(session_id)
        .fetch_all(self.storage.pool())
        .await
        .expect("observe Postgres lineage")
        .into_iter()
        .map(|(ancestor_session_id, fork_node_id, fork_generation)| {
            lash_core::store::ForkLineageAncestor {
                ancestor_session_id,
                fork_node_id,
                fork_generation: u64::try_from(fork_generation)
                    .expect("non-negative fork generation"),
            }
        })
        .collect()
    }

    async fn edge_path(&self, session_id: &str) -> Vec<GraphFactObservation> {
        let mut facts = self.all_graph_facts().await;
        let mut current = sqlx::query_scalar::<_, String>(
            "SELECT leaf_node_id FROM lash_sessions
             WHERE session_id = $1 AND leaf_node_id IS NOT NULL",
        )
        .bind(session_id)
        .fetch_optional(self.storage.pool())
        .await
        .expect("read Postgres lineage head");
        let mut path = Vec::new();
        while let Some(node_id) = current {
            let index = facts
                .iter()
                .position(|fact| fact.node_id == node_id)
                .expect("edge-path node exists in raw Postgres facts");
            let fact = facts.swap_remove(index);
            current = fact.parent_node_id.clone();
            path.push(fact);
        }
        path.reverse();
        path
    }

    async fn all_graph_facts(&self) -> Vec<GraphFactObservation> {
        use sqlx::Row;
        sqlx::query(
            "SELECT node.node_id, node.parent_node_id, node.session_id,
                    node.generation, node.frame_node_id,
                    node.node_json::jsonb ->> 'kind' = 'frame_open' AS is_frame
             FROM lash_graph_nodes AS node
             ORDER BY node.generation, node.node_id",
        )
        .fetch_all(self.storage.pool())
        .await
        .expect("observe Postgres graph facts")
        .into_iter()
        .map(|row| GraphFactObservation {
            node_id: row.get(0),
            parent_node_id: row.get(1),
            owning_session_id: row.get(2),
            generation: u64::try_from(row.get::<i64, _>(3)).expect("non-negative generation"),
            frame_node_id: row.get(4),
            is_frame: row.get(5),
        })
        .collect()
    }
}

pub(crate) struct PostgresFenceIntegrityInjector {
    pub(crate) _database_lock: SharedDatabaseLock,
    pub(crate) storage: Arc<PostgresStorage>,
}

#[async_trait::async_trait]
impl FenceIntegrityInjector for PostgresFenceIntegrityInjector {
    async fn inject_raw_value(&self, target: &FenceIntegrityTarget, value: i64) {
        let result = match target {
            FenceIntegrityTarget::QueuedWorkClaimFence { batch_id } => {
                sqlx::query(
                    "UPDATE lash_queued_work_batches
                 SET claim_fencing_token = $1 WHERE batch_id = $2",
                )
                .bind(value)
                .bind(batch_id)
                .execute(self.storage.pool())
                .await
            }
            FenceIntegrityTarget::SessionHeadRevision { session_id } => {
                sqlx::query("UPDATE lash_sessions SET head_revision = $1 WHERE session_id = $2")
                    .bind(value)
                    .bind(session_id)
                    .execute(self.storage.pool())
                    .await
            }
            FenceIntegrityTarget::SessionLeaseFencingToken { session_id } => {
                sqlx::query(
                    "UPDATE lash_session_execution_leases
                 SET lease_fencing_token = $1 WHERE session_id = $2",
                )
                .bind(value)
                .bind(session_id)
                .execute(self.storage.pool())
                .await
            }
            FenceIntegrityTarget::TriggerRevision { subscription_id } => {
                sqlx::query(
                    "UPDATE lash_trigger_subscriptions
                 SET revision = $1,
                     record_json = jsonb_set(
                         record_json::jsonb,
                         '{revision}',
                         to_jsonb($1::bigint)
                     )::text
                 WHERE subscription_id = $2",
                )
                .bind(value)
                .bind(subscription_id)
                .execute(self.storage.pool())
                .await
            }
        }
        .expect("inject raw Postgres fence value");
        assert_eq!(
            result.rows_affected(),
            1,
            "raw Postgres fence injection must target one row"
        );
    }

    async fn observe_raw_value(&self, target: &FenceIntegrityTarget) -> FenceIntegrityObservation {
        match target {
            FenceIntegrityTarget::QueuedWorkClaimFence { batch_id } => {
                let (value, claim_id, claim_token, generation): (
                    i64,
                    Option<String>,
                    Option<String>,
                    i64,
                ) = sqlx::query_as(
                    "SELECT claim_fencing_token, claim_id, claim_token,
                            claim_session_lease_generation
                     FROM lash_queued_work_batches WHERE batch_id = $1",
                )
                .bind(batch_id)
                .fetch_one(self.storage.pool())
                .await
                .expect("observe Postgres queued-work fence");
                FenceIntegrityObservation {
                    value,
                    mutation_fingerprint: format!("{claim_id:?}:{claim_token:?}:{generation}"),
                }
            }
            FenceIntegrityTarget::SessionHeadRevision { session_id } => {
                let (value, head_json, leaf, checkpoint): (
                    i64,
                    String,
                    Option<String>,
                    Option<String>,
                ) = sqlx::query_as(
                    "SELECT head_revision, head_json, leaf_node_id, checkpoint_ref
                     FROM lash_sessions WHERE session_id = $1",
                )
                .bind(session_id)
                .fetch_one(self.storage.pool())
                .await
                .expect("observe Postgres session-head revision");
                FenceIntegrityObservation {
                    value,
                    mutation_fingerprint: format!("{head_json}:{leaf:?}:{checkpoint:?}"),
                }
            }
            FenceIntegrityTarget::SessionLeaseFencingToken { session_id } => {
                let (value, owner, token, claimed, expires): (
                    i64,
                    Option<String>,
                    Option<String>,
                    i64,
                    i64,
                ) = sqlx::query_as(
                    "SELECT lease_fencing_token, lease_owner_id, lease_token,
                            lease_claimed_at_ms, lease_expires_at_ms
                     FROM lash_session_execution_leases WHERE session_id = $1",
                )
                .bind(session_id)
                .fetch_one(self.storage.pool())
                .await
                .expect("observe Postgres session-lease fence");
                FenceIntegrityObservation {
                    value,
                    mutation_fingerprint: format!("{owner:?}:{token:?}:{claimed}:{expires}"),
                }
            }
            FenceIntegrityTarget::TriggerRevision { subscription_id } => {
                let (value, json, enabled, tombstoned): (i64, String, bool, bool) = sqlx::query_as(
                    "SELECT revision, record_json, enabled, tombstoned
                         FROM lash_trigger_subscriptions WHERE subscription_id = $1",
                )
                .bind(subscription_id)
                .fetch_one(self.storage.pool())
                .await
                .expect("observe Postgres trigger revision");
                FenceIntegrityObservation {
                    value,
                    mutation_fingerprint: format!("{json}:{enabled}:{tombstoned}"),
                }
            }
        }
    }
}
