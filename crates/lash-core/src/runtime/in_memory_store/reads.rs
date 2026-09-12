use super::*;
use lash_sansio::sync::MutexExt;

impl InMemorySessionStore {
    pub(super) fn node_visible_to_bound_session(
        &self,
        node_id: &str,
    ) -> Result<bool, crate::StoreError> {
        let session_id = self
            .session_head_meta
            .lock_recover()
            .as_ref()
            .map(|head| head.session_id.clone())
            .or_else(|| {
                self.session_meta
                    .lock_recover()
                    .as_ref()
                    .map(|meta| meta.session_id.clone())
            });
        let Some(session_id) = session_id else {
            return Ok(false);
        };
        if self
            .global_node_owners
            .lock_recover()
            .get(node_id)
            .is_some_and(|owner| owner == session_id)
        {
            return Ok(true);
        }
        let leaf_node_id = self
            .global_session_heads
            .lock_recover()
            .get(&session_id)
            .cloned()
            .flatten();
        let global_graph = self.global_session_graph.lock_recover();
        let active_path = crate::SessionGraph::from_nodes(global_graph.nodes.clone(), leaf_node_id)
            .and_then(|graph| graph.try_trim_to_active_path())
            .map_err(|error| crate::StoreError::StoredDataCorrupt {
                record_kind: "SessionGraph",
                message: error.to_string(),
            })?;
        let Some(candidate_index) = active_path
            .nodes
            .iter()
            .position(|node| node.node_id == node_id)
        else {
            return Ok(false);
        };
        let tombstoned = self.tombstoned_node_ids.lock_recover();
        if active_path.nodes[candidate_index..]
            .iter()
            .any(|node| tombstoned.contains(&node.node_id))
        {
            return Err(crate::StoreError::StoredDataCorrupt {
                record_kind: "SessionGraph",
                message: "parent edge crosses a tombstone or generation gap".to_string(),
            });
        }
        Ok(true)
    }
}

#[cfg(test)]
mod conformance_mapping_tests {
    use super::*;
    use crate::SessionStoreFactory;

    #[tokio::test]
    async fn invalid_active_path_is_typed_corruption() {
        let factory = super::super::InMemorySessionStoreFactory::new();
        factory
            .create_store(&crate::testing::store_fixtures::session_store_request(
                &SessionId::from("reader"),
                "mapping-test",
                crate::SessionRelation::Root,
            ))
            .await
            .unwrap();
        let store = factory
            .raw_store_for_testing(&SessionId::from("reader"))
            .unwrap();
        // A foreign-node read must walk the bound session's active path. A
        // dangling head forces the path-validation error through reads.rs,
        // rather than the separately classified load_session path.
        store
            .global_session_heads
            .lock_recover()
            .insert("reader".into(), Some("missing".into()));
        let error = store
            .node_visible_to_bound_session("foreign-node")
            .unwrap_err();
        assert!(
            matches!(
                error,
                crate::StoreError::StoredDataCorrupt {
                    record_kind: "SessionGraph",
                    ..
                }
            ),
            "invalid active path must retain StoredDataCorrupt: {error:?}"
        );
    }
}
