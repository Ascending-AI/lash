//! Effect-host conformance registration.

#[macro_export]
macro_rules! effect_host_tests {
    ($fixture:block) => {
        $crate::effect_host_tests!(@catalogue $fixture; [
            (effect_host, "effect-host"),
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
