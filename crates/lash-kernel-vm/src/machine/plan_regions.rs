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
//!
//! Its expressions are fused (FIG-6847): a variable or a constant operand
//! is read in place rather than put in its register, and a call whose
//! prepared entry answers is handed the values directly. What a form
//! computes goes to its register as the plan's code puts it, and a
//! variable or constant operand goes to its register where the plan's
//! general call reads it or where the plan may stand, so that what the
//! registers and pins hold is what the generated code's hold wherever a
//! sweep, a raise or a stand can see them: a variable the plan holds is
//! rooted by its own register, and a constant is resident. A statement's
//! end empties only the registers it uses.

use lash_kernel_doc::plan::{Cursor as PlanCursor, PlanContext, PlanExit, Test};
use lash_kernel_doc::{NativeError, Retry, Value};

use super::plans::PlanRun;
use crate::compile::plan_regions::{Block, IN_PLACE, Node, NodeKind, StepKind};
use crate::compile::{Entry, LibId, position};

/// The capacity headroom past which a hole test need not build the tuple
/// it compares with: an empty tuple holds one value's 16 bytes, so the
/// plan's build of it passes the bound wherever at least this much is
/// left (FIG-6791).
const UNIT_HOLD: u64 = 1 << 10;

/// How a pass through a block ended.
enum Went {
    /// It ran to its end.
    Through,
    /// A `break` left the loop.
    Broke,
}

