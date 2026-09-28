use super::admission::SCENARIO_ROOT;
use super::*;

impl RuntimeScenarioContext {
    pub(super) async fn commit(&mut self, phase: RuntimeCommitPhase) {
        self.ensure_lease().await;
        let persisted_node_ids = self
            .state
            .pending_graph_commit()
            .appended_nodes()
            .map(|node| node.node_id.clone())
            .collect::<Vec<_>>();
        let mut final_commit = RuntimeCommit::persisted_state_for_test(&self.state, &[]);
        final_commit.drive_fence = Some(Box::new(self.owner_and_lease().1.clone()));
        final_commit.applied_commands = self.command_completion();
        if self.admission.is_some() || self.checkpoint_admission.is_some() {
            final_commit.ingress = Some(self.root_settlement());
            let root = TurnId::from(SCENARIO_ROOT);
            final_commit.root_terminal = Some(Box::new(lash_core::store::RootTerminalWrite {
                commit: lash_core::store::TurnCommitId::new(root.clone(), 0),
                turn: lash_core::store::PhysicalTurn::derive_turn_id(&root, 0),
                root,
                stop: None,
            }));
        }
        let result = self
            .store()
            .commit_runtime_state(final_commit)
            .await
            .expect("commit runtime scenario final state");
        self.state.apply_persisted_commit_result(result);
        self.state.mark_node_ids_persisted(persisted_node_ids);
        self.commands.clear();
        self.admission = None;
        self.checkpoint_admission = None;
        self.lease_released = true;

        if phase.pending_turn_inputs_empty_after_commit {
            assert!(
                self.store()
                    .list_pending_turn_inputs(&self.session_id)
                    .await
                    .unwrap_or_else(|err| panic!(
                        "{} failed to list pending turn inputs after commit: {err}",
                        self.name
                    ))
                    .is_empty(),
                "{} pending turn inputs should be empty after final commit",
                self.name
            );
        }
        let read = self
            .store()
            .load_session()
            .await
            .expect("load runtime scenario session")
            .expect("runtime scenario session read");
        assert_eq!(read.session_id, self.session_id);
        if let Some(expected_turn_index) = phase.checkpoint_turn_index {
            assert_eq!(
                read.checkpoint
                    .as_ref()
                    .map(|checkpoint| checkpoint.turn_state.turn_index),
                Some(expected_turn_index),
                "{} checkpoint invariant changed",
                self.name
            );
        }
        assert!(
            self.store()
                .list_queued_work(&self.session_id)
                .await
                .expect("list queued work after scenario")
                .is_empty(),
            "{} should complete all admitted queue work",
            self.name
        );
    }
}
