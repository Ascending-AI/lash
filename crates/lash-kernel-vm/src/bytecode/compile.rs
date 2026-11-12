//! The compiler's driver (cutover contract §1): one admitted document and
//! the library it reaches become one [`Program`].
//!
//! Units are compiled on first reach: main, then every function a call
//! names, every closure body where its expression stands, every function
//! value's trampoline and every spawn's stub. A unit index is reserved
//! before its body compiles, so recursion resolves. Then every function
//! each other loaded Link declares gets its trampoline, compiled as that
//! Link: a function value names a Link and a function, and calls that
//! Link's body or nothing. A library body, native, completion formula or
//! trampoline is the process's compiled library's
//! ([`library`](super::library)): the same compiler, in library mode,
//! compiles it there once for every run that reaches it, and the document
//! names it by its library id. [`lower`](super::lower)
//! emits each unit's statements; [`Compiler::seal`] cuts its blocks and
//! derives the charge and step tables.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::Arc;

use lash_kernel_doc::arena::ClosureIdentity;
use lash_kernel_doc::arena::native::InlineOperation;
use lash_kernel_doc::linked_site::LinkedSite;
use lash_kernel_doc::{
    BuildHelpers, Document, FunctionDefinition, FunctionRegistry, Literal, Stmt, Value,
};
use lash_kernel_value::content_ids::LinkId;
use lash_kernel_value::names::{CallableId, FunctionId, ImportSlot, Name};
use lash_kernel_value::sites::{Site, Unit as SiteUnit};

use super::charge_tables::{self, Charge};
use super::library::{LIB, LibCode, SharedLibrary};
use super::names::Names;
use super::op::Op;
use super::program::{
    ActionSite, Block, CallSite, Callee, Capture, Checked, ClosureSite, Constant, Event,
    FunctionEntry, LoopMark, LoopSite, Mark, NativeEntry, Point, Program, Unit, UnitId, UnitKind,
};
use super::site_tables::Path;
use crate::functions::{MachineFunction, machine_function};

/// A document the core does not compile. Admission has checked the
/// document, so this names a form the core has no lowering for yet.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("the bytecode core cannot compile this document: {0}")]
pub struct Unsupported(pub String);

pub(crate) type Result<T> = std::result::Result<T, Unsupported>;

pub(crate) fn unsupported<T>(what: impl Into<String>) -> Result<T> {
    Err(Unsupported(what.into()))
}

/// `count` as the core holds it: the bytecode core holds at most
/// 65,535 of the arguments of one call or spawn and of the parameters,
/// locals and temporaries of one function, and refuses a document past
/// that as one it cannot compile, never wrapping the count.
pub(crate) fn within(count: usize, what: &str) -> Result<u16> {
    match u16::try_from(count) {
        Ok(count) => Ok(count),
        Err(_) => unsupported(format!(
            "{count} {what}, more than the bytecode core's limit of {}",
            u16::MAX
        )),
    }
}

/// Compiles `document`, whose library functions `registry` holds, as
/// instance `link`: its `main`, or the declared function `entry`, is the
/// unit a start runs. `linked` holds the other Links the run's build loads
/// and what their function imports resolve to.
///
/// An open under another build image passes that image as `available`: a
/// build helper it lacks compiles to a unit that refuses when it runs
/// (contract §7), and open proves no resume point reaches one. `library`
/// is the compiled library of `registry` under that image.
pub(crate) fn compile(
    document: &Document,
    registry: &FunctionRegistry,
    link: LinkId,
    linked: &Linked<'_>,
    entry: Option<&Entry<'_>>,
    available: Option<&BuildHelpers>,
    library: &SharedLibrary,
) -> Result<Program> {
    let mut compiler = Compiler::new(document, registry, link, &linked.imports);
    compiler.available = available;
    compiler.library = Some(library);
    compiler.helpers = linked.helpers;
    compiler.links.extend(
        linked
            .others
            .iter()
            .map(|(other, document)| (*other, *document)),
    );
    let main = match entry {
        Some(entry) => super::conventions::entry(&mut compiler, entry)?,
        None => compiler.main()?,
    };
    // Every Link's declared functions get a trampoline, the root's too: a
    // function value of the root Link may enter as an argument, a host
    // binding or a delivery, or be held across a cold reopen, without its
    // code naming it.
    compiler.declare_all(true);
    for (other, document) in &linked.others {
        if *other != link {
            compiler.links.insert(*other, document);
            compiler.enter_link(*other)?;
            compiler.declare_all(true);
        }
    }
    for identity in &linked.closures {
        compiler.saved_closure(identity)?;
    }
    for callable in &linked.functions {
        compiler.enter_link(callable.link)?;
        let from = compiler.units.len();
        if let Err(error) = compiler.trampoline(callable.function) {
            compiler.refuse_from(from, &error);
        }
    }
    compiler.finish(main)
}

/// How a call through an import of a `PyArgsList` export names the
/// arguments it packs ([`Compiler::keywords`]).
pub(crate) struct Packing {
    /// How many leading arguments it passes in its list.
    pub positional: usize,
    /// The export's parameters' names, by which it passes the rest.
    pub names: Vec<Name>,
}

/// What a start runs instead of `main`: a declared or library function,
/// or a callable's, under the start's convention.
pub(crate) struct Entry<'a> {
    pub function: FunctionId,
    pub convention: &'a lash_kernel_doc::StartConvention,
    /// A callable's frozen captures, by name, in its closure's order.
    pub captures: Option<&'a [Name]>,
}

/// The function each Link's function import slot resolves to.
pub type Imports =
    BTreeMap<(LinkId, ImportSlot), (CallableId, lash_kernel_value::names::ExportName)>;

