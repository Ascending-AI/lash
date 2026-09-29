//! Response-derivation terminal recovery registration.

#[macro_export]
macro_rules! effect_controller_response_derivation_tests {
    ($fixture:block) => {
        $crate::effect_controller_response_derivation_tests!(@catalogue $fixture; [
            (effect_controller_response_derivation_terminals, "effect-controller-response-derivation-terminals"),
            (attempt_history_terminal_variants_survive_result_replay, "attempt-terminal-replay"),
        ]);
    };
    (@catalogue $fixture:block; [$(( $law:ident, $label:literal )),* $(,)?]) => {
        $(
            #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
            async fn $law() {
                let (_fixture_guard, make) = $fixture;
                $crate::registration_macro_support::$law(make).await;
            }
        )*
    };
}
