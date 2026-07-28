use super::*;

impl InMemorySessionStore {
    pub(super) fn node_visible_to_bound_session(&self, node_id: &str) -> bool {
        let session_id = self
            .session_head_meta
            .lock()
            .expect("lock session head")
            .as_ref()
            .map(|head| head.session_id.clone())
            .or_else(|| {
                self.session_meta
                    .lock()
                    .expect("lock session meta")
                    .as_ref()
                    .map(|meta| meta.session_id.clone())
            });
        let Some(session_id) = session_id else {
            return false;
        };
        if self
            .global_node_owners
            .lock()
            .expect("lock global node owners")
            .get(node_id)
            .is_some_and(|owner| owner == &session_id)
        {
            return true;
        }
        let leaf_node_id = self
            .global_session_heads
            .lock()
            .expect("lock global session heads")
            .get(&session_id)
            .cloned()
            .flatten();
        let mut active_path = self
            .global_session_graph
            .lock()
            .expect("lock global graph")
            .clone();
        active_path.set_leaf_node_id(leaf_node_id);
        active_path
            .trim_to_active_path()
            .find_node(node_id)
            .is_some()
    }
}
