//! A compiled document (cutover contract §1): one [`Program`] per machine
//! instance, never cached across instances. It holds code for each unit
//! (main, every declared function, every kernel-code body reached, every
//! closure body and spawn stub), the constant pool, each block's charge
//! and pace tables (§3) and each unit's side tables.
//!
//! A `Program` is built by the compiler and checked once by the validator;
//! nothing at run time re-checks its structure.

use std::sync::Arc;

use lash_kernel_doc::arena::ClosureIdentity;
use lash_kernel_doc::arena::native::{InlineOperation, NativeFunction};
use lash_kernel_doc::{Integer, JoinMode, Value};
use lash_kernel_value::names::{CallableId, FunctionId, ImportSlot, Name};
use lash_kernel_value::sites::Site;
use lash_kernel_value::types::Type;

use super::charge_tables::Charge;
use super::op::Op;

/// Index of a unit in [`Program::units`].
pub type UnitId = u32;

/// One compiled document.
pub struct Program {
    pub units: Vec<Unit>,
    pub constants: Vec<Constant>,
    pub natives: Vec<NativeEntry>,
    /// Completion formulas and guard limits, by index.
    pub charges: Vec<Charge>,
    /// The functions a function value may name, by index.
    pub functions: Vec<FunctionEntry>,
    /// The host bindings main's free names read (`CellStart::host`).
    pub globals: Vec<Name>,
    /// The unit a `Main` start runs.
    pub main: UnitId,
}

/// What a constant is before an instance materializes it: an immediate is
/// already a value; an object constant is placed in the instance's heap,
/// admitted and counted when materialized (contract §2).
#[derive(Clone, Debug)]
pub enum Constant {
    Immediate(Value),
    Text(Arc<str>),
    BigInt(Integer),
    Bytes(Arc<[u8]>),
    /// A function value (`Literal::Function`).
    Function(CallableId),
}

/// What a unit is, for frames and diagnostics.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum UnitKind {
    Main,
    /// A declared function of the document.
    Declared(FunctionId),
    /// A library function's kernel-code body.
    Body(FunctionId),
    /// A closure's body, by its own identity.
    Closure(FunctionId),
    /// A spawn's stub: it makes the spawned call (`K-TASK-002`).
    Spawn,
}

/// One op's fixed charge, billed at its start (`K-CHG-002`): `units`
/// one-unit events (statement, expression nodes, atoms, loop tests) and
/// then `atomic`, one event billed whole (a construction's members
/// written, or a loop test's `metered` units).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Event {
    pub units: u64,
    pub atomic: u64,
}

impl Event {
    #[inline]
    pub fn total(self) -> u64 {
        self.units.saturating_add(self.atomic)
    }

    #[inline]
    pub fn is_none(self) -> bool {
        self.units == 0 && self.atomic == 0
    }
}

/// One unit's code and tables.
pub struct Unit {
    pub kind: UnitKind,
    /// The unit runs inside a covered library extent (`K-CHG-007`): its
    /// event table holds only what charges there, and it creates no step.
    pub covered: bool,
    pub params: u16,
    /// Locals, parameters first.
    pub locals: u16,
    /// Locals a closure shares, parameters included: each is boxed into
    /// a cell at entry (`K-CLO-001`).
    pub boxed: Vec<u16>,
    pub ops: Vec<Op>,
    /// Each op's fixed charge under `Charge::Exact`.
    pub event: Vec<Event>,
    /// Each op's pace units under `Charge::Off`.
    pub pace: Vec<u32>,
    /// Whether the slice is tested before the op (`K-MACH-002`): a
    /// statement's start or a loop's back edge.
    pub step: Vec<bool>,
    /// For each op, the fixed charge its block's ops bill up to and
    /// including it: `rest(o) = cost - through[o]` (contract §3). `None`
    /// past an unrepresentable sum.
    pub through: Vec<u64>,
    /// The same for pace.
    pub pace_through: Vec<u64>,
    pub blocks: Vec<Block>,
    pub records: Vec<Arc<[u32]>>,
    pub closures: Vec<ClosureSite>,
    pub calls: Vec<CallSite>,
    pub loops: Vec<LoopSite>,
    pub actions: Vec<ActionSite>,
    /// Where each action stands, parallel to `actions`.
    pub action_marks: Vec<Arc<Mark>>,
    /// The unit is document code run charged: its frames are where an
    /// effect's origin is read (`K-EFF-008`).
    pub authored: bool,
    /// The function's name, for bound errors.
    pub name: Arc<str>,
    /// The unit's site root.
    pub site: Site,
    /// What an observer sees at an op, by op index, in op order: the
    /// document's statement starts, empty-block entries, `while` tests and
    /// action issues. Read only while the run is observed.
    pub points: Vec<(u32, Point)>,
    /// The fused runs `Op::Fused` names (`fuse`).
    pub fused: Vec<Fused>,
    /// The library bodies expanded in place of their calls (`inline`),
    /// each a region of ops past the unit's own.
    pub inlines: Vec<InlineSite>,
    /// The guard paths' folds the inline sites name (`fold`).
    pub folds: Vec<Fold>,
    /// The expression each op lowers, by its path under `site`. Ops added
    /// after sealing have none, and stand for the unit's root.
    pub origin: Vec<super::site_tables::Path>,
    /// Where each slot was declared, and the variable it holds if any.
    pub declared: Vec<(super::site_tables::Path, Option<Name>)>,
    /// Every point a frame of the unit can be saved at (contract §5).
    pub sites: super::site_tables::SiteTable,
}

