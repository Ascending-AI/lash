//! Registration macros for turn-ingress laws: direct-turn acceptance
//! (ADR 0069), the cancelled
//! turn's withheld input (FIG-3531), and the redrive that cedes rows it
//! restored from its journal (FIG-3552). All take the same
//! `(guard, prefix, backend, store)` fixture: the backend under test and a
//! session store of that backend's catalog. They share one catalogue arm.

/// Register one independently reported test per direct-turn acceptance law.
#[macro_export]
macro_rules! direct_turn_acceptance_tests {
    ($fixture:block) => {
        $crate::direct_turn_acceptance_tests!(@catalogue $fixture; [
            (direct_turn_accepts_before_driving, "direct-turn-accepts-before-driving"),
            (orphaned_direct_turn_input_is_drivable_by_another_worker, "direct-turn-orphan-recovery"),
            (direct_turn_acceptance_mints_no_idempotency_key, "direct-turn-identity"),
            (unclaimed_turn_input_settlement_is_a_conditional_write, "direct-turn-conditional-settlement"),
            (busy_execution_lane_defers_an_accepted_direct_turn, "direct-turn-busy-lane"),
            (vacuum_then_redrive_replays_receipt_single_row, "direct-turn-vacuum-redrive-single"),
            (vacuum_then_redrive_replays_receipt_absorbed_rows, "direct-turn-vacuum-redrive-absorbed"),
            (cancelled_vacuumed_acceptance_is_not_resurrected, "direct-turn-cancelled-vacuumed"),
            (uncommitted_redrive_drives_journaled_set_not_live_claim, "direct-turn-uncommitted-redrive"),
            (uncommitted_redrive_cedes_when_a_drain_answered_its_rows, "direct-turn-redrive-after-drain"),
            (drive_effect_refusal_is_journaled, "direct-turn-refused-drive"),
            (direct_turn_behind_earlier_admissions_runs_after_them, "direct-turn-queued-input"),
            (accept_turn_input_redrive_after_store_commit_admits_one_row, "direct-turn-acceptance-lost-outcome"),
        ]);
    };
    (@catalogue $fixture:block; [$(( $law:ident, $label:literal )),* $(,)?]) => {
        $(
            #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
            async fn $law() {
                let (_fixture_guard, prefix, backend, store) = $fixture;
                let _ = $label;
                $crate::registration_macro_support::$law(prefix, backend, store).await;
                $crate::law_receipt::record(module_path!(), stringify!($law), $label);
            }
        )*
    };
}

/// Register one independently reported test per cancelled-turn withheld-input
/// law (FIG-3531). The fixture shape is the direct-turn one, so the catalogue
/// arm is shared.
#[macro_export]
macro_rules! cancelled_turn_withheld_input_tests {
    ($fixture:block) => {
        $crate::direct_turn_acceptance_tests!(@catalogue $fixture; [
            (immediate_cancel_defers_withheld_inject_now_input, "cancel-defers-withheld-input"),
        ]);
    };
}

/// Register one independently reported test per restored-claim cede law
/// (FIG-3552). The fixture shape is the direct-turn one, so the catalogue arm
/// is shared.
#[macro_export]
macro_rules! restored_claim_cede_tests {
    ($fixture:block) => {
        $crate::direct_turn_acceptance_tests!(@catalogue $fixture; [
            (a_redrive_commits_nothing_for_input_a_recovery_drain_answered, "restored-claim-recovery-answered-input"),
            (a_redrive_commits_nothing_for_work_a_recovery_checkpoint_answered, "restored-claim-recovery-answered-work"),
            (a_redrive_cedes_checkpoint_input_a_peer_reclaimed, "restored-claim-cede-input"),
            (a_redrive_cedes_checkpoint_work_a_peer_reclaimed, "restored-claim-cede-queued-work"),
        ]);
    };
}
