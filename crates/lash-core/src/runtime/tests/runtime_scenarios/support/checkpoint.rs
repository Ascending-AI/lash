use super::*;

impl RuntimeScenarioContext {
    pub(super) async fn checkpoint(&mut self, phase: RuntimeCheckpointPhase) {
        self.ensure_lease().await;
        if let Some(turn_index) = phase.turn_index {
            self.state.turn_index = turn_index;
        }
        let (_, lease) = self.owner_and_lease();
        let mut commit = RuntimeCommit::persisted_state(&self.state, &[])
            .with_session_execution_lease(lease.fence())
            .completing_queue_claims(self.command_claim.iter().map(QueuedWorkClaim::completion));
        if let Some(turn_id) = phase.defer_interrupted_turn_id {
            commit = commit.deferring_interrupted_turn_inputs(turn_id);
        }
        let result = self
            .store()
            .commit_runtime_state(commit)
            .await
            .expect("commit runtime scenario checkpoint");
        self.state.head_revision = Some(result.head_revision);
        self.command_claim = None;

        if !phase.pending_turn_inputs_after_deferral.is_empty() {
            assert_pending_turn_inputs(
                self.name,
                self.store(),
                self.session_id,
                &self.enqueued_turn_inputs,
                &phase.pending_turn_inputs_after_deferral,
            )
            .await;
        }
        for alias in &phase.cancel_after_deferral {
            self.cancel_turn_input(alias, "deferred").await;
        }
        if phase.no_next_turn_input_claim_after_cancellations {
            let (owner, lease) = self.owner_and_lease();
            assert!(
                self.store()
                    .claim_next_turn_inputs(self.session_id, &lease.fence(), owner, 10)
                    .await
                    .unwrap_or_else(|err| panic!(
                        "{} failed to claim next-turn inputs after cancellation: {err}",
                        self.name
                    ))
                    .is_none(),
                "{} should not claim cancelled next-turn inputs",
                self.name
            );
        }
    }
}
