//! Native journal response-derivation recovery registration.

#[macro_export]
macro_rules! effect_controller_response_derivation_tests {
    ($fixture:block) => {
        $crate::effect_controller_response_derivation_tests!(@catalogue $fixture; [
            (effect_controller_response_derivation_retry, "effect-controller-response-derivation-retry"),
            (effect_controller_response_derivation_terminals, "effect-controller-response-derivation-terminals"),
        ]);
    };
    (@catalogue $fixture:block; [$(( $law:ident, $label:literal )),* $(,)?]) => {
        $(
            #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
            async fn $law() {
                let (_fixture_guard, make) = $fixture;
                $crate::registration_macro_support::$law(make).await;
                $crate::law_receipt::record(module_path!(), stringify!($law), $label);
            }
        )*
    };
}
