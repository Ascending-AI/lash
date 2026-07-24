use super::*;

impl RuntimeScenarioContext {
    pub(super) async fn fault(&mut self, phase: RuntimeFaultPhase) {
        match phase {
            RuntimeFaultPhase::StaleQueueCompletion => self.stale_queue_completion_fault().await,
            RuntimeFaultPhase::CommitAfterAdvisoryLeaseRelease => {
                self.commit_after_advisory_lease_release().await
            }
        }
    }

    async fn stale_queue_completion_fault(&mut self) {
        self.ensure_lease().await;
        let claim = self
            .turn_claim
            .as_ref()
            .expect("stale queue-completion fault requires a prior TurnWorkClaim phase");
        let (_, lease) = self.owner_and_lease();
        let mut stale_completion = claim.completion();
        stale_completion.lease_token.push_str(":stale");
        let err = self
            .store()
            .commit_runtime_state(
                RuntimeCommit::persisted_state(&self.state, &[])
                    .with_session_execution_lease(lease.fence())
                    .completing_queue_claim(stale_completion),
            )
            .await
            .expect_err("stale queue-completion fault should reject the commit");
        assert!(
            matches!(err, StoreError::QueuedWorkClaimSuperseded { .. }),
            "{} stale queue-completion fault produced the wrong error: {err:?}",
            self.name
        );
    }

    async fn commit_after_advisory_lease_release(&mut self) {
        if !self.lease_released {
            self.commit(RuntimeCommitPhase::new()).await;
        }
        let lease = self
            .lease
            .as_ref()
            .expect("released-lease check requires a previous session lease");
        let stale_head_revision = self.state.head_revision;
        self.state.turn_index = self.state.turn_index.saturating_add(1);
        let result = self
            .store()
            .commit_runtime_state(
                RuntimeCommit::persisted_state(&self.state, &[])
                    .with_session_execution_lease(lease.fence()),
            )
            .await
            .expect("released advisory lease must not reject a current-head commit");
        self.state.head_revision = Some(result.head_revision);

        let mut stale_state = self.state.clone();
        stale_state.head_revision = stale_head_revision;
        stale_state.turn_index = stale_state.turn_index.saturating_add(1);
        let err = self
            .store()
            .commit_runtime_state(
                RuntimeCommit::persisted_state(&stale_state, &[])
                    .with_session_execution_lease(lease.fence()),
            )
            .await
            .expect_err("stale head must reject the follow-up commit");
        assert!(
            matches!(
                err,
                StoreError::HeadRevisionConflict {
                    expected,
                    actual,
                } if expected == stale_head_revision
                    && actual == result.head_revision
            ),
            "{} stale head produced the wrong error after advisory lease release: {err:?}",
            self.name
        );
    }
}