/// The Links a run's build loads besides the one compiled, each with its
/// own document, and what every Link's function imports resolve to.
#[derive(Default)]
pub struct Linked<'a> {
    pub others: Vec<(LinkId, &'a Document)>,
    pub imports: Imports,
    /// The closures of other Links the run holds, and those a callable
    /// start's frozen captures hold, one for each callable: a saved
    /// function's code, compiled in its own Link with the capture shape
    /// its save froze.
    pub closures: Vec<ClosureIdentity>,
    /// The function values a callable start's frozen captures hold: each
    /// needs its Link's trampoline.
    pub functions: Vec<CallableId>,
    /// The program's build helpers.
    pub helpers: Option<&'a BuildHelpers>,
}

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
    pending: Event,
    /// The next op starts a step (a statement's start).
    pub mark: bool,
    /// What an observer sees before the next op: a statement start.
    pub start: Option<Point>,
    pub points: Vec<(u32, Point)>,
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
            self.points.push((at, point));
        }
    }

    /// `n` one-unit events of uncovered code (statement, expression node,
    /// atom, loop test), billed by the next op.
    pub(crate) fn units(&mut self, n: u64) {
        if !self.covered {
            self.pending.units = self.pending.units.saturating_add(n);
        }
    }

    /// One-unit events that charge in covered code too: the atoms of a
    /// `perform` or `spawn` (`K-CHG-007`).
    pub(crate) fn always(&mut self, n: u64) {
        self.pending.units = self.pending.units.saturating_add(n);
    }

    /// A `metered N` loop test's units: one declared event of `n`, billed
    /// whole and in covered code too (`K-CHG-007`).
    pub(crate) fn metered(&mut self, n: u64) {
        self.pending.atomic = self.pending.atomic.saturating_add(n);
    }

    /// A construction's members, one atomic event on the next op.
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
            self.points.push((self.here(), point));
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
    pub(crate) fn label(&mut self) -> u32 {
        self.leader();
        self.here() - 1
    }

    pub(crate) fn here(&self) -> u32 {
        self.ops.len() as u32
    }

    /// Declares `name` in the innermost scope (`K-FORM-003`): a new
    /// variable in a slot of its own, shadowing any outer one until the
    /// scope ends.
    pub(crate) fn declare(&mut self, name: &Name) -> u16 {
        let slot = self.temp();
        self.slots.insert(name.clone(), slot);
        self.declared[usize::from(slot)].1 = Some(name.clone());
        slot
    }

    /// A hidden local.
    /// Past the core's limit the slot saturates, and the unit is refused
    /// when it is sealed ([`Compiler::seal`]), never wrapped.
    pub(crate) fn temp(&mut self) -> u16 {
        let slot = self.locals;
        self.locals = self.locals.saturating_add(1);
        self.declared.push((Path::from(self.path.as_slice()), None));
        slot
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

/// What the compiler has made for one Link: a function id, a function
/// literal or a helper's perform means that Link's own, so each Link's
/// code is compiled, and found again, apart.
#[derive(Default)]
pub(super) struct LinkCode {
    immediates: HashMap<Literal, u32>,
    native_ids: HashMap<FunctionId, u32>,
    charge_ids: HashMap<FunctionId, u32>,
    bodies: HashMap<(FunctionId, bool), UnitId>,
    /// By the spawn's site, which roots the stub: a saved task names its
    /// stub by that site.
    stubs: HashMap<(Site, Option<CallableId>, u16), UnitId>,
    free: HashMap<FunctionId, BTreeSet<Name>>,
    /// The unit each unavailable helper compiled to.
    missing: HashMap<FunctionId, UnitId>,
    /// The entry unit a call through an import enters, by the export of
    /// the Link it enters: one per export, as a saved frame names it.
    pub(super) entries: HashMap<lash_kernel_value::names::ExportName, UnitId>,
}

/// The Link library code is compiled in: no loaded Link's.
const NO_LINK: [u8; 32] = [0; 32];

/// What the library compiler keeps between requests: what it has made,
/// found again by what made it, and its code since the library last
/// published.
#[derive(Default)]
pub(crate) struct Tables {
    scope: LinkCode,
    functions: BTreeMap<CallableId, UnitId>,
    occurrences: HashMap<(LinkId, Site), u32>,
    units: Vec<Option<Unit>>,
    constants: Vec<Constant>,
    natives: Vec<NativeEntry>,
    charges: Vec<Charge>,
    /// Each body and trampoline the library cannot compile, and why.
    failed: HashMap<(FunctionId, bool), Unsupported>,
    failed_functions: HashMap<FunctionId, Unsupported>,
    fresh_sites: Vec<(Site, u32)>,
    fresh_trampolines: Vec<(FunctionId, UnitId)>,
}

/// The library code compiled since the library last published.
pub(crate) struct Fresh {
    pub units: Vec<Unit>,
    pub constants: Vec<Constant>,
    pub natives: Vec<NativeEntry>,
    pub charges: Vec<Charge>,
    pub occurrences: Vec<(Site, u32)>,
    pub trampolines: Vec<(FunctionId, UnitId)>,
}

impl Tables {
    /// Whether everything compiled is published.
    pub(crate) fn is_published(&self) -> bool {
        self.units.is_empty()
            && self.constants.is_empty()
            && self.natives.is_empty()
            && self.charges.is_empty()
            && self.fresh_sites.is_empty()
            && self.fresh_trampolines.is_empty()
    }

    /// What was compiled since the last publication, taken to publish.
    pub(crate) fn take_fresh(&mut self) -> Result<Fresh> {
        Ok(Fresh {
            units: std::mem::take(&mut self.units)
                .into_iter()
                .map(|unit| {
                    unit.ok_or_else(|| {
                        Unsupported("a library unit was reserved and never compiled".into())
                    })
                })
                .collect::<Result<_>>()?,
            constants: std::mem::take(&mut self.constants),
            natives: std::mem::take(&mut self.natives),
            charges: std::mem::take(&mut self.charges),
            occurrences: std::mem::take(&mut self.fresh_sites),
            trampolines: std::mem::take(&mut self.fresh_trampolines),
        })
    }
}

/// Where the code a compiler makes is numbered from: past what the
/// library has published, in library mode; from 0 in a document.
#[derive(Clone, Copy, Default)]
struct Base {
    units: usize,
    constants: usize,
    natives: usize,
    charges: usize,
}

/// The document library code is compiled as: none.
static NO_DOCUMENT: std::sync::LazyLock<Document> =
    std::sync::LazyLock::new(|| Document::new(lash_kernel_doc::NumberPolicy::Float, Vec::new()));

/// The imports library code resolves: none.
static NO_IMPORTS: Imports = BTreeMap::new();

/// The compiler's program-wide state.
pub(crate) struct Compiler<'a> {
    /// The document of the Link being compiled.
    pub document: &'a Document,
    pub registry: &'a FunctionRegistry,
    /// The Link being compiled.
    pub link: LinkId,
    /// The run's root Link: main's and the entry's.
    root: LinkId,
    pub(super) scope: LinkCode,
    /// Every loaded Link's document.
    links: HashMap<LinkId, &'a Document>,
    /// What the compiler made for each Link other than the one it is in.
    parked: HashMap<LinkId, LinkCode>,
    pub(super) imports: &'a Imports,
    pub units: Vec<Option<Unit>>,
    pub constants: Vec<Constant>,
    pub natives: Vec<NativeEntry>,
    pub charges: Vec<Charge>,
    pub functions: BTreeMap<CallableId, UnitId>,
    /// The functions each compiled Link declares.
    declares: BTreeSet<CallableId>,
    /// The program's build helpers: an authored call of one's typed body
    /// checks its arguments.
    helpers: Option<&'a BuildHelpers>,
    pub globals: Vec<Name>,
    global_ids: HashMap<Name, u16>,
    /// The export entries calls through imports enter
    /// ([`Program::entered`]).
    pub(super) entered: BTreeMap<UnitId, lash_kernel_value::names::ExportName>,
    /// Each action site's occurrence counter index (`K-EFF-008`).
    occurrences: HashMap<(LinkId, Site), u32>,
    /// The build image an open compiles under, when it is not the one the
    /// run sealed under: the helpers it lacks are unavailable.
    available: Option<&'a BuildHelpers>,
    /// Compiling a document: the library its library code is in. `None`
    /// while compiling the library.
    library: Option<&'a SharedLibrary>,
    /// [`LIB`] while compiling the library, else 0: the tag of the ids of
    /// the units, constants, charges and occurrences made.
    tag: u32,
    base: Base,
    failed: HashMap<(FunctionId, bool), Unsupported>,
    failed_functions: HashMap<FunctionId, Unsupported>,
    fresh_sites: Vec<(Site, u32)>,
    fresh_trampolines: Vec<(FunctionId, UnitId)>,
}

