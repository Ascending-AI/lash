//! The resource owner and staged storage commit at the same source frontier.
use super::*;
use crate::admission::SourceResources;
use lash_kernel_state::kernel_internals::logical::MemberEdit;

use crate::machine::source_publication::{Admission, Context};
pub(super) use crate::machine::source_publication::{Prefix, native_fault};

pub(super) use crate::machine::source_publication::CellConstruction;
impl<const OBSERVING: bool> Island<'_, OBSERVING> {
    pub(super) fn publish_source_boundary<
        S: crate::machine::source_publication::BoundaryPreparation,
    >(
        &mut self,
        callee: Callee<'_>,
        activation: &mut Activation,
        prefix: Prefix,
        prepare: S,
        semantic: impl FnOnce(&mut Arena, &mut Session, Vec<MemberEdit>, S::Prepared),
    ) -> Result<bool, HeapFault> {
        let source = &self.exe.code(callee.plan.code).source_room;
        let document = self
            .exe
            .instances
            .get(&callee.link)
            .ok_or_else(|| malformed("source link has no admitted document"))?;
        let kernel = doc::KernelVersion::of(document.kernel())
            .ok_or_else(|| malformed("source kernel is unsupported"))?;
        let safe_point = self.charged;
        Context {
            admission: Admission::Envelope(self.memory_bound),
            source,
            link: callee.link,
            kernel,
            logical: &self.logical,
            heap: self.heap,
            arena: self.arena,
            session: self.session,
            resources: SourceResources {
                charged: &mut self.charged,
                counters: self.counters,
                bound: self.bound,
                slice: self.slice_end,
                depth: (self.base + u64::from(self.depth)).saturating_sub(1),
                limit: self.call_depth,
                safe_point,
            },
            logical_memory: self.logical_memory,
        }
        .publish_external_remaining(
            &activation.invocation,
            &mut activation.frontier,
            prefix,
            prepare,
            semantic,
            (&mut activation.original, self.recorder),
        )
        .map_err(|halt| match halt {
            crate::machine::Halt::Heap(error) => *error,
            _ => malformed("an envelope publication ended the run"),
        })
    }

    pub(super) fn publish_source_departure(
        &mut self,
        callee: Callee<'_>,
        activation: &mut Activation,
        recipe: crate::machine::source_publication::return_storage::ReturnRecipe,
    ) -> Result<bool, HeapFault> {
        let source = &self.exe.code(callee.plan.code).source_room;
        let document = self
            .exe
            .instances
            .get(&callee.link)
            .ok_or_else(|| malformed("source link has no admitted document"))?;
        let kernel = doc::KernelVersion::of(document.kernel())
            .ok_or_else(|| malformed("source kernel is unsupported"))?;
        let safe_point = self.charged;
        Context {
            admission: Admission::Envelope(self.memory_bound),
            source,
            link: callee.link,
            kernel,
            logical: &self.logical,
            heap: self.heap,
            arena: self.arena,
            session: self.session,
            resources: SourceResources {
                charged: &mut self.charged,
                counters: self.counters,
                bound: self.bound,
                slice: self.slice_end,
                depth: (self.base + u64::from(self.depth)).saturating_sub(1),
                limit: self.call_depth,
                safe_point,
            },
            logical_memory: self.logical_memory,
        }
        .publish_departure_remaining(
            &activation.invocation,
            &mut activation.frontier,
            recipe,
            (),
            |_, _, ()| {},
            (&mut activation.original, self.recorder),
        )
        .map_err(|halt| match halt {
            crate::machine::Halt::Heap(error) => *error,
            _ => malformed("an envelope departure ended the run"),
        })
    }
}
