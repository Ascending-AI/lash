//! Where in a workflow document something ran.
//!
//! The document is a kernel document, and the machine that runs it derives
//! every identity from the document's sites: a [`Site`] is one node, a
//! [`TaskLabel`] one task of a run, and an [`EffectId`] one
//! execution of a `perform` or a `sleep` by one task, with the loops around
//! it. Every layer that names an occurrence (durable effect and wait
//! records, trace facts, the overlay) holds these values as the machine
//! computed them; lash derives none of its own.

/// Occurrence `occurrence` of the body of a function named `label`, run by
/// `main` outside every loop, in a test fixture.
#[cfg(any(test, feature = "testing"))]
#[doc(hidden)]
pub fn effect_identity_fixture(label: &str, occurrence: u64) -> lash_kernel_value::sites::EffectId {
    lash_kernel_value::sites::testkit::effect(
        lash_kernel_value::sites::testkit::root_task(),
        lash_kernel_value::sites::Site::new(
            lash_kernel_value::sites::Unit::Function(lash_kernel_value::names::Name::new(label)),
            Vec::new(),
        ),
        occurrence,
        Vec::new(),
    )
}
