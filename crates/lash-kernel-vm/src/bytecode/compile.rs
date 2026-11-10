//! The compiler's driver (cutover contract §1): one admitted document and
//! the library it reaches become one [`Program`].
//!
//! Units are compiled on first reach: main, then every function a call
//! names, every closure body where its expression stands, every function
//! value's trampoline and every spawn's stub. A unit index is reserved
//! before its body compiles, so recursion resolves. Then every function
//! each other loaded Link declares gets its trampoline, compiled as that
//! Link: a function value names a Link and a function, and calls that
//! Link's body or nothing. A library body is laid out from the template
//! its prepared library holds ([`library`](super::library)), compiled once
//! for every run that reaches it. [`lower`](super::lower)
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
use super::library::{Library, OUTSIDE, Template};
use super::names::Names;
use super::op::Op;
use super::program::{
    ActionSite, Block, CallSite, Callee, Capture, ClosureSite, Constant, Event, FunctionEntry,
    LoopMark, LoopSite, Mark, NativeEntry, Point, Program, Unit, UnitId, UnitKind,
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

/// Compiles `document`, whose library functions `registry` holds, as
/// instance `link`: its `main`, or the declared function `entry`, is the
/// unit a start runs. `linked` holds the other Links the run's build loads
/// and what their function imports resolve to.
///
/// An open under another build image passes that image as `available`: a
/// build helper it lacks compiles to a unit that refuses when it runs
/// (contract §7), and open proves no resume point reaches one. `library`
/// holds the library bodies compiled for `registry` under that image.
pub(crate) fn compile(
    document: &Document,
    registry: &FunctionRegistry,
    link: LinkId,
    linked: &Linked<'_>,
    entry: Option<&Entry<'_>>,
    available: Option<&BuildHelpers>,
    library: &Library,
) -> Result<Program> {
    let mut compiler = Compiler::new(document, registry, link, &linked.imports);
    compiler.available = available;
    compiler.library = Some(library);
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
    compiler.declare_all(false);
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

/// What a start runs instead of `main`: a declared or library function,
/// or a callable's, under the start's convention.
pub(crate) struct Entry<'a> {
    pub function: FunctionId,
    pub convention: &'a lash_kernel_doc::StartConvention,
    /// A callable's frozen captures, by name, in its closure's order.
    pub captures: Option<&'a [Name]>,
}

/// The function each Link's function import slot resolves to.
pub type Imports = BTreeMap<(LinkId, ImportSlot), CallableId>;

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
    pub(crate) fn temp(&mut self) -> u16 {
        let slot = self.locals;
        self.locals += 1;
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
            other => other,
        };
    }
}

/// What the compiler has made for one Link: a function id, a function
/// literal or a helper's perform means that Link's own, so each Link's
/// code is compiled, and found again, apart.
#[derive(Default)]
struct LinkCode {
    immediates: HashMap<Literal, u32>,
    native_ids: HashMap<FunctionId, u32>,
    charge_ids: HashMap<FunctionId, u32>,
    bodies: HashMap<(FunctionId, bool), UnitId>,
    stubs: HashMap<(Option<CallableId>, u8), UnitId>,
    free: HashMap<FunctionId, BTreeSet<Name>>,
    /// The unit each unavailable helper compiled to.
    missing: HashMap<FunctionId, UnitId>,
}

/// What a template's code names outside it, each by its index there.
struct Outside {
    /// The body the template is.
    root: (FunctionId, bool),
    bodies: Vec<(FunctionId, bool)>,
    body_ids: HashMap<(FunctionId, bool), u32>,
    natives: Vec<FunctionId>,
    charges: Vec<FunctionId>,
    functions: BTreeSet<FunctionId>,
}

/// The Link a template's code is compiled in: no loaded Link's.
const NO_LINK: [u8; 32] = [0; 32];

