//! Loop regions of generated helper plans (FIG-6780): a `for` of a
//! helper's body, whose plan formed one ([`crate::compile::plan_regions`]),
//! runs whole iterations from the plan's step of its cursor.
//!
//! The region performs the forms the plan's generated code performs, in
//! its order, on its registers and variables: each statement opens the
//! plan's statement, each operand goes to the register the plan puts it
//! in, each value the plan holds is pinned where it pins it, and each
//! variable is bound, written and emptied as the plan does. What it saves
//! is the generated code's dispatch per form: a call its prepared entry
//! answers is answered directly, and a hole test builds no tuple. So where
//! it stops, the plan stands exactly where its own code would:
//!
//! - when the cursor is exhausted, or at a `break`, the step gives that
//!   the loop is done, and the plan's code leaves it;
//! - at a callback site whose callee does not return at once (no island,
//!   or an island that exits), the plan stands there and yields, its
//!   frames materialised by the segment path (`super::segments`); resumed,
//!   its code runs the rest of that iteration, and its next step enters
//!   the region again;
//! - a raise or a halt leaves the plan as the form that raised does.
//!
//! The callback's island is entered inline, from the region's own site.

use lash_kernel_doc::Value;
use lash_kernel_doc::plan::{Cursor as PlanCursor, PlanContext, PlanExit, Test};

use super::plans::PlanRun;
use crate::compile::plan_regions::{Block, Node, NodeKind, Step};

/// The headroom past which a hole test need not hold the tuple it
/// compares with: an empty tuple costs a value's 16 bytes, so the plan's
/// hold of it is granted, and released by its statement's end, wherever
/// at least this much is left.
const UNIT_HOLD: u64 = 1 << 10;

/// How a pass through a block ended.
enum Went {
    /// It ran to its end.
    Through,
    /// A `break` left the loop.
    Broke,
}

impl<const MEMORY: bool, const SHARED: bool> PlanRun<'_, MEMORY, SHARED> {
    /// Runs the loop on `cursor` as region `region` of the plan, entered at
    /// the plan's step of it, the test's units charged: `false` when the
    /// loop is done, as the plan's step and its `break` leave it.
    pub(super) fn walk_region(
        &mut self,
        cursor: PlanCursor,
        region: u32,
    ) -> Result<bool, PlanExit> {
        let prepared = self.prepared;
        let region = &prepared.regions[region as usize];
        let mut tested = true;
        loop {
            // The next test, in its own statement.
            if !tested && region.metered != 0 {
                self.meter(region.metered)?;
            }
            tested = false;
            let Some(element) = self.step(cursor)? else {
                return Ok(false);
            };
            self.bind_value(region.binding, element)?;
            let went = self.pass(&region.body)?;
            if let Some(tape) = self
                .machine
                .observer
                .as_deref()
                .and_then(|observer| observer.charges.as_ref())
            {
                tape.passed();
            }
            match went {
                Went::Broke => return Ok(false),
                Went::Through => self.open_statement()?,
            }
        }
    }

    /// One pass through `block`.
    fn pass(&mut self, block: &Block) -> Result<Went, PlanExit> {
        for step in &block.steps {
            self.open_statement()?;
            match step {
                Step::If {
                    cond,
                    then,
                    otherwise,
                } => {
                    self.eval(cond)?;
                    let arm = if self.test(cond.reg, Test::If)? {
                        then
                    } else {
                        otherwise
                    };
                    if let Went::Broke = self.pass(arm)? {
                        return Ok(Went::Broke);
                    }
                }
                Step::Break(left) => {
                    for slot in left {
                        self.unbind(*slot);
                    }
                    return Ok(Went::Broke);
                }
                Step::Assign { slot, value } => {
                    self.eval(value)?;
                    self.assign(*slot, value.reg)?;
                }
                Step::Bind { slot, value } => {
                    self.eval(value)?;
                    self.bind(*slot, value.reg)?;
                }
                Step::Store { order, target, src } => {
                    for node in order {
                        self.eval(node)?;
                    }
                    self.set_index(*target, *src)?;
                }
                Step::Discard(value) => self.eval(value)?,
            }
        }
        for slot in block.declares.iter().rev() {
            self.unbind(*slot);
        }
        Ok(Went::Through)
    }

    /// Computes `node` into its register.
    fn eval(&mut self, node: &Node) -> Result<(), PlanExit> {
        let at = self.base + node.reg as usize;
        match &node.kind {
            NodeKind::Local(slot) => {
                let value = self.machine.regs[self.slots + *slot as usize];
                self.machine.put(at, value);
            }
            NodeKind::Const(value) => self.machine.put(at, *value),
            NodeKind::Pin(value) => {
                self.eval(value)?;
                self.pin(node.reg)?;
            }
            NodeKind::Unit => self.tuple(node.reg, 0)?,
            NodeKind::IsUnit { lib, args } => {
                let [value, unit] = &args[..] else {
                    return Err(self.lost_form());
                };
                self.eval(value)?;
                // `same` of a value and the empty tuple is whether the value
                // is a tuple of no members; a tuple not resident is
                // compared as the plan compares it, which warms it.
                let empty = match self.machine.regs[self.base + value.reg as usize] {
                    _ if MEMORY && self.headroom() < UNIT_HOLD => None,
                    Value::Tuple(tuple) => self
                        .machine
                        .arena
                        .borrow()
                        .len(tuple)
                        .ok()
                        .map(|len| len == 0),
                    _ => Some(false),
                };
                match empty {
                    Some(empty) => self.machine.put(at, Value::Bool(empty)),
                    None => {
                        self.eval(unit)?;
                        self.call_at(*lib, at, 2)?;
                    }
                }
            }
            NodeKind::Call { lib, args } => {
                for arg in args {
                    self.eval(arg)?;
                }
                self.call_at(*lib, at, args.len())?;
            }
            NodeKind::Apply { site, callee, args } => {
                let callee = self.machine.regs[self.slots + *callee as usize];
                self.machine.put(at, callee);
                for arg in args {
                    self.eval(arg)?;
                }
                self.apply_at(*site, at, args.len())?;
            }
            NodeKind::Invoke { site, lib, args } => {
                for arg in args {
                    self.eval(arg)?;
                }
                self.offer(*site, *lib, at, args.len())?;
            }
        }
        Ok(())
    }

    /// What the memory bound leaves past what the census holds.
    fn headroom(&self) -> u64 {
        self.machine
            .bounds
            .memory
            .saturating_sub(self.machine.heap.certificate().upper.0)
    }

    #[cold]
    fn lost_form(&mut self) -> PlanExit {
        self.halt = Some(Box::new(super::fault(
            "a plan region's hole test has no operands",
        )));
        PlanExit::Halt
    }
}
