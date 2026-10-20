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
//! A region's body is compiled once, as its plan is prepared, into a tree
//! of closures over the running plan (FIG-6947): one per statement and per
//! computed value, each specialised to its form and to whether its
//! operands are variables, constants or computed. Its yield points stay
//! calls of the plan's own: [`PlanRun::offer`], [`PlanRun::apply_at`] and
//! the steady site's [`PlanRun::apply_steady`], whose callback's island is
//! resolved once per walk and entered directly.
//!
//! Its expressions are fused (FIG-6847): a variable or a constant operand
//! is read in place rather than put in its register, a call whose
//! prepared entry answers is handed the values directly (two floats or two
//! word integers through the entry's own typed kernels), `list.len` and
//! `list.get` of a resident list read the arena (FIG-6903), and an invoke
//! whose template answers its one argument unchanged answers it. What a
//! form computes goes to its register as the plan's code puts it, and a
//! variable or constant operand goes to its register where the plan's
//! general call reads it or where the plan may stand, so that what the
//! registers and pins hold is what the generated code's hold wherever a
//! sweep, a raise or a stand can see them: a variable the plan holds is
//! rooted by its own register, and a constant is resident. A statement's
//! end empties only the registers it uses.

mod merge;
mod slots;

use std::any::Any;

use lash_kernel_doc::plan::{Cursor as PlanCursor, PlanContext, PlanExit, Test};
use lash_kernel_doc::scalar::{self, Pair};
use lash_kernel_doc::{NativeError, Retry, TaskId, Value};

use super::plans::PlanRun;
use crate::compile::plan_regions::{
    Block, Forward, IN_PLACE, Node, NodeKind, PlanRegion, Step, StepKind,
};
use crate::compile::{Entry, LibId, position};
use crate::observe::Stepped;

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

/// A compiled value: what it computes, into its register where the plan's
/// code puts it there.
type Compute<const S: bool> =
    Box<dyn for<'r, 'm> Fn(&'r mut PlanRun<'m, S>) -> Result<Value, PlanExit> + Send + Sync>;

/// A compiled statement: it opens the plan's statement and performs it.
type Perform<const S: bool> =
    Box<dyn for<'r, 'm> Fn(&'r mut PlanRun<'m, S>) -> Result<Went, PlanExit> + Send + Sync>;

fn compute<const S: bool>(
    run: impl for<'r, 'm> Fn(&'r mut PlanRun<'m, S>) -> Result<Value, PlanExit> + Send + Sync + 'static,
) -> Compute<S> {
    Box::new(run)
}

fn perform<const S: bool>(
    run: impl for<'r, 'm> Fn(&'r mut PlanRun<'m, S>) -> Result<Went, PlanExit> + Send + Sync + 'static,
) -> Perform<S> {
    Box::new(run)
}

/// An operand of a compiled form, read in place where it is a variable or
/// a constant.
struct Operand<const S: bool> {
    /// The plan register its value goes to where the plan's code puts it.
    reg: u32,
    leaf: Leaf<S>,
}

enum Leaf<const S: bool> {
    /// A plan variable's value.
    Local(u32),
    /// A resident constant.
    Const(Value),
    Computed(Compute<S>),
}

impl<const S: bool> Operand<S> {
    #[inline(always)]
    fn get(&self, run: &mut PlanRun<'_, S>) -> Result<Value, PlanExit> {
        match &self.leaf {
            Leaf::Local(slot) => Ok(run.machine.regs[run.slots + *slot as usize]),
            Leaf::Const(value) => Ok(*value),
            Leaf::Computed(computed) => computed(run),
        }
    }

    /// Whether reading it computes nothing.
    fn leaf(&self) -> bool {
        !matches!(self.leaf, Leaf::Computed(_))
    }
}

/// A plan's regions, compiled for the plan's variant, by the variable that
/// holds what each walks.
pub(crate) struct Regions {
    walked: Box<[u32]>,
    /// `Box<[Region<S>]>`, `S` whether the plan has shared variables.
    compiled: Box<dyn Any + Send + Sync>,
}

