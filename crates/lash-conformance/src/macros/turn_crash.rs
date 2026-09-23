//! Registration macros for the level-one turn crash checks: the golden-trace
//! drift check, the crash matrix and the FIG-3524 error-return sweep.
//!
//! The fixture yields `(guard, make, make_invocation, make_error_invocation)`:
//! `make_invocation` drives the crash placements, and `make_error_invocation`
//! — a `(scenario, scope)` factory — supplies the journaled controller the
//! error-return sweep needs on tiers that have an effect journal.
//!
//! `turn_crash_matrix_tests!` registers all three. A tier that must defer the
//! crash-and-recover laws but can still hold the trace registers
//! `turn_crash_trace_tests!` and `turn_crash_recovery_tests!` instead, each
//! with its own attributes, so a deferral names exactly the laws it parks.

/// Register one level-one turn crash law.
#[macro_export]
macro_rules! __turn_crash_matrix_register {
    ([$($attr:tt)*] $fixture:block; $law:ident, $label:literal, trace) => {
        $($attr)*
        #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
        async fn $law() {
            let (_guard, make, _make_invocation, _make_error_invocation) = $fixture;
            let _ = $label;
            Box::pin($crate::registration_macro_support::$law(make)).await;
            $crate::law_receipt::record(module_path!(), stringify!($law), $label);
        }
    };
    ([$($attr:tt)*] $fixture:block; $law:ident, $label:literal, matrix) => {
        $($attr)*
        #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
        async fn $law() {
            let (_guard, make, make_invocation, _make_error_invocation) = $fixture;
            let _ = $label;
            Box::pin($crate::registration_macro_support::$law(
                make,
                make_invocation,
            ))
            .await;
            $crate::law_receipt::record(module_path!(), stringify!($law), $label);
        }
    };
    ([$($attr:tt)*] $fixture:block; $law:ident, $label:literal, error_return) => {
        $($attr)*
        #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
        async fn $law() {
            let (_guard, make, _make_invocation, make_error_invocation) = $fixture;
            let _ = $label;
            Box::pin($crate::registration_macro_support::$law(
                make,
                make_error_invocation,
            ))
            .await;
            $crate::law_receipt::record(module_path!(), stringify!($law), $label);
        }
    };
}

/// Register the level-one turn crash checks: trace, matrix and error return.
#[macro_export]
macro_rules! turn_crash_matrix_tests {
    ($(#[$attr:meta])* $fixture:block) => {
        $crate::turn_crash_matrix_tests!(@catalogue [$(#[$attr])*] $fixture; [
            (turn_crash_trace_drift_check, "turn-crash-trace-drift", trace),
            (turn_crash_matrix_level_1, "turn-crash-matrix-level-1", matrix),
            (
                turn_crash_matrix_error_return_fail_stop,
                "turn-crash-matrix-error-return-fail-stop",
                error_return
            ),
        ]);
    };
    (@catalogue $attrs:tt $fixture:block; [$(( $law:ident, $label:literal, $mode:ident )),* $(,)?]) => {
        $(
            $crate::__turn_crash_matrix_register!($attrs $fixture; $law, $label, $mode);
        )*
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

/// Register only the crash-and-recover laws: the level-one matrix and the
/// error-return sweep.
#[macro_export]
macro_rules! turn_crash_recovery_tests {
    ($(#[$attr:meta])* $fixture:block) => {
        $crate::turn_crash_recovery_tests!(@catalogue [$(#[$attr])*] $fixture; [
            (turn_crash_matrix_level_1, "turn-crash-matrix-level-1", matrix),
            (
                turn_crash_matrix_error_return_fail_stop,
                "turn-crash-matrix-error-return-fail-stop",
                error_return
            ),
        ]);
    };
    (@catalogue $attrs:tt $fixture:block; [$(( $law:ident, $label:literal, $mode:ident )),* $(,)?]) => {
        $(
            $crate::__turn_crash_matrix_register!($attrs $fixture; $law, $label, $mode);
        )*
    };
}
