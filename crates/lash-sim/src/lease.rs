use std::time::Duration;

/// Runtime leases are operational liveness guards, not generated scenario
/// events. Keep them beyond every practical sim schedule so Tokio starvation
/// cannot turn into an accidental lease-loss scenario.
const SIM_RUNTIME_LEASE_TTL: Duration = Duration::from_secs(100 * 365 * 24 * 60 * 60);

#[expect(
    clippy::expect_used,
    reason = "test support: the surrounding harness code establishes this value; a refusal panics the harness with its case name by design"
)]
pub(crate) fn sim_runtime_lease_timings() -> lash_core::facade_support::LeaseTimings {
    lash_core::facade_support::LeaseTimings::from_ttl(SIM_RUNTIME_LEASE_TTL)
        .expect("the simulation runtime lease policy is valid")
}
