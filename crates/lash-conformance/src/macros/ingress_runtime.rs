//! Registration macro for the session-ingress runtime laws (FIG-3600 S8).

/// Register one independently reported test per session-ingress runtime law
/// (ADR 0101 §16). The fixture yields the drive-admission tuple: a guard, a
/// prefix, the tier's effect host, the store set under test and its
/// [`ConformanceTurnRunner`](crate::ConformanceTurnRunner).
///
/// Each law is a function of the same name in
/// [`registration_macro_support::ingress_runtime`](crate::registration_macro_support::ingress_runtime).
#[macro_export]
macro_rules! ingress_runtime_tests {
    ($(#[$attr:meta])* $fixture:block) => {
        $crate::ingress_runtime_tests!(@laws ingress_runtime [$(#[$attr])*] $fixture; []);
    };
    (@laws $module:ident $attrs:tt $fixture:block; [$($(#[$law_attr:meta])* ( $law:ident, $label:literal )),* $(,)?]) => {
        $(
            $crate::ingress_runtime_tests!(@law $module $attrs [$(#[$law_attr])*] $fixture; ($law, $label));
        )*
    };
    (@law $module:ident [$($attr:tt)*] [$($law_attr:tt)*] $fixture:block; ($law:ident, $label:literal)) => {
        $($attr)*
        $($law_attr)*
        #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
        async fn $law() {
            let (_guard, prefix, host, stores, runner) = $fixture;
            $crate::registration_macro_support::$module::$law(prefix, host, stores, runner).await;
            $crate::law_receipt::record(module_path!(), stringify!($law), $label);
        }
    };
}
