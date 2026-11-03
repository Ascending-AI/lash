//! The FIG-7516 source derivation supplies bulk membership and holding counts.
//! Keep these counts, rather than reconstructing a copy pattern in the VM.
//! Logical byte bounds remain the source event program: members use their
//! actual immutable occurrence prices, and callees include frames and cells.
use super::*;
#[path = "bulk/path.rs"]
mod path;
pub(crate) use path::{CollectionProgram, CollectionPublicationStorage};

pub(crate) struct CollectionResult<'a> {
    pub(crate) writes: &'a [(
        lash_kernel_state::kernel_internals::logical::Slot,
        doc::Value,
    )],
    pub(crate) clears: &'a [lash_kernel_state::kernel_internals::logical::Slot],
    pub(crate) member: Option<&'a lash_kernel_state::kernel_internals::logical::MemberEdit>,
    pub(crate) branches: &'a [(Site, bool)],
    pub(crate) exit: &'a publication::Position,
}

impl Source {
    /// Resolve a collection kernel's physical bindings through the immutable
    /// source placement. A physical index never becomes a logical owner ID.
    pub(crate) fn collection_bindings(
        &self,
        placement: &placement::BindingPlacement,
        physical: &[usize],
    ) -> Result<Box<[lash_kernel_state::kernel_internals::logical::Slot]>, Refused> {
        if !std::ptr::eq(self, placement.source().as_ref()) {
            return Err(Refused::Source);
        }
        self.verify()?;
        physical
            .iter()
            .map(|&physical| {
                let physical = u32::try_from(physical).map_err(|_| Refused::Source)?;
                let mut selected = self.bindings.iter().filter_map(|binding| {
                    let lash_kernel_state::kernel_internals::logical::Owner::Value { slot, .. } =
                        self.binding_root(binding.index, doc::TaskId::MAIN, 0)?
                    else {
                        return None;
                    };
                    (placement.physical(slot) == Some(physical)).then_some((binding, slot))
                });
                let (binding, slot) = selected.next().ok_or(Refused::Source)?;
                if binding.shared || selected.next().is_some() || !placement.unshared(physical) {
                    return Err(Refused::Source);
                }
                Ok(slot)
            })
            .collect()
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Bulk {
    pub(crate) function: FunctionId,
    pub(crate) dense: bool,
    pub(crate) temporary_values: u32,
    pub(crate) read_values: u32,
    pub(crate) depth: u32,
    pub(crate) read_units: u64,
    pub(crate) continuation_units: u64,
    pub(crate) native_requirements: Box<[FunctionId]>,
}
impl Bulk {
    pub(super) fn project(function: FunctionId, path: &doc::plan::CopyLoop) -> Self {
        Self {
            function,
            dense: matches!(path.kind, doc::plan::CopyKind::Dense),
            temporary_values: path.temporary_values,
            read_values: path.read.room,
            depth: path.read.depth,
            read_units: path.read_units,
            continuation_units: path.loop_units,
            native_requirements: path.read.requires.iter().map(|r| r.function).collect(),
        }
    }
}
/// The event verifier still reads the original AST. This additionally binds
/// the projected induction to the exact HelperPlan's upstream source proof.
/// Its counts do not substitute 16-byte slots for dynamic immutable payloads.
pub(super) fn verify(source: &Source, room: &SourceRoom) -> Result<(), Refused> {
    match (source.bulk, room.bulk.as_ref()) {
        (None, None) => Ok(()),
        (None, Some(_)) => Err(Refused::Source),
        (Some(_), None) => Err(Refused::Peak),
        (Some(path), Some(projected)) => {
            if source.owner != doc::constant_pool::PoolOwner::Function(projected.function) {
                return Err(Refused::Source);
            }
            if projected.dense != matches!(path.kind, doc::plan::CopyKind::Dense)
                || projected.native_requirements.iter().copied().ne(path
                    .read
                    .requires
                    .iter()
                    .map(|r| r.function))
            {
                return Err(Refused::Source);
            }
            if projected.temporary_values != path.temporary_values
                || projected.read_values != path.read.room
                || projected.depth != path.read.depth
            {
                return Err(Refused::Peak);
            }
            if projected.read_units != path.read_units
                || projected.continuation_units != path.loop_units
            {
                return Err(Refused::Exit);
            }
            Ok(())
        }
    }
}
