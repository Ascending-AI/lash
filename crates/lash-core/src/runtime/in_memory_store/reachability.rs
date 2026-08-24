//! Reachability-derived graph retirement for the in-memory store.

use lash_sansio::sync::MutexExt;
use std::collections::{HashMap, HashSet};

use super::InMemorySessionStore;

impl InMemorySessionStore {
    pub(super) fn live_child_counts(
        graph: &crate::SessionGraph,
        tombstoned: &HashSet<String>,
    ) -> HashMap<String, usize> {
        let mut counts = HashMap::new();
        for child in graph
            .nodes
            .iter()
            .filter(|node| !tombstoned.contains(&node.node_id))
        {
            if let Some(parent_node_id) = &child.parent_node_id {
                *counts.entry(parent_node_id.clone()).or_default() += 1;
            }
        }
        counts
    }

    /// Reclaim the ancestry prefix that has no live child, session-head root,
    /// or explicit anchor. The child-count map is derived once from the graph
    /// inside the surrounding write transaction and updated only as this
    /// destructive decision tombstones nodes.
    pub(super) fn reclaim_unreachable_ancestry(
        graph: &crate::SessionGraph,
        live_child_counts: &mut HashMap<String, usize>,
        tombstoned: &mut HashSet<String>,
        first_node_id: &str,
        session_heads: &HashMap<String, Option<String>>,
        anchored_node_ids: &HashSet<String>,
    ) {
        let mut node_id = first_node_id.to_string();
        loop {
            let is_live = graph.find_node(&node_id).is_some() && !tombstoned.contains(&node_id);
            let has_child = live_child_counts.get(&node_id).copied().unwrap_or_default() > 0;
            let has_head = session_heads
                .values()
                .any(|leaf| leaf.as_deref() == Some(node_id.as_str()));
            let is_anchored = anchored_node_ids.contains(&node_id);
            if !is_live || has_child || has_head || is_anchored {
                return;
            }
            tombstoned.insert(node_id.clone());
            let Some(parent_node_id) = graph
                .find_node(&node_id)
                .and_then(|node| node.parent_node_id.clone())
            else {
                return;
            };
            if let Some(count) = live_child_counts.get_mut(&parent_node_id) {
                *count = count.saturating_sub(1);
            }
            node_id = parent_node_id;
        }
    }

