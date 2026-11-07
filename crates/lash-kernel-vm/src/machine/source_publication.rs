//! The source frontier, resource admission and staged storage share one commit.
mod binding;
mod calling;
mod cell;
mod closure;
mod departure;
mod end;
mod exact;
pub(in crate::machine) mod frame_entry;
pub(in crate::machine) use cell::{CellConstruction, PreparedObjectConstruction};
pub(in crate::machine) mod construction;
mod join;
mod leaf;
mod literal;
pub(in crate::machine) mod literal_storage;
mod member;
pub(in crate::machine) use member::SourceMember;
mod native;
pub(in crate::machine) mod native_storage;
mod occurrence;
mod read;
mod resuming;
pub(in crate::machine) mod return_storage;
mod returning;
pub(in crate::machine) use occurrence::source_loops;
mod scope;
mod session;
mod test;
use super::{Halt, limits::Limiters, session::Session};
use crate::admission::{Allowance, SourceResources};
use crate::compile::room::{
    self,
    publication::{Frontier, Position, PublicationError},
};
use lash_kernel_doc::{self as doc, arena::ArenaHandle, heap::HeapFault};
use lash_kernel_state::{
    arena::Arena,
    kernel_internals::{
        PagedHeap,
        logical::{Invocation, Ledger, MemberEdit},
    },
};
use std::{cell::Cell, sync::Arc};
fn malformed(message: &str) -> HeapFault {
    HeapFault::Malformed(message.into())
}
#[derive(Clone, Copy)]
pub(in crate::machine) enum ScopeLoop {
    While,
    For { continued: bool },
}

#[derive(Default)]
pub(in crate::machine) struct LeafFacts {
    pub(in crate::machine) dependencies: Vec<ArenaHandle>,
    pub(in crate::machine) call: Option<(
        lash_kernel_value::sites::Site,
        room::CallSource,
        room::CheckedRoom,
    )>,
    pub(in crate::machine) values: std::collections::BTreeMap<room::Measure, i128>,
}
impl room::Facts for LeafFacts {
    fn dependencies(&self) -> Option<&[ArenaHandle]> {
        Some(&self.dependencies)
    }
    fn measure(&self, measure: &room::Measure) -> Option<i128> {
        self.values.get(measure).copied()
    }
    fn call(
        &self,
        at: &lash_kernel_value::sites::Site,
        callee: room::CallSource,
    ) -> Option<&room::CheckedRoom> {
        self.call
            .as_ref()
            .filter(|(site, source, _)| site == at && *source == callee)
            .map(|(_, _, room)| room)
    }
    fn raised(&self) -> bool {
        false
    }
    fn resolved(
        &self,
        _: &lash_kernel_value::sites::Site,
    ) -> Option<lash_kernel_value::FunctionId> {
        None
    }
}
pub(in crate::machine) struct LeafExecution(
    pub(in crate::machine) std::collections::BTreeMap<room::Point, LeafFacts>,
);
impl room::Execution for LeafExecution {
    fn event(&mut self, at: &room::Point) -> Option<Box<dyn room::Facts + '_>> {
        struct Borrowed<'a>(&'a LeafFacts);
        impl room::Facts for Borrowed<'_> {
            fn dependencies(&self) -> Option<&[ArenaHandle]> {
                self.0.dependencies()
            }
            fn measure(&self, measure: &room::Measure) -> Option<i128> {
                self.0.measure(measure)
            }
            fn call(
                &self,
                at: &lash_kernel_value::sites::Site,
                callee: room::CallSource,
            ) -> Option<&room::CheckedRoom> {
                self.0.call(at, callee)
            }
            fn raised(&self) -> bool {
                false
            }
            fn resolved(
                &self,
                _: &lash_kernel_value::sites::Site,
            ) -> Option<lash_kernel_value::FunctionId> {
                None
            }
        }
        Some(Box::new(Borrowed(self.0.get(at)?)))
    }
    fn choose(&mut self, _: &lash_kernel_value::sites::Site) -> Option<bool> {
        None
    }
}

pub(in crate::machine) fn native_fault(error: doc::NativeError) -> HeapFault {
    match error {
        doc::NativeError::Host(error) => error,
        error => malformed(&error.to_string()),
    }
}
pub(in crate::machine) struct Prefix {
    pub(in crate::machine) interval: Arc<room::CompiledInterval>,
    pub(in crate::machine) facts: LeafExecution,
    pub(in crate::machine) start: Position,
    pub(in crate::machine) next: Position,
    pub(in crate::machine) allowance: Allowance,
}

/// An external storage owner binds actual facts while it still owns
/// registration, then finishes against that same transaction.
pub(in crate::machine) struct ExternalEntry<'a> {
    pub(in crate::machine) event: &'a room::publication::ExactEvent,
    pub(in crate::machine) frontier: &'a Frontier,
    pub(in crate::machine) ledger: &'a lash_kernel_state::kernel_internals::logical::Ledger,
    pub(in crate::machine) heap: &'a PagedHeap,
    pub(in crate::machine) prefix: Prefix,
    pub(in crate::machine) resources: crate::admission::ResourceFacts,
}
pub(in crate::machine) trait BoundaryPreparation: Sized {
    type Prepared;
    fn prepare<'a>(
        self,
        arena: &'a mut Arena,
        entry: ExternalEntry<'_>,
    ) -> Result<Option<(room::publication::PreparedEvent<'a>, Self::Prepared)>, HeapFault>;
}

/// Source publication admits the exact source transition.
/// Exact source is the run's verdict point.
pub(in crate::machine) enum Admission<'a> {
    Exact(MemoryLimit<'a>),
}
/// A metered run's memory bound and the limiter that may grow it.
pub(in crate::machine) struct MemoryLimit<'a> {
    limit: Option<&'a mut u64>,
    limiters: &'a mut Limiters,
}
impl<'a> MemoryLimit<'a> {
    pub(in crate::machine) fn of(
        meter: crate::MemoryMeter,
        limit: &'a mut u64,
        limiters: &'a mut Limiters,
    ) -> Self {
        Self {
            limit: (meter != crate::MemoryMeter::Off).then_some(limit),
            limiters,
        }
    }
    /// The live peak past the limit asks the memory limiter, which grows
    /// the limit or ends the run. An unmetered run has no bound.
    fn admit(&mut self, peak: u64) -> Result<u64, Halt> {
        self.limiters.observe_peak(peak);
        let Some(limit) = self.limit.as_deref_mut() else {
            return Ok(u64::MAX);
        };
        if peak > *limit {
            self.limiters.memory_reached(peak, limit)?;
        }
        Ok(*limit)
    }
    fn exceeded(&self, peak: u64) -> bool {
        self.limit.as_deref().is_some_and(|limit| peak > *limit)
    }
}
pub(in crate::machine) struct Context<'a> {
    pub(in crate::machine) admission: Admission<'a>,
    pub(in crate::machine) source: &'a Arc<room::Source>,
    pub(in crate::machine) link: lash_kernel_value::content_ids::LinkId,
    pub(in crate::machine) kernel: doc::KernelVersion,
    pub(in crate::machine) logical: &'a std::rc::Rc<std::cell::RefCell<Ledger>>,
    pub(in crate::machine) heap: &'a PagedHeap,
    pub(in crate::machine) arena: &'a mut Arena,
    pub(in crate::machine) session: &'a mut Session,
    pub(in crate::machine) resources: SourceResources<'a>,
    pub(in crate::machine) logical_memory: &'a Cell<u64>,
}
