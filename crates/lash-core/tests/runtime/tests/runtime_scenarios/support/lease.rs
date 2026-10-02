use super::*;

impl RuntimeScenarioContext {
    pub(super) async fn lease_phase(&mut self, phase: RuntimeLeasePhase) {
        match phase {
            RuntimeLeasePhase::ExpireStaleHolder {
                assert_successor_busy,
            } => self.supersede_stale_shift(assert_successor_busy).await,
        }
    }

    async fn supersede_stale_shift(&mut self, assert_stale_refused: bool) {
        assert!(
            self.lease.is_none(),
            "{} stale shift phase must precede an admission phase",
            self.name
        );
        let stale_owner = lease_owner("runtime-scenario-stale-shift");
        let stale = self
            .store()
            .seal_shift_epoch_for_test(&self.session_id, &stale_owner, "stale-shift", 0)
            .await
            .expect("seal stale shift")
            .acquired()
            .expect("stale shift sealed");
        self.store()
            .supersede_shift_epoch_for_test(&stale)
            .await
            .expect("seal successor shift");
        let refusal = self
            .stale_admission(&stale)
            .await
            .expect_err("stale shift cannot admit a run");
        assert!(
            matches!(refusal, StoreError::StaleShiftFence { .. }),
            "{} stale shift must be refused typed: {refusal:?}",
            self.name
        );

        let owner = local_lease_owner(self.host_behavior.lease_owner_id, "successor");
        let current = self
            .store()
            .seal_shift_epoch_for_test(&self.session_id, &owner, "current-shift", 0)
            .await
            .expect("seal current shift")
            .acquired()
            .expect("current shift sealed");
        assert!(current.epoch() > stale.epoch());
        if assert_stale_refused {
            let refusal = self
                .stale_admission(&stale)
                .await
                .expect_err("older shift must remain fenced out");
            assert!(matches!(refusal, StoreError::StaleShiftFence { .. }));
        }
        self.owner = Some(owner);
        self.lease = Some(current);
    }

    /// A run admission under `stale`: the fence is checked before anything
    /// is read, so the head need not exist.
    async fn stale_admission(
        &self,
        stale: &ShiftFence,
    ) -> Result<Option<RunAdmission>, StoreError> {
        self.store()
            .admit_run(
                &lash_core::testing::store_fixtures::admit_run_request_for_test(
                    stale,
                    &TurnId::from("runtime-scenario-stale-run"),
                    AdmittedHead::Input(lash_core::InputId::from("runtime-scenario-no-input")),
                ),
            )
            .await
    }
}