/// The compiler's program-wide state.
pub(crate) struct Compiler<'a> {
    /// The document of the Link being compiled.
    pub document: &'a Document,
    pub registry: &'a FunctionRegistry,
    /// The Link being compiled.
    pub link: LinkId,
    /// The run's root Link: main's and the entry's.
    root: LinkId,
    scope: LinkCode,
    /// Every loaded Link's document.
    links: HashMap<LinkId, &'a Document>,
    /// What the compiler made for each Link other than the one it is in.
    parked: HashMap<LinkId, LinkCode>,
    imports: &'a Imports,
    pub units: Vec<Option<Unit>>,
    pub constants: Vec<Constant>,
    pub natives: Vec<NativeEntry>,
    pub charges: Vec<Charge>,
    pub functions: BTreeMap<CallableId, UnitId>,
    /// The functions each compiled Link declares.
    declares: BTreeSet<CallableId>,
    pub globals: Vec<Name>,
    global_ids: HashMap<Name, u16>,
    /// Each action site's occurrence counter index (`K-EFF-008`).
    occurrences: HashMap<(LinkId, Site), u32>,
    /// The build image an open compiles under, when it is not the one the
    /// run sealed under: the helpers it lacks are unavailable.
    available: Option<&'a BuildHelpers>,
    /// The library bodies already compiled; `None` while compiling one.
    library: Option<&'a Library>,
    /// Compiling a template: what its code names outside it.
    outside: Option<Outside>,
    /// Each unit whose site tables are built: a laid-out template's.
    built: Vec<bool>,
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
            globals: Vec::new(),
            global_ids: HashMap::new(),
            occurrences: HashMap::new(),
            available: None,
            library: None,
            outside: None,
            built: Vec::new(),
        }
    }

    /// Compiles library function `id`'s body, `covered` or not, as a
    /// template under build image `available`.
    pub(crate) fn template(
        registry: &FunctionRegistry,
        available: Option<&BuildHelpers>,
        id: FunctionId,
        covered: bool,
    ) -> Result<Template> {
        let document = Document::new(lash_kernel_doc::NumberPolicy::Float, Vec::new());
        let imports = Imports::new();
        let mut compiler =
            Compiler::new(&document, registry, LinkId::from_bytes(NO_LINK), &imports);
        compiler.available = available;
        compiler.outside = Some(Outside {
            root: (id, covered),
            bodies: Vec::new(),
            body_ids: HashMap::new(),
            natives: Vec::new(),
            charges: Vec::new(),
            functions: BTreeSet::new(),
        });
        let at = compiler.body(id, covered)?;
        debug_assert_eq!(at, 0, "a template's body is its first unit");
        let reads = !compiler.free_names(id)?.is_empty();
        let mut units = Vec::with_capacity(compiler.units.len());
        for (at, unit) in compiler.units.into_iter().enumerate() {
            match unit {
                Some(mut unit) => {
                    unit.sites = super::site_tables::build(&unit);
                    units.push(unit);
                }
                None => return unsupported(format!("unit {at} was reserved and never compiled")),
            }
        }
        let Some(outside) = compiler.outside else {
            return unsupported("a template lost what it names");
        };
        Ok(Template {
            units,
            constants: compiler.constants,
            globals: compiler.globals,
            bodies: outside.bodies,
            natives: outside.natives,
            charges: outside.charges,
            functions: outside.functions.into_iter().collect(),
            reads,
        })
    }

    /// Lays out `template`, library function `id`'s body `covered` or not,
    /// in the Link being compiled: the program compiling it here makes, up
    /// to the numbering of units and constants.
    fn instantiate(
        &mut self,
        id: FunctionId,
        covered: bool,
        template: &Template,
    ) -> Result<UnitId> {
        let base = self.units.len() as UnitId;
        for _ in &template.units {
            self.reserve();
        }
        self.scope.bodies.insert((id, covered), base);
        match self.lay_out(base, template) {
            Ok(()) => Ok(base),
            Err(error) => {
                self.scope.bodies.remove(&(id, covered));
                for at in base as usize..base as usize + template.units.len() {
                    if self.units[at].is_none() {
                        self.units[at] = Some(self.unavailable(&error.0));
                    }
                }
                Err(error)
            }
        }
    }

    fn lay_out(&mut self, base: UnitId, template: &Template) -> Result<()> {
        let constants: Vec<u32> = template
            .constants
            .iter()
            .map(|constant| {
                let constant = match constant {
                    Constant::Function(callable) => Constant::Function(CallableId {
                        link: self.link,
                        function: callable.function,
                    }),
                    other => other.clone(),
                };
                self.constants.push(constant);
                (self.constants.len() - 1) as u32
            })
            .collect();
        let globals: Vec<u16> = template
            .globals
            .iter()
            .map(|name| self.global(name))
            .collect();
        for function in &template.functions {
            self.trampoline(*function)?;
        }
        for (at, unit) in template.units.iter().enumerate() {
            let mut unit = unit.clone();
            unit.link = self.link;
            for op in &mut unit.ops {
                match op {
                    Op::Const(constant)
                    | Op::Field(constant)
                    | Op::SetField(constant)
                    | Op::SetFieldPlaceFirst(constant)
                    | Op::RemoveField(constant)
                    | Op::Unavailable(constant) => *constant = constants[*constant as usize],
                    Op::Global(global) | Op::SetGlobal(global) => {
                        *global = globals[*global as usize];
                    }
                    _ => {}
                }
            }
            for record in &mut unit.records {
                *record = record
                    .iter()
                    .map(|field| constants[*field as usize])
                    .collect();
            }
            for closure in &mut unit.closures {
                closure.unit += base;
                closure.identity = Arc::new(ClosureIdentity {
                    capture_names: closure.identity.capture_names.clone(),
                    callable: CallableId {
                        link: self.link,
                        function: closure.identity.callable.function,
                    },
                    site: LinkedSite {
                        link: self.link,
                        site: closure.identity.site.site.clone(),
                    },
                });
            }
            for call in &mut unit.calls {
                call.callee = match call.callee {
                    Callee::Unit { unit, charge } => Callee::Unit {
                        unit: self.laid_out(base, template, unit)?,
                        charge: self.charge_of(template, charge)?,
                    },
                    Callee::Native { native, charged } => Callee::Native {
                        native: self.native_of(template.natives[native as usize])?,
                        charged,
                    },
                    Callee::Machine {
                        function,
                        id,
                        charge,
                    } => Callee::Machine {
                        function,
                        id,
                        charge: self.charge_of(template, charge)?,
                    },
                    Callee::Value => Callee::Value,
                };
                call.mark = self.remark(&call.mark, &call.site);
            }
            for action in &mut unit.actions {
                if let ActionSite::Spawn { unit, .. } = action {
                    *unit += base;
                }
            }
            for (mark, action) in unit.action_marks.iter_mut().zip(&unit.actions) {
                *mark = self.remark(mark, action.site());
            }
            let at = base as usize + at;
            self.units[at] = Some(unit);
            self.built[at] = true;
        }
        Ok(())
    }

    /// The unit template unit `unit` is once laid out at `base`.
    fn laid_out(&mut self, base: UnitId, template: &Template, unit: UnitId) -> Result<UnitId> {
        if unit & OUTSIDE == 0 {
            return Ok(base + unit);
        }
        let (id, covered) = template.bodies[(unit & !OUTSIDE) as usize];
        self.body(id, covered)
    }

    /// A template's completion formula, by its index there, in the program.
    fn charge_of(&mut self, template: &Template, charge: Option<u32>) -> Result<Option<u32>> {
        let Some(charge) = charge else {
            return Ok(None);
        };
        let id = template.charges[charge as usize];
        let definition = self.def(&id)?;
        match self.charge(id, definition) {
            Some(at) => Ok(Some(at)),
            None => unsupported(format!("function {id} has no completion formula")),
        }
    }

    /// Library function `id`'s native, by its index in the program.
    fn native_of(&mut self, id: FunctionId) -> Result<u32> {
        let definition = self.def(&id)?;
        let Some(native) = self
            .registry
            .get(&id)
            .and_then(|registered| registered.native().cloned())
        else {
            return unsupported(format!("native {id} is not linked in this build"));
        };
        self.native(id, definition, native)
    }

    /// Compiles Link `link` from here on, finding again what was already
    /// made for it.
    fn enter_link(&mut self, link: LinkId) -> Result<()> {
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
            Some(callable) => Ok(*callable),
            None => unsupported(format!("import {slot:?} names no function")),
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
        self.seal(f, at, UnitKind::Main, 0, "main".into());
        Ok(at)
    }

    fn finish(self, main: UnitId) -> Result<Program> {
        let mut units = Vec::with_capacity(self.units.len());
        for (at, (unit, built)) in self.units.into_iter().zip(self.built).enumerate() {
            match unit {
                // A unit's tables are built from its final code: a
                // convention may add to a unit after it was sealed. A
                // laid-out template's were built with it.
                Some(mut unit) => {
                    if !built {
                        unit.sites = super::site_tables::build(&unit);
                    }
                    units.push(unit);
                }
                None => return unsupported(format!("unit {at} was reserved and never compiled")),
            }
        }
        Ok(Program {
            units,
            constants: self.constants,
            natives: self.natives,
            charges: self.charges,
            functions: self
                .functions
                .into_iter()
                .map(|(callable, unit)| FunctionEntry { callable, unit })
                .collect(),
            declared: self.declares,
            link: self.root,
            globals: self.globals,
            main,
        })
    }

    pub(crate) fn reserve(&mut self) -> UnitId {
        self.units.push(None);
        self.built.push(false);
        (self.units.len() - 1) as UnitId
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
        let at = self.constants.len() as u32;
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
        let at = self.globals.len() as u16;
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
                f.captures
                    .insert(name.clone(), f.capture_names.len() as u16);
                f.capture_names.push(name);
                captures.push(source);
            }
        }
        // A saved closure's captures are the ones its save froze, in order.
        for name in saved.into_iter().flatten() {
            f.captures
                .insert(name.clone(), f.capture_names.len() as u16);
            f.capture_names.push(name.clone());
        }
        self.declare(&mut f, &params, &body.block)?;
        f.leader();
        self.block(&mut f, &body.block)?;
        self.fall_off(&mut f);
        let name: Arc<str> = Arc::from(definition.name.as_str());
        let params = params.len() as u16;
        let capture_names = f.capture_names.clone();
        self.seal(f, at, kind, params, name);
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
        if let Some(outside) = &mut self.outside {
            if outside.root != (id, covered) {
                let next = outside.bodies.len() as u32;
                let at = *outside.body_ids.entry((id, covered)).or_insert(next);
                if at == next {
                    outside.bodies.push((id, covered));
                }
                return Ok(OUTSIDE | at);
            }
        } else if let Some(library) = self.library
            && !self.document.functions.contains_key(&id)
            // A template another Link cannot run compiles here, to say why.
            && let Ok(template) = library.template(self.registry, self.available, id, covered)
            && (self.link == self.root || !template.reads)
        {
            return self.instantiate(id, covered, &template);
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
                self.units[at as usize] = Some(self.unavailable(&error.0));
                Err(error)
            }
        }
    }

    /// The unit build helper `id` compiles to when the opening build image
    /// lacks it: an `Unavailable` op owned by `id` (contract §7). `None`
    /// for a function the document declares or the image holds.
    fn missing(&mut self, id: FunctionId) -> Option<UnitId> {
        let available = self.available?;
        if self.document.functions.contains_key(&id) || available.contains(&id) {
            return None;
        }
        if let Some(unit) = self.scope.missing.get(&id) {
            return Some(*unit);
        }
        let at = self.reserve();
        let mut unit = self.unavailable(&format!("build helper {id} is unavailable"));
        unit.owner = Some(id);
        self.units[at as usize] = Some(unit);
        self.scope.missing.insert(id, at);
        Some(at)
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
        let at = match &mut self.outside {
            Some(outside) => {
                outside.charges.push(id);
                (outside.charges.len() - 1) as u32
            }
            None => self.formula(formula, definition),
        };
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
        (self.charges.len() - 1) as u32
    }

    /// What a call of library or declared function `id` from `f` calls.
    pub(crate) fn callee(&mut self, f: &Fun, id: FunctionId) -> Result<Callee> {
        if let Some(unit) = self.missing(id) {
            return Ok(Callee::Unit { unit, charge: None });
        }
        let definition = self.def(&id)?;
        let covered = f.covered;
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
        self.units[at as usize] = Some(unit);
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
        if let Some(outside) = &mut self.outside {
            outside.natives.push(id);
            let at = (outside.natives.len() - 1) as u32;
            self.scope.native_ids.insert(id, at);
            return Ok(at);
        }
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
                | InlineOperation::Length(_)
                | InlineOperation::Membership(_)
                | InlineOperation::IntegerText(_),
            ) if guard.is_none() => scalar,
            _ => None,
        };
        let at = self.natives.len() as u32;
        self.natives.push(NativeEntry {
            function: id,
            name: Arc::from(definition.name.as_str()),
            params: definition.signature.params.len() as u8,
            native,
            charge,
            guard,
            body: None,
            scalar,
        });
        self.scope.native_ids.insert(id, at);
        // A decline runs the definition's body under the native's cover
        // (`K-LIB-006`). A body the core cannot compile leaves the decline
        // to refuse.
        if definition.body().is_some()
            && let Ok(body) = self.body(id, true)
        {
            self.natives[at as usize].body = Some(body);
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
        if let Some(outside) = &mut self.outside {
            outside.functions.insert(id);
            return Ok(());
        }
        if let Some(unit) = self.missing(id) {
            self.functions.insert(callable, unit);
            return Ok(());
        }
        let definition = self.def(&id)?;
        let at = self.reserve();
        self.functions.insert(callable, at);
        let argc = definition.signature.params.len();
        let mut f = Fun::new(SiteUnit::Library(id), false, false, None);
        f.locals = argc as u16;
        f.leader();
        for param in 0..argc {
            f.emit(Op::Local(param as u16));
        }
        let site = f.site();
        let callee = self.callee(&f, id)?;
        self.call(&mut f, callee, argc as u8, site);
        f.emit(Op::Return);
        let name = Arc::from(format!("&{}", definition.name.as_str()));
        self.seal(f, at, UnitKind::Body(id), argc as u16, name);
        Ok(())
    }

    /// Emits a call through `callee` with `argc` arguments on the stack. A
    /// call that pushes a frame ends its block.
    pub(crate) fn call(&mut self, f: &mut Fun, callee: Callee, argc: u8, site: Site) {
        let at = f.calls.len() as u32;
        let mark = self.mark(f, &site);
        f.calls.push(CallSite {
            callee,
            argc,
            site,
            mark,
        });
        f.emit(Op::Call(at));
        if !matches!(callee, Callee::Native { .. } | Callee::Machine { .. }) {
            f.leader();
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

    /// `mark`, of a laid-out template's call or action at `site`, among
    /// the Link being compiled's occurrence counters.
    fn remark(&mut self, mark: &Mark, site: &Site) -> Arc<Mark> {
        Arc::new(Mark {
            statement: mark.statement.clone(),
            loops: Arc::clone(&mark.loops),
            occurrence: self.occurrence(site),
        })
    }

    fn occurrence(&mut self, site: &Site) -> u32 {
        let next = self.occurrences.len() as u32;
        *self
            .occurrences
            .entry((self.link, site.clone()))
            .or_insert(next)
    }

    /// `spawn`'s stub: its parameters are the call's arguments (a value
    /// callee first), and it makes the call. A task is no part of its
    /// spawner's call, so a covered spawner's cover does not reach it.
    pub(crate) fn stub(&mut self, f: &Fun, callee: Option<CallableId>, argc: u8) -> Result<UnitId> {
        if let Some(unit) = self.scope.stubs.get(&(callee, argc)) {
            return Ok(*unit);
        }
        let at = self.reserve();
        self.scope.stubs.insert((callee, argc), at);
        let values = u16::from(argc) + u16::from(callee.is_none());
        let site_unit = f.site.clone();
        let mut g = Fun::new(site_unit, false, false, f.owner);
        g.locals = values;
        g.leader();
        for value in 0..values {
            g.emit(Op::Local(value));
        }
        let site = f.site();
        let target = match callee {
            None => Callee::Value,
            Some(callable) => self.callee_at(&g, callable)?,
        };
        self.call(&mut g, target, argc, site);
        g.emit(Op::Return);
        self.seal(g, at, UnitKind::Spawn, values, Arc::from("spawn"));
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
                    self.units[at as usize] = Some(self.unavailable(&error.0));
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
            self.units[at as usize] = Some(self.unavailable(&error.0));
        }
        Ok(())
    }

    // --- Sealing.

    /// Cuts `f` into blocks and stores it at `at`.
    pub(crate) fn seal(&mut self, f: Fun, at: UnitId, kind: UnitKind, params: u16, name: Arc<str>) {
        let unit = self.sealed(f, kind, params, name);
        self.units[at as usize] = Some(unit);
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
            mut points,
            origin,
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
        // A loop's back edge starts a step (`K-MACH-002`).
        for (at, op) in ops.iter().enumerate() {
            if let Op::Jump(to) = op
                && (*to as usize) < at
                && !covered
            {
                step[at] = true;
            }
        }
        if covered {
            step.iter_mut().for_each(|flag| *flag = false);
        }
        let mut boxed: Vec<u16> = boxed
            .iter()
            .filter_map(|name| slots.get(name).copied())
            .collect();
        boxed.sort_unstable();
        Unit {
            kind,
            link: self.link,
            covered,
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
            site: Site::new(site, Vec::new()),
            points,
            fused: Vec::new(),
            inlines: Vec::new(),
            folds: Vec::new(),
            origin,
            declared,
            sites: Default::default(),
            cleanup: None,
        }
    }

    /// A function call's machine identity, for `brand.built` answered
    /// where it is written.
    pub(crate) fn brand_built(&mut self, f: &Fun, id: FunctionId) -> Result<Option<u32>> {
        if self.missing(id).is_some() {
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