impl Default for Regions {
    fn default() -> Self {
        Self {
            walked: Box::new([]),
            compiled: Box::new(()),
        }
    }
}

impl Regions {
    /// Compiles `regions` of a plan that has shared variables when
    /// `shared`.
    pub(crate) fn new(regions: Vec<PlanRegion>, shared: bool) -> Self {
        let walked = regions.iter().map(|region| region.walked).collect();
        let compiled: Box<dyn Any + Send + Sync> = if shared {
            Box::new(Self::compiled::<true>(regions))
        } else {
            Box::new(Self::compiled::<false>(regions))
        };
        Self { walked, compiled }
    }

    fn compiled<const S: bool>(regions: Vec<PlanRegion>) -> Box<[Region<S>]> {
        regions
            .into_iter()
            .map(|region| Region {
                binding: region.binding,
                metered: region.metered,
                body: body(region.body),
                merge: region.merge.and_then(merge::compiled),
                slots: region.slots.and_then(slots::compiled),
            })
            .collect()
    }

    /// The region of the loop that walks what plan variable `walked`
    /// holds, by its index.
    #[inline]
    pub(crate) fn position(&self, walked: u32) -> Option<u32> {
        self.walked
            .iter()
            .position(|held| *held == walked)
            .map(|at| at as u32)
    }

    fn get<const S: bool>(&self, at: u32) -> Option<&Region<S>> {
        self.compiled
            .downcast_ref::<Box<[Region<S>]>>()?
            .get(at as usize)
    }
}

/// A compiled region: the plan variable each element is bound to, the
/// units each test of the loop charges, its body, and the merge pass of a
/// comparator sort ([`merge`]) or the slots walk of an array callback
/// helper ([`slots`]) the body is, which the walk steps natively.
struct Region<const S: bool> {
    binding: u32,
    metered: u64,
    body: Body<S>,
    merge: Option<merge::Merged<S>>,
    slots: Option<slots::Slotted<S>>,
}

/// A compiled block: its statements in order, and the plan variables it
/// declares, which its end empties.
struct Body<const S: bool> {
    steps: Box<[Perform<S>]>,
    declares: Box<[u32]>,
}

fn body<const S: bool>(block: Block) -> Body<S> {
    Body {
        steps: block.steps.into_vec().into_iter().map(statement).collect(),
        declares: block.declares,
    }
}

fn operand<const S: bool>(node: Node) -> Operand<S> {
    let reg = node.reg;
    let leaf = match node.kind {
        NodeKind::Local(slot) => Leaf::Local(slot),
        NodeKind::Const(value) => Leaf::Const(value),
        kind => Leaf::Computed(computed(reg, kind)),
    };
    Operand { reg, leaf }
}

fn operands<const S: bool>(args: Box<[Node]>) -> Box<[Operand<S>]> {
    args.into_vec().into_iter().map(operand).collect()
}

/// A form whose shape its plan does not have: it halts the plan.
fn lost<const S: bool>(fault: &'static str) -> Compute<S> {
    compute(move |run| Err(run.lost_form(fault)))
}

