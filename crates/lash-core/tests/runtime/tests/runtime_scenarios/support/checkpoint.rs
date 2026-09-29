use super::*;

impl RuntimeScenarioContext {
    pub(super) async fn checkpoint(&mut self, phase: RuntimeCheckpointPhase) {
        self.ensure_lease().await;
        if let Some(turn_index) = phase.turn_index {
            self.state.turn_index = turn_index;
        }
        let persisted_node_ids = self
            .state
            .pending_graph_commit()
            .appended_nodes()
            .map(|node| node.node_id.clone())
            .collect::<Vec<_>>();
        let mut commit = RuntimeCommit::persisted_state_for_test(&self.state, &[]);
        commit.drive_fence = Some(Box::new(self.owner_and_lease().1.clone()));
        commit.applied_commands = self.command_completion();
        if let Some(turn_id) = phase.defer_interrupted_turn_id {
            commit = commit.deferring_interrupted_turn_inputs(TurnId::from(turn_id), None);
            commit = lash_core::testing::store_fixtures::authorize_completion_deferral_for_test(
                self.store(),
                &self.turn_control,
                self.owner_and_lease().1,
                commit,
            )
            .await
            .expect("authorize scenario deferral");
        }
        let result = self
            .store()
            .commit_runtime_state(commit)
            .await
            .expect("commit runtime scenario checkpoint");
        self.state.apply_persisted_commit_result(result);
        self.state.mark_node_ids_persisted(persisted_node_ids);
        self.commands.clear();

        if !phase.pending_turn_inputs_after_deferral.is_empty() {
            assert_pending_turn_inputs(
                self.name,
                self.store(),
                &self.session_id,
                &self.enqueued_turn_inputs,
                &phase.pending_turn_inputs_after_deferral,
            )
            .await;
        }
        for alias in &phase.cancel_after_deferral {
            self.cancel_turn_input(alias, "deferred").await;
        }
        if phase.no_next_turn_input_claim_after_cancellations {
            assert!(
                self.next_turn_input_head().await.is_none(),
                "{} should not admit cancelled next-turn inputs",
                self.name
            );
        }
    }
}
