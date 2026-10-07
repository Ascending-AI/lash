//! Registration macro for the session-ingress store laws: the shared
//! admission sequence and the reserved source keys.

/// Register one independently reported test per session-ingress store law.
///
/// The fixture block yields `(guard, handles)`: a value kept alive for the
/// test's duration, and the [`SessionIngressHandles`](crate::SessionIngressHandles)
/// of a fresh session named
/// [`SESSION_INGRESS_SESSION_ID`](crate::SESSION_INGRESS_SESSION_ID).
#[macro_export]
macro_rules! session_ingress_tests {
    ($fixture:block) => {
        $crate::session_ingress_tests!(@catalogue $fixture; [
            (every_ingress_producer_shares_the_session_sequence, "ingress-shared-sequence"),
            (ingress_reserved_source_keys_are_refused_before_admission, "ingress-reserved-source-keys"),
        ]);
    };
    (@catalogue $fixture:block; [$(( $law:ident, $label:literal )),* $(,)?]) => {
        $(
            #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
            async fn $law() {
                let (_fixture_guard, handles) = $fixture;
                let _ = $label;
                $crate::registration_macro_support::$law(handles).await;
            }
        )*
    };
}
