//! Registration macro for the build-generation drain laws (FIG-3799,
//! FIG-3884).

/// Register one independently reported test per generation drain law.
///
/// The fixture block yields `(guard, fixture)`: a value kept alive for the
/// test's duration, and a
/// [`GenerationDrainLawFixture`](crate::GenerationDrainLawFixture) over a
/// fresh store set.
#[macro_export]
macro_rules! generation_drain_tests {
    ($fixture:block) => {
        $crate::generation_drain_tests!(@catalogue $fixture; [
            (
                in_flight_turns_follow_their_admitting_generation,
                "generation-drain-in-flight"
            ),
        ]);
    };
    (@catalogue $fixture:block; [$(( $law:ident, $label:literal )),* $(,)?]) => {
        $(
            #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
            async fn $law() {
                let (_fixture_guard, fixture) = $fixture;
                let _ = $label;
                $crate::$law(fixture).await;
                $crate::law_receipt::record(module_path!(), stringify!($law), $label);
            }
        )*
    };
}