    /// Drop one session-head root and reclaim only nodes no longer reachable
    /// from a live child, another session head, or an explicit anchor.
    pub(super) fn reclaim_history_for_delete(
        &self,
        session_id: &str,
    ) -> Result<(), crate::StoreError> {
        // The whole read-modify-write below runs inside the factory's write
        // transaction (see `InMemorySessionStoreFactory::delete_session`), so
        // these snapshots cannot be raced by another writer.
        let mut heads = self.global_session_heads.lock_recover().clone();
        let mut graph = self.global_session_graph.lock_recover().clone();
        let mut owners = self.global_node_owners.lock_recover().clone();
        let mut tombstoned = self.tombstoned_node_ids.lock_recover().clone();
        let anchors = self
            .node_anchors
            .lock_recover()
            .keys()
            .cloned()
            .collect::<HashSet<_>>();
        let mut live_child_counts = Self::live_child_counts(&graph, &tombstoned);

        if let Some(leaf) = heads.remove(session_id).flatten() {
            Self::reclaim_unreachable_ancestry(
                &graph,
                &mut live_child_counts,
                &mut tombstoned,
                &leaf,
                &heads,
                &anchors,
            );
        }

        let candidates = owners
            .iter()
            .filter(|(node_id, owner)| {
                owner.as_str() == session_id
                    && !tombstoned.contains(*node_id)
                    && live_child_counts.get(*node_id).copied().unwrap_or_default() == 0
                    && !heads
                        .values()
                        .any(|leaf| leaf.as_deref() == Some(node_id.as_str()))
                    && !anchors.contains(*node_id)
            })
            .map(|(node_id, _)| node_id.clone())
            .collect::<Vec<_>>();
        for node_id in candidates {
            Self::reclaim_unreachable_ancestry(
                &graph,
                &mut live_child_counts,
                &mut tombstoned,
                &node_id,
                &heads,
                &anchors,
            );
        }

        // Delete-time reclaim, scoped to sessions that can never vacuum again:
        // physically drop the tombstoned rows this session owns, plus tombstoned
        // rows owned by an already-deleted session. Exactly as the SQLite and
        // Postgres backends do (`DELETE FROM graph_nodes WHERE tombstoned AND
        // (session_id = ? OR session_id IN (deleted sessions))`).
        //
        // The already-deleted arm is what keeps reclaim total: a node can be
        // tombstoned *after* its owning session is gone (unpin of a pinned leaf
        // whose session was deleted, or ancestry retired at a fork child's
        // delete), and no session-scoped vacuum can ever reach it — the owner's
        // id is permanently unbindable. Those ids can never be re-bound, so this
        // stays inside the session-scoping law and is not a catalog-wide sweep:
        // rows owned by live sessions stay resident for their own vacuum.
        let reclaimed = {
            let deleted_session_ids = self.deleted_session_ids.lock_recover();
            tombstoned
                .iter()
                .filter(|node_id| {
                    owners.get(*node_id).is_some_and(|owner| {
                        owner.as_str() == session_id || deleted_session_ids.contains(owner)
                    })
                })
                .cloned()
                .collect::<HashSet<_>>()
        };
        if !reclaimed.is_empty() {
            let nodes = graph
                .nodes
                .iter()
                .filter(|node| !reclaimed.contains(&node.node_id))
                .cloned()
                .collect::<Vec<_>>();
            graph = crate::SessionGraph::from_nodes(nodes, None)?;
            owners.retain(|node_id, _| !reclaimed.contains(node_id));
            tombstoned.retain(|node_id| !reclaimed.contains(node_id));
        }

        *self.global_session_heads.lock_recover() = heads;
        *self.global_session_graph.lock_recover() = graph;
        *self.global_node_owners.lock_recover() = owners;
        *self.tombstoned_node_ids.lock_recover() = tombstoned;
        *self.session_graph.lock_recover() = crate::SessionGraph::default();
        *self.session_head_meta.lock_recover() = None;
        *self.session_meta.lock_recover() = None;
        *self.session_state_version.lock_recover() = None;
        *self.checkpoint.lock_recover() = None;
        self.attachment_manifest.lock_recover().clear();
        self.usage_deltas.lock_recover().clear();
        self.runtime_turn_commits.lock_recover().clear();
        self.queued_work.lock_recover().clear();
        self.pending_turn_inputs.lock_recover().clear();
        self.session_execution_leases.lock_recover().clear();
        self.wake_redelivery_fences
            .lock_recover()
            .retain(|(target_session_id, _), _| target_session_id != session_id);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use crate::SessionStoreFactory;
    use crate::runtime::in_memory_store::InMemorySessionStoreFactory;
    use crate::testing::conformance::{
        append_conformance_event_node, commit_conformance_state, session_store_request,
    };
    use lash_sansio::sync::MutexExt;

    /// Node ids still physically resident in the factory-global graph, tombstoned
    /// or not. `load_node` hides tombstones, so only this raw view can tell a
    /// reclaimed row from a merely hidden one.
    fn resident_graph_node_ids(factory: &InMemorySessionStoreFactory) -> Vec<String> {
        let mut ids = factory
            .global_session_graph
            .lock_recover()
            .nodes
            .iter()
            .map(|node| node.node_id.clone())
            .collect::<Vec<_>>();
        ids.sort();
        ids
    }

    fn resident_tombstoned_node_ids(factory: &InMemorySessionStoreFactory) -> Vec<String> {
        let mut ids = factory
            .tombstoned_node_ids
            .lock_recover()
            .iter()
            .cloned()
            .collect::<Vec<_>>();
        ids.sort();
        ids
    }

    /// Unpinning a pinned leaf *after* its owning session was deleted tombstones a
    /// node whose owner can never be bound again, so no session-scoped vacuum can
    /// reach it. The next session delete must drain that residue.
    ///
    /// The pre-delete store handle is deliberately dropped before the delete: a
    /// live handle can still vacuum its own session and would mask the leak.
    #[tokio::test]
    async fn delete_reclaims_tombstone_orphaned_by_unpin_after_owner_delete() {
        let factory = InMemorySessionStoreFactory::new();
        let owner = session_store_request(
            "orphan-owner",
            "tombstone-model",
            crate::SessionRelation::Root,
        );
        let leaf = {
            let store = factory
                .create_store(&owner)
                .await
                .expect("create owner store");
            let mut state = crate::RuntimeSessionState {
                session_id: owner.session_id.clone(),
                ..crate::RuntimeSessionState::new(owner.policy.clone())
            };
            state.ensure_agent_frame_initialized();
            let leaf = state
                .session_graph
                .leaf_node_id
                .clone()
                .expect("owner leaf");
            store
                .commit_runtime_state(crate::RuntimeCommit::persisted_state_for_test(&state, &[]))
                .await
                .expect("commit owner");
            leaf
        };

        factory.pin(&leaf).await.expect("pin owner leaf");
        factory
            .delete_session(&owner.session_id)
            .await
            .expect("delete owner session");
        factory
            .unpin(&leaf)
            .await
            .expect("unpin after owner delete");

        assert_eq!(
            resident_tombstoned_node_ids(&factory),
            vec![leaf.clone()],
            "the unpin must tombstone the deleted owner's leaf"
        );

        let sweeper = session_store_request(
            "orphan-sweeper",
            "tombstone-model",
            crate::SessionRelation::Root,
        );
        drop(
            factory
                .create_store(&sweeper)
                .await
                .expect("create sweeper store"),
        );
        factory
            .delete_session(&sweeper.session_id)
            .await
            .expect("delete sweeper session");

        assert!(
            resident_tombstoned_node_ids(&factory).is_empty(),
            "a delete must reclaim tombstones owned by already-deleted sessions"
        );
        assert!(
            !resident_graph_node_ids(&factory).contains(&leaf),
            "the orphaned tombstone row must be physically gone, not just hidden"
        );
    }

    /// Fork ancestry owned by a session deleted *before* its child is the second
    /// orphaning flow: the ancestry is only tombstoned when the child is deleted,
    /// by which time its owner is already unbindable. The same delete must reclaim
    /// it.
    #[tokio::test]
    async fn delete_reclaims_fork_ancestry_orphaned_by_earlier_owner_delete() {
        let factory = InMemorySessionStoreFactory::new();
        let parent = session_store_request(
            "orphan-fork-parent",
            "tombstone-model",
            crate::SessionRelation::Root,
        );
        let parent_leaf = {
            let store = factory
                .create_store(&parent)
                .await
                .expect("create parent store");
            let mut state = crate::RuntimeSessionState {
                session_id: parent.session_id.clone(),
                ..crate::RuntimeSessionState::new(parent.policy.clone())
            };
            state.ensure_agent_frame_initialized();
            let leaf = state
                .session_graph
                .leaf_node_id
                .clone()
                .expect("parent leaf");
            store
                .commit_runtime_state(crate::RuntimeCommit::persisted_state_for_test(&state, &[]))
                .await
                .expect("commit parent");
            leaf
        };

        let child_session_id = "orphan-fork-child".to_string();
        factory
            .fork_at(&crate::ForkSessionRequest {
                session_id: child_session_id.clone(),
                node_id: parent_leaf.clone(),
                relation: crate::SessionRelation::Root,
                policy: parent.policy.clone(),
            })
            .await
            .expect("fork at the parent's live tip");
        let child_leaf = {
            let child = factory
                .open_existing_store(&crate::SessionStoreCreateRequest {
                    session_id: child_session_id.clone(),
                    relation: crate::SessionRelation::Root,
                    policy: parent.policy.clone(),
                })
                .await
                .expect("open forked child")
                .expect("forked child exists");
            let mut child_state = crate::store::load_persisted_session_state(child.as_ref())
                .await
                .expect("load child state")
                .expect("child state exists");
            append_conformance_event_node(&mut child_state, "orphan-fork-child-node", "child node");
            commit_conformance_state(&child, &mut child_state)
                .await
                .expect("advance forked child");
            child_state
                .session_graph
                .leaf_node_id
                .clone()
                .expect("child leaf")
        };

        factory
            .delete_session(&parent.session_id)
            .await
            .expect("delete parent session");
        assert!(
            resident_graph_node_ids(&factory).contains(&parent_leaf),
            "the parent's node survives its own delete while the fork child hangs off it"
        );

        factory
            .delete_session(&child_session_id)
            .await
            .expect("delete forked child session");

        assert!(
            resident_tombstoned_node_ids(&factory).is_empty(),
            "the child's delete must reclaim ancestry owned by the already-deleted parent"
        );
        let resident = resident_graph_node_ids(&factory);
        assert!(
            !resident.contains(&parent_leaf) && !resident.contains(&child_leaf),
            "both the child's node and the orphaned parent ancestry must be gone, got {resident:?}"
        );
    }
}
