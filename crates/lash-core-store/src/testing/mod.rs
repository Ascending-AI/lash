// Test-support module: these fixtures run inside a test, and a broken setup
// assumption must abort it loudly rather than be reshaped into a runtime error
// the test under way would then report as a runtime defect. Clippy's
// `allow-expect-in-tests` reaches `#[test]` functions only, not the fixtures
// they call.
#![expect(
    clippy::expect_used,
    reason = "test-support fixtures: a broken setup assumption aborts the test"
)]

// Each submodule documents itself in its own file. Adding an outer doc comment
// here as well would merge two fragments written in different scopes, and a
// reader or editor following the merged doc comment — including the
// submodule's own intra-doc links — would resolve it against *this* module's
// scope, where none of the linked items exist.
pub mod graph_integrity;
pub mod lineage;
pub mod store_fixtures;

pub use lash_core_ids::test_clock::TestClock;

/// Generous claim bounds for store/runtime conformance tests whose subject is
/// not batching policy. Batching-specific tests construct exact policies.
///
/// The drain policy is deliberately [`DrainMode::All`](crate::DrainMode::All)
/// rather than the shipped one-row default: these suites exercise the store's
/// coalescing laws, and a one-row drain would hide them. Tests whose subject is
/// the drain policy itself set it explicitly — including
/// `queued_work_redrive_ignores_a_changed_drain_policy`, which pins the shipped
/// default on the successor. This pin cannot mask an exact-selection defect:
/// exact claims bypass the configured policy entirely
/// ([`select_exact_turn_work_claim_prefix`](crate::store::queued_work::select_exact_turn_work_claim_prefix)).
pub fn queued_work_claim_policy(max_rows: usize) -> crate::QueuedWorkClaimPolicy {
    crate::QueuedWorkClaimPolicy {
        max_context_tokens: usize::MAX / 4,
        action_token_reserve: 1,
        max_rows,
        max_pending_age_ms: u64::MAX,
        drain_policy: crate::queued_drain_policy::shared_drain_mode_policy(
            crate::queued_drain_policy::DrainMode::All,
        ),
    }
}
