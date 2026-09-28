//! Registration macros for the obligation relay and recovery leader lease
//! laws (ADR 0109 §1).

/// Register one independently reported test per obligation relay law.
///
/// The fixture block yields `(guard, fixture)`: a value kept alive for the
/// test's duration, and an [`ObligationLawFixture`](crate::ObligationLawFixture)
/// over a fresh store set.
#[macro_export]
macro_rules! obligation_relay_tests {
    ($fixture:block) => {
        $crate::obligation_relay_tests!(@catalogue $fixture; [
            (arming_takes_only_an_idle_row, "obligation-arm"),
            (the_claim_token_fences_settlement, "obligation-fencing"),
            (a_retryable_failure_backs_off, "obligation-backoff"),
            (the_attempt_ceiling_stalls, "obligation-ceiling"),
            (
                a_refused_or_undecodable_row_stalls_without_failing_the_page,
                "obligation-stall-page"
            ),
            (a_rearm_returns_a_stalled_obligation_to_due, "obligation-rearm"),
            (immediate_delivery_takes_only_a_due_obligation, "obligation-immediate"),
            (registered_processes_are_claimed_through_every_obligation_page, "process-start-obligation-pages"),
        ]);
    };
    (@catalogue $fixture:block; [$(( $law:ident, $label:literal )),* $(,)?]) => {
        $(
            #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
            async fn $law() {
                let (_fixture_guard, fixture) = $fixture;
                let _ = $label;
                $crate::$law(fixture).await;
                $crate::law_receipt::record(module_path!(), stringify!($law), $label);
            }
        )*
    };
}

/// Register one independently reported test per recovery leader lease law.
///
/// The fixture block sees the law's label under the name the caller binds
/// and yields `(guard, fixture)`: a value kept alive for the test's
/// duration, and a [`LeaseLawFixture`](crate::LeaseLawFixture) whose lease
/// name no other law uses.
#[macro_export]
macro_rules! recovery_leader_tests {
    (|$label_binding:ident| $fixture:block) => {
        $crate::recovery_leader_tests!(@catalogue $label_binding $fixture; [
            (the_lease_has_a_single_holder, "lease-single-holder"),
            (an_expired_lease_fails_over, "lease-expiry-failover"),
            (
                a_newer_generation_preempts_after_the_minimum_tenure,
                "lease-generation-preemption"
            ),
            (a_resigned_lease_is_taken_at_once, "lease-resign"),
        ]);
    };
    (@catalogue $label_binding:ident $fixture:block;
        [$(( $law:ident, $label:literal )),* $(,)?]) => {
        $(
            #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
            async fn $law() {
                let $label_binding: &'static str = $label;
                let (_fixture_guard, fixture) = $fixture;
                $crate::$law(fixture).await;
                $crate::law_receipt::record(module_path!(), stringify!($law), $label);
            }
        )*
    };
}
