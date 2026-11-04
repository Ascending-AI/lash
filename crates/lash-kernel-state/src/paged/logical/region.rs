//! Affine authority for one verified region's unpublished state exit.
use super::{Invocation, InvocationRange, MemberEdit, Owner, PublicationValue};
use crate::arena::Arena;
use crate::paged::heap_impl::PagedHeap;
use lash_kernel_doc::heap::HeapFault;

/// Exclusive state authority, consumed by preparation or abandoned on drop.
pub struct StateLease<'s> {
    heap: &'s mut PagedHeap,
    arena: &'s mut Arena,
}

/// The actual surviving state of C's sealed reached prefix. This payload
/// alone does not prove semantic correspondence or authorize publication.
pub struct StateExit {
    pub inputs: Box<[Owner]>,
    pub outputs: Box<[(Owner, PublicationValue)]>,
    pub members: Box<[MemberEdit]>,
    pub invocations: Option<InvocationRange>,
    pub peak: u64,
    pub delta: i128,
}

/// A fully authenticated exit awaiting the machine's prepared replacements.
pub struct PreparedStateExit<'s> {
    _authority: StateLease<'s>,
}

impl<'s> StateLease<'s> {
    /// Acquire shared-ledger exclusion and bind the complete entry stamp.
    pub fn acquire(_heap: &'s mut PagedHeap, _arena: &'s mut Arena) -> Result<Self, HeapFault> {
        Err(unavailable())
    }
    pub fn arena(&self) -> &Arena {
        self.arena
    }
    pub fn heap(&self) -> &PagedHeap {
        self.heap
    }
    /// Reserve task-qualified identities without advancing the ledger cursor.
    pub fn reserve_invocations(
        &mut self,
        _caller: &Invocation,
        _count: u64,
    ) -> Result<InvocationRange, HeapFault> {
        Err(unavailable())
    }
    /// Authenticate and stage the entire exit before any semantic edit.
    pub fn prepare(
        self,
        _exit: StateExit,
        _memory_bound: u64,
    ) -> Result<PreparedStateExit<'s>, HeapFault> {
        Err(unavailable())
    }
}
impl PreparedStateExit<'_> {
    /// Install prepared state and the machine's infallible replacements once.
    pub fn commit(self, _publish_vm: impl FnOnce()) -> Result<(), HeapFault> {
        Err(unavailable())
    }
}
fn unavailable() -> HeapFault {
    HeapFault::Malformed("region state authority has not been installed".into())
}

/// Payload builders for independent qualification fixtures. They confer no
/// lease, prepared transaction, or production semantic witness.
#[cfg(any(test, feature = "perf-witness"))]
pub mod fixtures {
    use super::*;
    pub fn exit(
        inputs: impl Into<Box<[Owner]>>,
        outputs: impl Into<Box<[(Owner, PublicationValue)]>>,
        peak: u64,
        delta: i128,
    ) -> StateExit {
        StateExit {
            inputs: inputs.into(),
            outputs: outputs.into(),
            members: Box::new([]),
            invocations: None,
            peak,
            delta,
        }
    }
}
