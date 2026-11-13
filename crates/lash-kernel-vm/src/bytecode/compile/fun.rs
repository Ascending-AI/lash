//! Per-unit lowering state and checked local allocation.

use std::collections::{BTreeSet, HashMap};
use std::sync::Arc;

use lash_kernel_doc::Stmt;
use lash_kernel_value::names::{FunctionId, Name};
use lash_kernel_value::sites::{Site, Unit as SiteUnit};

use super::{Result, coordinate, within};
use crate::bytecode::op::Op;
use crate::bytecode::program::{
    ActionSite, CallSite, ClosureSite, Event, LoopMark, LoopSite, Mark, Point,
};
use crate::bytecode::site_tables::Path;

/// Where a read or write of a name goes.
#[derive(Clone, Copy, Debug)]
pub(crate) enum Var {
    Local(u16),
    Cell(u16),
    Capture(u16),
    Global(u16),
}

/// A loop being lowered: where `continue` goes and the jumps `break`
/// leaves to patch.
pub(crate) struct LoopCtx {
    pub head: u32,
    pub breaks: Vec<usize>,
}

/// A scope a departure (`break`, `continue`, `return`) leaves.
#[derive(Clone)]
pub(crate) enum Scope {
    Handler,
    Catch(u32),
    /// A finally block and its site path.
    Finally(Arc<[Stmt]>, Vec<u32>),
}

/// One unit while it is lowered.
pub(crate) struct Fun {
    pub ops: Vec<Op>,
    pub event: Vec<Event>,
    pub step: Vec<bool>,
    pub(super) pending: Event,
    /// The next op starts a step (a statement's start).
    pub mark: bool,
    /// What an observer sees before the next op: a statement start.
    pub start: Option<Point>,
    pub points: Vec<(usize, Point)>,
    pub slots: HashMap<Name, u16>,
    pub locals: u16,
    pub boxed: BTreeSet<Name>,
    pub captures: HashMap<Name, u16>,
    pub capture_names: Vec<Name>,
    pub loops: Vec<LoopCtx>,
    /// Open handlers and finally blocks, innermost last, each with the
    /// loop depth it opened at.
    pub scopes: Vec<(usize, Scope)>,
    pub main: bool,
    /// The unit runs inside a covered library extent (`K-CHG-007`).
    pub covered: bool,
    /// The library function whose body (or a closure in it) this is;
    /// `None` in the document's own code (`K-LIB-010`).
    pub owner: Option<FunctionId>,
    pub records: Vec<Arc<[u32]>>,
    pub closures: Vec<ClosureSite>,
    pub calls: Vec<CallSite>,
    pub loop_sites: Vec<LoopSite>,
    pub actions: Vec<ActionSite>,
    pub action_marks: Vec<Arc<Mark>>,
    /// The statement being lowered.
    pub statement: Site,
    /// The loops that enclose the code being lowered, outermost first.
    pub enclosing: Vec<LoopMark>,
    pub site: SiteUnit,
    pub path: Vec<u32>,
    /// The expression each op lowers, by path (`site_tables`).
    pub origin: Vec<Path>,
    /// How much of each op's origin is the statement it runs in.
    pub stated: Vec<u16>,
    /// Where each slot was declared, and the variable it holds if any
    /// (`site_tables`).
    pub declared: Vec<(Path, Option<Name>)>,
}

impl Fun {
    pub(crate) fn new(
        site: SiteUnit,
        main: bool,
        covered: bool,
        owner: Option<FunctionId>,
    ) -> Self {
        Self {
            ops: Vec::new(),
            event: Vec::new(),
            step: Vec::new(),
            pending: Event::default(),
            mark: false,
            start: None,
            points: Vec::new(),
            slots: HashMap::new(),
            locals: 0,
            boxed: BTreeSet::new(),
            captures: HashMap::new(),
            capture_names: Vec::new(),
            loops: Vec::new(),
            scopes: Vec::new(),
            main,
            covered,
            owner,
            records: Vec::new(),
            closures: Vec::new(),
            calls: Vec::new(),
            loop_sites: Vec::new(),
            actions: Vec::new(),
            action_marks: Vec::new(),
            statement: Site::new(site.clone(), Vec::new()),
            enclosing: Vec::new(),
            site,
            path: Vec::new(),
            origin: Vec::new(),
            stated: Vec::new(),
            declared: Vec::new(),
        }
    }

    /// The site of the node being lowered.
    pub(crate) fn site(&self) -> Site {
        Site::new(self.site.clone(), self.path.clone())
    }

    /// Whether the unit is the document's own code, which an observer
    /// sees (`observe/`): library code (`K-LIB-010`) never is.
    pub(crate) fn observed(&self) -> bool {
        !self.covered && self.owner.is_none()
    }

