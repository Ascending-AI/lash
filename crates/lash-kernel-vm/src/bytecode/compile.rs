//! The compiler's driver (cutover contract §1): one admitted document and
//! the library it reaches become one [`Program`].
//!
//! Units are compiled on first reach: main, then every function a call
//! names, every closure body where its expression stands, every function
//! value's trampoline and every spawn's stub. A unit index is reserved
//! before its body compiles, so recursion resolves. [`lower`](super::lower)
//! emits each unit's statements; [`Compiler::seal`] cuts its blocks and
//! derives the charge, pace and step tables.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::Arc;

use lash_kernel_doc::arena::ClosureIdentity;
use lash_kernel_doc::arena::native::InlineOperation;
use lash_kernel_doc::linked_site::LinkedSite;
use lash_kernel_doc::{Document, FunctionDefinition, FunctionRegistry, Literal, Stmt, Value};
use lash_kernel_value::content_ids::LinkId;
use lash_kernel_value::names::{CallableId, FunctionId, Name};
use lash_kernel_value::sites::{Site, Unit as SiteUnit};

use super::charge_tables::{self, Charge};
use super::names::Names;
use super::op::Op;
use super::program::{
    ActionSite, Block, CallSite, Callee, Capture, ClosureSite, Constant, Event, FunctionEntry,
    LoopMark, LoopSite, Mark, NativeEntry, Point, Program, Unit, UnitId, UnitKind,
};
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
/// unit a start runs.
pub fn compile(
    document: &Document,
    registry: &FunctionRegistry,
    link: LinkId,
    entry: Option<FunctionId>,
) -> Result<Program> {
    let mut compiler = Compiler::new(document, registry, link);
    let main = match entry {
        Some(entry) => compiler.body(entry, false)?,
        None => compiler.main()?,
    };
    compiler.finish(main)
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
    pub pace: Vec<u32>,
    pub step: Vec<bool>,
    pending: Event,
    pending_pace: u32,
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
            pace: Vec::new(),
            step: Vec::new(),
            pending: Event::default(),
            pending_pace: 0,
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

    /// One unit of pace (`Charge::Off`): a statement, loop test or action
    /// atom of uncovered code.
    pub(crate) fn pace(&mut self) {
        if !self.covered {
            self.pending_pace += 1;
        }
    }

    /// Emits `op`, which bills nothing, leaving pending events and the
    /// step mark to the op after it.
    pub(crate) fn quiet(&mut self, op: Op) {
        self.ops.push(op);
        self.event.push(Event::default());
        self.pace.push(0);
        self.step.push(false);
    }

    pub(crate) fn emit(&mut self, op: Op) -> usize {
        if let Some(point) = self.start.take() {
            self.points.push((self.here(), point));
        }
        self.ops.push(op);
        self.event.push(std::mem::take(&mut self.pending));
        self.pace.push(std::mem::take(&mut self.pending_pace));
        self.step.push(std::mem::take(&mut self.mark));
        self.ops.len() - 1
    }

    /// A block starts here; events still pending are billed before it.
    pub(crate) fn leader(&mut self) {
        if !self.pending.is_none() || self.pending_pace != 0 || self.mark {
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
        slot
    }

    /// A hidden local.
    pub(crate) fn temp(&mut self) -> u16 {
        let slot = self.locals;
        self.locals += 1;
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

/// The compiler's program-wide state.
pub(crate) struct Compiler<'a> {
    pub document: &'a Document,
    pub registry: &'a FunctionRegistry,
    pub link: LinkId,
    pub units: Vec<Option<Unit>>,
    pub constants: Vec<Constant>,
    immediates: HashMap<Literal, u32>,
    pub natives: Vec<NativeEntry>,
    native_ids: HashMap<FunctionId, u32>,
    pub charges: Vec<Charge>,
    charge_ids: HashMap<FunctionId, u32>,
    pub functions: BTreeMap<FunctionId, UnitId>,
    pub globals: Vec<Name>,
    global_ids: HashMap<Name, u16>,
    bodies: HashMap<(FunctionId, bool), UnitId>,
    pub stubs: HashMap<(Option<FunctionId>, u8), UnitId>,
    free: HashMap<FunctionId, BTreeSet<Name>>,
    /// Each action site's occurrence counter index (`K-EFF-008`).
    occurrences: HashMap<Site, u32>,
}

impl<'a> Compiler<'a> {
    fn new(document: &'a Document, registry: &'a FunctionRegistry, link: LinkId) -> Self {
        Self {
            document,
            registry,
            link,
            units: Vec::new(),
            constants: Vec::new(),
            immediates: HashMap::new(),
            natives: Vec::new(),
            native_ids: HashMap::new(),
            charges: Vec::new(),
            charge_ids: HashMap::new(),
            functions: BTreeMap::new(),
            globals: Vec::new(),
            global_ids: HashMap::new(),
            bodies: HashMap::new(),
            stubs: HashMap::new(),
            free: HashMap::new(),
            occurrences: HashMap::new(),
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
        for (at, unit) in self.units.into_iter().enumerate() {
            match unit {
                Some(unit) => units.push(unit),
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
                .map(|(function, unit)| FunctionEntry { function, unit })
                .collect(),
            globals: self.globals,
            main,
        })
    }

    pub(crate) fn reserve(&mut self) -> UnitId {
        self.units.push(None);
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
        if let Some(at) = self.immediates.get(literal) {
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
        self.immediates.insert(literal.clone(), at);
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
        if let Some(free) = self.free.get(&id) {
            return Ok(free.clone());
        }
        // A closure that names itself recurses no further.
        self.free.insert(id, BTreeSet::new());
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
        self.free.insert(id, free.clone());
        Ok(free)
    }

    /// Compiles function `id`'s body as a unit. `outer` is the unit a
    /// closure is made in, whose variables it captures.
    pub(crate) fn function(
        &mut self,
        id: FunctionId,
        outer: Option<&Fun>,
        at: UnitId,
        covered: bool,
        kind: UnitKind,
    ) -> Result<(Vec<Capture>, Vec<Name>)> {
        let definition = self.def(&id)?;
        let Some(body) = definition.body() else {
            return unsupported(format!("function {id} has no kernel body"));
        };
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
        if let Some(unit) = self.bodies.get(&(id, covered)) {
            return Ok(*unit);
        }
        let at = self.reserve();
        self.bodies.insert((id, covered), at);
        let kind = if self.document.functions.contains_key(&id) {
            UnitKind::Declared(id)
        } else {
            UnitKind::Body(id)
        };
        match self.function(id, None, at, covered, kind) {
            Ok(_) => Ok(at),
            Err(error) => {
                self.bodies.remove(&(id, covered));
                // The reserved index keeps a unit that says why it cannot run.
                self.units[at as usize] = Some(self.unavailable(&error.0));
                Err(error)
            }
        }
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
        if let Some(at) = self.charge_ids.get(&id) {
            return Some(*at);
        }
        let formula = charge_tables::completion(definition, false)?;
        let at = self.formula(formula, definition);
        self.charge_ids.insert(id, at);
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
        let definition = self.def(&id)?;
        let covered = f.covered;
        if let Some(function) = machine_function(definition) {
            let charge = if covered {
                None
            } else {
                self.charge(id, definition)
            };
            return Ok(Callee::Machine { function, charge });
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

    fn native(
        &mut self,
        id: FunctionId,
        definition: &FunctionDefinition,
        native: Arc<dyn lash_kernel_doc::NativeFunction>,
    ) -> Result<u32> {
        if let Some(at) = self.native_ids.get(&id) {
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
            scalar @ Some(InlineOperation::Scalar { .. }) if guard.is_none() => scalar,
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
        self.native_ids.insert(id, at);
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
        if self.functions.contains_key(&id) {
            return Ok(());
        }
        let definition = self.def(&id)?;
        let at = self.reserve();
        self.functions.insert(id, at);
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
        let next = self.occurrences.len() as u32;
        let occurrence = *self.occurrences.entry(site.clone()).or_insert(next);
        Arc::new(Mark {
            statement: f.statement.clone(),
            loops: f.enclosing.iter().cloned().collect(),
            occurrence,
        })
    }

    /// `spawn`'s stub: its parameters are the call's arguments (a value
    /// callee first), and it makes the call. A task is no part of its
    /// spawner's call, so a covered spawner's cover does not reach it.
    pub(crate) fn stub(&mut self, f: &Fun, callee: Option<FunctionId>, argc: u8) -> Result<UnitId> {
        if let Some(unit) = self.stubs.get(&(callee, argc)) {
            return Ok(*unit);
        }
        let at = self.reserve();
        self.stubs.insert((callee, argc), at);
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
            Some(id) => self.callee(&g, id)?,
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
            match self.function(id, Some(f), at, false, UnitKind::Closure(id)) {
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
            pace,
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
        let mut pace_through = vec![0u64; ops.len()];
        let mut pc = 0;
        while pc < ops.len() {
            debug_assert!(matches!(ops[pc], Op::Block(_)), "a block starts every run");
            let start = pc;
            let mut charge = Some(0u64);
            let mut paced = Some(0u64);
            let mut sum = 0u64;
            let mut pace_sum = 0u64;
            pc += 1;
            while pc < ops.len() && !matches!(ops[pc], Op::Block(_)) {
                let cost = event[pc].total();
                charge = charge.and_then(|total| total.checked_add(cost));
                paced = paced.and_then(|total| total.checked_add(u64::from(pace[pc])));
                sum = sum.saturating_add(cost);
                pace_sum = pace_sum.saturating_add(u64::from(pace[pc]));
                through[pc] = sum;
                pace_through[pc] = pace_sum;
                pc += 1;
            }
            ops[start] = Op::Block(blocks.len() as u32);
            blocks.push(Block {
                start: start as u32,
                end: pc as u32,
                charge,
                pace: paced,
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
            covered,
            params,
            locals: locals.max(params),
            boxed,
            ops,
            event,
            pace,
            step,
            through,
            pace_through,
            blocks,
            records,
            closures,
            calls,
            loops: loop_sites,
            actions,
            action_marks,
            authored,
            name,
            site: Site::new(site, Vec::new()),
            points,
        }
    }

    /// A function call's machine identity, for `brand.built` answered
    /// where it is written.
    pub(crate) fn brand_built(&mut self, f: &Fun, id: FunctionId) -> Result<Option<u32>> {
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
