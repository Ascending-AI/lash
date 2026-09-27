use super::*;

impl RuntimeScenarioContext {
    pub(super) async fn lease_phase(&mut self, phase: RuntimeLeasePhase) {
        match phase {
            RuntimeLeasePhase::ExpireStaleHolder {
                assert_successor_busy,
            } => self.supersede_stale_drive(assert_successor_busy).await,
        }
    }

    async fn supersede_stale_drive(&mut self, assert_stale_refused: bool) {
        assert!(
            self.lease.is_none(),
            "{} stale drive phase must precede a claim phase",
            self.name
        );
        let stale_owner = lease_owner("runtime-scenario-stale-drive");
        let stale = self
            .store()
            .seal_claim_epoch_for_test(&self.session_id, &stale_owner, "stale-drive", 0)
            .await
            .expect("seal stale drive")
            .acquired()
            .expect("stale drive sealed");
        self.store()
            .supersede_claim_epoch_for_test(&stale)
            .await
            .expect("seal successor drive");
        let refusal = self
            .store()
            .claim_next_turn_inputs(&self.session_id, &stale, &stale_owner, 1)
            .await
            .expect_err("stale drive cannot claim turn inputs");
        assert!(
            matches!(refusal, StoreError::StaleDriveFence { .. }),
            "{} stale drive must be refused typed: {refusal:?}",
            self.name
        );

        let owner = local_lease_owner(self.host_behavior.lease_owner_id, "successor");
        let current = self
            .store()
            .seal_claim_epoch_for_test(&self.session_id, &owner, "current-drive", 0)
            .await
            .expect("seal current drive")
            .acquired()
            .expect("current drive sealed");
        assert!(current.fencing_token > stale.fencing_token);
        if assert_stale_refused {
            let refusal = self
                .store()
                .claim_next_turn_inputs(&self.session_id, &stale, &stale_owner, 1)
                .await
                .expect_err("older drive must remain fenced out");
            assert!(matches!(refusal, StoreError::StaleDriveFence { .. }));
        }
        self.owner = Some(owner);
        self.lease = Some(current);
    }
}