    /// Records what an observer sees before the op at `at`.
    pub(crate) fn point(&mut self, at: u32, point: Point) {
        if self.observed() {
            self.points.push((at as usize, point));
        }
    }

    /// `n` one-unit events of uncovered code (statement, expression node,
    /// atom, loop test), billed by the next op. Lowering emits each atom
    /// and value; at most the validator's 64-deep source path is pending.
    pub(crate) fn units(&mut self, n: u64) {
        if !self.covered {
            self.pending.units = self.pending.units.saturating_add(n);
        }
    }

    /// One-unit events that charge in covered code too: the atoms of a
    /// `perform` or `spawn` (`K-CHG-007`), emitted one atom at a time by lower::atoms.
    pub(crate) fn always(&mut self, n: u64) {
        self.pending.units = self.pending.units.saturating_add(n);
    }

    /// A `metered N` loop test's units: one declared event of `n`, billed
    /// whole and in covered code too (`K-CHG-007`). lower::loop_test emits
    /// this event before any other atomic event, so its u64 value is exact.
    pub(crate) fn metered(&mut self, n: u64) {
        self.pending.atomic = self.pending.atomic.saturating_add(n);
    }

    /// A construction's members, one atomic event on the next op. Lowering
    /// admits u32 member counts and emits each construction separately.
    pub(crate) fn members(&mut self, n: u64) {
        if !self.covered {
            self.pending.atomic = self.pending.atomic.saturating_add(n);
        }
    }

    /// Emits `op`, which bills nothing, leaving pending events and the
    /// step mark to the op after it.
    pub(crate) fn quiet(&mut self, op: Op) {
        self.originate();
        self.ops.push(op);
        self.event.push(Event::default());
        self.step.push(false);
    }

    pub(crate) fn emit(&mut self, op: Op) -> usize {
        if let Some(point) = self.start.take() {
            self.points.push((self.ops.len(), point));
        }
        self.originate();
        self.ops.push(op);
        self.event.push(std::mem::take(&mut self.pending));
        self.step.push(std::mem::take(&mut self.mark));
        self.ops.len() - 1
    }

    /// Records the path of the op about to be emitted, sharing the last
    /// op's where it is the same.
    fn originate(&mut self) {
        let path = match self.origin.last() {
            Some(last) if **last == *self.path => Arc::clone(last),
            _ => Path::from(self.path.as_slice()),
        };
        self.origin.push(path);
        let stated = if self.path.starts_with(&self.statement.path) {
            self.statement.path.len()
        } else {
            0
        };
        // validate::check_depth and KernelDataLimits cap every source path at 64.
        self.stated.push(stated as u16);
    }

    /// A block starts here; events still pending are billed before it.
    pub(crate) fn leader(&mut self) {
        if !self.pending.is_none() || self.mark {
            self.emit(Op::Nop);
        }
        if !matches!(self.ops.last(), Some(Op::Block(_))) {
            self.emit(Op::Block(0));
        }
    }

    /// Starts a block and gives its head, a jump target.
    pub(crate) fn label(&mut self) -> Result<u32> {
        self.leader();
        Ok(self.here()? - 1)
    }

    pub(crate) fn here(&self) -> Result<u32> {
        coordinate(self.ops.len(), "ops of one function")
    }

    /// Declares `name` in the innermost scope (`K-FORM-003`): a new
    /// variable in a slot of its own, shadowing any outer one until the
    /// scope ends.
    pub(crate) fn declare(&mut self, name: &Name) -> Result<u16> {
        let slot = self.temp()?;
        self.slots.insert(name.clone(), slot);
        self.declared[usize::from(slot)].1 = Some(name.clone());
        Ok(slot)
    }

    /// A hidden local, admitted before its slot can be used in arithmetic.
    pub(crate) fn temp(&mut self) -> Result<u16> {
        self.temps(1)
    }

    /// Reserves a contiguous block. Every offset below `count` fits a slot.
    pub(crate) fn temps(&mut self, count: u16) -> Result<u16> {
        let slot = self.locals;
        let end = within(
            usize::from(slot) + usize::from(count),
            "parameters and locals of one function",
        )?;
        self.declared
            .extend((0..count).map(|_| (Path::from(self.path.as_slice()), None)));
        self.locals = end;
        Ok(slot)
    }

    /// Patches the target of the jump at `at`.
    pub(crate) fn patch(&mut self, at: usize, to: u32) {
        self.ops[at] = match self.ops[at] {
            Op::Jump(_) => Op::Jump(to),
            Op::JumpIfFalse(_) => Op::JumpIfFalse(to),
            Op::WhileFalse(_) => Op::WhileFalse(to),
            Op::TryEnter(_) => Op::TryEnter(to),
            Op::CatchEnter(_) => Op::CatchEnter(to),
            other => other,
        };
    }
}
