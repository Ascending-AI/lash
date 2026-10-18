//! A durable run's collection point (seam I, FIG-6748): under pressure it
//! refines exactly over what it holds where it is. Its frames, session
//! bindings, registers and pins are priced in memory and traced over the
//! heap's pages; nothing is projected, published or given an id, so the
//! work of a refinement does not grow with what the run changed since its
//! last projection point. The verdict is the one a refinement over the
//! published roots gives.

use lash_kernel_doc::TaskId;
use lash_kernel_doc::Value;
use lash_kernel_doc::heap::{EntityId, HeapFault, Identity, PageValue};
use lash_kernel_state::arena::{Pricing, Reached};
use lash_kernel_state::kernel_internals::resident::{Priced, ResidentRoots, Traced};
use lash_kernel_state::kernel_internals::tree::{Access, Tree};

use super::project::SharedArena;
use super::{Halt, KernelMachine, ListJoin, Pins, session, tasks};

/// What the run holds, as a resident refinement reads it.
struct Holdings<'a> {
    arena: &'a SharedArena,
    access: &'a Access<'static>,
    tasks: &'a mut tasks::Tasks,
    session: &'a session::Session,
    joins: &'a Tree<ListJoin>,
    registers: Vec<Value>,
    pins: &'a Pins,
    /// What the walk of the arena gave the census, in page form.
    roots: Vec<PageValue>,
    pricing: Option<Pricing>,
    /// The cost of the objects without an id the pricing reached.
    priced: u64,
}

/// Keeps what the walk of the arena reached that a trace follows: an
/// identity, or a page form naming one.
fn keep(roots: &mut Vec<PageValue>, reached: Reached<'_>) {
    match reached {
        Reached::Identity(identity) => roots.push(PageValue::Ref(identity)),
        Reached::Page(page) => match page {
            PageValue::List(_)
            | PageValue::Map(_)
            | PageValue::Set(_)
            | PageValue::Record(_)
            | PageValue::Closure(_)
            | PageValue::Task(_)
            | PageValue::Ref(_) => roots.push(page.clone()),
            PageValue::Tuple(tuple) if tuple.summary.has_edges => roots.push(page.clone()),
            PageValue::Error(error) if error.has_edges => roots.push(page.clone()),
            _ => {}
        },
    }
}

impl ResidentRoots for Holdings<'_> {
    fn price(&mut self) -> Result<Priced, HeapFault> {
        let mut arena = self.arena.borrow_mut();
        let pricing = self.pricing.get_or_insert_default();
        let roots = &mut self.roots;
        let mut out = |reached: Reached<'_>| {
            keep(roots, reached);
            Ok(())
        };
        let session = self.session.price(&mut arena, pricing, &mut out)?;
        let (tasks, retained) = self.tasks.price(&mut arena, pricing, &mut out)?;
        let mut problem = None;
        self.joins.visit(self.access, &mut |_, join| {
            let visited = join.members.visit(self.access, &mut |_, member| {
                out(Reached::Identity(Identity::Task(*member))).is_ok()
            });
            problem = visited.err();
            problem.is_none()
        })?;
        if let Some(problem) = problem {
            return Err(problem);
        }
        let mut statement = 0_u64;
        for value in self.registers.iter().chain(&self.pins.values) {
            statement = statement.saturating_add(arena.cost_of(value));
            arena.price(pricing, value, &mut out)?;
        }
        // What a statement borrows is its caller's: priced there, and
        // traced for what it reaches.
        for value in &self.pins.borrowed {
            arena.price(pricing, value, &mut out)?;
        }
        self.priced = pricing.unidentified;
        Ok(Priced {
            control: tasks.saturating_add(self.joins.cost()),
            retained,
            statement,
            session,
            unidentified: self.priced,
        })
    }
    fn page_roots(
        &mut self,
        visit: &mut dyn FnMut(&PageValue) -> Result<(), HeapFault>,
    ) -> Result<(), HeapFault> {
        self.roots.iter().try_for_each(visit)
    }
    fn changed_cost(&mut self, entity: EntityId) -> Option<u64> {
        self.arena.borrow().changed_cost(entity)
    }
    fn changed_edges(
        &mut self,
        entity: EntityId,
        out: &mut dyn FnMut(Identity) -> Result<(), HeapFault>,
        pages: &mut Vec<PageValue>,
    ) -> Result<(), HeapFault> {
        let pricing = self.pricing.get_or_insert_default();
        self.arena
            .borrow_mut()
            .price_changed(pricing, entity, &mut |reached| match reached {
                Reached::Identity(identity) => out(identity),
                Reached::Page(page) => {
                    pages.push(page.clone());
                    Ok(())
                }
            })
    }
    fn ended_result(&mut self, task: TaskId) -> Result<Option<PageValue>, HeapFault> {
        self.tasks.ended_result(task)
    }
    fn ended_tasks_after(
        &mut self,
        after: Option<TaskId>,
        limit: usize,
    ) -> Result<Vec<TaskId>, HeapFault> {
        self.tasks.ended_after(after, limit)
    }
    fn swept(&mut self, entities: &[EntityId]) {
        self.arena.borrow_mut().forget_swept(entities);
    }
    fn retire(&mut self, tasks: &[TaskId]) -> Result<(), HeapFault> {
        self.tasks.retire(tasks)
    }
    fn finish(&mut self) -> Traced {
        let Some(pricing) = self.pricing.take() else {
            return Traced::default();
        };
        let unidentified = pricing.unidentified.saturating_sub(self.priced);
        Traced {
            unidentified,
            unreached: self.arena.borrow_mut().end_pricing(pricing),
        }
    }
}

impl KernelMachine {
    /// The collection point of a durable run: under pressure, refines
    /// exactly over what it holds in memory with `bytes` pending, so the
    /// verdict is given here, over current holdings, with nothing
    /// published (FIG-6748).
    pub(super) fn refine_resident(&mut self, bytes: u64) -> Result<(), Halt> {
        // A changed object's members are read from its body: one partly on
        // its pages is decoded first.
        let cold = self.arena.borrow().changed_cold();
        for handle in cold {
            self.warm_whole(handle)?;
        }
        let access = self.heap.pages().clone();
        let mut holdings = Holdings {
            arena: &self.arena,
            access: &access,
            tasks: &mut self.tasks,
            session: &self.session,
            joins: &self.joins,
            registers: self.regs.projection(),
            pins: &self.pins,
            roots: Vec::new(),
            pricing: None,
            priced: 0,
        };
        let refined = self.heap.refine_resident(&mut holdings, bytes);
        refined.map_err(|error| self.heap_halt(error))?;
        self.arena.borrow_mut().rebased();
        // The rows the refinement retired results in are the machine's
        // own: nothing of the heap's replaces them.
        self.census_refinements = self.heap.certificate().refinements;
        Ok(())
    }
}
