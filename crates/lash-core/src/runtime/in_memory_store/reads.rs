use super::*;
use lash_sansio::sync::MutexExt;

impl InMemorySessionStore {
    pub(super) fn node_visible_to_bound_session(&self, node_id: &str) -> bool {
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
            return false;
        };
        if self
            .global_node_owners
            .lock_recover()
            .get(node_id)
            .is_some_and(|owner| owner == &session_id)
        {
            return true;
        }
        let leaf_node_id = self
            .global_session_heads
            .lock_recover()
            .get(&session_id)
            .cloned()
            .flatten();
        let mut active_path = self.global_session_graph.lock_recover().clone();
        active_path.set_leaf_node_id(leaf_node_id);
        active_path
            .trim_to_active_path()
            .find_node(node_id)
            .is_some()
    }
}
