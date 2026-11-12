//! A compiled document (cutover contract §1): one [`Program`] per machine
//! instance, never shared across instances, linked to the process's
//! compiled library ([`library`](super::library)). Its own units are the
//! document's code (main, every declared function of every Link the run's
//! build loads, every closure body, trampoline and spawn stub the
//! document's code makes); every library body, native and completion
//! formula it reaches is the library's, named by an id tagged [`LIB`].
//! It holds its own constant pool, charge tables (§3) and side tables.
//! Each own unit is compiled as the Link it belongs to; library code
//! belongs to no Link and runs as the Link of the frame that calls it.
//!
//! The library being compiled is a `Program` too: its own units are the
//! library's new ones, tagged, over the library compiled before them, so
//! one inliner, validator, folder and fuser serve both.
//!
//! A `Program` is built by the compiler and checked once by the validator;
//! nothing at run time re-checks its structure.

use std::sync::Arc;

use lash_kernel_doc::arena::ClosureIdentity;
use lash_kernel_doc::arena::native::{InlineOperation, ScalarDomain};
use lash_kernel_doc::scalar;
use lash_kernel_doc::{Integer, JoinMode, Value};
use lash_kernel_value::content_ids::LinkId;
use lash_kernel_value::names::{CallableId, FunctionId, ImportSlot, Name};
use lash_kernel_value::sites::Site;
use lash_kernel_value::types::Type;

use super::charge_tables::Charge;
use super::op::Op;

/// A unit: an index in [`Program::units`], or tagged [`LIB`], the
/// library's.
pub type UnitId = u32;

pub use super::library::LIB;

