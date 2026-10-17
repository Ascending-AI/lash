//! Where in a workflow document something ran.
//!
//! The document is a kernel document, and the machine that runs it derives
//! every identity from the document's sites: a [`Site`] is one node, a
//! [`TaskLabel`] one task of a run, and an [`EffectId`] one
//! execution of a `perform` or a `sleep` by one task, with the loops around
//! it. Every layer that names an occurrence (durable effect and wait
//! records, trace facts, the overlay) holds these values as the machine
//! computed them; lash derives none of its own.

pub use lash_kernel_doc::{
    EffectId, LoopCounts, ObservationReader, PositionLabel, Site, TaskLabel, Unit, VisitId, WaitId,
};

/// Occurrence `occurrence` of the body of a function named `label`, run by
/// `main` outside every loop, in a test fixture.
#[cfg(any(test, feature = "testing"))]
#[doc(hidden)]
pub fn effect_identity_fixture(label: &str, occurrence: u64) -> EffectId {
    lash_kernel_doc::testkit::effect(
        lash_kernel_doc::testkit::root_task(),
        Site::new(
            Unit::Function(lash_kernel_doc::Name::new(label)),
            Vec::new(),
        ),
        occurrence,
        Vec::new(),
    )
}

pub use lash_kernel_doc::testkit as workflow_testkit;
