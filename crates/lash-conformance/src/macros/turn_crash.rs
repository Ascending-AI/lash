//! Registration macros for the turn crash checks. Every law runs its turns on
//! the tier's [`ConformanceTurnRunner`](crate::ConformanceTurnRunner), so one
//! law body states the contract in process and inside a Restate handler.
//!
//! Every fixture yields `(guard, stores, make, host, runner)`: `stores`
//! supplies every port the laws do not certify, `make` opens a conformance
//! handle on a scenario's session store over the same substrate, `host` is the
//! tier's effect host, and `runner` runs, crashes and recovers the turns the
//! tier's way (see the `turn_runner` module docs).
//!
//! `turn_crash_matrix_tests!` registers the golden-trace drift check, the
//! FIG-3524 error-return sweep and the after-commit redrive, and
//! `turn_crash_level_1_tests!` the level-one matrix, so a tier whose crashed
//! attempt keeps running work it cannot stop parks the matrix alone. A tier
//! that must park one of the first three registers the single-law macros
//! instead: `turn_crash_trace_tests!`, `turn_crash_error_return_tests!` and
//! `turn_crash_after_commit_redrive_tests!`.

/// Register the golden-trace drift check, the error-return sweep and the
/// after-commit redrive. The level-one matrix registers on its own
/// ([`turn_crash_level_1_tests!`]).
#[macro_export]
macro_rules! turn_crash_matrix_tests {
    ($(#[$attr:meta])* $fixture:block) => {
        $crate::__turn_crash_runner_register!([$(#[$attr])*] $fixture;
            (turn_crash_trace_drift_check, "turn-crash-trace-drift"));
        $crate::__turn_crash_runner_register!([$(#[$attr])*] $fixture;
            (turn_crash_matrix_error_return_fail_stop, "turn-crash-matrix-error-return-fail-stop"));
        $crate::__turn_crash_runner_register!([$(#[$attr])*] $fixture;
            (turn_crash_after_commit_redrive_replays_the_committed_receipt,
                "turn-crash-after-commit-redrive"));
    };
}

/// Register the golden-trace drift check of [`turn_crash_matrix_tests!`]
/// alone.
#[macro_export]
macro_rules! turn_crash_trace_tests {
    ($(#[$attr:meta])* $fixture:block) => {
        $crate::__turn_crash_runner_register!([$(#[$attr])*] $fixture;
            (turn_crash_trace_drift_check, "turn-crash-trace-drift"));
    };
}

/// Register the FIG-3524 error-return sweep of [`turn_crash_matrix_tests!`]
/// alone.
#[macro_export]
macro_rules! turn_crash_error_return_tests {
    ($(#[$attr:meta])* $fixture:block) => {
        $crate::__turn_crash_runner_register!([$(#[$attr])*] $fixture;
            (turn_crash_matrix_error_return_fail_stop, "turn-crash-matrix-error-return-fail-stop"));
    };
}

/// Register the after-commit redrive of [`turn_crash_matrix_tests!`] alone.
#[macro_export]
macro_rules! turn_crash_after_commit_redrive_tests {
    ($(#[$attr:meta])* $fixture:block) => {
        $crate::__turn_crash_runner_register!([$(#[$attr])*] $fixture;
            (turn_crash_after_commit_redrive_replays_the_committed_receipt,
                "turn-crash-after-commit-redrive"));
    };
}

/// Register the engine-neutral layer law: a layer over the tier's effect
/// host observes the effects of the group children its turns open. The
/// fixture is the runner fixture of [`turn_crash_runner_tests!`].
#[macro_export]
macro_rules! effect_layer_group_child_tests {
    ($(#[$attr:meta])* $fixture:block) => {
        $crate::__turn_crash_runner_register!([$(#[$attr])*] $fixture;
            (a_host_layer_observes_its_group_childrens_effects,
                "effect-layer-group-child"));
    };
}

/// Register only the level-one crash matrix. Its crashes kill the turn's
/// execution where it stands, so a tier whose crashed attempt keeps running
/// work it cannot stop registers it with its own attributes.
#[macro_export]
macro_rules! turn_crash_level_1_tests {
    // A tier that cannot recover some crash points yet parks each under its
    // known-defect ticket (`ParkedTurnCrashPoint`); every other point runs.
    (parked: $parked:expr; $(#[$attr:meta])* $fixture:block) => {
        $(#[$attr])*
        #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
        async fn turn_crash_matrix_level_1() {
            let (_guard, stores, make, host, runner) = $fixture;
            Box::pin($crate::registration_macro_support::turn_crash_matrix_level_1_parking(
                stores, make, host, runner, $parked,
            ))
            .await;
            $crate::law_receipt::record(
                module_path!(),
                "turn_crash_matrix_level_1",
                "turn-crash-matrix-level-1",
            );
        }
    };
    ($(#[$attr:meta])* $fixture:block) => {
        $crate::__turn_crash_runner_register!([$(#[$attr])*] $fixture;
            (turn_crash_matrix_level_1, "turn-crash-matrix-level-1"));
    };
}

/// Register the turn crash laws that run their turns on the tier's
/// [`ConformanceTurnRunner`](crate::ConformanceTurnRunner): the FIG-3571
/// generation-refusal pair, the direct-acceptance crash after its store
/// commit, and the turn-cancel closure across a crash at each of its cuts.
///
/// The fixture yields `(guard, stores, make, host, runner)`: `stores` supplies
/// every port the laws do not certify, `make` opens a conformance handle on a
/// scenario's session store over the same substrate, `host` is the tier's
/// effect host, and `runner` runs, crashes and recovers the turns the tier's
/// way (see the `turn_runner` module docs).
///
/// A tier that must park one of these laws registers the single-law macros
/// instead, each with its own attributes, so a deferral names exactly the law
/// it parks: [`turn_crash_generation_redrive_tests!`],
/// [`turn_crash_generation_claim_tests!`],
/// [`turn_crash_direct_acceptance_tests!`] and
/// [`turn_crash_cancel_closure_tests!`].
#[macro_export]
macro_rules! turn_crash_runner_tests {
    ($(#[$attr:meta])* $fixture:block) => {
        $crate::__turn_crash_runner_register!([$(#[$attr])*] $fixture;
            (pre_cutover_generation_turn_redrive_is_refused_before_any_effect,
                "turn-crash-pre-cutover-redrive"));
        $crate::__turn_crash_runner_register!([$(#[$attr])*] $fixture;
            (pre_cutover_generation_turn_claim_is_refused_typed, "turn-crash-pre-cutover-claim"));
        $crate::__turn_crash_runner_register!([$(#[$attr])*] $fixture;
            (direct_turn_acceptance_crash_after_store_commit_admits_one_row,
                "turn-crash-direct-acceptance"));
        $crate::__turn_crash_runner_register!([$(#[$attr])*] $fixture;
            (turn_cancel_closure_recovers_from_a_crash_at_every_cut, "turn-crash-cancel-closure"));
    };
}

/// Register the FIG-3571 redrive refusal of [`turn_crash_runner_tests!`]
/// alone.
#[macro_export]
macro_rules! turn_crash_generation_redrive_tests {
    ($(#[$attr:meta])* $fixture:block) => {
        $crate::__turn_crash_runner_register!([$(#[$attr])*] $fixture;
            (pre_cutover_generation_turn_redrive_is_refused_before_any_effect, "turn-crash-pre-cutover-redrive"));
    };
}

/// Register the FIG-3619 claim refusal of [`turn_crash_runner_tests!`] alone.
#[macro_export]
macro_rules! turn_crash_generation_claim_tests {
    ($(#[$attr:meta])* $fixture:block) => {
        $crate::__turn_crash_runner_register!([$(#[$attr])*] $fixture;
            (pre_cutover_generation_turn_claim_is_refused_typed, "turn-crash-pre-cutover-claim"));
    };
}

/// Register the direct-acceptance crash of [`turn_crash_runner_tests!`]
/// alone.
#[macro_export]
macro_rules! turn_crash_direct_acceptance_tests {
    ($(#[$attr:meta])* $fixture:block) => {
        $crate::__turn_crash_runner_register!([$(#[$attr])*] $fixture;
            (direct_turn_acceptance_crash_after_store_commit_admits_one_row, "turn-crash-direct-acceptance"));
    };
}

/// Register the turn-cancel closure cuts of [`turn_crash_runner_tests!`]
/// alone.
#[macro_export]
macro_rules! turn_crash_cancel_closure_tests {
    ($(#[$attr:meta])* $fixture:block) => {
        $crate::__turn_crash_runner_register!([$(#[$attr])*] $fixture;
            (turn_cancel_closure_recovers_from_a_crash_at_every_cut, "turn-crash-cancel-closure"));
    };
}

/// Register one runner-driven turn crash law.
#[macro_export]
macro_rules! __turn_crash_runner_register {
    ([$($attr:tt)*] $fixture:block; ($law:ident, $label:literal)) => {
        $($attr)*
        #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
        async fn $law() {
            let (_guard, stores, make, host, runner) = $fixture;
            Box::pin($crate::registration_macro_support::$law(stores, make, host, runner)).await;
            $crate::law_receipt::record(module_path!(), stringify!($law), $label);
        }
    };
}