/// One compiled document, or the library's code being added.
pub struct Program {
    /// The own units: the document's, or the library's new ones.
    pub units: Vec<Unit>,
    pub constants: Vec<Constant>,
    /// The natives this program declares: the library's new ones, or a
    /// document's own.
    pub natives: Vec<NativeEntry>,
    /// Completion formulas and guard limits, by index.
    pub charges: Vec<Charge>,
    /// `LIB` when the own code is the library's, else 0: the tag its own
    /// unit, constant and charge ids carry.
    pub tag: u32,
    /// The library compiled so far, which the own code links to.
    pub lib: Arc<super::library::LibCode>,
    /// The run's root Link: the one main and the entry belong to.
    pub link: LinkId,
    /// Every Link the program's code runs as, the root first: a frame
    /// runs as one of them, by index.
    pub links: Vec<LinkId>,
    /// What a function value calls, by Link and function: each the root
    /// Link's code names, and every function another loaded Link declares.
    pub functions: Vec<FunctionEntry>,
    /// Every function each loaded Link declares: what a function value
    /// entering the run may name. A value naming any other is refused
    /// where it enters.
    pub declared: std::collections::BTreeSet<CallableId>,
    /// The host bindings main's free names read (`CellStart::host`).
    pub globals: Vec<Name>,
    /// The unit a `Main` start runs.
    pub main: UnitId,
    /// The export entries calls through imports enter: a frame of one is
    /// saved as an entry, apart from its function's plain unit.
    pub entered: std::collections::BTreeSet<UnitId>,
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
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
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
#[derive(Clone)]
pub struct Unit {
    pub kind: UnitKind,
    /// A frame of it is an active call (`K-BND-002`): not `main`'s body, a
    /// spawn's stub or a function value's trampoline, which make no call
    /// of their own or only the one call they forward.
    pub call: bool,
    /// The Link whose code this is: its function values, closures and
    /// operation fills are that Link's.
    pub link: LinkId,
    /// The unit runs inside a covered library extent (`K-CHG-007`): its
    /// event table holds only what charges there.
    pub covered: bool,
    /// Its statements are steps (`K-MACH-002`): guest code, a closure
    /// of it, or a body-only library body, covered or not. A native's
    /// alternative body, entered by Decline, and the other covered units
    /// make none.
    pub steps: bool,
    pub params: u16,
    /// Locals, parameters first.
    pub locals: u16,
    /// The cells each closure of it captures (`K-CLO-001`); none for a
    /// unit no closure runs.
    pub captures: u16,
    /// Locals a closure shares, parameters included: each is boxed into
    /// a cell at entry (`K-CLO-001`).
    pub boxed: Vec<u16>,
    pub ops: Vec<Op>,
    /// Each op's fixed charge under `Charge::Exact`.
    pub event: Vec<Event>,
    /// A statement's start or a loop's back edge: where the slice is
    /// tested before the op when the unit steps (`K-MACH-002`).
    pub step: Vec<bool>,
    /// For each op, the fixed charge its block's ops bill up to and
    /// including it: `rest(o) = cost - through[o]` (contract §3). `None`
    /// past an unrepresentable sum.
    pub through: Vec<u64>,
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
    /// The library function whose body this unit is, or whose body made
    /// it: what a frame of it pins when its build image changes (contract
    /// §7). `None` in the document's own code.
    pub owner: Option<FunctionId>,
    /// The unit's site root.
    pub site: Site,
    /// What an observer sees at an op, by op index, in op order: the
    /// document's statement starts, empty-block entries, `while` tests and
    /// action issues. Read only while the run is observed.
    pub points: Vec<(u32, Point)>,
    /// The fused runs `Op::Fused` names (`fuse`).
    pub fused: Vec<Fused>,
    /// The block heads' traces `Op::Trace` names, and their micro-ops
    /// (`trace`).
    pub traces: Vec<super::trace::Trace>,
    pub micros: Vec<super::trace::Step>,
    pub trace_steps: Vec<super::trace::Step>,
    pub trace_operands: Vec<super::trace::Operands>,
    /// The library bodies expanded in place of their calls (`inline`),
    /// each a region of ops past the unit's own.
    pub inlines: Vec<InlineSite>,
    /// The guard paths' folds the inline sites name (`fold`).
    pub folds: Vec<Fold>,
    /// A constant-key lookup followed by a word helper and a local store.
    pub lookup_words: Vec<LookupWords>,
    /// The expression each op lowers, by its path under `site`. Ops added
    /// after sealing have none, and stand for the unit's root.
    pub origin: Vec<super::site_tables::Path>,
    /// How much of each op's origin is the statement it runs in: where a
    /// bound refused there stands.
    pub stated: Vec<u16>,
    /// Where each slot was declared, and the variable it holds if any.
    pub declared: Vec<(super::site_tables::Path, Option<Name>)>,
    /// Every point a frame of the unit can be saved at (contract §5).
    pub sites: super::site_tables::SiteTable,
    /// The entry's run-root cleanup, when its convention declares one.
    pub cleanup: Option<RootCleanup>,
}

/// Where an entry's unit calls its run-root cleanup (`K-TASK-019`). Ops
/// before `body` are the entry's body; a raise that leaves the root frame
/// from there goes on at `raised` with the raised value pushed, where the
/// cleanup is called and the value raised again.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RootCleanup {
    pub body: u32,
    pub raised: u32,
}

impl Unit {
    /// Whether the slice is tested before the op at `pc` (`K-MACH-002`):
    /// at a step's start, and where a stepped frame's last statement ends
    /// by falling off its body. The resume table saves a point at each.
    pub fn slices(&self, pc: usize) -> bool {
        (self.step[pc] || matches!(self.ops[pc], Op::Fall)) && self.steps
    }

    /// Whether `pc` starts a statement or a loop's pass that a frame can
    /// stand at, stepping or not: a library body's are its points under
    /// every image.
    fn marks(&self, pc: usize) -> bool {
        self.step[pc] || (matches!(self.ops[pc], Op::Fall) && self.marked())
    }

    /// Whether its statements are marked: code that steps, and a library
    /// body that may step under another image.
    pub fn marked(&self) -> bool {
        self.steps || matches!(self.kind, UnitKind::Body(_) | UnitKind::Declared(_))
    }

    /// Whether a frame is saved standing before `pc`'s op: at a step, or
    /// at the unit's head, before it has begun (a fresh run or task).
    pub fn stands(&self, pc: usize) -> bool {
        pc == 0 || self.marks(pc)
    }

    /// The statement op `pc` runs in, where the origin table covers it.
    pub fn statement(&self, pc: usize) -> Option<Site> {
        let origin = self.origin.get(pc)?;
        let stated = usize::from(*self.stated.get(pc)?).min(origin.len());
        Some(Site::new(self.site.unit.clone(), origin[..stated].to_vec()))
    }

