//! Native journal response-derivation recovery registration.

#[macro_export]
macro_rules! effect_controller_response_derivation_tests {
    ($fixture:block) => {
        $crate::effect_controller_replay_tests!(@catalogue $fixture; [
            (effect_controller_response_derivation_retry, "effect-controller-response-derivation-retry"),
            (effect_controller_response_derivation_terminals, "effect-controller-response-derivation-terminals"),
        ]);
    };
}