impl<'a> Compiler<'a> {
    fn new(
        document: &'a Document,
        registry: &'a FunctionRegistry,
        link: LinkId,
        imports: &'a Imports,
    ) -> Self {
        Self {
            document,
            registry,
            link,
            root: link,
            scope: LinkCode::default(),
            links: HashMap::from([(link, document)]),
            parked: HashMap::new(),
            imports,
            units: Vec::new(),
            constants: Vec::new(),
            natives: Vec::new(),
            charges: Vec::new(),
            functions: BTreeMap::new(),
            declares: BTreeSet::new(),
            helpers: None,
            globals: Vec::new(),
            entered: BTreeMap::new(),
            global_ids: HashMap::new(),
            occurrences: HashMap::new(),
            available: None,
            library: None,
            tag: 0,
            base: Base::default(),
            failed: HashMap::new(),
            failed_functions: HashMap::new(),
            fresh_sites: Vec::new(),
            fresh_trampolines: Vec::new(),
        }
    }

    /// The library's compiler under build image `available`, over `held`,
    /// the code it has published, and `tables`, what it has made.
    pub(crate) fn library(
        registry: &'a FunctionRegistry,
        available: Option<&'a BuildHelpers>,
        held: &LibCode,
        tables: Tables,
    ) -> Self {
        let mut compiler = Self::new(
            &NO_DOCUMENT,
            registry,
            LinkId::from_bytes(NO_LINK),
            &NO_IMPORTS,
        );
        compiler.available = available;
        compiler.tag = LIB;
        compiler.base = Base {
            units: held.units.len(),
            constants: held.constants.len(),
            natives: held.natives.len(),
            charges: held.charges.len(),
        };
        let Tables {
            scope,
            functions,
            occurrences,
            units,
            constants,
            natives,
            charges,
            failed,
            failed_functions,
            fresh_sites,
            fresh_trampolines,
        } = tables;
        compiler.scope = scope;
        compiler.functions = functions;
        compiler.occurrences = occurrences;
        compiler.units = units;
        compiler.constants = constants;
        compiler.natives = natives;
        compiler.charges = charges;
        compiler.failed = failed;
        compiler.failed_functions = failed_functions;
        compiler.fresh_sites = fresh_sites;
        compiler.fresh_trampolines = fresh_trampolines;
        compiler
    }

    /// What the library's compiler keeps for its next request. A unit a
    /// failed compile reserved and left refuses when it runs.
    pub(crate) fn into_tables(mut self) -> Tables {
        self.refuse_from(
            0,
            &Unsupported("the library left this unit uncompiled".into()),
        );
        Tables {
            scope: self.scope,
            functions: self.functions,
            occurrences: self.occurrences,
            units: self.units,
            constants: self.constants,
            natives: self.natives,
            charges: self.charges,
            failed: self.failed,
            failed_functions: self.failed_functions,
            fresh_sites: self.fresh_sites,
            fresh_trampolines: self.fresh_trampolines,
        }
    }

    /// Whether the compiler is compiling the library.
    fn in_library(&self) -> bool {
        self.tag == LIB
    }

