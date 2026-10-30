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
    operand_bytes: u64,
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
        let mut root_and_control_bytes = self
            .tasks
            .logical_control_bytes(&arena)?
            .checked_add(self.session.logical_written_bytes(&arena)?)
            .ok_or_else(|| HeapFault::Malformed("root holding cost overflow".into()))?;
        root_and_control_bytes = root_and_control_bytes
            .checked_add(self.operand_bytes)
            .ok_or_else(|| HeapFault::Malformed("operand holding cost overflow".into()))?;
        Ok(Walked {
            session,
            root_and_control_bytes,
        })
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
    fn anonymous_entity_bytes(&self) -> u64 {
        self.reach.as_ref().map_or(0, Reach::anonymous_local_bytes)
    }
    fn entity_local_bytes(&mut self, entity: EntityId) -> Result<u64, HeapFault> {
        self.arena.borrow().logical_entity_bytes(entity)
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
    fn operand_roots(&self) -> Vec<Value> {
        self.regs.source_projection()
    }

    /// The census's sweep over what the run holds in memory, with nothing
    /// published (FIG-6748): it removes the entities nothing reaches and
    /// advances the sweep generation. It retires the results of the ended
    /// tasks nothing reaches only when `retires` (FIG-6791).
    pub(super) fn refine_resident(&mut self, retires: bool) -> Result<(), Halt> {
        self.logical_flush()?;
        // A changed object's members are read from its body: one partly on
        // its pages is decoded first.
        let cold = self.arena.borrow().changed_cold();
        for handle in cold {
            self.warm_whole(handle)?;
        }
        self.heap.set_saved_functions(&self.functions);
        let access = self.heap.pages().clone();
        let registers = self.operand_roots();
        let operand_bytes = self.regs.source_bytes(&self.arena.borrow())?;
        let mut holdings = Holdings {
            arena: &self.arena,
            access: &access,
            tasks: &mut self.tasks,
            session: &self.session,
            joins: &self.joins,
            registers,
            operand_bytes,
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

#[cfg(test)]
mod laws {
    use super::*;
    use crate::compile::{Form, Stmt, Stream};
    use crate::laws::embedder::Embedder;
    use lash_kernel_exec::{ConstIx, LocalRef, Op, OpRange, Reg};
    use lash_kernel_value::sites::{Site, Unit};
    use std::sync::Arc;

    fn normalized_operands(machine: &mut KernelMachine) -> (u64, u64) {
        let access = machine.heap.pages().clone();
        let registers = machine.operand_roots();
        let operand_bytes = machine.regs.source_bytes(&machine.arena.borrow()).unwrap();
        let mut roots = Holdings {
            arena: &machine.arena,
            access: &access,
            tasks: &mut machine.tasks,
            session: &machine.session,
            joins: &machine.joins,
            registers,
            operand_bytes,
            pins: &machine.pins,
            roots: Vec::new(),
            reach: None,
        };
        let walked = roots.walk().unwrap();
        let mut entities = std::collections::BTreeSet::new();
        for value in &roots.roots {
            if let PageValue::Ref(Identity::Object(id)) = value {
                entities.insert(*id);
            }
        }
        let entity_bytes = entities
            .into_iter()
            .map(|id| roots.entity_local_bytes(id).unwrap())
            .sum::<u64>();
        let result = (
            walked.root_and_control_bytes,
            roots.anonymous_entity_bytes() + entity_bytes,
        );
        roots.finish();
        result
    }

    /// K-BND-003: normalization prices source operand occurrences, including
    /// a value whose register mirror was cleared, and never storage ghosts.
    #[test]
    fn source_normalization_ignores_unwritten_and_consumed_register_mirrors() {
        let mut embedder = Embedder::new(
            "import 3 operation host.pair(a: Any, b: Any) -> Any \
             main { let out = perform 3(true, \"abcd\") as Any return null }",
        );
        let machine = &mut embedder.machine;
        let baseline = normalized_operands(machine);
        machine
            .source_entry(&[Value::Int(7), Value::Int(11)])
            .unwrap();
        machine.regs.clear_range(0..2);
        assert_eq!(
            normalized_operands(machine),
            (baseline.0 + 34, baseline.1),
            "exported arguments survive their cleared ABI mirrors"
        );
        machine.source_entry(&[]).unwrap();
        assert_eq!(
            normalized_operands(machine),
            baseline,
            "entry transfer releases its old bank"
        );
        let site = Arc::new(Site::new(Unit::Main, vec![0, 0]));
        let stream = Stream {
            ops: vec![
                Op::Const {
                    dst: Reg(19),
                    c: ConstIx(0),
                },
                Op::Bind {
                    local: LocalRef { slot: 0, at: 0 },
                    src: Reg(19),
                },
            ],
            source_sites: vec![Some(Arc::clone(&site)), Some(site)],
            ..Default::default()
        };
        let stmt = Stmt {
            site: lash_kernel_value::sites::Site::new(lash_kernel_value::sites::Unit::Main, vec![]),
            ordinal: None,
            ops: OpRange { start: 0, len: 2 },
            regs: 64,
            form: Form::Plain,
            source_roles: std::sync::Mutex::new(Default::default()),
        };
        machine.regs.statement(stmt.regs);
        machine
            .source_begin(lash_kernel_doc::TaskId::MAIN, &stream, &stmt, 0)
            .unwrap();
        assert_eq!(normalized_operands(machine), baseline, "unwritten storage");
        let held = machine.arena.borrow_mut().create_list(&[Value::Null]);
        machine.regs.put(19, held);
        machine.source_complete(&stream, 0).unwrap();
        // Source ownership survives a cleared physical mirror.
        machine.regs.clear_range(19..20);
        let (bytes, bodies) = normalized_operands(machine);
        assert_eq!(bytes, baseline.0 + 16);
        assert_eq!(bodies, baseline.1 + 48, "one list header and one member");
        // A consumed occurrence must disappear even if its mirror is restored.
        machine.regs.put(19, held);
        machine.source_complete(&stream, 1).unwrap();
        assert_eq!(normalized_operands(machine), baseline, "consumed storage");

        let action_site = Site::new(Unit::Main, vec![0, 0]);
        machine.regs.source_atoms_begin(&action_site).unwrap();
        let exe = Arc::clone(&machine.exe);
        let atoms = [
            crate::compile::Atom::Literal(crate::compile::Constant::Resident(Value::Bool(true))),
            crate::compile::Atom::Literal(crate::compile::Constant::Deferred(
                lash_kernel_doc::Literal::Text("abcd".into()),
            )),
        ];
        let args = machine
            .perform_atoms(lash_kernel_doc::TaskId::MAIN, &exe, &atoms)
            .unwrap();
        assert_eq!(args.len(), 2);
        assert_eq!(
            normalized_operands(machine),
            (baseline.0 + 36, baseline.1),
            "perform arguments need source owners even outside the register window"
        );
        let action = Stream {
            ops: vec![Op::Act],
            source_sites: vec![Some(Arc::new(action_site))],
            ..Default::default()
        };
        machine.regs.put(0, Value::Null);
        machine.source_complete(&action, 0).unwrap();
        assert_eq!(
            normalized_operands(machine),
            (baseline.0 + 16, baseline.1),
            "only the completed action result remains"
        );
        machine.regs.kill_statement();
        assert_eq!(normalized_operands(machine), baseline);
    }
}
