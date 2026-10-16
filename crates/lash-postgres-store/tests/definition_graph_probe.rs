use std::sync::Arc;

use lash_postgres_store::PostgresStorage;

/// The definition graph laws' rows and cuts (FIG-6469), over the store's
/// pool.
pub(crate) struct PostgresDefinitionGraphProbe {
    storage: Arc<PostgresStorage>,
}

impl PostgresDefinitionGraphProbe {
    pub(crate) fn new(storage: Arc<PostgresStorage>) -> Self {
        Self { storage }
    }
}

#[async_trait::async_trait]
impl lash_conformance::DefinitionGraphProbe for PostgresDefinitionGraphProbe {
    async fn module_row(&self, artifact_ref: &str) -> lash_conformance::ModuleRow {
        let row: Option<(i16, Option<String>)> = sqlx::query_as(
            "SELECT storage_kind, graph_ref FROM lash_lash_vm_artifacts
             WHERE namespace = 'kernel_document' AND artifact_ref = $1",
        )
        .bind(artifact_ref)
        .fetch_optional(self.storage.pool())
        .await
        .expect("read the Postgres module row");
        match row {
            None => lash_conformance::ModuleRow::Absent,
            Some((1, Some(hash))) => lash_conformance::ModuleRow::Node(hash),
            Some(_) => lash_conformance::ModuleRow::Bytes,
        }
    }

    async fn graph_nodes(&self) -> std::collections::BTreeSet<String> {
        sqlx::query_scalar::<_, String>("SELECT hash FROM lash_blobs WHERE encoding = 1")
            .fetch_all(self.storage.pool())
            .await
            .expect("read the Postgres graph nodes")
            .into_iter()
            .collect()
    }

    async fn cut(&self, cut: lash_conformance::DefinitionGraphCut) {
        let trigger = match cut {
            lash_conformance::DefinitionGraphCut::Publication => {
                "BEFORE INSERT ON lash_artifact_referrer_edges FOR EACH ROW \
                 WHEN (NEW.namespace = 'kernel_document')"
            }
            lash_conformance::DefinitionGraphCut::Severance => {
                "BEFORE DELETE ON lash_artifact_referrer_edges FOR EACH ROW \
                 WHEN (OLD.namespace = 'kernel_document')"
            }
            lash_conformance::DefinitionGraphCut::Retirement => {
                "BEFORE DELETE ON lash_blobs FOR EACH ROW WHEN (OLD.encoding = 1)"
            }
        };
        sqlx::query(
            "CREATE OR REPLACE FUNCTION lash_definition_graph_cut() RETURNS trigger \
             LANGUAGE plpgsql AS $$ \
             BEGIN RAISE EXCEPTION 'injected definition graph cut'; END $$",
        )
        .execute(self.storage.pool())
        .await
        .expect("create the Postgres cut function");
        sqlx::query(&format!(
            "CREATE TRIGGER definition_graph_cut {trigger} \
             EXECUTE FUNCTION lash_definition_graph_cut()"
        ))
        .execute(self.storage.pool())
        .await
        .expect("install the Postgres definition graph cut");
    }

    async fn clear_cut(&self) {
        for table in ["lash_artifact_referrer_edges", "lash_blobs"] {
            sqlx::query(&format!(
                "DROP TRIGGER IF EXISTS definition_graph_cut ON {table}"
            ))
            .execute(self.storage.pool())
            .await
            .expect("remove the Postgres definition graph cut");
        }
    }
}
