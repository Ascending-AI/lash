//! Registration macros for the level-one turn crash checks: the golden-trace
//! drift check, the crash matrix and the FIG-3524 error-return sweep.
//!
//! The fixture yields `(guard, stores, make, make_invocation,
//! make_error_invocation)`: `stores` supplies every port the law does not
//! certify, `make_invocation` drives the crash placements, and
//! `make_error_invocation` — a `(scenario, scope)` factory — supplies the
//! controller whose journal faults the error-return sweep arms, which the
//! after-commit redrive law (FIG-3590) also redrives.
//!
//! `turn_crash_matrix_tests!` registers the trace and the three journaled
//! laws, and `turn_crash_level_1_tests!` the level-one matrix. A tier that must
//! defer the crash-and-recover laws but can still hold the trace registers
//! `turn_crash_trace_tests!` and `turn_crash_recovery_tests!` instead, each
//! with its own attributes, so a deferral names exactly the laws it parks.

/// Register one level-one turn crash law.
#[macro_export]
macro_rules! __turn_crash_matrix_register {
    ([$($attr:tt)*] $fixture:block; $law:ident, $label:literal, trace) => {
        $($attr)*
        #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
        async fn $law() {
            let (_guard, stores, make, make_invocation, _make_error_invocation) = $fixture;
            let _ = $label;
            Box::pin($crate::registration_macro_support::$law(stores, make, make_invocation))
                .await;
            $crate::law_receipt::record(module_path!(), stringify!($law), $label);
        }
    };
    ([$($attr:tt)*] $fixture:block; $law:ident, $label:literal, matrix) => {
        $($attr)*
        #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
        async fn $law() {
            let (_guard, stores, make, make_invocation, _make_error_invocation) = $fixture;
            let _ = $label;
            Box::pin($crate::registration_macro_support::$law(
                stores,
                make,
                make_invocation,
            ))
            .await;
            $crate::law_receipt::record(module_path!(), stringify!($law), $label);
        }
    };
    ([$($attr:tt)*] $fixture:block; $law:ident, $label:literal, journaled) => {
        $($attr)*
        #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
        async fn $law() {
            let (_guard, stores, make, _make_invocation, make_journaled_invocation) = $fixture;
            let _ = $label;
            Box::pin($crate::registration_macro_support::$law(
                stores,
                make,
                make_journaled_invocation,
            ))
            .await;
            $crate::law_receipt::record(module_path!(), stringify!($law), $label);
        }
    };
    ([$($attr:tt)*] $fixture:block; $law:ident, $label:literal, error_return) => {
        $($attr)*
        #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
        async fn $law() {
            let (_guard, stores, make, _make_invocation, make_error_invocation) = $fixture;
            let _ = $label;
            Box::pin($crate::registration_macro_support::$law(
                stores,
                make,
                make_error_invocation,
            ))
            .await;
            $crate::law_receipt::record(module_path!(), stringify!($law), $label);
        }
    };
}

/// Register the turn crash checks a journaled engine runs in process: the
/// golden-trace drift check, the direct-acceptance crash, the error-return
/// sweep and the after-commit redrive. The level-one matrix registers on its
/// own ([`turn_crash_level_1_tests!`]).
#[macro_export]
macro_rules! turn_crash_matrix_tests {
    ($(#[$attr:meta])* $fixture:block) => {
        $crate::turn_crash_matrix_tests!(@catalogue [$(#[$attr])*] $fixture; [
            (turn_crash_trace_drift_check, "turn-crash-trace-drift", trace),
            (
                direct_turn_acceptance_crash_after_store_commit_admits_one_row,
                "turn-crash-direct-acceptance",
                matrix
            ),
            (
                turn_crash_matrix_error_return_fail_stop,
                "turn-crash-matrix-error-return-fail-stop",
                error_return
            ),
            (
                turn_crash_after_commit_redrive_replays_the_committed_receipt,
                "turn-crash-after-commit-redrive",
                journaled
            ),
        ]);
    };
    (@catalogue $attrs:tt $fixture:block; [$(( $law:ident, $label:literal, $mode:ident )),* $(,)?]) => {
        $(
            $crate::__turn_crash_matrix_register!($attrs $fixture; $law, $label, $mode);
        )*
    };
}

/// Register only the level-one crash matrix. Its crashes are simulated in
/// process, so a tier whose crashed attempt keeps running work it cannot stop
/// registers it with its own attributes.
#[macro_export]
macro_rules! turn_crash_level_1_tests {
    ($(#[$attr:meta])* $fixture:block) => {
        $crate::__turn_crash_matrix_register!([$(#[$attr])*] $fixture;
            turn_crash_matrix_level_1, "turn-crash-matrix-level-1", matrix);
    };
}

/// Register only the golden-trace drift check.
#[macro_export]
macro_rules! turn_crash_trace_tests {
    ($(#[$attr:meta])* $fixture:block) => {
        $crate::turn_crash_trace_tests!(@catalogue [$(#[$attr])*] $fixture; [
            (turn_crash_trace_drift_check, "turn-crash-trace-drift", trace),
        ]);
    };
    (@catalogue $attrs:tt $fixture:block; [$(( $law:ident, $label:literal, $mode:ident )),* $(,)?]) => {
        $(
            $crate::__turn_crash_matrix_register!($attrs $fixture; $law, $label, $mode);
        )*
    };
}

/// Register only the crash-and-recover laws: the level-one matrix, the
/// error-return sweep and the after-commit redrive.
#[macro_export]
macro_rules! turn_crash_recovery_tests {
    ($(#[$attr:meta])* $fixture:block) => {
        $crate::turn_crash_recovery_tests!(@catalogue [$(#[$attr])*] $fixture; [
            (turn_crash_matrix_level_1, "turn-crash-matrix-level-1", matrix),
            (
                direct_turn_acceptance_crash_after_store_commit_admits_one_row,
                "turn-crash-direct-acceptance",
                matrix
            ),
            (
                turn_crash_matrix_error_return_fail_stop,
                "turn-crash-matrix-error-return-fail-stop",
                error_return
            ),
            (
                turn_crash_after_commit_redrive_replays_the_committed_receipt,
                "turn-crash-after-commit-redrive",
                journaled
            ),
        ]);
    };
    (@catalogue $attrs:tt $fixture:block; [$(( $law:ident, $label:literal, $mode:ident )),* $(,)?]) => {
        $(
            $crate::__turn_crash_matrix_register!($attrs $fixture; $law, $label, $mode);
        )*
    };
}
