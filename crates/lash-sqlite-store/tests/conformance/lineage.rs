use super::*;

struct SqliteLineageConformanceInjector {
    path: PathBuf,
    _dir: TempDir,
}

#[async_trait::async_trait]
impl LineageConformanceInjector for SqliteLineageConformanceInjector {
    async fn force_lineage(&self, session_id: &SessionId, ancestor_node_id: &str) {
        let conn = rusqlite::Connection::open(&self.path).expect("open SQLite lineage catalog");
        let (ancestor_session_id, generation): (String, i64) = conn
            .query_row(
                "SELECT session_id, generation FROM graph_nodes WHERE node_id = ?1",
                rusqlite::params![ancestor_node_id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .expect("read false-lineage ancestor facts");
        conn.execute(
            "INSERT OR REPLACE INTO fork_lineage
             (session_id, ancestor_session_id, fork_node_id, fork_generation)
             VALUES (?1, ?2, ?3, ?4)",
            rusqlite::params![
                session_id.as_str(),
                ancestor_session_id,
                ancestor_node_id,
                generation
            ],
        )
        .expect("inject false SQLite lineage");
    }

    async fn tombstone_node(&self, node_id: &str) {
        let conn = rusqlite::Connection::open(&self.path).expect("open SQLite lineage catalog");
        assert_eq!(
            conn.execute(
                "UPDATE graph_nodes SET tombstoned = 1 WHERE node_id = ?1",
                rusqlite::params![node_id],
            )
            .expect("tombstone intermediate SQLite node"),
            1
        );
    }

    async fn lineage_ancestors(
        &self,
        session_id: &SessionId,
    ) -> Vec<lash_core::store::ForkLineageAncestor> {
        let conn = rusqlite::Connection::open(&self.path).expect("open SQLite lineage catalog");
        let mut stmt = conn
            .prepare(
                "SELECT ancestor_session_id, fork_node_id, fork_generation FROM fork_lineage
                 WHERE session_id = ?1 ORDER BY ancestor_session_id",
            )
            .expect("prepare SQLite lineage observation");
        stmt.query_map(rusqlite::params![session_id.as_str()], |row| {
            Ok(lash_core::store::ForkLineageAncestor {
                ancestor_session_id: SessionId::from(row.get::<_, String>(0)?),
                fork_node_id: row.get(1)?,
                fork_generation: u64::try_from(row.get::<_, i64>(2)?)
                    .expect("non-negative fork generation"),
            })
        })
        .expect("query SQLite lineage observation")
        .collect::<Result<Vec<_>, _>>()
        .expect("collect SQLite lineage observation")
    }

    async fn edge_path(&self, session_id: &SessionId) -> Vec<GraphFactObservation> {
        let mut facts = self.all_graph_facts().await;
        let conn = rusqlite::Connection::open(&self.path).expect("open SQLite lineage catalog");
        let mut current = conn
            .query_row(
                "SELECT leaf_node_id FROM session_head WHERE session_id = ?1",
                rusqlite::params![session_id.as_str()],
                |row| row.get::<_, Option<String>>(0),
            )
            .expect("read SQLite lineage head");
        let mut path = Vec::new();
        while let Some(node_id) = current {
            let index = facts
                .iter()
                .position(|fact| fact.node_id == node_id)
                .expect("edge-path node exists in raw SQLite facts");
            let fact = facts.swap_remove(index);
            current = fact.parent_node_id.clone();
            path.push(fact);
        }
        path.reverse();
        path
    }

    async fn all_graph_facts(&self) -> Vec<GraphFactObservation> {
        let conn = rusqlite::Connection::open(&self.path).expect("open SQLite lineage catalog");
        let mut stmt = conn
            .prepare(
                "SELECT node.node_id, node.parent_node_id, node.session_id,
                        node.generation, node.frame_node_id,
                        json_extract(node.node_json, '$.kind') = 'frame_open'
                 FROM graph_nodes AS node
                 ORDER BY node.generation, node.node_id",
            )
            .expect("prepare SQLite graph facts");
        stmt.query_map([], |row| {
            Ok(GraphFactObservation {
                node_id: row.get(0)?,
                parent_node_id: row.get(1)?,
                owning_session_id: SessionId::from(row.get::<_, String>(2)?),
                generation: u64::try_from(row.get::<_, i64>(3)?).expect("non-negative generation"),
                frame_node_id: row.get(4)?,
                is_frame: row.get(5)?,
            })
        })
        .expect("query SQLite graph facts")
        .collect::<Result<Vec<_>, _>>()
        .expect("collect SQLite graph facts")
    }
}

fn sqlite_lineage_handles() -> LineageConformanceHandles {
    let dir = tempfile::tempdir().expect("SQLite lineage tempdir");
    let path = dir.path().join("durable-core.db");
    LineageConformanceHandles {
        factory: Arc::new(SqliteSessionStoreFactory::new(dir.path())),
        injector: Arc::new(SqliteLineageConformanceInjector { path, _dir: dir }),
    }
}

#[tokio::test]
async fn sqlite_fork_lineage_conformance() {
    lash_conformance::fork_lineage_conformance(sqlite_lineage_handles()).await;
}

#[tokio::test]
async fn sqlite_fork_lineage_no_carrier_law() {
    lash_conformance::fork_lineage_no_carrier_law(sqlite_lineage_handles()).await;
}

#[tokio::test]
async fn sqlite_fork_plan_matches_edge_walk_law() {
    lash_conformance::fork_plan_matches_edge_walk_law(sqlite_lineage_handles()).await;
}
