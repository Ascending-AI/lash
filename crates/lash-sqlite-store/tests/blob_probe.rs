use std::sync::Arc;

/// Raw probes into one durable-core catalog, reached by the name a raw SQLite
/// connection opens it through (a path, or a backend's URI).
pub(crate) struct SqliteBlobProbe {
    _guard: Option<Arc<dyn std::any::Any + Send + Sync>>,
    catalog: String,
    fault_name: &'static str,
}

impl SqliteBlobProbe {
    pub(crate) fn new(
        catalog: String,
        fault_name: &'static str,
        guard: Option<Arc<dyn std::any::Any + Send + Sync>>,
    ) -> Self {
        assert!(matches!(
            fault_name,
            "fail_session_blob_delete" | "fail_process_prune_blob_delete"
        ));
        Self {
            _guard: guard,
            catalog,
            fault_name,
        }
    }
}

#[async_trait::async_trait]
impl lash_conformance::SessionDeleteBlobProbe for SqliteBlobProbe {
    async fn session_config_ref(
        &self,
        session_id: &lash_vocabulary::identity::SessionId,
    ) -> lash_core_store::store::BlobRef {
        let config_ref: String = rusqlite::Connection::open(&self.catalog)
            .expect("open SQLite config-root probe")
            .query_row(
                "SELECT lower(hex(revision.config_ref)) FROM session_head AS head
                 JOIN session_revisions AS revision USING (session_id, head_revision)
                 WHERE head.session_id = ?1",
                rusqlite::params![session_id.as_str()],
                |row| row.get(0),
            )
            .expect("query SQLite config root");
        config_ref.into()
    }

    async fn blob_exists(&self, blob_ref: &lash_core_store::store::BlobRef) -> bool {
        rusqlite::Connection::open(&self.catalog)
            .expect("open SQLite blob probe")
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM blobs WHERE hash = unhex(?1))",
                rusqlite::params![blob_ref.as_str()],
                |row| row.get(0),
            )
            .expect("query SQLite blob existence")
    }

    async fn fail_next_blob_delete(&self) {
        rusqlite::Connection::open(&self.catalog)
            .expect("open SQLite blob-delete fault")
            .execute_batch(&format!(
                "CREATE TRIGGER {} BEFORE DELETE ON blobs BEGIN \
                 SELECT RAISE(ABORT, 'injected blob delete failure'); END;",
                self.fault_name
            ))
            .expect("install SQLite blob-delete fault");
    }

    async fn fail_next_retirement_hold(&self) {
        let conn =
            rusqlite::Connection::open(&self.catalog).expect("open SQLite retirement-hold fault");
        lash_sqlite_store::testing::cut_retirement_holds(&conn, self.fault_name)
            .expect("install SQLite retirement-hold fault");
    }

    async fn clear_injected_failure(&self) {
        rusqlite::Connection::open(&self.catalog)
            .expect("open SQLite fault cleanup")
            .execute_batch(&format!("DROP TRIGGER {}", self.fault_name))
            .expect("remove SQLite fault");
    }

    async fn checkpoint_component_edge_exists(
        &self,
        checkpoint_ref: &lash_core_store::store::BlobRef,
        blob_ref: &lash_core_store::store::BlobRef,
    ) -> Option<bool> {
        Some(
            rusqlite::Connection::open(&self.catalog)
                .expect("open SQLite checkpoint-edge probe")
                .query_row(
                    "SELECT EXISTS(
                         SELECT 1 FROM blob_edges
                         WHERE parent_hash = unhex(?1) AND child_hash = unhex(?2)
                     )",
                    rusqlite::params![checkpoint_ref.as_str(), blob_ref.as_str()],
                    |row| row.get(0),
                )
                .expect("query SQLite checkpoint edge"),
        )
    }
}

/// The definition graph laws' rows and cuts (FIG-6469), over the catalog a
/// raw SQLite connection opens.
pub(crate) struct SqliteDefinitionGraphProbe {
    catalog: String,
}

impl SqliteDefinitionGraphProbe {
    pub(crate) fn new(catalog: String) -> Self {
        Self { catalog }
    }

    fn open(&self) -> rusqlite::Connection {
        rusqlite::Connection::open(&self.catalog).expect("open the SQLite definition graph probe")
    }
}

#[async_trait::async_trait]
impl lash_conformance::DefinitionGraphProbe for SqliteDefinitionGraphProbe {
    async fn module_row(&self, artifact_ref: &str) -> lash_conformance::ModuleRow {
        use rusqlite::OptionalExtension as _;
        let row: Option<(String, Option<i64>)> = self
            .open()
            .query_row(
                "SELECT lower(hex(refs.blob_ref)), blobs.encoding FROM artifact_refs AS refs
                 LEFT JOIN blobs ON blobs.hash = refs.blob_ref
                 WHERE refs.namespace = 'kernel_document' AND refs.artifact_ref = ?1",
                rusqlite::params![artifact_ref],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()
            .expect("read the SQLite module row");
        match row {
            None => lash_conformance::ModuleRow::Absent,
            Some((hash, Some(1))) => lash_conformance::ModuleRow::Node(hash),
            Some(_) => lash_conformance::ModuleRow::Bytes,
        }
    }

    async fn graph_nodes(&self) -> std::collections::BTreeSet<String> {
        self.open()
            .prepare("SELECT lower(hex(hash)) FROM blobs WHERE encoding = 1")
            .expect("prepare the SQLite graph node read")
            .query_map([], |row| row.get(0))
            .expect("read the SQLite graph nodes")
            .collect::<Result<_, _>>()
            .expect("collect the SQLite graph nodes")
    }

    async fn cut(&self, cut: lash_conformance::DefinitionGraphCut) {
        let trigger = match cut {
            lash_conformance::DefinitionGraphCut::Publication => {
                "BEFORE INSERT ON artifact_referrer_edges WHEN NEW.namespace = 'kernel_document'"
            }
            lash_conformance::DefinitionGraphCut::Severance => {
                "BEFORE DELETE ON artifact_referrer_edges WHEN OLD.namespace = 'kernel_document'"
            }
            lash_conformance::DefinitionGraphCut::Retirement => {
                "BEFORE DELETE ON blobs WHEN OLD.encoding = 1"
            }
        };
        self.open()
            .execute_batch(&format!(
                "CREATE TRIGGER definition_graph_cut {trigger} BEGIN \
                 SELECT RAISE(ABORT, 'injected definition graph cut'); END;"
            ))
            .expect("install the SQLite definition graph cut");
    }

    async fn clear_cut(&self) {
        self.open()
            .execute_batch("DROP TRIGGER definition_graph_cut")
            .expect("remove the SQLite definition graph cut");
    }
}