/// The compiled statement `step`.
fn statement<const S: bool>(step: Step) -> Perform<S> {
    let span = step.span;
    match step.kind {
        StepKind::If {
            cond,
            then,
            otherwise,
        } => {
            let (then, otherwise) = (body::<S>(then), body::<S>(otherwise));
            match special(&cond) {
                Some(Special::Typed(typed)) => perform(move |run| {
                    run.open_statement_of(span)?;
                    let arm = match run.typed(&typed)? {
                        Value::Bool(true) => &then,
                        Value::Bool(false) => &otherwise,
                        other => return Err(run.not_bool(typed.reg, other)),
                    };
                    run.pass(arm)
                }),
                _ => {
                    let cond = operand::<S>(cond);
                    perform(move |run| {
                        run.open_statement_of(span)?;
                        let arm = match cond.get(run)? {
                            Value::Bool(true) => &then,
                            Value::Bool(false) => &otherwise,
                            other => return Err(run.not_bool(cond.reg, other)),
                        };
                        run.pass(arm)
                    })
                }
            }
        }
        StepKind::Break(left) => perform(move |run| {
            run.open_statement_of(span)?;
            for slot in &left {
                run.unbind(*slot);
            }
            Ok(Went::Broke)
        }),
        StepKind::Assign { slot, value } => written::<S>(span, value, move |run, value| {
            run.machine.put(run.slots + slot as usize, value);
            Ok(())
        }),
        StepKind::Bind { slot, value } => written::<S>(span, value, move |run, value| {
            run.bind_value(slot, value).map(drop)
        }),
        StepKind::Store {
            order,
            target,
            src,
            placed,
        } => {
            let order = order.map(operand::<S>);
            perform(move |run| {
                run.open_statement_of(span)?;
                run.store(&order, (target, src), placed)?;
                Ok(Went::Through)
            })
        }
        StepKind::Discard(value) => {
            let value = operand::<S>(value);
            perform(move |run| {
                run.open_statement_of(span)?;
                value.get(run)?;
                Ok(Went::Through)
            })
        }
    }
}

/// A statement that writes the value `value` computes to a plan variable
/// by `write`, its value computed in place where it is a typed call or a
/// list read of operands read in place.
fn written<const S: bool>(
    span: u32,
    value: Node,
    write: impl for<'r, 'm> Fn(&'r mut PlanRun<'m, S>, Value) -> Result<(), PlanExit>
    + Copy
    + Send
    + Sync
    + 'static,
) -> Perform<S> {
    match special(&value) {
        Some(Special::Typed(typed)) => perform(move |run| {
            run.open_statement_of(span)?;
            let value = run.typed(&typed)?;
            write(run, value)?;
            Ok(Went::Through)
        }),
        Some(Special::Item(read)) => perform(move |run| {
            run.open_statement_of(span)?;
            let value = run.held_item(&read)?;
            write(run, value)?;
            Ok(Went::Through)
        }),
        None => {
            let value = operand::<S>(value);
            perform(move |run| {
                run.open_statement_of(span)?;
                let value = value.get(run)?;
                write(run, value)?;
                Ok(Went::Through)
            })
        }
    }
}

/// A value read in place: a plan variable's, or a resident constant.
#[derive(Clone, Copy)]
enum Place {
    Local(u32),
    Const(Value),
}

impl Place {
    #[inline(always)]
    fn get<const S: bool>(self, run: &PlanRun<'_, S>) -> Value {
        match self {
            Self::Local(slot) => run.machine.regs[run.slots + slot as usize],
            Self::Const(value) => value,
        }
    }
}

/// A fused call of library function `lib`, its register `reg`, on two
/// operands read in place, each with its register, whose prepared entry
/// `direct` has the typed kernels `pair`.
struct Typed {
    lib: LibId,
    reg: u32,
    operands: [(u32, Place); 2],
    pair: Pair,
    direct: Entry,
}

/// `list.get` by `lib`, its register `reg`, of a list and a position read
/// in place, each with its register, whose value a statement's variable
/// takes.
struct ItemRead {
    lib: LibId,
    reg: u32,
    operands: [(u32, Place); 2],
}

/// A statement's root value that computes in place.
enum Special {
    Typed(Typed),
    Item(ItemRead),
}

impl Place {
    fn of(node: &Node) -> Option<(u32, Self)> {
        match node.kind {
            NodeKind::Local(slot) => Some((node.reg, Self::Local(slot))),
            NodeKind::Const(value) => Some((node.reg, Self::Const(value))),
            _ => None,
        }
    }
}

/// How the root value `node` of a statement computes in place, if it
/// does: a fused call with typed kernels, or a list read, of two operands
/// read in place.
fn special(node: &Node) -> Option<Special> {
    let places = |args: &[Node]| match args {
        [left, right] => Some([Place::of(left)?, Place::of(right)?]),
        _ => None,
    };
    match &node.kind {
        NodeKind::Call {
            lib,
            args,
            direct: Some(direct),
        } => Some(Special::Typed(Typed {
            lib: *lib,
            reg: node.reg,
            operands: places(args)?,
            pair: direct.pair()?,
            direct: direct.clone(),
        })),
        NodeKind::Item { lib, args } => Some(Special::Item(ItemRead {
            lib: *lib,
            reg: node.reg,
            operands: places(args)?,
        })),
        _ => None,
    }
}