impl<const SHARED: bool> PlanRun<'_, SHARED> {
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
        // Its steady callbacks are resolved afresh for each walk.
        self.steady = None;
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
                // The test's statement puts nothing in the registers.
                Went::Through => self.open_statement_of(0)?,
            }
        }
    }

    /// One pass through `block`.
    fn pass(&mut self, block: &Block) -> Result<Went, PlanExit> {
        for step in &block.steps {
            self.open_statement_of(step.span)?;
            match &step.kind {
                StepKind::If {
                    cond,
                    then,
                    otherwise,
                } => {
                    let arm = match self.value(cond)? {
                        Value::Bool(true) => then,
                        Value::Bool(false) => otherwise,
                        // The plan's test of what is no bool raises.
                        other => {
                            self.widen();
                            self.machine.put(self.base + cond.reg as usize, other);
                            self.test(cond.reg, Test::If)?;
                            return Err(self.lost_form("a plan region's test passed no bool"));
                        }
                    };
                    if let Went::Broke = self.pass(arm)? {
                        return Ok(Went::Broke);
                    }
                }
                StepKind::Break(left) => {
                    for slot in left {
                        self.unbind(*slot);
                    }
                    return Ok(Went::Broke);
                }
                StepKind::Assign { slot, value } => {
                    let value = self.value(value)?;
                    self.machine.put(self.slots + *slot as usize, value);
                }
                StepKind::Bind { slot, value } => {
                    let value = self.value(value)?;
                    self.bind_value(*slot, value)?;
                }
                StepKind::Store {
                    order,
                    target,
                    src,
                    placed,
                } => self.store(order, (*target, *src), *placed)?,
                StepKind::Discard(value) => {
                    self.value(value)?;
                }
            }
        }
        for slot in block.declares.iter().rev() {
            self.unbind(*slot);
        }
        Ok(Went::Through)
    }

    /// `set target[index] = value`, its three operands `order` in the
    /// plan's order, the target's register `target` and the value's `src`.
    /// A list is written from the values in place, as the plan's member
    /// write writes it; any other write reads its operands from their
    /// registers, where the plan's code leaves them, as does one whose
    /// operands go to their registers as they are evaluated (`placed`).
    fn store(
        &mut self,
        order: &[Node; 3],
        (target, src): (u32, u32),
        placed: bool,
    ) -> Result<(), PlanExit> {
        let mut values = [Value::Absent; 3];
        for (value, node) in values.iter_mut().zip(order) {
            *value = self.value(node)?;
            if placed {
                self.machine.put(self.base + node.reg as usize, *value);
            }
        }
        let operand = |reg: u32| {
            order
                .iter()
                .position(|node| node.reg == reg)
                .map_or(Value::Absent, |at| values[at])
        };
        if !placed && self.write_list(operand(target), operand(target + 1), operand(src))? {
            self.machine.take(self.base + src as usize);
            return Ok(());
        }
        self.widen();
        for (node, value) in order.iter().zip(values) {
            self.machine.put(self.base + node.reg as usize, value);
        }
        self.set_index(target, src)
    }

    /// Writes `value` at `index` of `target` where `target` is a list and
    /// `index` a position up to its length that the arena holds resident,
    /// as the plan's member write writes it: `false`, having written
    /// nothing, otherwise, for that write to raise or decode first.
    fn write_list(&mut self, target: Value, index: Value, value: Value) -> Result<bool, PlanExit> {
        let (Value::List(list), Some(at)) = (target, position(&index)) else {
            return Ok(false);
        };
        let written = {
            let mut arena = self.machine.arena.borrow_mut();
            match arena.len(list) {
                Ok(length) if at <= length => arena.list_set(list, at, value).map(|()| true),
                Ok(_) => Ok(false),
                Err(error) => Err(error),
            }
        };
        match written {
            Ok(written) => {
                if written {
                    self.machine.grown();
                }
                Ok(written)
            }
            Err(NativeError::Retry(Retry::Cold { .. })) => Ok(false),
            Err(error) => {
                let failed = Err(super::Interrupt::from(error));
                self.check(failed)
            }
        }
    }

    /// The value of `node`: what it computes is in its register, as the
    /// plan's code puts it; a variable or a constant is read in place.
    #[inline]
    fn value(&mut self, node: &Node) -> Result<Value, PlanExit> {
        match &node.kind {
            NodeKind::Local(slot) => Ok(self.machine.regs[self.slots + *slot as usize]),
            NodeKind::Const(value) => Ok(*value),
            _ => self.computed(node),
        }
    }

    /// The value `node` computes: into its register, unless it answers as
    /// fused.
    fn computed(&mut self, node: &Node) -> Result<Value, PlanExit> {
        match &node.kind {
            NodeKind::Local(_) | NodeKind::Const(_) => self.value(node),
            NodeKind::Pin(value) => {
                let pinned = self.value(value)?;
                let held = self.machine.pin_value(&pinned);
                self.held(held)?;
                Ok(pinned)
            }
            NodeKind::IsUnit { lib, args } => self.is_unit(node.reg, *lib, args),
            NodeKind::Length { lib, list } => self.length(node.reg, *lib, list),
            NodeKind::Call {
                lib,
                args,
                direct: Some(direct),
            } => self.fused_call(*lib, node.reg, args, direct),
            _ => self.placed(node),
        }
    }

    /// `list.len` by `lib` of what `list` gives, its value to register
    /// `reg` where the plan's general call answers it: a resident list's
    /// length is read from the arena, as that call's native reads it, at
    /// that call's verdict points, its reservation of nothing built and its
    /// check once its value is held.
    #[inline(always)]
    fn length(&mut self, reg: u32, lib: LibId, list: &Node) -> Result<Value, PlanExit> {
        let value = self.value(list)?;
        let length = match value {
            Value::List(handle) if !self.machine.charging && self.machine.observer.is_none() => {
                self.machine.arena.borrow().len(handle).ok()
            }
            _ => None,
        };
        if let Some(length) = length {
            let verdicts = self
                .machine
                .reserve(0)
                .and_then(|()| self.machine.capacity_check());
            self.held(verdicts)?;
            return Ok(Value::Int(length as i64));
        }
        self.widen();
        self.machine.put(self.base + list.reg as usize, value);
        let at = self.base + reg as usize;
        self.call_at(lib, at, 1)?;
        Ok(self.machine.regs[at])
    }

    /// The hole test `same(value, ())` by `lib` on `args`, its value to
    /// register `reg` where it builds the tuple.
    #[inline(always)]
    fn is_unit(&mut self, reg: u32, lib: LibId, args: &[Node]) -> Result<Value, PlanExit> {
        let [value, unit] = args else {
            return Err(self.lost_form("a plan region's hole test has no operands"));
        };
        let tested = self.value(value)?;
        // `same` of a value and the empty tuple is whether the value is a
        // tuple of no members; a tuple not resident is compared as the plan
        // compares it, which warms it.
        let empty = match tested {
            _ if self.machine.capacity_headroom() < UNIT_HOLD => None,
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
            Some(empty) => Ok(Value::Bool(empty)),
            None => self.built_unit(reg, lib, (value, unit), tested),
        }
    }

    /// The hole test of `value`, whose value is `tested`, against the
    /// empty tuple `unit` builds, as the plan's code tests it.
    #[cold]
    #[inline(never)]
    fn built_unit(
        &mut self,
        reg: u32,
        lib: LibId,
        (value, unit): (&Node, &Node),
        tested: Value,
    ) -> Result<Value, PlanExit> {
        self.widen();
        self.machine.put(self.base + value.reg as usize, tested);
        self.placed(unit)?;
        let at = self.base + reg as usize;
        self.call_at(lib, at, 2)?;
        Ok(self.machine.regs[at])
    }

    /// The value `node` computes into its register as the plan's code
    /// does: its operands in their registers, and the form on them.
    #[inline(never)]
    fn placed(&mut self, node: &Node) -> Result<Value, PlanExit> {
        let at = self.base + node.reg as usize;
        match &node.kind {
            NodeKind::Local(_)
            | NodeKind::Const(_)
            | NodeKind::Pin(_)
            | NodeKind::IsUnit { .. }
            | NodeKind::Length { .. } => return self.computed(node),
            NodeKind::Unit => self.tuple(node.reg, 0)?,
            NodeKind::Call { lib, args, .. } => {
                self.operands(args)?;
                self.call_at(*lib, at, args.len())?;
            }
            NodeKind::Apply {
                site,
                callee,
                args,
                steady,
            } => {
                let callee = self.machine.regs[self.slots + *callee as usize];
                self.machine.put(at, callee);
                self.operands(args)?;
                if *steady {
                    self.apply_steady(*site, at, args.len())?;
                } else {
                    self.apply_at(*site, at, args.len())?;
                }
            }
            NodeKind::Invoke { site, lib, args } => {
                self.operands(args)?;
                self.offer(*site, *lib, at, args.len())?;
            }
        }
        Ok(self.machine.regs[at])
    }

    /// Puts the values of `args` in their registers, in order, as the
    /// plan's code puts a call's operands.
    fn operands(&mut self, args: &[Node]) -> Result<(), PlanExit> {
        for arg in args {
            let value = self.value(arg)?;
            self.machine.put(self.base + arg.reg as usize, value);
        }
        Ok(())
    }

    /// Calls library function `lib` on `args` as the plan's general call
    /// does, `direct` the prepared entry it takes first: on the values in
    /// place, its answer in no register. Where the entry declines, or the
    /// call is observed, its operands go to their registers and the rest
    /// of that call puts its value in `at`.
    #[inline(always)]
    fn fused_call(
        &mut self,
        lib: LibId,
        reg: u32,
        args: &[Node],
        direct: &Entry,
    ) -> Result<Value, PlanExit> {
        let mut values = [Value::Absent; IN_PLACE];
        for (value, arg) in values.iter_mut().zip(args) {
            *value = self.value(arg)?;
        }
        let values = &values[..args.len()];
        if !self.machine.charging
            && self.machine.observer.is_none()
            && let Some(answer) = direct.answer(values, false)
        {
            let pinned = self.machine.pin_value(&answer.value);
            self.held(pinned)?;
            return Ok(answer.value);
        }
        self.unfused_call(lib, reg, args, values)
    }

    /// [`Self::fused_call`] where its entry declines, or the call is
    /// observed: its operands, whose values are `values`, go to their
    /// registers, and the rest of the plan's general call puts its value in
    /// the call's register `reg`.
    #[cold]
    #[inline(never)]
    fn unfused_call(
        &mut self,
        lib: LibId,
        reg: u32,
        args: &[Node],
        values: &[Value],
    ) -> Result<Value, PlanExit> {
        let at = self.base + reg as usize;
        let unobserved = !self.machine.charging && self.machine.observer.is_none();
        self.widen();
        for (arg, value) in args.iter().zip(values) {
            self.machine.put(self.base + arg.reg as usize, *value);
        }
        if unobserved {
            let called = self.machine.call_declined(self.task, self.exe, lib, at);
            self.check(called)?;
        } else {
            self.call_at(lib, at, args.len())?;
        }
        Ok(self.machine.regs[at])
    }

    #[cold]
    fn lost_form(&mut self, fault: &str) -> PlanExit {
        self.halt = Some(Box::new(super::fault(fault)));
        PlanExit::Halt
    }
}
