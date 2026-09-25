//! Registration macro for the cancel-by-author law (FIG-3543).

/// Register the cancel-by-author law. The fixture is the one
/// [`ingress_runtime_tests!`](crate::ingress_runtime_tests) takes; each law is
/// a function of the same name in
/// [`registration_macro_support::cancel_by_author`](crate::registration_macro_support::cancel_by_author).
#[macro_export]
macro_rules! cancel_by_author_tests {
    ($(#[$attr:meta])* $fixture:block) => {
        $crate::ingress_runtime_tests!(@laws cancel_by_author [$(#[$attr])*] $fixture; []);
    };
}
