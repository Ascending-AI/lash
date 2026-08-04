use std::time::Duration;

/// Runtime leases are operational liveness guards, not generated scenario
/// events. Keep them beyond every practical sim schedule so Tokio starvation
/// cannot turn into an accidental lease-loss scenario.
const SIM_RUNTIME_LEASE_TTL: Duration = Duration::from_secs(100 * 365 * 24 * 60 * 60);

pub(crate) fn sim_runtime_lease_timings() -> lash_core::facade_support::LeaseTimings {
    lash_core::facade_support::LeaseTimings::from_ttl(SIM_RUNTIME_LEASE_TTL)
        .expect("the simulation runtime lease policy is valid")
}

#[cfg(test)]
mod tests {
    use lash_core::{
        LeaseOwnerIdentity, SessionExecutionLeaseClaimOutcome, SessionExecutionLeaseStore,
        StoreError,
    };

    use super::*;
    use crate::clock::SimClock;

    #[tokio::test]
    async fn runtime_lease_survives_starvation_while_deliberate_lease_expires() {
        let clock = SimClock::new();
        let store = lash_core::facade_support::InMemorySessionStore::with_clock(clock.clone());
        let owner = LeaseOwnerIdentity::opaque("sim-owner", "sim-owner:001");
        let runtime_timings = sim_runtime_lease_timings();
        let runtime_lease = match store
            .try_claim_session_execution_lease("runtime-session", &owner, runtime_timings.ttl_ms())
            .await
            .expect("claim sim runtime lease")
        {
            SessionExecutionLeaseClaimOutcome::Acquired(acquisition) => acquisition.lease,
            SessionExecutionLeaseClaimOutcome::Busy { .. } => {
                panic!("fresh sim runtime lease was busy")
            }
        };

        // No renewal task runs in this test. Crossing several production TTL
        // windows therefore models complete renewal-task starvation directly.
        let production_ttl_ms = lash_core::facade_support::LeaseTimings::default().ttl_ms();
        clock.advance_by(3 * production_ttl_ms + 1).await;
        store
            .renew_session_execution_lease(&runtime_lease.fence(), runtime_timings.ttl_ms())
            .await
            .expect("sim runtime lease survives scheduler starvation");

        let expiring_lease = match store
            .try_claim_session_execution_lease(
                "deliberate-expiry-session",
                &owner,
                production_ttl_ms,
            )
            .await
            .expect("claim deliberate finite lease")
        {
            SessionExecutionLeaseClaimOutcome::Acquired(acquisition) => acquisition.lease,
            SessionExecutionLeaseClaimOutcome::Busy { .. } => {
                panic!("fresh deliberate lease was busy")
            }
        };
        clock.advance_by(production_ttl_ms + 1).await;
        assert!(matches!(
            store
                .renew_session_execution_lease(&expiring_lease.fence(), production_ttl_ms)
                .await,
            Err(StoreError::SessionExecutionLeaseExpired { .. })
        ));
    }
}
