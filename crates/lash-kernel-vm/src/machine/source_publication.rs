//! The source frontier, resource admission and semantic journal share one commit.
mod binding;
pub(in crate::machine) mod construction;
mod leaf;
use super::session::Session;
use crate::admission::{Allowance, SourceResources};
use crate::compile::room::{
    self,
    publication::{Frontier, Position, PublicationError},
};
use lash_kernel_doc::{self as doc, arena::ArenaHandle, heap::HeapFault};
use lash_kernel_state::{
    arena::{Arena, Registration},
    kernel_internals::{
        PagedHeap,
        logical::{Invocation, Lifecycle, MemberEdit, Program},
    },
};
use std::{cell::Cell, sync::Arc};
fn malformed(message: &str) -> HeapFault {
    HeapFault::Malformed(message.into())
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

pub(in crate::machine) struct PreparedStorage<'a, S> {
    pub(in crate::machine) registration: Registration<'a>,
    pub(in crate::machine) storage: S,
    pub(in crate::machine) actual: Program,
}

pub(in crate::machine) trait StoragePreparation {
    type Prepared;
    const MATERIALIZES: bool = false;
    fn prepare<'a>(
        self,
        arena: &'a mut Arena,
        actual: Program,
    ) -> Result<Option<PreparedStorage<'a, Self::Prepared>>, HeapFault>;
    fn allowance(_: &Self::Prepared, _: &mut Allowance) {}
    fn facts(_: &Self::Prepared, _: &mut LeafExecution) -> Result<(), HeapFault> {
        Ok(())
    }
}
pub(in crate::machine) struct Reserve<F>(pub(in crate::machine) F);
impl<F, S> StoragePreparation for Reserve<F>
where
    F: FnOnce(&mut Arena) -> Result<Option<S>, HeapFault>,
{
    type Prepared = S;
    fn prepare<'a>(
        self,
        arena: &'a mut Arena,
        actual: Program,
    ) -> Result<Option<PreparedStorage<'a, S>>, HeapFault> {
        let Some(storage) = (self.0)(arena)? else {
            return Ok(None);
        };
        let registration = arena.registration().map_err(native_fault)?;
        Ok(Some(PreparedStorage {
            registration,
            storage,
            actual,
        }))
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

pub(in crate::machine) enum Admission {
    Envelope,
    Exact,
}
pub(in crate::machine) struct Context<'a> {
    pub(in crate::machine) admission: Admission,
    pub(in crate::machine) source: &'a Arc<room::Source>,
    pub(in crate::machine) link: lash_kernel_value::content_ids::LinkId,
    pub(in crate::machine) kernel: doc::KernelVersion,
    pub(in crate::machine) logical: &'a Lifecycle,
    pub(in crate::machine) heap: &'a PagedHeap,
    pub(in crate::machine) arena: &'a mut Arena,
    pub(in crate::machine) session: &'a mut Session,
    pub(in crate::machine) resources: SourceResources<'a>,
    pub(in crate::machine) memory_bound: u64,
    pub(in crate::machine) logical_memory: &'a Cell<u64>,
}
impl Context<'_> {
    pub(in crate::machine) fn publish<S: StoragePreparation>(
        self,
        invocation: &Invocation,
        frontier: &mut Option<Frontier>,
        mut prefix: Prefix,
        make: impl Fn() -> Program,
        prepare: S,
        semantic: impl FnOnce(&mut Arena, &mut Session, Vec<MemberEdit>, S::Prepared),
    ) -> Result<bool, HeapFault> {
        let store = self.logical.ledger();
        if let Some(frontier) = frontier.as_mut()
            && frontier.position() != &prefix.start
        {
            frontier.handoff(&mut store.borrow_mut(), prefix.start.clone())?;
        }
        let ledger = store.borrow();
        if frontier.is_none() {
            *frontier = Some(
                self.source
                    .frontier(
                        &ledger,
                        self.heap,
                        invocation,
                        self.link,
                        self.kernel,
                        prefix.start,
                    )
                    .map_err(|_| malformed("source frontier failed authentication"))?,
            );
        }
        let frontier = frontier
            .as_mut()
            .ok_or_else(|| malformed("source frontier disappeared"))?;
        // Detached bodies and bookkeeping are complete before acceptance.
        // Their registration returns every reserved slot on any later miss.
        let Some(PreparedStorage {
            mut registration,
            storage,
            actual,
        }) = prepare.prepare(self.arena, make())?
        else {
            return Ok(false);
        };
        S::allowance(&storage, &mut prefix.allowance);
        S::facts(&storage, &mut prefix.facts)?;
        let resources = self.resources;
        let Some(resource_facts) = resources.facts(&prefix.allowance) else {
            return Ok(false);
        };
        let Ok(evaluated) = frontier.evaluate_interval(
            prefix.interval.clone(),
            &mut prefix.facts,
            &ledger,
            registration.arena(),
            self.heap,
            actual.clone(),
            prefix.next.clone(),
            prefix.allowance.clone(),
            resource_facts,
        ) else {
            return Ok(false);
        };
        let prepared = match self.admission {
            Admission::Envelope => {
                let Some(accepted) = evaluated.admit_interval(
                    frontier,
                    &ledger,
                    registration.arena(),
                    resources,
                    &prefix.allowance,
                    self.memory_bound,
                )?
                else {
                    return Ok(false);
                };
                accepted.prepare_publication(
                    frontier,
                    &ledger,
                    registration,
                    self.heap,
                    actual,
                    prefix.interval.exit(),
                    &prefix.next,
                )
            }
            Admission::Exact => match evaluated.prepare_exact(
                frontier,
                &ledger,
                registration,
                self.heap,
                actual,
                prefix.interval.exit(),
                &prefix.next,
            ) {
                Ok(exact) => exact
                    .admit_exact(
                        frontier,
                        &ledger,
                        resources,
                        &prefix.allowance,
                        self.memory_bound,
                    )?
                    .ok_or(PublicationError::Miss("exact source admission declined")),
                Err(error) => Err(error),
            },
        };
        let prepared = match prepared {
            Ok(prepared) => prepared,
            Err(PublicationError::Miss(_)) => return Ok(false),
            Err(PublicationError::Heap(error)) => return Err(error),
        };
        drop(ledger);
        let committed = if S::MATERIALIZES {
            prepared.publish_materialized(
                frontier,
                &mut store.borrow_mut(),
                |arena, edits, _entities| semantic(arena, self.session, edits, storage),
            )
        } else {
            prepared.publish(frontier, &mut store.borrow_mut(), |arena, edits| {
                semantic(arena, self.session, edits, storage)
            })
        }
        .map_err(|error| match error {
            PublicationError::Heap(error) => error,
            PublicationError::Miss(reason) => malformed(reason),
        })?;
        debug_assert!(committed.materializations.entities().is_empty());
        self.logical_memory.set(committed.balance.total()?);
        Ok(true)
    }
}
