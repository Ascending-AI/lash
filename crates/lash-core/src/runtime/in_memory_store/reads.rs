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
            .is_some_and(|owner| owner == &session_id)
        {
            return Ok(true);
        }
        let leaf_node_id = self
            .global_session_heads
            .lock_recover()
            .get(&session_id)
            .cloned()
            .flatten();
        let mut active_path = self.global_session_graph.lock_recover().clone();
        active_path.set_leaf_node_id(leaf_node_id);
        let active_path = active_path.try_trim_to_active_path().map_err(|error| {
            crate::StoreError::StoredDataCorrupt {
                record_kind: "SessionGraph",
                message: error.to_string(),
            }
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
