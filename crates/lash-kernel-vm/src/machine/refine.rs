//! A durable run's sweep (seam I, FIG-6748): it traces over what the run
//! holds where it is. Its frames, session bindings, registers and pins are
//! walked in memory and traced over the heap's pages; nothing is
//! projected, published or given an id, so the work of a sweep does not
//! grow with what the run changed since its last projection point.

use lash_kernel_doc::TaskId;
use lash_kernel_doc::Value;
use lash_kernel_doc::heap::{EntityId, HeapFault, Identity, PageValue};
use lash_kernel_state::arena::{Reach, Reached};
use lash_kernel_state::kernel_internals::resident::{ResidentRoots, Walked};
use lash_kernel_state::kernel_internals::tree::{Access, Tree};

use super::project::SharedArena;
use super::{Halt, KernelMachine, ListJoin, Pins, session, tasks};

/// What the run holds, as a resident sweep reads it.
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
    reach: Option<Reach>,
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
    fn walk(&mut self) -> Result<Walked, HeapFault> {
        let mut arena = self.arena.borrow_mut();
        let reach = self.reach.get_or_insert_default();
        let roots = &mut self.roots;
        let mut out = |reached: Reached<'_>| {
            keep(roots, reached);
            Ok(())
        };
        let session = self.session.reach(&mut arena, reach, &mut out)?;
        self.tasks.reach(&mut arena, reach, &mut out)?;
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
        // What a statement borrows is its caller's: traced for what it
        // reaches.
        for value in self
            .registers
            .iter()
            .chain(&self.pins.values)
            .chain(&self.pins.borrowed)
        {
            arena.reach(reach, value, &mut out)?;
        }
        Ok(Walked { session })
    }
    fn page_roots(
        &mut self,
        visit: &mut dyn FnMut(&PageValue) -> Result<(), HeapFault>,
    ) -> Result<(), HeapFault> {
        self.roots.iter().try_for_each(visit)
    }
    fn contains(&mut self, entity: EntityId) -> bool {
        self.arena.borrow().holds_resident(entity)
    }
    fn entity_edges(
        &mut self,
        entity: EntityId,
        out: &mut dyn FnMut(Identity) -> Result<(), HeapFault>,
        pages: &mut Vec<PageValue>,
    ) -> Result<(), HeapFault> {
        let reach = self.reach.get_or_insert_default();
        self.arena
            .borrow_mut()
            .reach_entity(reach, entity, &mut |reached| match reached {
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
    fn changed_entities(&mut self) -> Vec<EntityId> {
        self.arena.borrow().changed_entities()
    }
    fn swept(&mut self, entities: &[EntityId]) {
        self.arena.borrow_mut().forget_swept(entities);
    }
    fn retire(&mut self, tasks: &[TaskId]) -> Result<(), HeapFault> {
        self.tasks.retire(tasks)
    }
    fn finish(&mut self) {
        if let Some(reach) = self.reach.take() {
            self.arena.borrow_mut().end_reach(reach);
        }
    }
}

impl KernelMachine {
    /// The census's sweep over what the run holds in memory, with nothing
    /// published (FIG-6748): it removes the entities nothing reaches and
    /// advances the sweep generation. It retires the results of the ended
    /// tasks nothing reaches only when `retires` (FIG-6791).
    pub(super) fn refine_resident(&mut self, retires: bool) -> Result<(), Halt> {
        // A changed object's members are read from its body: one partly on
        // its pages is decoded first.
        let cold = self.arena.borrow().changed_cold();
        for handle in cold {
            self.warm_whole(handle)?;
        }
        self.heap.set_saved_functions(&self.functions);
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
            reach: None,
        };
        self.heap.refine_resident(&mut holdings, retires)?;
        // The rows the sweep retired results in are the machine's own:
        // nothing of the heap's replaces them.
        self.census_refinements = self.heap.certificate().refinements;
        Ok(())
    }
}
