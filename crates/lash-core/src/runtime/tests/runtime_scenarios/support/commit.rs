use super::*;

impl RuntimeScenarioContext {
    pub(super) async fn commit(&mut self, phase: RuntimeCommitPhase) {
        self.ensure_lease().await;
        let (_, lease) = self.owner_and_lease();
        let persisted_node_ids = self
            .state
            .pending_graph_commit()
            .appended_nodes()
            .map(|node| node.node_id.clone())
            .collect::<Vec<_>>();
        let final_commit = RuntimeCommit::persisted_state_for_test(&self.state, &[])
            .releasing_session_execution_lease(lease.completion())
            .completing_queue_claims(
                self.command_claim
                    .iter()
                    .chain(self.turn_claim.iter())
                    .map(QueuedWorkClaim::completion),
            )
            .completing_turn_input_claims(
                self.turn_input_claim.iter().map(TurnInputClaim::completion),
            );
        let result = self
            .store()
            .commit_runtime_state(final_commit)
            .await
            .expect("commit runtime scenario final state");
        self.state.apply_persisted_commit_result(result);
        self.state.mark_node_ids_persisted(persisted_node_ids);
        self.command_claim = None;
        self.turn_claim = None;
        self.turn_input_claim = None;
        self.lease_released = true;

        if phase.pending_turn_inputs_empty_after_commit {
            assert!(
                self.store()
                    .list_pending_turn_inputs(self.session_id)
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
                .list_queued_work(self.session_id)
                .await
                .expect("list queued work after scenario")
                .is_empty(),
            "{} should complete all claimed queue work",
            self.name
        );
    }
}
