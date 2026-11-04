//! Bounded cold reconstruction tables; no history of loop occurrences.
use super::Reg;
use crate::compile::room::Point;
use lash_kernel_state::kernel_internals::logical::LeaseValue;
use lash_kernel_state::kernel_internals::logical::Slot;

pub(crate) struct RecoveryMap {
    pub(crate) roles: Box<[(Slot, Reg<LeaseValue>)]>,
    pub(crate) operands: Box<[Reg<LeaseValue>]>,
    pub(crate) pending: Option<Reg<LeaseValue>>,
    pub(crate) continuation: Point,
    pub(crate) graph: super::GraphRecovery,
}

/// Untrusted inside RegionDraft; verified only as part of immutable RegionCode.
/// These tables grant no state or semantic authority on their own.
pub(crate) struct GraphRecovery {
    source: Point,
    roles: Box<[(Slot, Reg<LeaseValue>)]>,
    pending: Option<Reg<LeaseValue>>,
    scratch: Box<[Reg<LeaseValue>]>,
    release_phase: u32,
    versions: Box<[(Reg<LeaseValue>, u32)]>,
    identities: Box<[(Reg<LeaseValue>, u32)]>,
}
impl GraphRecovery {
    #[cfg(any(test, feature = "testing"))]
    pub(super) fn fixture(source: &Point) -> Self {
        Self {
            source: source.clone(),
            roles: Box::new([]),
            pending: None,
            scratch: Box::new([]),
            release_phase: 0,
            versions: Box::new([]),
            identities: Box::new([]),
        }
    }
}
