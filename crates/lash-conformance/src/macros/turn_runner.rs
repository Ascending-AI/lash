//! Registration macros for surviving turn-runner and binding laws.
//! Split from `macros.rs` to keep each catalogue file inside the support-file
//! line budget.

/// Register the laws that execute a real turn through the tier's
/// [`ConformanceTurnRunner`](crate::ConformanceTurnRunner): the public
/// signal-intent wake.
///
/// The fixture hands back a guard, a session prefix, the tier's effect host,
/// the store set under test (whose session catalog and process registry the
/// law's runtime uses), the process-work substrate, the tier's turn runner and
/// a post-law verification handed the law's name.
#[macro_export]
macro_rules! turn_runner_tests {
    ($(#[$attr:meta])* $fixture:block) => {
        $crate::__turn_runner_register!([$(#[$attr])*] $fixture;
            (public_signal_intent_wakes_parked_process, "public-signal-intent-wake"));
    };
}

/// Register the FIG-1293 migrated-tools crash-redrive law. The fixture hands
/// back a guard, a prefix, the effect host, the store set under test, the tier's
/// turn runner and the plugin factories (the standard protocol, `spawn_agent`,
/// `cancel_process`) from the crates above this one.
#[macro_export]
macro_rules! migrated_tools_redrive_tests {
    ($(#[$attr:meta])* $fixture:block) => {
        $crate::migrated_tools_redrive_tests!(@law [$(#[$attr])*] $fixture;
            (public_migrated_tools_redrive_to_literal_outcomes, "migrated-tools-redrive"));
    };
    (@law [$($attr:tt)*] $fixture:block; ($law:ident, $label:literal)) => {
        $($attr)*
        #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
        async fn $law() {
            let (_guard, prefix, host, stores, runner, plugins) = $fixture;
            $crate::registration_macro_support::$law(prefix, host, stores, runner, plugins).await;
        }
    };
}

/// Register the turn-config law (FIG-4508): a run without its recorded
/// termination refuses terminal assembly typed, across the plugin and host
/// boundaries. The fixture hands back a guard, a prefix, the tier's effect
/// host, the store set under test and its
/// [`ConformanceTurnRunner`](crate::ConformanceTurnRunner).
#[macro_export]
macro_rules! turn_config_tests {
    ($(#[$attr:meta])* $fixture:block) => {
        $crate::turn_config_tests!(@law [$(#[$attr])*] $fixture;
            (a_missing_recorded_termination_is_a_typed_terminal_refusal, "turn-config-missing-recorded-termination"));
    };
    (@law [$($attr:tt)*] $fixture:block; ($law:ident, $label:literal)) => {
        $($attr)*
        #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
        async fn $law() {
            let (_guard, prefix, host, stores, runner) = $fixture;
            $crate::registration_macro_support::$law(prefix, host, stores, runner).await;
        }
    };
}

/// Register one turn-runner law.
#[macro_export]
macro_rules! __turn_runner_register {
    ([$($attr:tt)*] $fixture:block; ($law:ident, $label:literal)) => {
        $($attr)*
        #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
        async fn $law() {
            let (_guard, prefix, host, stores, work, runner, verify) = $fixture;
            $crate::registration_macro_support::$law(prefix, host, stores, work, runner).await;
            verify(stringify!($law)).await;
        }
    };
}
