//! Registration macros for turn-ingress laws: direct-turn acceptance
//! (ADR 0069), the cancelled
//! turn's withheld input (FIG-3531). All take the same
//! `(guard, prefix, backend, store)` fixture: the backend under test and a
//! session store of that backend's catalog. They share one catalogue arm.

/// Register one independently reported test per direct-turn acceptance law.
///
/// A tier that must park the laws hands them attributes:
/// `direct_turn_acceptance_tests!(#[ignore = "why"] { fixture })`.
#[macro_export]
macro_rules! direct_turn_acceptance_tests {
    ($(#[$attr:meta])* $fixture:block) => {
        $crate::direct_turn_acceptance_tests!(@catalogue [$(#[$attr])*] $fixture; [
            (direct_turn_accepts_before_executing, "direct-turn-accepts-before-executing"),
            (direct_turn_acceptance_mints_no_idempotency_key, "direct-turn-identity"),
        ]);
    };
    (@catalogue $attrs:tt $fixture:block; [$(( $law:ident, $label:literal )),* $(,)?]) => {
        $(
            $crate::direct_turn_acceptance_tests!(@law $attrs $fixture; ($law, $label));
        )*
    };
    (@law [$($attr:tt)*] $fixture:block; ($law:ident, $label:literal)) => {
        $($attr)*
        #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
        async fn $law() {
            let (_fixture_guard, prefix, backend, store) = $fixture;
            let _ = $label;
            $crate::registration_macro_support::$law(prefix, backend, store).await;
        }
    };
}
