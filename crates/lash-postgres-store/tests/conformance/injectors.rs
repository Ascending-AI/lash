//! Backend adapters that let the shared conformance suites shift raw
//! PostgreSQL state: lineage forcing and fence-integrity injection.

use super::*;
use lash_sansio::SessionId;

pub(crate) struct PostgresLineageConformanceInjector {
    pub(crate) storage: Arc<PostgresStorage>,
}

#[async_trait::async_trait]
impl LineageConformanceInjector for PostgresLineageConformanceInjector {
    async fn force_lineage(&self, session_id: &SessionId, ancestor_node_id: &str) {
        sqlx::query(
            "INSERT INTO lash_fork_lineage
             (session_id, ancestor_session_id, fork_node_id, fork_generation)
             SELECT $1, session_id, node_id, generation
             FROM lash_graph_nodes WHERE node_id = $2
             ON CONFLICT (session_id, ancestor_session_id) DO UPDATE SET
                 fork_node_id = EXCLUDED.fork_node_id,
                 fork_generation = EXCLUDED.fork_generation",
        )
        .bind(session_id.as_str())
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
        session_id: &SessionId,
    ) -> Vec<lash_core_execution::store::ForkLineageAncestor> {
        sqlx::query_as::<_, (String, String, i64)>(
            "SELECT ancestor_session_id, fork_node_id, fork_generation
             FROM lash_fork_lineage
             WHERE session_id = $1 ORDER BY ancestor_session_id",
        )
        .bind(session_id.as_str())
        .fetch_all(self.storage.pool())
        .await
        .expect("observe Postgres lineage")
        .into_iter()
        .map(|(ancestor_session_id, fork_node_id, fork_generation)| {
            lash_core_execution::store::ForkLineageAncestor {
                ancestor_session_id: SessionId::fixture(ancestor_session_id),
                fork_node_id: lash_core_execution::NodeId::fixture(fork_node_id),
                fork_generation: u64::try_from(fork_generation)
                    .expect("non-negative fork generation"),
            }
        })
        .collect()
    }

    async fn edge_path(&self, session_id: &SessionId) -> Vec<GraphFactObservation> {
        let mut facts = self.all_graph_facts().await;
        let mut current = sqlx::query_scalar::<_, String>(
            "SELECT leaf_node_id FROM lash_session_head JOIN lash_session_revisions USING (session_id, head_revision)
             WHERE session_id = $1 AND leaf_node_id IS NOT NULL",
        )
        .bind(session_id.as_str())
        .fetch_optional(self.storage.pool())
        .await
        .expect("read Postgres lineage head")
        .map(lash_core_execution::NodeId::fixture);
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
            node_id: lash_core_execution::NodeId::fixture(row.get::<String, _>(0)),
            parent_node_id: row
                .get::<Option<String>, _>(1)
                .map(lash_core_execution::NodeId::fixture),
            owning_session_id: SessionId::fixture(row.get::<String, _>(2)),
            generation: u64::try_from(row.get::<i64, _>(3)).expect("non-negative generation"),
            frame_node_id: lash_core_execution::NodeId::fixture(row.get::<String, _>(4)),
            is_frame: row.get(5),
        })
        .collect()
    }
}

pub(crate) struct PostgresFenceIntegrityInjector {
    pub(crate) _database_fixture: IsolatedDatabase,
    pub(crate) storage: Arc<PostgresStorage>,
}

#[async_trait::async_trait]
impl FenceIntegrityInjector for PostgresFenceIntegrityInjector {
    async fn inject_raw_value(&self, target: &FenceIntegrityTarget, value: i64) {
        let result = match target {
            FenceIntegrityTarget::SessionHeadRevision { session_id } => {
                let mut tx = self
                    .storage
                    .pool()
                    .begin()
                    .await
                    .expect("begin pointer fault injection");
                sqlx::query("SET LOCAL session_replication_role = replica")
                    .execute(&mut *tx)
                    .await
                    .expect("enable pointer fault injection");
                let result = sqlx::query(
                    "UPDATE lash_session_head SET head_revision = $1 WHERE session_id = $2",
                )
                .bind(value)
                .bind(session_id.as_str())
                .execute(&mut *tx)
                .await;
                tx.commit().await.expect("commit corrupt pointer");
                result
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
            FenceIntegrityTarget::SessionHeadRevision { session_id } => {
                let (value, head_json, leaf, checkpoint): (
                    i64,
                    String,
                    Option<String>,
                    Option<String>,
                ) = sqlx::query_as(
                    "SELECT head.head_revision, revision.head_json, revision.leaf_node_id, revision.checkpoint_ref
                     FROM lash_session_head AS head JOIN lash_session_revisions AS revision USING (session_id)
                     WHERE head.session_id = $1 ORDER BY revision.head_revision DESC LIMIT 1",
                )
                .bind(session_id.as_str())
                .fetch_one(self.storage.pool())
                .await
                .expect("observe Postgres session-head revision");
                FenceIntegrityObservation {
                    value,
                    mutation_fingerprint: format!("{head_json}:{leaf:?}:{checkpoint:?}"),
                }
            }
        }
    }
}

/// A published pointer must name a revision; enforcement waits until commit.
#[tokio::test]
async fn session_head_pointer_requires_a_revision() {
    let (_database, storage) = storage().await.expect("hermetic PostgreSQL");
    storage
        .session_store_factory()
        .admit_session(
            &lash_core_execution::testing::store_fixtures::root_session_request(
                &SessionId::fixture("head-pointer"),
            ),
        )
        .await
        .expect("create a session with revision zero");
    let mut tx = storage
        .pool()
        .begin()
        .await
        .expect("begin pointer publication");
    sqlx::query("UPDATE lash_session_head SET head_revision = 1 WHERE session_id = 'head-pointer'")
        .execute(&mut *tx)
        .await
        .expect("pointer enforcement is deferred");
    let error = tx
        .commit()
        .await
        .expect_err("a dangling head pointer cannot commit");
    assert_eq!(
        error
            .as_database_error()
            .and_then(|error| error.code())
            .as_deref(),
        Some("23503")
    );
    sqlx::query(
        "INSERT INTO lash_session_revisions (session_id, head_revision, head_json)
         SELECT session_id, 1, head_json FROM lash_session_revisions
         WHERE session_id = 'head-pointer' AND head_revision = 0",
    )
    .execute(storage.pool())
    .await
    .expect("record an unpublished revision");
    let revisions = storage
        .session_store_factory()
        .revisions(&SessionId::fixture("head-pointer"))
        .await
        .expect("list retained revisions");
    assert_eq!(
        revisions
            .iter()
            .map(|row| (row.head_revision, row.head))
            .collect::<Vec<_>>(),
        vec![(0, true), (1, false)],
        "the published pointer defines the head, even with a newer revision row"
    );
}
