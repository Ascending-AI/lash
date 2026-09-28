//! Registration macro for the session-ingress store laws: the shared
//! admission sequence and the drive-epoch seal.

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
            (the_drive_epoch_seal_is_idempotent_per_admission, "ingress-drive-epoch-seal"),
            (concurrent_seals_serialize, "ingress-concurrent-seals"),
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