impl Unit {
    /// The op compiled at `pc`, before inlining or fusion replaced it.
    pub fn original(&self, pc: usize) -> Option<Op> {
        match *self.ops.get(pc)? {
            Op::Fused(n) => Some(self.fused[n as usize].first),
            Op::Inline(n) => Some(Op::Call(self.inlines[n as usize].call)),
            op => Some(op),
        }
    }

    /// The inline region `pc` lies in, if any.
    pub fn region(&self, pc: usize) -> Option<&InlineSite> {
        self.inlines
            .iter()
            .find(|site| (site.entry as usize..site.end as usize).contains(&pc))
    }
}

/// A covered library body's call expanded in place (`inline`): the call's
/// op became `Op::Inline`, and ops `entry..end` of the unit are the body's
/// guard path, run over the body's own locals laid out where its frame
/// would hold them. The body's logical activation stays: its depth check,
/// frame admission and completion bill are the call's, and any block off
/// the guard path materializes the body's frame there (`Op::Deopt`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct InlineSite {
    /// The body's unit, whose frame the region stands for.
    pub callee: UnitId,
    /// The original call site, kept in the unit's calls: its callee, mark
    /// and resume point are the expansion's.
    pub call: u32,
    /// The `Op::Inline` op.
    pub at: u32,
    pub entry: u32,
    pub end: u32,
    /// The guard path's fold, in the unit's folds (`fold`), if it has one.
    pub fold: Option<u32>,
}

/// The most steps, and values made, of one fold.
pub const FOLD_STEPS: usize = 8;

/// An inline region's guard path from its entry to one `Op::Leave`, as
/// one guarded op (`fold`): its guards and the value it returns, read
/// with no effect. Where a guard does not hold or a read declines, the
/// region runs from its entry; otherwise the region's `Op::Leave` at
/// `leave` returns the value.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Fold {
    pub steps: [Step; FOLD_STEPS],
    pub len: u8,
    pub result: Slot,
    pub leave: u32,
}

/// Where a fold step reads a value: a body local, a constant, or a value
/// an earlier step made.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Slot {
    Local(u16),
    Const(u32),
    Made(u8),
}

/// One step of a fold.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Step {
    /// A native call's inline answer (R8), making a value: the operation
    /// the native's version declares, its arguments the first `argc`.
    Answer {
        operation: InlineOperation,
        args: [Slot; 2],
        argc: u8,
    },
    /// A branch the path takes: the value must be `Bool(holds)`.
    Guard { value: Slot, holds: bool },
    /// An index read of a list or tuple at an integer that finds an
    /// element, making it.
    Index { target: Slot, index: Slot },
}

/// A run of one prepaid block's ops that dispatches at once (`fuse`): up
/// to two operand pushes, the scalar native call they feed, and what
/// takes its answer. Its ops after the first stay in place, so a frame
/// that stands inside the run continues op by op.
#[derive(Clone, Copy, Debug)]
pub struct Fused {
    /// The run's first op, which the exact path dispatches alone.
    pub first: Op,
    /// The operands the run pushes before the call, first `pushes` used.
    pub operands: [Operand; 2],
    pub pushes: u8,
    /// The call's site and op index.
    pub site: u32,
    pub call: u32,
    /// What takes the call's answer.
    pub then: Then,
}

/// An operand a fused run pushes.
#[derive(Clone, Copy, Debug)]
pub enum Operand {
    Local(u16),
    Const(u32),
}

/// The op after a fused call that takes its answer.
#[derive(Clone, Copy, Debug)]
pub enum Then {
    /// None: the answer stays on the stack.
    Push,
    SetLocal(u16),
    Pop,
    /// `JumpIfFalse(to)`, or `WhileFalse(to)` when `looping`.
    Branch {
        to: u32,
        looping: bool,
    },
}

/// What an observed run records before an op runs (`observe/`).
#[derive(Clone, Debug)]
pub enum Point {
    /// The document statement at this site starts.
    Start(Site),
    /// An `if` tests its condition: an empty branch it takes is entered
    /// and left at once.
    Branch {
        then: Option<Site>,
        otherwise: Option<Site>,
    },
    /// The `while` statement at this site tests its condition.
    Test(Site),
    /// The document action at this site issues, after its operands:
    /// a call, perform, spawn or task action.
    Action(Site),
}