/// The compiled value of form `kind`, its register `reg`.
fn computed<const S: bool>(reg: u32, kind: NodeKind) -> Compute<S> {
    match kind {
        NodeKind::Local(slot) => {
            compute(move |run| Ok(run.machine.regs[run.slots + slot as usize]))
        }
        NodeKind::Const(value) => compute(move |_| Ok(value)),
        NodeKind::Pin(value) => {
            let value = operand::<S>(*value);
            compute(move |run| {
                let pinned = value.get(run)?;
                let held = run.machine.pin_value(&pinned);
                run.held(held)?;
                Ok(pinned)
            })
        }
        NodeKind::IsUnit { lib, args } => {
            let Ok([value, unit]) = <[Node; 2]>::try_from(args.into_vec()) else {
                return lost("a plan region's hole test has no operands");
            };
            let (value, unit) = (operand::<S>(value), operand::<S>(unit));
            compute(move |run| run.is_unit(reg, lib, (&value, &unit)))
        }
        NodeKind::Length { lib, list } => {
            let list = operand::<S>(*list);
            compute(move |run| run.length(reg, lib, &list))
        }
        NodeKind::Item { lib, args } => {
            let Ok([list, index]) = <[Node; 2]>::try_from(args.into_vec()) else {
                return lost("a plan region's item read has no operands");
            };
            let (list, index) = (operand::<S>(list), operand::<S>(index));
            compute(move |run| {
                let values = (list.get(run)?, index.get(run)?);
                run.item(reg, lib, (list.reg, index.reg), values)
            })
        }
        NodeKind::Call {
            lib,
            args,
            direct: Some(direct),
        } => fused(reg, lib, operands(args), direct),
        NodeKind::Call {
            lib,
            args,
            direct: None,
        } => {
            let args = operands::<S>(args);
            compute(move |run| {
                let at = run.base + reg as usize;
                run.operands(&args)?;
                run.call_at(lib, at, args.len())?;
                Ok(run.machine.regs[at])
            })
        }
        NodeKind::Unit => compute(move |run| {
            run.tuple(reg, 0)?;
            Ok(run.machine.regs[run.base + reg as usize])
        }),
        NodeKind::Apply {
            site,
            callee,
            args,
            steady,
        } => {
            let args = operands::<S>(args);
            if steady && args.iter().all(Operand::leaf) {
                // Its operands compute nothing: nothing sees the callee in
                // its register before the site takes it.
                return compute(move |run| {
                    let at = run.base + reg as usize;
                    let callee = run.machine.regs[run.slots + callee as usize];
                    run.operands(&args)?;
                    run.apply_steady(site, at, args.len(), callee)?;
                    Ok(run.machine.regs[at])
                });
            }
            compute(move |run| {
                let at = run.base + reg as usize;
                let callee = run.machine.regs[run.slots + callee as usize];
                run.machine.put(at, callee);
                run.operands(&args)?;
                if steady {
                    let callee = run.machine.take(at);
                    run.apply_steady(site, at, args.len(), callee)?;
                } else {
                    run.apply_at(site, at, args.len())?;
                }
                Ok(run.machine.regs[at])
            })
        }
        NodeKind::Invoke {
            site,
            lib,
            args,
            forward: Some(forward),
        } => {
            let Ok([arg]) = <[Node; 1]>::try_from(args.into_vec()) else {
                return lost("a plan region's forwarded invoke has no operand");
            };
            let arg = operand::<S>(arg);
            compute(move |run| run.forwarded(site, reg, lib, &arg, &forward))
        }
        NodeKind::Invoke {
            site,
            lib,
            args,
            forward: None,
        } => {
            let args = operands::<S>(args);
            compute(move |run| {
                let at = run.base + reg as usize;
                run.operands(&args)?;
                run.offer(site, lib, at, args.len())?;
                Ok(run.machine.regs[at])
            })
        }
    }
}