    /// The op compiled at `pc`, before inlining or fusion replaced it.
    pub fn original(&self, pc: usize) -> Option<Op> {
        match *self.ops.get(pc)? {
            Op::Fused(n) => Some(self.fused[n as usize].first),
            Op::Trace(n) => Some(Op::Block(self.traces[n as usize].block)),
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
    /// The body makes steps: its region bills nothing (validated), so a
    /// slice falls due in it only where it is due at the region's entry,
    /// and there the body runs as its own frame instead.
    pub steps: bool,
    /// Its only fold is one body-derived guarded element read.
    pub element: bool,
    /// Its guard paths' folds (`fold`): where they start in the unit's
    /// folds and where they end, an empty span when no path folds.
    pub folds: (u32, u32),
    pub lookup_words: Option<u32>,
}

/// Two body-derived folds with their original completion bills and blocks.
#[derive(Clone, Copy)]
pub struct LookupWords {
    pub key: lash_kernel_doc::arena::ShortText,
    pub lookup: u32,
    pub words: u32,
    pub get_callee: UnitId,
    pub word_callee: UnitId,
    pub get_fees: [u64; 2],
    pub word_fees: [u64; 8],
    pub operation: scalar::ScalarOp,
    pub between: u64,
    pub after: u64,
    pub resume: u32,
    pub destination: u16,
}

/// The most steps, and values made, of one fold.
pub const FOLD_STEPS: usize = 8;

/// The most guard paths of one region that fold.
pub const FOLD_PATHS: usize = 4;

/// An inline region's guard path from its entry to one `Op::Leave`, as
/// one guarded op (`fold`): its guards and the value it returns, read
/// with no effect. Where a guard does not hold or a read declines, the
/// path does not hold; otherwise the region's `Op::Leave` at `leave`
/// returns the value.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Fold {
    pub steps: [Step; FOLD_STEPS],
    pub len: u8,
    pub result: Slot,
    pub leave: u32,
    /// The fold's answer where both of its operands are word ints, when
    /// its steps are only their int kind tests, guards that each holds,
    /// and one word-domain scalar answer over them: then the fold holds
    /// exactly when `scalar::ints` answers, with that answer.
    pub words: Option<WordFold>,
}

/// A fold's word-int form (`Fold::words`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct WordFold {
    pub operation: lash_kernel_doc::scalar::ScalarOp,
    pub operands: [Slot; 2],
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
    /// A kind answer compared with a kind's name by a same answer,
    /// making the same answer's value: whether the value is of the kind,
    /// read from its tag (`InlineOperation::kind_is`).
    KindIs {
        value: Slot,
        kind: lash_kernel_doc::ValueKind,
    },
    /// A branch the path takes: the value must be `Bool(holds)`.
    Guard { value: Slot, holds: bool },
    /// An index read that finds a value, making it: a list or tuple at
    /// an integer, or a map at a key it holds.
    Index { target: Slot, index: Slot },
    /// A map's membership answer, the guard that it holds and the index
    /// read of the same map at the same key, as one lookup making the
    /// value: it declines where the membership answer declines or finds
    /// no entry, and otherwise makes what the index read would.
    Lookup { target: Slot, key: Slot },
    /// A list's nonnegative-index and length guards followed by its
    /// element read, with the same decline outside that guarded path.
    Element { target: Slot, index: Slot },
}

/// A run of one prepaid block's ops that dispatches at once (`fuse`): up
/// to two operand pushes, the native call with an inline answer they
/// feed, and what takes its answer. Its ops after the first stay in
/// place, so a frame that stands inside the run continues op by op.
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
    /// The call's entry counter (`CallSite::entry`), held here so that
    /// the word path reads no call site.
    pub entry: Option<u32>,
    /// What takes the call's answer.
    pub then: Then,
    /// The call's answer and fee over two word ints, where the native
    /// answers them in place: `None` sends every pair through the
    /// native's ordinary answer.
    pub words: Option<Words>,
    /// The call's answer and fee over two floats, or a word and a float,
    /// where the native's domain names them: `None` sends them through
    /// the native's ordinary answer.
    pub numbers: Option<Words>,
    /// A body native's number form, checked against its call-depth reach
    /// only after the body-free forms decline.
    pub guarded_numbers: Option<Words>,
}