    /// The library the document's library code is in, while compiling a
    /// document's code that names `id` as library code.
    fn library_of(&self, id: &FunctionId) -> Option<&'a SharedLibrary> {
        self.library
            .filter(|_| !self.document.functions.contains_key(id))
    }

    /// Where unit `at` is in [`Self::units`].
    fn local(&self, at: UnitId) -> usize {
        (at & !LIB) as usize - self.base.units
    }

    /// Stores `unit` at `at`.
    pub(crate) fn put(&mut self, at: UnitId, unit: Unit) {
        let local = self.local(at);
        self.units[local] = Some(unit);
    }

    /// Takes unit `at` out to change it.
    pub(crate) fn take(&mut self, at: UnitId) -> Option<Unit> {
        let local = self.local(at);
        self.units[local].take()
    }

    /// Compiles Link `link` from here on, finding again what was already
    /// made for it.
    pub(super) fn enter_link(&mut self, link: LinkId) -> Result<()> {
        if link == self.link {
            return Ok(());
        }
        let Some(document) = self.links.get(&link).copied() else {
            return unsupported(format!("Link {link:?} is not loaded"));
        };
        let scope = self.parked.remove(&link).unwrap_or_default();
        let left = std::mem::replace(&mut self.scope, scope);
        self.parked.insert(self.link, left);
        self.document = document;
        self.link = link;
        Ok(())
    }

    /// The function import `slot` of the Link being compiled resolves to.
    pub(crate) fn imported(&self, slot: ImportSlot) -> Result<CallableId> {
        match self.imports.get(&(self.link, slot)) {
            Some((callable, _)) => Ok(*callable),
            None => unsupported(format!("import {slot:?} names no function")),
        }
    }

    /// The contract a call through import `slot`, resolved to `callable`,
    /// holds to: the import's declared signature, the export's public
    /// one, which the arguments fit before the callee's own ABI adapts
    /// them and its result fits as it returns (FIG-7143), when it types
    /// something.
    pub(crate) fn import_checked(
        &self,
        slot: ImportSlot,
        callable: CallableId,
    ) -> Option<Arc<Checked>> {
        let signature = self.import_signature(slot)?;
        let typed = signature.result != lash_kernel_value::Type::Any
            || signature
                .params
                .iter()
                .any(|param| param.ty != lash_kernel_value::Type::Any);
        typed.then(|| Arc::new(Checked::new(callable, signature.clone())))
    }

    /// The public signature import `slot` of the Link being compiled
    /// declares: the contract a call through it holds to (FIG-7143).
    pub(crate) fn import_signature(
        &self,
        slot: ImportSlot,
    ) -> Option<&lash_kernel_value::Signature> {
        match &self.document.imports.get(&slot)?.kind {
            lash_kernel_value::names::ImportKind::Code { signature, .. } => Some(signature),
            _ => None,
        }
    }

    /// The callee `callable` is from `f`: its own Link's code.
    pub(crate) fn callee_at(&mut self, f: &Fun, callable: CallableId) -> Result<Callee> {
        let home = self.link;
        self.enter_link(callable.link)?;
        let callee = self.callee(f, callable.function);
        self.enter_link(home)?;
        callee
    }

    /// Records the functions the Link being compiled declares and, when
    /// `compile`, gives each its trampoline. One the core cannot compile
    /// is a unit that refuses when it runs (contract §7), as is every
    /// unit its attempt reserved.
    fn declare_all(&mut self, compile: bool) {
        let document = self.document;
        for id in document.functions.keys() {
            self.declares.insert(CallableId {
                link: self.link,
                function: *id,
            });
            if !compile {
                continue;
            }
            // A body that reads names of a scope around it is a closure's:
            // its values are closures made where those names are bound.
            if self.free_names(*id).is_ok_and(|free| !free.is_empty()) {
                continue;
            }
            let from = self.units.len();
            if let Err(error) = self.trampoline(*id) {
                self.refuse_from(from, &error);
            }
        }
    }

    /// Every unit reserved from `from` and left uncompiled refuses with
    /// `error` when it runs.
    fn refuse_from(&mut self, from: usize, error: &Unsupported) {
        for at in from..self.units.len() {
            if self.units[at].is_none() {
                self.units[at] = Some(self.unavailable(&error.0));
            }
        }
    }

    fn main(&mut self) -> Result<UnitId> {
        let at = self.reserve();
        let empty = Vec::new();
        let block = self.document.main.as_ref().unwrap_or(&empty);
        let mut f = Fun::new(SiteUnit::Main, true, false, None);
        self.declare(&mut f, &[], block)?;
        f.leader();
        self.block(&mut f, block)?;
        self.fall_off(&mut f);
        self.seal(f, at, UnitKind::Main, 0, "main".into())?;
        Ok(at)
    }

    fn finish(self, main: UnitId) -> Result<Program> {
        let Some(library) = self.library else {
            return unsupported("a document compiled with no library");
        };
        let mut units = Vec::with_capacity(self.units.len());
        for (at, unit) in self.units.into_iter().enumerate() {
            match unit {
                // A unit's tables are built from its final code: a
                // convention may add to a unit after it was sealed.
                Some(mut unit) => {
                    unit.sites = super::site_tables::build(&unit);
                    units.push(unit);
                }
                None => return unsupported(format!("unit {at} was reserved and never compiled")),
            }
        }
        let mut others: Vec<LinkId> = self
            .links
            .keys()
            .filter(|link| **link != self.root)
            .copied()
            .collect();
        others.sort_unstable_by_key(|link| *link.as_bytes());
        let mut links = vec![self.root];
        links.extend(others);
        Ok(Program {
            units,
            constants: self.constants,
            natives: self.natives,
            charges: self.charges,
            tag: 0,
            lib: library.code()?,
            links,
            functions: self
                .functions
                .into_iter()
                .map(|(callable, unit)| FunctionEntry { callable, unit })
                .collect(),
            declared: self.declares,
            link: self.root,
            globals: self.globals,
            main,
            entered: self.entered,
        })
    }

    pub(crate) fn reserve(&mut self) -> UnitId {
        self.units.push(None);
        self.tag | (self.base.units + self.units.len() - 1) as UnitId
    }

    /// A function's definition: the document's own, else the library's.
    pub(crate) fn def(&self, id: &FunctionId) -> Result<&'a FunctionDefinition> {
        let document = self.document;
        let registry = self.registry;
        match document.functions.get(id) {
            Some(definition) => Ok(definition),
            None => match registry.get(id) {
                Some(registered) => Ok(&**registered.definition()),
                None => unsupported(format!("function {id} is neither declared nor registered")),
            },
        }
    }

    // --- Constants.

    pub(crate) fn constant(&mut self, literal: &Literal) -> u32 {
        if let Some(at) = self.scope.immediates.get(literal) {
            return *at;
        }
        let constant = match literal {
            Literal::Null => Constant::Immediate(Value::Null),
            Literal::Absent => Constant::Immediate(Value::Absent),
            Literal::Bool(b) => Constant::Immediate(Value::Bool(*b)),
            Literal::Int(int) => match int.word() {
                Some(word) => Constant::Immediate(Value::Int(word)),
                None => Constant::BigInt(int.clone()),
            },
            Literal::Float(float) => Constant::Immediate(Value::Float(*float)),
            Literal::Text(text) => match lash_kernel_doc::arena::ShortText::new(text) {
                Some(short) => Constant::Immediate(Value::Text(short)),
                None => Constant::Text(Arc::from(text.as_str())),
            },
            Literal::Bytes(bytes) => Constant::Bytes(Arc::from(bytes.as_slice())),
            Literal::Function(function) => Constant::Function(CallableId {
                link: self.link,
                function: *function,
            }),
        };
        let at = self.tag | (self.base.constants + self.constants.len()) as u32;
        self.constants.push(constant);
        self.scope.immediates.insert(literal.clone(), at);
        at
    }

    /// A text constant: a field name.
    pub(crate) fn text(&mut self, text: &str) -> u32 {
        self.constant(&Literal::Text(text.to_owned()))
    }

    pub(crate) fn global(&mut self, name: &Name) -> u16 {
        if let Some(at) = self.global_ids.get(name) {
            return *at;
        }
        // Past the core's limit the id saturates, and the unit naming it
        // is refused when it is sealed ([`Compiler::seal`]).
        let at = u16::try_from(self.globals.len()).unwrap_or(u16::MAX);
        self.globals.push(name.clone());
        self.global_ids.insert(name.clone(), at);
        at
    }

    // --- Units.

    /// Declares a unit's parameters, and marks the names it declares that
    /// a closure it makes shares: their variables live in cells.
    fn declare(&mut self, f: &mut Fun, params: &[Name], block: &[Stmt]) -> Result<()> {
        for param in params {
            f.declare(param);
        }
        let (declared, closures) = {
            let mut closure_free = |closure| self.free_names(closure);
            let mut found = Names::new(params.iter().cloned(), &mut closure_free);
            found.block(block)?;
            (found.declared, found.closures)
        };
        let mut shared = BTreeSet::new();
        for closure in closures {
            shared.extend(self.free_names(closure)?);
        }
        f.boxed = params
            .iter()
            .chain(&declared)
            .filter(|name| shared.contains(*name))
            .cloned()
            .collect();
        Ok(())
    }

    /// The names a function body reads or writes and does not bind,
    /// through the closures it makes.
    pub(crate) fn free_names(&mut self, id: FunctionId) -> Result<BTreeSet<Name>> {
        if let Some(free) = self.scope.free.get(&id) {
            return Ok(free.clone());
        }
        // A closure that names itself recurses no further.
        self.scope.free.insert(id, BTreeSet::new());
        let definition = self.def(&id)?;
        let Some(body) = definition.body() else {
            return unsupported(format!("closure {id} has no body"));
        };
        let params: Vec<Name> = definition
            .signature
            .params
            .iter()
            .map(|param| param.name.clone())
            .collect();
        let free = {
            let mut closure_free = |closure| self.free_names(closure);
            let mut found = Names::new(params, &mut closure_free);
            found.block(&body.block)?;
            found.free
        };
        self.scope.free.insert(id, free.clone());
        Ok(free)
    }

    /// Compiles function `id`'s body as a unit. `outer` is the unit a
    /// closure is made in, whose variables it captures.
    pub(crate) fn function(
        &mut self,
        id: FunctionId,
        outer: Option<&Fun>,
        saved: Option<&[Name]>,
        at: UnitId,
        covered: bool,
        kind: UnitKind,
    ) -> Result<(Vec<Capture>, Vec<Name>)> {
        let definition = self.def(&id)?;
        let Some(body) = definition.body() else {
            return unsupported(format!("function {id} has no kernel body"));
        };
        // A free name is a host binding of the root Link's main: another
        // Link's function must not read the root's.
        if outer.is_none()
            && saved.is_none()
            && self.link != self.root
            && !self.free_names(id)?.is_empty()
        {
            return unsupported(format!(
                "function {id} of another Link reads names its own Link binds"
            ));
        }
        let owner = match outer {
            Some(outer) => outer.owner,
            None if self.document.functions.contains_key(&id) => None,
            None => Some(id),
        };
        let mut f = Fun::new(SiteUnit::Library(id), false, covered, owner);
        let params: Vec<Name> = definition
            .signature
            .params
            .iter()
            .map(|param| param.name.clone())
            .collect();
        let mut captures = Vec::new();
        if let Some(outer) = outer {
            for name in self.free_names(id)? {
                let source = match (outer.slots.get(&name), outer.captures.get(&name)) {
                    (Some(slot), _) if outer.boxed.contains(&name) => Capture::Local(*slot),
                    (None, Some(capture)) => Capture::Capture(*capture),
                    // A host binding of main, read where it is.
                    _ => continue,
                };
                let at = within(f.capture_names.len() + 1, "captures of one function")? - 1;
                f.captures.insert(name.clone(), at);
                f.capture_names.push(name);
                captures.push(source);
            }
        }
        // A saved closure's captures are the ones its save froze, in order.
        for name in saved.into_iter().flatten() {
            let at = within(f.capture_names.len() + 1, "captures of one function")? - 1;
            f.captures.insert(name.clone(), at);
            f.capture_names.push(name.clone());
        }
        self.declare(&mut f, &params, &body.block)?;
        f.leader();
        self.block(&mut f, &body.block)?;
        self.fall_off(&mut f);
        let name: Arc<str> = Arc::from(definition.name.as_str());
        let params = within(params.len(), "parameters of one function")?;
        let capture_names = f.capture_names.clone();
        self.seal(f, at, kind, params, name)?;
        Ok((captures, capture_names))
    }

    /// Falling off a body's end returns null (`K-FORM-015`).
    fn fall_off(&self, f: &mut Fun) {
        f.emit(Op::Fall);
    }

    /// The unit that runs library or declared function `id`'s body,
    /// `covered` or not.
    pub(crate) fn body(&mut self, id: FunctionId, covered: bool) -> Result<UnitId> {
        if let Some(unit) = self.scope.bodies.get(&(id, covered)) {
            return Ok(*unit);
        }
        if let Some(library) = self.library_of(&id) {
            return library.request(self.registry, self.available, |library| {
                library.body(id, covered)
            });
        }
        self.own_body(id, covered)
    }

    /// The unit a start runs for function `id`'s uncovered body: the
    /// program's own, whichever code `id` is.
    pub(crate) fn entry_body(&mut self, id: FunctionId) -> Result<UnitId> {
        match self.scope.bodies.get(&(id, false)) {
            Some(unit) if *unit & LIB == self.tag => Ok(*unit),
            _ => self.own_body(id, false),
        }
    }

    /// Compiles `id`'s body, `covered` or not, as the compiler's own.
    fn own_body(&mut self, id: FunctionId, covered: bool) -> Result<UnitId> {
        if self.in_library() {
            if let Some(error) = self.failed.get(&(id, covered)) {
                return Err(error.clone());
            }
            // Library code is no Link's: a body that reads names a Link
            // binds is not library code.
            if !self.free_names(id)?.is_empty() {
                let error = Unsupported(format!(
                    "library function {id} reads names its own Link binds"
                ));
                self.failed.insert((id, covered), error.clone());
                return Err(error);
            }
        }
        let at = self.reserve();
        self.scope.bodies.insert((id, covered), at);
        let kind = if self.document.functions.contains_key(&id) {
            UnitKind::Declared(id)
        } else {
            UnitKind::Body(id)
        };
        match self.function(id, None, None, at, covered, kind) {
            Ok(_) => Ok(at),
            Err(error) => {
                self.scope.bodies.remove(&(id, covered));
                // The reserved index keeps a unit that says why it cannot run.
                let unit = self.unavailable(&error.0);
                self.put(at, unit);
                if self.in_library() {
                    self.failed.insert((id, covered), error.clone());
                }
                Err(error)
            }
        }
    }

    /// The unit build helper `id` compiles to when the opening build image
    /// lacks it: an `Unavailable` op owned by `id` (contract §7). `None`
    /// for a function the document declares or the image holds.
    fn missing(&mut self, id: FunctionId) -> Option<UnitId> {
        if !self.lacks(&id) {
            return None;
        }
        if let Some(unit) = self.scope.missing.get(&id) {
            return Some(*unit);
        }
        let at = self.reserve();
        let mut unit = self.unavailable(&format!("build helper {id} is unavailable"));
        unit.owner = Some(id);
        self.put(at, unit);
        self.scope.missing.insert(id, at);
        Some(at)
    }

    /// Whether `id` is a build helper the opening build image lacks.
    fn lacks(&self, id: &FunctionId) -> bool {
        self.available.is_some_and(|available| {
            !self.document.functions.contains_key(id) && !available.contains(id)
        })
    }

    /// A unit that refuses when it runs: a body the core cannot compile,
    /// reached only from a path that does not run it (contract §7).
    fn unavailable(&mut self, why: &str) -> Unit {
        let reason = self.text(why);
        let mut f = Fun::new(SiteUnit::Main, false, true, None);
        f.leader();
        f.emit(Op::Unavailable(reason));
        self.sealed(f, UnitKind::Spawn, 0, Arc::from("unavailable"))
    }

    /// The completion formula of `definition`, by index.
    pub(crate) fn charge(
        &mut self,
        id: FunctionId,
        definition: &FunctionDefinition,
    ) -> Option<u32> {
        if let Some(at) = self.scope.charge_ids.get(&id) {
            return Some(*at);
        }
        let formula = charge_tables::completion(definition, false)?;
        let at = self.formula(formula, definition);
        if let Some(charge) = self.charges.last_mut() {
            charge.function = Some(id);
        }
        self.scope.charge_ids.insert(id, at);
        Some(at)
    }

    fn formula(
        &mut self,
        formula: &lash_kernel_doc::Formula,
        definition: &FunctionDefinition,
    ) -> u32 {
        let params = definition
            .signature
            .params
            .iter()
            .map(|param| param.name.clone())
            .collect();
        self.charges.push(Charge::new(formula, params));
        self.tag | (self.base.charges + self.charges.len() - 1) as u32
    }

    /// What a call of library or declared function `id` from `f` calls.
    pub(crate) fn callee(&mut self, f: &Fun, id: FunctionId) -> Result<Callee> {
        let covered = f.covered;
        match self.library_of(&id) {
            Some(library) => library.request(self.registry, self.available, |library| {
                library.called(id, covered)
            }),
            None => self.called(id, covered),
        }
    }

    /// What a call of `id` from code `covered` or not calls.
    fn called(&mut self, id: FunctionId, covered: bool) -> Result<Callee> {
        if let Some(unit) = self.missing(id) {
            return Ok(Callee::Unit { unit, charge: None });
        }
        let definition = self.def(&id)?;
        if let Some(function) = machine_function(definition) {
            let charge = if covered {
                None
            } else {
                self.charge(id, definition)
            };
            return Ok(Callee::Machine {
                function,
                id,
                charge,
            });
        }
        let Some(_) = definition.charge() else {
            // A statement-metered body charges its own nodes wherever it
            // is called from; under a cover it stays covered.
            return Ok(Callee::Unit {
                unit: self.body(id, covered)?,
                charge: None,
            });
        };
        let native = self
            .registry
            .get(&id)
            .and_then(|registered| registered.native().cloned());
        match native {
            Some(native) => {
                let at = self.native(id, definition, native)?;
                Ok(Callee::Native {
                    native: at,
                    charged: !covered,
                })
            }
            None if definition.body().is_none() => Ok(Callee::Unit {
                unit: self.unlinked(id),
                charge: None,
            }),
            None => {
                let charge = if covered {
                    None
                } else {
                    self.charge(id, definition)
                };
                Ok(Callee::Unit {
                    unit: self.body(id, true)?,
                    charge,
                })
            }
        }
    }

    /// A native the build declares and does not link: a unit that refuses
    /// when it runs, so code that never calls it still compiles.
    fn unlinked(&mut self, id: FunctionId) -> UnitId {
        if let Some(unit) = self.scope.missing.get(&id) {
            return *unit;
        }
        let at = self.reserve();
        let mut unit = self.unavailable(&format!("native {id} is not linked in this build"));
        unit.owner = Some(id);
        self.put(at, unit);
        self.scope.missing.insert(id, at);
        at
    }

    fn native(
        &mut self,
        id: FunctionId,
        definition: &FunctionDefinition,
        native: Arc<dyn lash_kernel_doc::NativeFunction>,
    ) -> Result<u32> {
        if let Some(at) = self.scope.native_ids.get(&id) {
            return Ok(*at);
        }
        let Some(charge) = self.charge(id, definition) else {
            return unsupported(format!("native {id} has no completion formula"));
        };
        let guard = definition
            .guard
            .as_ref()
            .map(|guard| self.formula(&guard.limit, definition));
        let scalar = match native
            .specialization()
            .inline
            .and_then(|inline| inline.for_version(definition.native_version))
        {
            // A guarded native's answer stays the call's, under its guard.
            scalar @ Some(
                InlineOperation::Scalar { .. }
                | InlineOperation::Kind
                | InlineOperation::Same
                | InlineOperation::Not
                | InlineOperation::FloatIdentity
                | InlineOperation::FloatUnary(_)
                | InlineOperation::ToFloat
                | InlineOperation::Length(_)
                | InlineOperation::Membership(_)
                | InlineOperation::IntegerText(_),
            ) if guard.is_none() => scalar,
            _ => None,
        };
        // A plan's template states the depth its answer stands for: near
        // the bound the body answers instead (`HelperTemplate::depth`).
        let elided = self
            .registry
            .get(&id)
            .and_then(|registered| registered.plan())
            .and_then(|plan| plan.template.as_ref())
            .map_or(0, |template| template.depth);
        let at = self.tag | (self.base.natives + self.natives.len()) as u32;
        self.natives.push(NativeEntry {
            function: id,
            name: Arc::from(definition.name.as_str()),
            params: within(
                definition.signature.params.len(),
                "parameters of one function",
            )?,
            charge,
            guard,
            body: None,
            elided,
            scalar,
        });
        self.scope.native_ids.insert(id, at);
        // A decline runs the definition's body under the native's cover
        // (`K-LIB-006`). A body the core cannot compile leaves the decline
        // to refuse.
        if definition.body().is_some()
            && let Ok(body) = self.body(id, true)
        {
            let local = (at & !LIB) as usize - self.base.natives;
            self.natives[local].body = Some(body);
        }
        Ok(at)
    }

    /// The trampoline a function value naming `id` applies: it makes the
    /// ordinary call, so applying the value charges what calling `id` does.
    pub(crate) fn trampoline(&mut self, id: FunctionId) -> Result<()> {
        let callable = CallableId {
            link: self.link,
            function: id,
        };
        if self.functions.contains_key(&callable) {
            return Ok(());
        }
        // A function value of library function `id` in any Link applies
        // the library's trampoline.
        if let Some(library) = self.library_of(&id) {
            let unit = library.request(self.registry, self.available, |library| {
                library.trampoline_unit(id)
            })?;
            self.functions.insert(callable, unit);
            return Ok(());
        }
        if let Some(error) = self.failed_functions.get(&id) {
            return Err(error.clone());
        }
        if let Some(unit) = self.missing(id) {
            self.functions.insert(callable, unit);
            self.fresh(id, unit);
            return Ok(());
        }
        let definition = self.def(&id)?;
        let at = self.reserve();
        self.functions.insert(callable, at);
        self.fresh(id, at);
        if let Err(error) = self.trampoline_body(id, definition, at) {
            if self.in_library() {
                self.failed_functions.insert(id, error.clone());
            }
            return Err(error);
        }
        Ok(())
    }

    /// The library's trampoline for library function `id`.
    fn trampoline_unit(&mut self, id: FunctionId) -> Result<UnitId> {
        self.trampoline(id)?;
        let callable = CallableId {
            link: self.link,
            function: id,
        };
        match self.functions.get(&callable) {
            Some(unit) => Ok(*unit),
            None => unsupported(format!("function {id} has no trampoline")),
        }
    }

    /// Records the library's new trampoline for `id`.
    fn fresh(&mut self, id: FunctionId, unit: UnitId) {
        if self.in_library() {
            self.fresh_trampolines.push((id, unit));
        }
    }

    /// Compiles the trampoline of `id` at `at`.
    fn trampoline_body(
        &mut self,
        id: FunctionId,
        definition: &FunctionDefinition,
        at: UnitId,
    ) -> Result<()> {
        let argc = within(
            definition.signature.params.len(),
            "parameters of one function",
        )?;
        let mut f = Fun::new(SiteUnit::Library(id), false, false, None);
        f.locals = argc;
        f.leader();
        for param in 0..argc {
            f.emit(Op::Local(param));
        }
        let site = f.site();
        let callee = self.callee(&f, id)?;
        self.call(&mut f, callee, argc, site);
        f.emit(Op::Return);
        let name = Arc::from(format!("&{}", definition.name.as_str()));
        // The value's call is the one its trampoline makes.
        let mut unit = self.sealed(f, UnitKind::Body(id), argc, name);
        unit.call = false;
        self.put(at, unit);
        Ok(())
    }

    /// The contract a call from `f` of build helper `id` through `callee`
    /// is checked against: an authored, uncovered caller's call of a
    /// helper body whose signature types something (`K-FN-004`), whether
    /// or not the document carries the helper's definition: a function's
    /// identity is its content. A helper whose definition states a native
    /// checks its own arguments (`K-FN-007`), whichever implementation the
    /// plan runs, so its body is held to no contract the native is not.
    pub(crate) fn checked(&self, f: &Fun, id: FunctionId, callee: &Callee) -> Option<Arc<Checked>> {
        let authored = !self.in_library()
            && !f.covered
            && match &f.site {
                SiteUnit::Main | SiteUnit::Function(_) => true,
                SiteUnit::Library(owner) => self.document.functions.contains_key(owner),
            };
        if !authored
            || !matches!(callee, Callee::Unit { .. })
            || !self.helpers.is_some_and(|helpers| helpers.contains(&id))
        {
            return None;
        }
        let definition = self.registry.get(&id)?.definition();
        if definition.has_native() {
            return None;
        }
        let signature = &definition.signature;
        let typed = signature.result != lash_kernel_value::Type::Any
            || signature
                .params
                .iter()
                .any(|param| param.ty != lash_kernel_value::Type::Any);
        typed.then(|| {
            Arc::new(Checked::new(
                CallableId {
                    link: self.link,
                    function: id,
                },
                signature.clone(),
            ))
        })
    }

    /// Emits a call through `callee` with `argc` arguments on the stack. A
    /// call that pushes a frame ends its block.
    pub(crate) fn call(&mut self, f: &mut Fun, callee: Callee, argc: u16, site: Site) {
        let at = f.calls.len() as u32;
        let mark = self.mark(f, &site);
        f.calls.push(CallSite {
            callee,
            argc,
            site,
            mark,
            checked: None,
            names: Box::default(),
            entry: None,
        });
        f.emit(Op::Call(at));
        if !matches!(callee, Callee::Native { .. } | Callee::Machine { .. }) {
            f.leader();
        }
    }

    /// A call action at `site`: [`Self::call`], its entries counted
    /// (`K-EFF-008`).
    pub(crate) fn entered_call(&mut self, f: &mut Fun, callee: Callee, argc: u16, site: Site) {
        self.call(f, callee, argc, site);
        if let Some(call) = f.calls.last_mut() {
            call.entry = Some(call.mark.occurrence);
        }
    }

    /// Where `f` stands at the call or action at `site`.
    pub(crate) fn mark(&mut self, f: &Fun, site: &Site) -> Arc<Mark> {
        Arc::new(Mark {
            statement: f.statement.clone(),
            loops: f.enclosing.iter().cloned().collect(),
            occurrence: self.occurrence(site),
        })
    }

    /// The occurrence counter of the call or action at `site`: the
    /// library's for a site in library code, whichever Link counts it.
    fn occurrence(&mut self, site: &Site) -> u32 {
        if let SiteUnit::Library(id) = &site.unit
            && let Some(library) = self.library_of(id)
        {
            return library
                .request(self.registry, self.available, |library| {
                    Ok(library.occurrence(site))
                })
                .unwrap_or(self.occurrences.len() as u32);
        }
        let next = self.tag | self.occurrences.len() as u32;
        let at = *self
            .occurrences
            .entry((self.link, site.clone()))
            .or_insert(next);
        if at == next && self.in_library() {
            self.fresh_sites.push((site.clone(), at));
        }
        at
    }

    /// The names a call through an import of a `PyArgsList` export
    /// (`packing`) keys its map by: those of its `argc` arguments past
    /// `positional`, in order, as constants of the caller's code, held
    /// with it as an authored literal is. They are no operands: the
    /// adaptation reads them after it has checked the arguments
    /// ([`Adapt::Python`]), so the call allocates no text and the frame
    /// reserves nothing for them. An argument past the export's
    /// parameters, which the import's contract refuses before any
    /// collection is built, is named absent.
    ///
    /// [`Adapt::Python`]: super::program::Adapt::Python
    pub(crate) fn keywords(&mut self, packing: Option<&Packing>, argc: usize) -> Box<[u32]> {
        let Some(Packing { positional, names }) = packing else {
            return Box::default();
        };
        ((*positional).min(argc)..argc)
            .map(|at| match names.get(at) {
                Some(name) => self.text(name.as_str()),
                None => self.constant(&Literal::Absent),
            })
            .collect()
    }

    /// `spawn`'s stub: its parameters are the call's arguments (a value
    /// callee first), and it makes the call, its names under `packing` as
    /// [`Self::keywords`] gives them. A task is no part of its
    /// spawner's call, so a covered spawner's cover does not reach it.
    pub(crate) fn stub(
        &mut self,
        f: &Fun,
        callee: Option<CallableId>,
        resolved: Option<(Callee, Option<Arc<Checked>>)>,
        packing: Option<&Packing>,
        argc: u16,
    ) -> Result<UnitId> {
        let site = f.site();
        let key = (site.clone(), callee, argc);
        if let Some(unit) = self.scope.stubs.get(&key) {
            return Ok(*unit);
        }
        let at = self.reserve();
        self.scope.stubs.insert(key, at);
        let values = argc + u16::from(callee.is_none());
        let mut g = Fun::new(site.unit.clone(), false, false, f.owner);
        g.path.clone_from(&site.path);
        g.locals = values;
        g.leader();
        let first = values - argc;
        if first > 0 {
            g.emit(Op::Local(0));
        }
        for at in 0..argc {
            g.emit(Op::Local(first + at));
        }
        let (target, checked) = match (callee, resolved) {
            (_, Some(resolved)) => resolved,
            (None, None) => (Callee::Value, None),
            (Some(callable), None) => (self.callee_at(&g, callable)?, None),
        };
        let names = self.keywords(packing, usize::from(argc));
        self.call(&mut g, target, argc, site);
        if let Some(call) = g.calls.last_mut() {
            call.checked = checked;
            call.names = names;
        }
        g.emit(Op::Return);
        self.seal(g, at, UnitKind::Spawn, values, Arc::from("spawn"))?;
        Ok(at)
    }

    /// A closure made where `f` stands, by its index in `f`'s closure
    /// table: its body is a unit of its own, charged wherever it runs.
    pub(crate) fn closure(&mut self, f: &mut Fun, id: FunctionId) -> Result<u32> {
        let at = self.reserve();
        let (captures, capture_names) =
            match self.function(id, Some(f), None, at, false, UnitKind::Closure(id)) {
                Ok(found) => found,
                Err(error) => {
                    let unit = self.unavailable(&error.0);
                    self.put(at, unit);
                    return Err(error);
                }
            };
        let identity = Arc::new(ClosureIdentity {
            capture_names,
            callable: CallableId {
                link: self.link,
                function: id,
            },
            site: LinkedSite {
                link: self.link,
                site: f.site(),
            },
        });
        let index = f.closures.len() as u32;
        f.closures.push(ClosureSite {
            unit: at,
            identity,
            captures,
        });
        Ok(index)
    }

    /// The unit of a closure of another Link the run holds, compiled in
    /// that Link over the captures `identity` names. A Link the build does
    /// not load gets none: calling the closure refuses where it runs.
    fn saved_closure(&mut self, identity: &ClosureIdentity) -> Result<()> {
        let callable = identity.callable;
        if !self.links.contains_key(&callable.link) {
            return Ok(());
        }
        self.enter_link(callable.link)?;
        let at = self.reserve();
        let kind = UnitKind::Closure(callable.function);
        let saved = Some(identity.capture_names.as_slice());
        if let Err(error) = self.function(callable.function, None, saved, at, false, kind) {
            let unit = self.unavailable(&error.0);
            self.put(at, unit);
        }
        Ok(())
    }

    // --- Sealing.

    /// Cuts `f` into blocks and stores it at `at`, refusing a unit past
    /// the core's limit ([`within`]) in locals and temporaries, which
    /// [`Fun::temp`] saturates, or global names ([`Compiler::global`]).
    pub(crate) fn seal(
        &mut self,
        f: Fun,
        at: UnitId,
        kind: UnitKind,
        params: u16,
        name: Arc<str>,
    ) -> Result<()> {
        within(f.declared.len(), "parameters and locals of one function")?;
        within(self.globals.len(), "global names of one program")?;
        let unit = self.sealed(f, kind, params, name);
        self.put(at, unit);
        Ok(())
    }

    fn sealed(&mut self, f: Fun, kind: UnitKind, params: u16, name: Arc<str>) -> Unit {
        let Fun {
            mut ops,
            event,
            mut step,
            locals,
            slots,
            boxed,
            covered,
            records,
            closures,
            calls,
            loop_sites,
            actions,
            action_marks,
            site,
            path,
            mut points,
            origin,
            stated,
            declared,
            owner,
            capture_names,
            ..
        } = f;
        // Points are recorded in lowering order; an `if`'s test and a
        // loop's head may be recorded after ops past them.
        points.sort_by_key(|(at, _)| *at);
        let authored = !covered
            && match &kind {
                UnitKind::Main | UnitKind::Declared(_) => true,
                UnitKind::Closure(id) => self.document.functions.contains_key(id),
                UnitKind::Body(_) | UnitKind::Spawn => false,
            };
        // Library code enters no site.
        let mut calls = calls;
        if !authored {
            calls.iter_mut().for_each(|call| call.entry = None);
        }
        let mut blocks = Vec::new();
        let mut through = vec![0u64; ops.len()];
        let mut pc = 0;
        while pc < ops.len() {
            debug_assert!(matches!(ops[pc], Op::Block(_)), "a block starts every run");
            let start = pc;
            let mut charge = Some(0u64);
            let mut sum = 0u64;
            pc += 1;
            while pc < ops.len() && !matches!(ops[pc], Op::Block(_)) {
                let cost = event[pc].total();
                charge = charge.and_then(|total| total.checked_add(cost));
                sum = sum.saturating_add(cost);
                through[pc] = sum;
                pc += 1;
            }
            ops[start] = Op::Block(blocks.len() as u32);
            blocks.push(Block {
                start: start as u32,
                end: pc as u32,
                charge,
            });
        }
        // A step is a statement of guest code, a closure or a body-only
        // library body; a native's Decline and entry into its alternative
        // body create none (`K-MACH-002`). A library body marks its
        // statements either way: whether it steps follows the image, and
        // a frame saved at one under an image where it steps opens there
        // under one where it does not.
        let (marked, steps) = match kind {
            _ if !covered => (true, true),
            UnitKind::Body(id) | UnitKind::Declared(id) => (
                true,
                self.registry
                    .get(&id)
                    .and_then(|registered| registered.native())
                    .is_none(),
            ),
            _ => (false, false),
        };
        // A loop's back edge starts a step.
        for (at, op) in ops.iter().enumerate() {
            if let Op::Jump(to) = op
                && (*to as usize) < at
                && marked
            {
                step[at] = true;
            }
        }
        if !marked {
            step.iter_mut().for_each(|flag| *flag = false);
        }
        let mut boxed: Vec<u16> = boxed
            .iter()
            .filter_map(|name| slots.get(name).copied())
            .collect();
        boxed.sort_unstable();
        Unit {
            call: !matches!(kind, UnitKind::Main | UnitKind::Spawn),
            kind,
            link: self.link,
            covered,
            steps,
            params,
            locals: locals.max(params),
            captures: capture_names.len() as u16,
            boxed,
            ops,
            event,
            step,
            through,
            blocks,
            records,
            closures,
            calls,
            loops: loop_sites,
            actions,
            action_marks,
            authored,
            name,
            owner,
            site: Site::new(site, path),
            points,
            fused: Vec::new(),
            negated_branches: Vec::new(),
            traces: Vec::new(),
            micros: Vec::new(),
            trace_steps: Vec::new(),
            trace_operands: Vec::new(),
            inlines: Vec::new(),
            folds: Vec::new(),
            lookup_words: Vec::new(),
            origin,
            stated,
            declared,
            sites: Default::default(),
            cleanup: None,
        }
    }

    /// A function call's machine identity, for `brand.built` answered
    /// where it is written.
    pub(crate) fn brand_built(&mut self, f: &Fun, id: FunctionId) -> Result<Option<u32>> {
        if self.lacks(&id) {
            return Ok(None);
        }
        let definition = self.def(&id)?;
        if machine_function(definition) != Some(MachineFunction::BrandBuilt) {
            return Ok(None);
        }
        let literal = match f.owner {
            Some(owner) => Literal::Bytes(lash_kernel_doc::Bytes::new(owner.as_bytes().to_vec())),
            None => Literal::Absent,
        };
        Ok(Some(self.constant(&literal)))
    }
}
