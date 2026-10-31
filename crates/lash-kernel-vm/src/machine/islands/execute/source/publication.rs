//! The resource owner and canonical journal commit at the same source frontier.
use super::*;
use crate::admission::SourceResources;
use lash_kernel_state::kernel_internals::logical::{MemberEdit, Program};

use crate::machine::source_publication::{Admission, Context};
pub(super) use crate::machine::source_publication::{
    Prefix, PreparedStorage, Reserve, StoragePreparation, native_fault,
};

pub(super) use crate::machine::source_publication::{CellConstruction, ClosureConstruction};
impl<const OBSERVING: bool> Island<'_, OBSERVING> {
    pub(super) fn publish_source(
        &mut self,
        callee: Callee<'_>,
        activation: &mut Activation,
        prefix: Prefix,
        make: impl Fn() -> Program,
        semantic: impl FnOnce(&mut Arena, Vec<MemberEdit>),
    ) -> Result<bool, HeapFault> {
        self.publish_source_prepared(
            callee,
            activation,
            prefix,
            make,
            Reserve(|_: &mut Arena| Ok(Some(()))),
            |arena, _, edits, ()| semantic(arena, edits),
        )
    }

    pub(super) fn publish_source_prepared<S: StoragePreparation>(
        &mut self,
        callee: Callee<'_>,
        activation: &mut Activation,
        prefix: Prefix,
        make: impl Fn() -> Program,
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
            admission: Admission::Envelope,
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
                depth: self.base + u64::from(self.depth),
                limit: self.call_depth,
                safe_point,
            },
            memory_bound: self.memory_bound,
            logical_memory: self.logical_memory,
        }
        .publish(
            &activation.invocation,
            &mut activation.frontier,
            prefix,
            make,
            prepare,
            semantic,
        )
    }
}