/// A block (contract §3): its events are billed by its ops in order, and
/// it ends at every action, frame call, jump and handler boundary.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Block {
    pub start: u32,
    pub end: u32,
    /// The sum of the block's fixed events, `None` when it is not
    /// representable in `u64`: such a block always runs event by event.
    pub charge: Option<u64>,
    /// The block's pace units.
    pub pace: Option<u64>,
}

/// A closure a `MakeClosure` makes.
#[derive(Clone, Debug)]
pub struct ClosureSite {
    pub unit: UnitId,
    pub identity: Arc<ClosureIdentity>,
    pub captures: Vec<Capture>,
}

/// Where a closure's captured variable comes from in its maker.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Capture {
    /// The cell in the maker's local.
    Local(u16),
    /// The maker's own capture.
    Capture(u16),
}

/// What a call site calls.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Callee {
    /// A registered native (`natives[n]`); `charged` when the call bills
    /// the native's formula (an uncovered call, `K-CHG-007`).
    Native { native: u32, charged: bool },
    /// A unit. `charge` is the completion formula the call bills when it
    /// returns or raises (an outermost uncovered library call), if any.
    Unit { unit: UnitId, charge: Option<u32> },
    /// The value below the arguments: a closure or a function value.
    Value,
    /// A function the machine answers itself; `charge` as for a unit.
    Machine {
        function: crate::functions::MachineFunction,
        charge: Option<u32>,
    },
}

#[derive(Clone, Debug)]
pub struct CallSite {
    pub callee: Callee,
    pub argc: u8,
    pub site: Site,
    /// Where the calling frame stands while the callee runs.
    pub mark: Arc<Mark>,
}

/// Where a frame stands at a call or an action (`K-EVAL-008`,
/// `K-EFF-008`): its statement, the loops of its unit that enclose it,
/// outermost first, and the action site's index among the program's
/// occurrence counters.
#[derive(Clone, Debug)]
pub struct Mark {
    pub statement: Site,
    pub loops: Arc<[LoopMark]>,
    /// The program-wide index of the site, for occurrence counts.
    pub occurrence: u32,
}

/// A loop that encloses a mark: its site and the hidden local that counts
/// the passes it has begun.
#[derive(Clone, Debug)]
pub struct LoopMark {
    pub site: Site,
    pub started: u16,
}

/// A loop's state: hidden locals from `slot`.
#[derive(Clone, Debug)]
pub struct LoopSite {
    /// `slot` holds what the loop walks and `slot + 1` its cursor; a
    /// protocol loop also keeps the size, what remains, the raw item and
    /// whether the step gave one at `slot + 2 ..= slot + 5`.
    pub slot: u16,
    /// The local each pass binds (plain loops).
    pub binding: u16,
    /// Where the loop goes when it is exhausted.
    pub exit: u32,
    /// A protocol loop: its driver's `next` call, or `None`.
    pub driver: Option<u32>,
    /// A protocol loop: where its step continues after `next` or
    /// `check` (the more test).
    pub join: Option<u32>,
    pub site: Site,
}

/// An action other than a call.
#[derive(Clone, Debug)]
pub enum ActionSite {
    Perform {
        /// The library function whose body performs, `None` in document
        /// code: the slot is in that body's imports.
        owner: Option<FunctionId>,
        slot: ImportSlot,
        argc: u8,
        result: Type,
        then_yield: bool,
        site: Site,
    },
    /// `spawn` through stub `unit`, `argc` values (a value callee
    /// included) on the stack.
    Spawn {
        unit: UnitId,
        argc: u8,
        site: Site,
    },
    Join {
        site: Site,
    },
    JoinMany {
        mode: JoinMode,
        site: Site,
    },
    Yield {
        site: Site,
    },
    Cancel {
        site: Site,
    },
    CancelEffect {
        site: Site,
    },
}

impl ActionSite {
    pub fn site(&self) -> &Site {
        match self {
            Self::Perform { site, .. }
            | Self::Spawn { site, .. }
            | Self::Join { site }
            | Self::JoinMany { site, .. }
            | Self::Yield { site }
            | Self::Cancel { site }
            | Self::CancelEffect { site } => site,
        }
    }
}

/// A native a call site names: the implementation, its argument count and
/// the definition's formulas, resolved once at compile.
pub struct NativeEntry {
    pub function: FunctionId,
    pub name: Arc<str>,
    pub params: u8,
    pub native: Arc<dyn NativeFunction>,
    /// The completion formula.
    pub charge: u32,
    /// The guard's limit, if the definition states one (`K-LIB-008`).
    pub guard: Option<u32>,
    /// The definition's kernel body, covered, for a Decline (K-LIB-006).
    pub body: Option<UnitId>,
    /// The scalar operation and domain this native version declares it
    /// answers inline over immediate pairs (R8, `K-LIB-011`): selected
    /// from the declaration, never from the native's identity.
    pub scalar: Option<InlineOperation>,
}

/// A function a function value names: applying it calls `unit`, a
/// trampoline that makes the ordinary charged call.
#[derive(Clone, Debug)]
pub struct FunctionEntry {
    pub function: FunctionId,
    pub unit: UnitId,
}