/// The fused call of library function `lib` on `args` by its prepared
/// entry `direct`, its register `reg`: on the values in place, its answer
/// in no register; two floats or two word integers through the entry's
/// own typed kernels, where it has them.
fn fused<const S: bool>(
    reg: u32,
    lib: LibId,
    args: Box<[Operand<S>]>,
    direct: Entry,
) -> Compute<S> {
    match <Box<[Operand<S>; 2]>>::try_from(args) {
        Ok(pair) => {
            let [left, right] = *pair;
            match direct.pair() {
                Some(typed) => compute(move |run| {
                    let values = [left.get(run)?, right.get(run)?];
                    if run.quiet()
                        && let Some(value) = typed_answer(typed, values)
                    {
                        return Ok(value);
                    }
                    run.fused_call(lib, reg, [left.reg, right.reg], &values, &direct)
                }),
                None => compute(move |run| {
                    let values = [left.get(run)?, right.get(run)?];
                    run.fused_call(lib, reg, [left.reg, right.reg], &values, &direct)
                }),
            }
        }
        Err(args) => {
            if args.len() > IN_PLACE {
                return lost("a plan region's fused call has too many operands");
            }
            compute(move |run| {
                let mut values = [Value::Absent; IN_PLACE];
                for (value, arg) in values.iter_mut().zip(&args) {
                    *value = arg.get(run)?;
                }
                run.fused_call_n(lib, reg, &args, &values[..args.len()], &direct)
            })
        }
    }
}

