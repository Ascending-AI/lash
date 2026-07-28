use super::*;

impl RuntimeScenarioContext {
    pub(super) async fn lease_phase(&mut self, phase: RuntimeLeasePhase) {
        match phase {
            RuntimeLeasePhase::ExpireStaleHolder {
                assert_successor_busy,
            } => self.expire_stale_holder(assert_successor_busy).await,
        }
    }

    async fn expire_stale_holder(&mut self, assert_successor_busy: bool) {
        if self.lease.is_some() {
            panic!(
                "{} stale-holder expiry must run before any other session lease claim",
                self.name
            );
        }
        let stale_owner = lease_owner("runtime-scenario-stale-holder");
        let holder = self
            .store()
            .try_claim_session_execution_lease(self.session_id, &stale_owner, 50)
            .await
            .expect("claim stale-holder session execution lease")
            .acquired()
            .expect("stale-holder session execution lease");
        let claimant = local_lease_owner(self.host_behavior.lease_owner_id, "claimant-start");
        let busy = self
            .store()
            .try_claim_session_execution_lease(self.session_id, &claimant, 60_000)
            .await
            .expect("claimant observes busy stale-holder lease");
        assert!(
            matches!(busy, SessionExecutionLeaseClaimOutcome::Busy { .. }),
            "{} expected the stale-holder lease to remain busy before TTL",
            self.name
        );
        tokio::time::sleep(std::time::Duration::from_millis(75)).await;
        let reclaimed = self
            .store()
            .try_claim_session_execution_lease(self.session_id, &claimant, 60_000)
            .await
            .expect("claim session execution lease after stale-holder TTL")
            .acquired()
            .expect("stale-holder session execution lease should expire by TTL");
        assert!(
            reclaimed.fencing_token > holder.fencing_token,
            "{} TTL successor session lease should advance the fencing token",
            self.name
        );
        if assert_successor_busy {
            let stale = self
                .store()
                .try_claim_session_execution_lease(
                    self.session_id,
                    &local_lease_owner("runtime-scenario-late-claimant", "late-claimant-start"),
                    60_000,
                )
                .await
                .expect("late claimant observes successor");
            assert!(
                matches!(stale, SessionExecutionLeaseClaimOutcome::Busy { .. }),
                "{} late claimant should not clear the newer lease",
                self.name
            );
        }
        self.owner = Some(claimant);
        self.lease = Some(reclaimed);
    }
}