/// A fused call's scalar form: its operation, the pairs its native's
/// domain names, and its fee by which of the two operands and the answer
/// are of size two (as [`Charge::pair`] indexes it), `None` for an
/// uncharged call.
#[derive(Clone, Copy, Debug)]
pub struct Words {
    pub operation: lash_kernel_doc::scalar::ScalarOp,
    pub domain: ScalarDomain,
    pub fee: Option<[u64; 8]>,
}

impl Words {
    /// The answer and fee over two floats, or a word and a float, of the
    /// domain, as the native's ordinary answer gives them; `None` for any
    /// other pair. A float or a bool answer is of size one. Out of line:
    /// the word path holds its registers.
    #[inline(never)]
    pub fn numbers(&self, args: [Value; 2]) -> Option<(Value, Option<u64>)> {
        self.numbers_of(self.operation, args)
    }

    /// [`Self::numbers`] where the caller knows the form's `operation`.
    #[inline(always)]
    pub fn numbers_of(
        &self,
        operation: scalar::ScalarOp,
        args: [Value; 2],
    ) -> Option<(Value, Option<u64>)> {
        debug_assert_eq!(operation, self.operation);
        let (value, at) = match args {
            [Value::Float(a), Value::Float(b)] if self.domain != ScalarDomain::Words => {
                (scalar::floats(operation, a, b)?, 0)
            }
            [a, b] if self.domain.mixed() => {
                let at = match (a, b) {
                    (Value::Int(a), _) => usize::from(a != 0),
                    (_, Value::Int(b)) => usize::from(b != 0) << 1,
                    _ => 0,
                };
                (scalar::mixed(operation, a, b)?, at)
            }
            _ => return None,
        };
        Some((value, self.fee.as_ref().map(|table| table[at])))
    }
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
    /// An export's entry that a call through an import enters: the
    /// arguments, held to the site's `checked`, made the entry body's as
    /// `adapt` says, as a start makes them (`K-DOC-003`).
    Entry { unit: UnitId, adapt: Adapt },
    /// The value below the arguments: a closure or a function value.
    Value,
    /// A function the machine answers itself; `charge` as for a unit.
    Machine {
        function: crate::functions::MachineFunction,
        /// The definition the call names.
        id: FunctionId,
        charge: Option<u32>,
    },
}

#[derive(Clone, Debug)]
pub struct CallSite {
    pub callee: Callee,
    pub argc: u16,
    pub site: Site,
    /// Where the calling frame stands while the callee runs.
    pub mark: Arc<Mark>,
    /// The contract an authored call of a typed build helper's body
    /// holds its arguments to (`function_argument`), as an export's are.
    pub checked: Option<Arc<Checked>>,
    /// A call action of document code (a call or an await): the
    /// program-wide number of the task's counter of its entries, which the
    /// call op advances before the call reads its arguments, and which a
    /// host that observes sites sees (`K-EFF-008`). An expression's
    /// library call, and any call in library code, is no entry.
    pub entry: Option<u32>,
}

/// A build helper's public contract, checked at an authored call.
#[derive(Debug)]
pub struct Checked {
    pub callable: CallableId,
    pub signature: lash_kernel_value::Signature,
    /// How many leading parameters a call must supply.
    pub required: usize,
    /// What each parameter's type holds a value to by its kind alone
    /// (`Shallow`), in order.
    pub shallow: Box<[Shallow]>,
    /// The names a packed Python export takes its keyword arguments by,
    /// from the export's own signature: packing, apart from the contract
    /// a call holds to (empty for any other callee).
    pub keywords: Box<[lash_kernel_value::Name]>,
}

impl Checked {
    pub fn new(callable: CallableId, signature: lash_kernel_value::Signature) -> Self {
        Self {
            callable,
            required: signature.required(),
            shallow: signature
                .params
                .iter()
                .map(|param| Shallow::of(&param.ty))
                .collect(),
            signature,
            keywords: Box::default(),
        }
    }
}