/// What the typed kernels `typed` answer on `values`, two floats or two
/// word integers, as the entry they belong to answers them.
#[inline(always)]
fn typed_answer(typed: Pair, values: [Value; 2]) -> Option<Value> {
    match values {
        [Value::Float(left), Value::Float(right)] => scalar::floats(typed.floats?, left, right),
        [Value::Int(left), Value::Int(right)] => scalar::ints(typed.ints?, left, right),
        _ => None,
    }
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
        let Some(region) = prepared.regions.get::<SHARED>(region) else {
            return Err(self.lost_form("a plan region is not compiled for its plan"));
        };
        // Its steady callbacks and its call depth are resolved afresh for
        // each walk.
        self.steady = None;
        self.depth = None;
        // A merge pass and a slots walk step natively where nothing but a
        // charge tape observes the run: their forms charge nothing, and the
        // walk's tests and the callbacks' islands record their own charges
        // (FIG-6948, FIG-6978).
        let native = !self.machine.charging
            && self
                .machine
                .observer
                .as_deref()
                .is_none_or(|observer| observer.only_charges());
        let merge = region.merge.as_ref().filter(|_| native);
        let slotted = region.slots.as_ref().filter(|_| native);
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
            let (went, stepped) = match (merge, slotted) {
                (Some(merge), _) => match self.merge_step(merge)? {
                    Some(went) => (went, Stepped::Merge),
                    None => (self.pass(&region.body)?, Stepped::Pass),
                },
                (None, Some(slotted)) => (self.slots_step(slotted)?, Stepped::Slots),
                (None, None) => (self.pass(&region.body)?, Stepped::Pass),
            };
            if let Some(tape) = self
                .machine
                .observer
                .as_deref()
                .and_then(|observer| observer.charges.as_ref())
            {
                tape.passed(stepped);
            }
            match went {
                Went::Broke => return Ok(false),
                // The test's statement puts nothing in the registers.
                Went::Through => self.open_statement_of(0)?,
            }
        }
    }

    /// One pass through `body`.
    #[inline]
    fn pass(&mut self, body: &Body<SHARED>) -> Result<Went, PlanExit> {
        for step in &body.steps {
            if let Went::Broke = step(self)? {
                return Ok(Went::Broke);
            }
        }
        for slot in body.declares.iter().rev() {
            self.unbind(*slot);
        }
        Ok(Went::Through)
    }

    /// Whether the plan's general calls are unobserved and uncharged: a
    /// fused form answers without them.
    #[inline(always)]
    fn quiet(&self) -> bool {
        !self.machine.charging && self.machine.observer.is_none()
    }

    /// An `if`'s test in register `reg` gave `other`, which is no bool:
    /// the plan's test of it raises.
    #[cold]
    #[inline(never)]
    fn not_bool(&mut self, reg: u32, other: Value) -> PlanExit {
        self.widen();
        self.machine.put(self.base + reg as usize, other);
        if let Err(exit) = self.test(reg, Test::If) {
            return exit;
        }
        self.lost_form("a plan region's test passed no bool")
    }

    /// Puts the values of `args` in their registers, in order, as the
    /// plan's code puts a call's operands.
    #[inline]
    fn operands(&mut self, args: &[Operand<SHARED>]) -> Result<(), PlanExit> {
        for arg in args {
            let value = arg.get(self)?;
            self.machine.put(self.base + arg.reg as usize, value);
        }
        Ok(())
    }

    /// `set target[index] = value`, its three operands `order` in the
    /// plan's order, the target's register `target` and the value's `src`.
    /// A list is written from the values in place, as the plan's member
    /// write writes it; any other write reads its operands from their
    /// registers, where the plan's code leaves them, as does one whose
    /// operands go to their registers as they are evaluated (`placed`).
    #[inline(always)]
    fn store(
        &mut self,
        order: &[Operand<SHARED>; 3],
        (target, src): (u32, u32),
        placed: bool,
    ) -> Result<(), PlanExit> {
        let mut values = [Value::Absent; 3];
        for (value, node) in values.iter_mut().zip(order) {
            *value = node.get(self)?;
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
    #[inline(always)]
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

    /// `list.len` by `lib` of what `list` gives, its value to register
    /// `reg` where the plan's general call answers it: a resident list's
    /// length is read from the arena, as that call's native reads it, at
    /// that call's verdict points, its reservation of nothing built and its
    /// check once its value is held.
    #[inline(always)]
    fn length(&mut self, reg: u32, lib: LibId, list: &Operand<SHARED>) -> Result<Value, PlanExit> {
        let value = list.get(self)?;
        let length = match value {
            Value::List(handle) if self.quiet() => self.machine.arena.borrow().len(handle).ok(),
            _ => None,
        };
        if let Some(length) = length {
            let verdicts = self.machine.verdict_points();
            self.held(verdicts)?;
            return Ok(Value::Int(length as i64));
        }
        self.widen();
        self.machine.put(self.base + list.reg as usize, value);
        let at = self.base + reg as usize;
        self.call_at(lib, at, 1)?;
        Ok(self.machine.regs[at])
    }

    /// `list.get` by `lib` of a list at a position, `values`, whose
    /// operands' registers are `regs`, its value to register `reg` where
    /// the plan's general call answers it: a resident list's item at a
    /// position it has is read from the arena, as that call's native reads
    /// it, at that call's verdict points, and pinned where the call's
    /// register would hold it, as a fused call's answer is.
    #[inline(always)]
    fn item(
        &mut self,
        reg: u32,
        lib: LibId,
        regs: (u32, u32),
        values: (Value, Value),
    ) -> Result<Value, PlanExit> {
        let item = match values {
            (Value::List(handle), Value::Int(position)) if position >= 0 && self.quiet() => self
                .machine
                .arena
                .borrow()
                .item(handle, position as u64)
                .ok()
                .flatten(),
            _ => None,
        };
        let Some(item) = item else {
            return self.item_called(reg, lib, regs, values);
        };
        let verdicts = self.machine.verdict_points();
        self.held(verdicts)?;
        let pinned = self.machine.pin_value(&item);
        self.held(pinned)?;
        Ok(item)
    }

    /// The value of the typed call `typed`: its entry's typed kernels on
    /// two floats or two word integers, and its fused call otherwise.
    #[inline(always)]
    fn typed(&mut self, typed: &Typed) -> Result<Value, PlanExit> {
        let [(_, left), (_, right)] = typed.operands;
        let values = [left.get(self), right.get(self)];
        if self.quiet()
            && let Some(value) = typed_answer(typed.pair, values)
        {
            return Ok(value);
        }
        self.untyped(typed, values)
    }

    /// [`Self::typed`] where its kernels do not answer `values`.
    #[inline(never)]
    fn untyped(&mut self, typed: &Typed, values: [Value; 2]) -> Result<Value, PlanExit> {
        let [(left, _), (right, _)] = typed.operands;
        self.fused_call(typed.lib, typed.reg, [left, right], &values, &typed.direct)
    }

    /// The list read `read` whose value its statement's variable takes:
    /// [`Self::item`], save that it does not pin what the arena answers.
    /// The pin the general call's register would hold lasts until the
    /// statement ends, and no verdict point comes between it and the
    /// variable's write, so no sweep could see it.
    #[inline(always)]
    fn held_item(&mut self, read: &ItemRead) -> Result<Value, PlanExit> {
        let [(list_reg, list), (index_reg, index)] = read.operands;
        let values = (list.get(self), index.get(self));
        let item = match values {
            (Value::List(handle), Value::Int(position)) if position >= 0 && self.quiet() => self
                .machine
                .arena
                .borrow()
                .item(handle, position as u64)
                .ok()
                .flatten(),
            _ => None,
        };
        let Some(item) = item else {
            return self.item_called(read.reg, read.lib, (list_reg, index_reg), values);
        };
        let verdicts = self.machine.verdict_points();
        self.held(verdicts)?;
        Ok(item)
    }

    /// [`Self::item`] where the arena does not answer: the plan's general
    /// call, its operands `values` in their registers `regs`.
    #[cold]
    #[inline(never)]
    fn item_called(
        &mut self,
        reg: u32,
        lib: LibId,
        (list, index): (u32, u32),
        (target, at): (Value, Value),
    ) -> Result<Value, PlanExit> {
        self.widen();
        self.machine.put(self.base + list as usize, target);
        self.machine.put(self.base + index as usize, at);
        let out = self.base + reg as usize;
        self.call_at(lib, out, 2)?;
        Ok(self.machine.regs[out])
    }

    /// The hole test `same(value, ())` by `lib`, its value to register
    /// `reg` where it builds the tuple `unit` gives.
    #[inline(always)]
    fn is_unit(
        &mut self,
        reg: u32,
        lib: LibId,
        (value, unit): (&Operand<SHARED>, &Operand<SHARED>),
    ) -> Result<Value, PlanExit> {
        let tested = value.get(self)?;
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
        (value, unit): (&Operand<SHARED>, &Operand<SHARED>),
        tested: Value,
    ) -> Result<Value, PlanExit> {
        self.widen();
        self.machine.put(self.base + value.reg as usize, tested);
        unit.get(self)?;
        let at = self.base + reg as usize;
        self.call_at(lib, at, 2)?;
        Ok(self.machine.regs[at])
    }

    /// Calls library function `lib` on two operands whose values are
    /// `values`, in registers `regs`, as the plan's general call does, `direct` the prepared
    /// entry it takes first: on the values in place, its answer in no
    /// register.
    #[inline(always)]
    fn fused_call(
        &mut self,
        lib: LibId,
        reg: u32,
        regs: [u32; 2],
        values: &[Value; 2],
        direct: &Entry,
    ) -> Result<Value, PlanExit> {
        if self.quiet()
            && let Some(answer) = direct.answer(values, false)
        {
            let pinned = self.machine.pin_value(&answer.value);
            self.held(pinned)?;
            return Ok(answer.value);
        }
        self.unfused_call(lib, reg, &regs, values)
    }

    /// [`Self::fused_call`] of any number of operands.
    #[inline(always)]
    fn fused_call_n(
        &mut self,
        lib: LibId,
        reg: u32,
        args: &[Operand<SHARED>],
        values: &[Value],
        direct: &Entry,
    ) -> Result<Value, PlanExit> {
        if self.quiet()
            && let Some(answer) = direct.answer(values, false)
        {
            let pinned = self.machine.pin_value(&answer.value);
            self.held(pinned)?;
            return Ok(answer.value);
        }
        let mut regs = [0u32; IN_PLACE];
        for (held, arg) in regs.iter_mut().zip(args) {
            *held = arg.reg;
        }
        self.unfused_call(lib, reg, &regs[..args.len()], values)
    }

    /// A fused call where its entry declines, or the call is observed: its
    /// operands, whose values are `values`, go to their registers `regs`,
    /// and the rest of the plan's general call puts its value in the
    /// call's register `reg`.
    #[cold]
    #[inline(never)]
    fn unfused_call(
        &mut self,
        lib: LibId,
        reg: u32,
        regs: &[u32],
        values: &[Value],
    ) -> Result<Value, PlanExit> {
        let at = self.base + reg as usize;
        let unobserved = self.quiet();
        self.widen();
        for (held, value) in regs.iter().zip(values) {
            self.machine.put(self.base + *held as usize, *value);
        }
        if unobserved {
            let called = self.machine.call_declined(self.task, self.exe, lib, at);
            self.check(called)?;
        } else {
            self.call_at(lib, at, regs.len())?;
        }
        Ok(self.machine.regs[at])
    }

    /// The call of `lib` the plan offers at site `site` on `arg`, its value
    /// to register `reg`, which its prepared entry may answer (`forward`):
    /// an unobserved call that the entry answers at the caller's depth is
    /// that answer, pinned, as the general call answers it; any other is
    /// offered.
    #[inline(always)]
    fn forwarded(
        &mut self,
        site: u32,
        reg: u32,
        lib: LibId,
        arg: &Operand<SHARED>,
        forward: &Forward,
    ) -> Result<Value, PlanExit> {
        let value = arg.get(self)?;
        if self.quiet()
            && self.entry_fits(forward)
            && let Some(answer) = forward.entry.call(&[value])
        {
            let pinned = self.machine.pin_value(&answer);
            self.held(pinned)?;
            return Ok(answer);
        }
        self.offered(site, reg, lib, arg.reg, value)
    }

    /// [`Self::forwarded`] where the entry does not answer: the
    /// argument `value` goes to its register `held` and the call is
    /// offered.
    #[cold]
    #[inline(never)]
    fn offered(
        &mut self,
        site: u32,
        reg: u32,
        lib: LibId,
        held: u32,
        value: Value,
    ) -> Result<Value, PlanExit> {
        self.widen();
        self.machine.put(self.base + held as usize, value);
        let at = self.base + reg as usize;
        self.offer(site, lib, at, 1)?;
        Ok(self.machine.regs[at])
    }

    /// Whether the general call admits a call of a function `forward`
    /// describes and takes its entry at the plan's call depth, as
    /// [`super::KernelMachine::answer`] decides it: an operation's
    /// admission refuses past the depth bound, and an entry takes the
    /// depth it needs below the bound. The depth is the walk's, read once:
    /// no frame is pushed and no plan entered between its forms.
    #[inline]
    fn entry_fits(&mut self, forward: &Forward) -> bool {
        let depth = match self.depth {
            Some(depth) => depth,
            None => {
                let main = self.task == TaskId::MAIN && self.machine.session_cell;
                let Ok(task) = self.machine.task(self.task) else {
                    return false;
                };
                let depth = task.frames.len() as u64 + self.machine.plans as u64 + u64::from(!main);
                self.depth = Some(depth);
                depth
            }
        };
        let bound = u64::from(self.machine.bounds.call_depth);
        let needed = u64::from(forward.entry.depth());
        if forward.operation {
            depth <= bound && (needed <= 1 || needed <= bound - depth)
        } else {
            needed == 0 || depth + needed <= bound
        }
    }

    #[cold]
    fn lost_form(&mut self, fault: &str) -> PlanExit {
        self.halt = Some(Box::new(super::fault(fault)));
        PlanExit::Halt
    }
}
