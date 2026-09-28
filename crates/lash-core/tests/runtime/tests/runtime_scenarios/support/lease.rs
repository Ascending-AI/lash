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
            "{} stale drive phase must precede an admission phase",
            self.name
        );
        let stale_owner = lease_owner("runtime-scenario-stale-drive");
        let stale = self
            .store()
            .seal_drive_epoch_for_test(&self.session_id, &stale_owner, "stale-drive", 0)
            .await
            .expect("seal stale drive")
            .acquired()
            .expect("stale drive sealed");
        self.store()
            .supersede_drive_epoch_for_test(&stale)
            .await
            .expect("seal successor drive");
        let refusal = self
            .stale_admission(&stale)
            .await
            .expect_err("stale drive cannot admit a root");
        assert!(
            matches!(refusal, StoreError::StaleDriveFence { .. }),
            "{} stale drive must be refused typed: {refusal:?}",
            self.name
        );

        let owner = local_lease_owner(self.host_behavior.lease_owner_id, "successor");
        let current = self
            .store()
            .seal_drive_epoch_for_test(&self.session_id, &owner, "current-drive", 0)
            .await
            .expect("seal current drive")
            .acquired()
            .expect("current drive sealed");
        assert!(current.epoch() > stale.epoch());
        if assert_stale_refused {
            let refusal = self
                .stale_admission(&stale)
                .await
                .expect_err("older drive must remain fenced out");
            assert!(matches!(refusal, StoreError::StaleDriveFence { .. }));
        }
        self.owner = Some(owner);
        self.lease = Some(current);
    }

    /// A root admission under `stale`: the fence is checked before anything
    /// is read, so the head need not exist.
    async fn stale_admission(
        &self,
        stale: &DriveFence,
    ) -> Result<Option<RootAdmission>, StoreError> {
        self.store()
            .admit_root(
                &lash_core::testing::store_fixtures::admit_root_request_for_test(
                    stale,
                    &TurnId::from("runtime-scenario-stale-root"),
                    AdmittedHead::Input(lash_core::InputId::from("runtime-scenario-no-input")),
                ),
            )
            .await
    }
}
