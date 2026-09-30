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

    /// A completion of the scenario root's admitted work by any other root
    /// is refused: an admitted row is settled only by its own root.
    async fn stale_queue_completion_fault(&mut self) {
        self.ensure_lease().await;
        let admission = self
            .admission
            .as_ref()
            .expect("stale queue-completion fault requires a prior TurnWorkAdmission phase");
        let mut foreign =
            lash_core::store::IngressSettlement::new(TurnId::from("runtime-scenario-foreign-root"));
        foreign
            .completed_batches
            .extend(admission.queued.as_ref().map(|queued| queued.completion()));
        let mut commit = RuntimeCommit::persisted_state_for_test(&self.state, &[]);
        commit.drive_fence = Some(Box::new(self.owner_and_lease().1.clone()));
        commit.ingress = Some(foreign);
        let err = self
            .store()
            .commit_runtime_state(commit)
            .await
            .expect_err("stale queue-completion fault should reject the commit");
        assert!(
            matches!(err, StoreError::IngressRowNotAdmitted { .. }),
            "{} stale queue-completion fault produced the wrong error: {err:?}",
            self.name
        );
    }

    async fn commit_after_advisory_lease_release(&mut self) {
        if !self.lease_released {
            self.commit(RuntimeCommitPhase::new()).await;
        }
        let stale_head_revision = self.state.head_revision;
        self.state.turn_index = self.state.turn_index.saturating_add(1);
        let result = self
            .store()
            .commit_runtime_state(RuntimeCommit::persisted_state_for_test(&self.state, &[]))
            .await
            .expect("released advisory lease must not reject a current-head commit");
        self.state.head_revision = result.head_revision;

        let mut stale_state = self.state.clone();
        stale_state.head_revision = stale_head_revision;
        stale_state.turn_index = stale_state.turn_index.saturating_add(1);
        let err = self
            .store()
            .commit_runtime_state(RuntimeCommit::persisted_state_for_test(&stale_state, &[]))
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
