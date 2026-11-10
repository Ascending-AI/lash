//! A parked run's carry faults and the predecessor facts its embedding
//! authenticates before the run is rewritten ([`crate::carry_run`]).

/// Infrastructure faults remain distinct from a semantic carry refusal.
#[derive(Debug, thiserror::Error)]
pub enum CarryFault {
    #[error(transparent)]
    Refused(#[from] crate::ParkedRefusal),
    #[error(transparent)]
    Heap(#[from] lash_kernel_doc::heap::HeapFault),
}

/// Authenticated predecessor facts carried intact to the rewrite boundary.
/// The live selection is transient; the root remains its sole durable owner
/// of the executing helper-library identity.
#[derive(Clone, Debug)]
pub struct CarryContext {
    pub kernel: u32,
    pub closure: lash_kernel_doc::heap::PageId,
    /// Ordinary defining code authenticated by the pinned closure's producer.
    pub code: std::collections::BTreeMap<
        lash_kernel_value::LinkId,
        std::collections::BTreeSet<lash_kernel_value::FunctionId>,
    >,
    pub links:
        std::collections::BTreeMap<lash_kernel_value::LinkId, lash_kernel_value::package::Link>,
    pub selection: lash_kernel_doc::Selection,
    /// Live function values keep their defining code and occurrence history.
    pub retained: std::collections::BTreeSet<lash_kernel_value::CallableId>,
    /// Producer-validated live/held operation uses; no resolution occurs here.
    pub uses: std::collections::BTreeMap<
        lash_kernel_value::OperationUse,
        lash_kernel_value::OperationUse,
    >,
}