/// How a call through an import makes its arguments the export body's
/// under the export's start convention, after they fit its signature. A
/// TypeScript export's callers may pass fewer or more than it declares.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Adapt {
    /// `TsDirect`: the body's parameter slots, the arguments past them
    /// dropped and the ones short of them absent.
    Slots(u16),
    /// `TsArgsList`: an absent `this`, and every argument in a list.
    Listed,
    /// `PyArgsList`: the first `positional` arguments in a list, the rest
    /// in a map by their parameters' names, and no active exception.
    Python { positional: u16 },
}

/// What a parameter's type decides of a value by its kind alone. A value
/// it does not decide is checked whole.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Shallow {
    /// An inline scalar the type names fits (`fits::plainly_fits`).
    Plain,
    /// A map, list or set of the kind fits a collection type whose members
    /// are of any type, wherever its members stand within the value
    /// depth: below the depth's last level, every member does.
    Map,
    List,
    Set,
}

impl Shallow {
    fn of(ty: &lash_kernel_value::Type) -> Self {
        use lash_kernel_value::Type;
        match ty {
            Type::Map(map) if matches!((&map.key, &map.value), (Type::Any, Type::Any)) => Self::Map,
            Type::List(item) if matches!(**item, Type::Any) => Self::List,
            Type::Set(item) if matches!(**item, Type::Any) => Self::Set,
            _ => Self::Plain,
        }
    }
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
        argc: u16,
        result: Type,
        then_yield: bool,
        site: Site,
    },
    /// `spawn` through stub `unit`, `argc` values (a value callee
    /// included) on the stack.
    Spawn {
        unit: UnitId,
        argc: u16,
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

/// A native a call site names: its argument count and the definition's
/// formulas, resolved once at compile. The implementation is the
/// instance's registry's ([`Program::native`]).
#[derive(Clone)]
pub struct NativeEntry {
    pub function: FunctionId,
    pub name: Arc<str>,
    pub params: u16,
    /// The completion formula.
    pub charge: u32,
    /// The guard's limit, if the definition states one (`K-LIB-008`).
    pub guard: Option<u32>,
    /// The definition's kernel body, covered, for a Decline (K-LIB-006).
    pub body: Option<UnitId>,
    /// The call depth the native's answer stands for when its plan's
    /// template states one: its body with the helper frames the answer
    /// elides, its own included (`HelperTemplate::depth`). Short of that
    /// headroom the body runs, and refuses where it does. 0 with no
    /// template: an operation of one unit (`K-BND-002`).
    pub elided: u32,
    /// The scalar operation and domain this native version declares it
    /// answers inline over immediate pairs (R8, `K-LIB-011`): selected
    /// from the declaration, never from the native's identity.
    pub scalar: Option<InlineOperation>,
}

impl Program {
    /// Where own id `id` stands in the own tables whose library part
    /// holds `before` entries, `None` for the library's.
    #[inline(always)]
    fn own(&self, id: u32, before: usize) -> Option<usize> {
        let at = (id & !LIB) as usize;
        let base = if self.tag == LIB { before } else { 0 };
        (id & LIB == self.tag && at >= base).then(|| at - base)
    }

    /// The id own unit `at` has.
    #[inline]
    pub fn own_unit(&self, at: usize) -> UnitId {
        let base = if self.tag == LIB {
            self.lib.units.len()
        } else {
            0
        };
        self.tag | (base + at) as UnitId
    }

    /// Own unit `id`'s index in [`Self::units`], `None` for the library's.
    #[inline]
    pub fn own_index(&self, id: UnitId) -> Option<usize> {
        self.own(id, self.lib.units.len())
    }

    /// Unit `id`, the program's own or the library's.
    #[inline(always)]
    pub fn unit(&self, id: UnitId) -> &Unit {
        match self.own(id, self.lib.units.len()) {
            Some(at) => &self.units[at],
            None => &self.lib.units[(id & !LIB) as usize],
        }
    }

    /// Unit `id`, if the program or the library has it.
    pub fn get_unit(&self, id: UnitId) -> Option<&Unit> {
        match self.own(id, self.lib.units.len()) {
            Some(at) => self.units.get(at),
            None if id & LIB != 0 => self.lib.units.get((id & !LIB) as usize).map(|unit| &**unit),
            None => None,
        }
    }

    /// Constant `id`, the program's own or the library's.
    #[inline(always)]
    pub fn constant(&self, id: u32) -> &Constant {
        match self.own(id, self.lib.constants.len()) {
            Some(at) => &self.constants[at],
            None => &self.lib.constants[(id & !LIB) as usize],
        }
    }

    /// Whether constant `id` reads without placing: an own constant of a
    /// document's program, placed when the run starts, or an immediate.
    /// A library constant with an identity is placed at its first read
    /// by an op (contract §2), so no fused run, fold or trace holds it.
    pub(crate) fn readable(&self, id: u32) -> bool {
        (self.tag == 0 && id & LIB == 0)
            || matches!(self.get_constant(id), Some(Constant::Immediate(_)))
    }

    /// Constant `id`, if the program or the library has it.
    pub fn get_constant(&self, id: u32) -> Option<&Constant> {
        match self.own(id, self.lib.constants.len()) {
            Some(at) => self.constants.get(at),
            None if id & LIB != 0 => self.lib.constants.get((id & !LIB) as usize),
            None => None,
        }
    }

    /// Completion formula or guard limit `id`.
    #[inline(always)]
    pub fn charge(&self, id: u32) -> &Charge {
        match self.own(id, self.lib.charges.len()) {
            Some(at) => &self.charges[at],
            None => &self.lib.charges[(id & !LIB) as usize],
        }
    }

    /// Completion formula or guard limit `id`, if there is one.
    pub fn get_charge(&self, id: u32) -> Option<&Charge> {
        match self.own(id, self.lib.charges.len()) {
            Some(at) => self.charges.get(at),
            None if id & LIB != 0 => self.lib.charges.get((id & !LIB) as usize),
            None => None,
        }
    }

    /// Native `n`, the program's own or the library's.
    #[inline(always)]
    pub fn native(&self, n: u32) -> &NativeEntry {
        match self.own(n, self.lib.natives.len()) {
            Some(at) => &self.natives[at],
            None => &self.lib.natives[(n & !LIB) as usize],
        }
    }

    /// Native `n`, if the program or the library has it.
    pub fn get_native(&self, n: u32) -> Option<&NativeEntry> {
        match self.own(n, self.lib.natives.len()) {
            Some(at) => self.natives.get(at),
            None if n & LIB != 0 => self.lib.natives.get((n & !LIB) as usize),
            None => None,
        }
    }

    /// The key of the occurrence counter `occurrence` a frame running as
    /// Link `link` advances: a library counter is each Link's own.
    #[inline]
    pub fn counter(link: u32, occurrence: u32) -> u64 {
        if occurrence & LIB == 0 {
            u64::from(occurrence)
        } else {
            u64::from(link) << 32 | u64::from(occurrence)
        }
    }

    /// The charge that is `function`'s completion formula.
    pub fn charge_of(&self, function: &FunctionId) -> Option<u32> {
        if let Some(at) = self
            .charges
            .iter()
            .position(|charge| charge.function.as_ref() == Some(function))
        {
            let base = if self.tag == LIB {
                self.lib.charges.len()
            } else {
                0
            };
            return Some(self.tag | (base + at) as u32);
        }
        self.lib.charge_ids.get(function).copied()
    }

    /// The index of Link `link` in [`Self::links`].
    #[inline]
    pub fn link_index(&self, link: &LinkId) -> Option<u32> {
        self.links
            .iter()
            .position(|held| held == link)
            .map(|at| at as u32)
    }

    /// Whether a function value naming `callable` can enter a run of the
    /// program: its Link is loaded and declares it.
    pub fn callable(&self, callable: &CallableId) -> bool {
        self.declared.contains(callable)
    }

    /// Whether some loaded Link declares `function`: otherwise it is a
    /// library function, which a build image supplies.
    pub fn declares(&self, function: &FunctionId) -> bool {
        self.declared
            .iter()
            .any(|callable| callable.function == *function)
    }
}

/// A function a function value names: applying it calls `unit`, a
/// trampoline that makes the ordinary charged call.
#[derive(Clone, Debug)]
pub struct FunctionEntry {
    pub callable: CallableId,
    pub unit: UnitId,
}
