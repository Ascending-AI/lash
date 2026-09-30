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
            (direct_turn_accepts_before_driving, "direct-turn-accepts-before-driving"),
            (direct_turn_acceptance_mints_no_idempotency_key, "direct-turn-identity"),
            (vacuum_then_redrive_replays_receipt_single_row, "direct-turn-vacuum-redrive-single"),
            (vacuum_then_redrive_replays_receipt_absorbed_rows, "direct-turn-vacuum-redrive-absorbed"),
            (cancelled_vacuumed_acceptance_is_not_resurrected, "direct-turn-cancelled-vacuumed"),
            (uncommitted_redrive_drives_journaled_set_not_live_admission, "direct-turn-uncommitted-redrive"),
            (drive_effect_refusal_is_journaled, "direct-turn-refused-drive"),
            (direct_turn_behind_earlier_admissions_runs_after_them, "direct-turn-queued-input"),
            (accept_turn_input_redrive_after_store_commit_admits_one_row, "direct-turn-acceptance-lost-outcome"),
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

/// Register one independently reported test per cancelled-turn withheld-work
/// law (FIG-3531, FIG-3543). The fixture shape is the direct-turn one, so the catalogue
/// arm is shared.
#[macro_export]
macro_rules! cancelled_turn_withheld_input_tests {
    ($(#[$attr:meta])* $fixture:block) => {
        $crate::direct_turn_acceptance_tests!(@catalogue [$(#[$attr])*] $fixture; [
            (immediate_cancel_defers_withheld_inject_now_input, "cancel-defers-withheld-input"),
            (immediate_cancel_defers_withheld_process_wakes, "cancel-defers-withheld-wakes"),
        ]);
    };
}
