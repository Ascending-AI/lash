//! The executable: an admitted document compiled to tables the machine
//! walks.
//!
//! The executable is a cache (`docs/kernel/design.md` §5). It is derived
//! deterministically from the document and the registry, is never saved,
//! and nothing outside this crate sees its shape. Names are resolved to
//! frame slots, declared and library functions to table indexes, and every
//! node that an identity names keeps its site.
//!
//! It is compiled in two parts. A [`PreparedLibrary`] holds every library
//! body of a registry, compiled once for all the runs that use the
//! registry. Each immutable document's admission and own code are reused
//! at start and import against that library. Its code is numbered after
//! the library's, so preparation does not grow with the library. A
//! function's identity is its content, so the body compiled under it is
//! the one every document that lists it runs, and the one a parked run
//! that pins it resumes in.
//!
//! A [`Layout`] permutes the tables and the frame slots, and chooses
//! whether a native implementation's fast path answers the calls it can. No
//! value, effect identity or charge may depend on it; the laws compile one
//! document under several layouts and compare the runs.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::sync::Arc;

use lash_kernel_doc as doc;
use lash_kernel_doc::{
    Document, FunctionDefinition, FunctionId, FunctionRegistry, JoinMode, Name, NativeFunction,
    PreparedNative, Site, Type, Unit, Value,
};
use lash_kernel_exec::{
    BoundaryKind, KirError, LocalRef, Op, OpRange, StepEdge, StmtRanges, Tables as StreamTables,
    verify_stream,
};

use crate::functions::{MachineFunction, machine_function};

mod formula;
mod lower;
mod preparation;

pub(crate) use formula::{Charge, Fast, Plan, Source};

/// How the executable is laid out. Layout `0` is the natural order; any
/// other value permutes the code, block, statement and library tables and
/// each frame's slots, and an odd one also calls every native
/// implementation through its general call, never its fast path. It exists
/// for tests: every layout runs a document the same way.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Layout(pub u64);

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct CodeId(pub(crate) u32);
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct BlockId(pub(crate) u32);
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct StmtId(pub(crate) u32);
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct LibId(pub(crate) u32);
/// A variable of a code, by declaration order.
pub(crate) type Slot = u32;

/// A variable as the code that declares it reaches it: its slot, where the
/// slot sits in a frame, and whether a closure shares it. The compiler
/// writes the last two once the whole code is compiled, so the machine
/// reads a variable without looking up its code.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Local {
    pub(crate) slot: Slot,
    pub(crate) at: u32,
    pub(crate) shared: bool,
}

impl Local {
    /// A variable whose place is written when its code is complete.
    fn unplaced(slot: Slot) -> Self {
        Self {
            slot,
            at: slot,
            shared: false,
        }
    }
}

/// Compiled code: a library's bodies, or one document's own code.
#[derive(Default)]
struct Tables {
    codes: Vec<Code>,
    blocks: Vec<Block>,
    stmts: Vec<Stmt>,
    /// Every statement's ops, and what they index.
    stream: Stream,
    /// Each statement's step edge, by statement.
    edges: Vec<StepEdge>,
    /// Each function body by its site, and each block by its own: how a
    /// parked run's coordinates find their place in this layout.
    code_at: BTreeMap<Site, CodeId>,
    block_at: BTreeMap<Site, BlockId>,
}

impl Tables {
    /// Checks the ops of every statement a code of these tables runs, each
    /// with its code's frame, against `codes`, `blocks` and `libs` in all:
    /// the ids the ops name start before these tables' own, at `base`.
    fn verify_stream(
        &self,
        base: &Base,
        codes: usize,
        blocks: usize,
        libs: usize,
    ) -> Result<usize, KirError> {
        let mut stmts = Vec::with_capacity(self.stmts.len());
        for code in &self.codes {
            let mut pending = vec![code.body];
            while let Some(block) = pending.pop() {
                for id in &self.blocks[(block.0 - base.blocks) as usize].stmts {
                    let stmt = &self.stmts[(id.0 - base.stmts) as usize];
                    let test = match stmt.form {
                        Form::While { test, .. } => Some(test),
                        _ => None,
                    };
                    let ranges = StmtRanges {
                        main: stmt.ops,
                        test,
                        regs: stmt.regs,
                    };
                    stmts.push((ranges, code.slots.len()));
                    pending.extend(stmt.form.blocks());
                }
            }
        }
        let stream = &self.stream;
        let sizes = StreamTables {
            consts: stream.consts.len(),
            names: stream.names.len(),
            fields: stream.fields.len(),
            field_lists: stream.field_lists.iter().map(|list| list.len()).collect(),
            codes,
            blocks,
            libs,
        };
        verify_stream(&stream.ops, &stmts, &sizes)?;
        Ok(stmts.len())
    }
}

/// A function registry with every library body in it compiled, once, for
/// all the runs of all the documents that use it. An embedder prepares
/// its registry once and hands the same library to every [`Program`]
/// (`crate::Program`); each document is admitted and compiled once against
/// that library, then shared by its starts and imports.
///
/// Like the executable it is a cache: derived deterministically from the
/// registry and never saved. A body is found by its function's identity,
/// which is the function's content, so a parked run that pins a function
/// resumes in the body it parked in (`K-MACH-008`).
#[derive(Clone)]
pub struct PreparedLibrary(Arc<Prepared>);

struct Prepared {
    registry: Arc<FunctionRegistry>,
    layout: Layout,
    tables: Tables,
    /// Every registered function, in the layout's order.
    libs: Vec<Lib>,
    lib_of: BTreeMap<FunctionId, LibId>,
    /// Each function whose body reaches, through the bodies the machine
    /// runs, a function the registry does not hold: that function.
    missing: BTreeMap<FunctionId, FunctionId>,
    documents: std::sync::Mutex<preparation::Documents>,
}

impl fmt::Debug for PreparedLibrary {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PreparedLibrary")
            .field("functions", &self.0.libs.len())
            .field("bodies", &self.0.tables.codes.len())
            .finish_non_exhaustive()
    }
}

impl PreparedLibrary {
    /// Compiles every library body in `registry`.
    pub fn new(registry: Arc<FunctionRegistry>) -> Self {
        Self::with_layout(registry, Layout::default())
    }

    /// Checks the ops of every statement of the library's bodies
    /// (`lash_kernel_exec::verify_stream`): each range is well formed and
    /// spans no Boundary. Gives how many statements it checked.
    pub fn verify_stream(&self) -> Result<usize, KirError> {
        let tables = &self.0.tables;
        tables.verify_stream(
            &Base {
                codes: 0,
                blocks: 0,
                stmts: 0,
            },
            tables.codes.len(),
            tables.blocks.len(),
            self.0.libs.len(),
        )
    }

    /// The registry the library was prepared from.
    pub fn registry(&self) -> &Arc<FunctionRegistry> {
        &self.0.registry
    }

    /// This library laid out as `layout` says: itself, or for a test's
    /// other layout, the registry compiled afresh in that layout.
    pub(crate) fn in_layout(&self, layout: Layout) -> Self {
        if self.0.layout == layout {
            self.clone()
        } else {
            Self::with_layout(Arc::clone(&self.0.registry), layout)
        }
    }

    /// Compiles every library body in `registry` laid out as `layout`
    /// says. Every body gets a code, even one whose calls a native
    /// implementation runs: a parked run inside such a body resumes there
    /// (`K-MACH-008`). A test prepares its other layout once, and starts
    /// each run in it with [`crate::KernelMachine::start_with_layout`].
    pub fn with_layout(registry: Arc<FunctionRegistry>, layout: Layout) -> Self {
        let mut shuffler = Shuffler(layout.0);
        let mut library: Vec<(FunctionId, &doc::RegisteredFunction)> = registry
            .iter()
            .map(|(function, registered)| (*function, registered))
            .collect();
        shuffler.shuffle(layout, &mut library);
        let lib_of: BTreeMap<FunctionId, LibId> = library
            .iter()
            .enumerate()
            .map(|(index, (function, _))| (*function, LibId(index as u32)))
            .collect();
        let mut units: Vec<(FunctionId, &FunctionDefinition)> = library
            .iter()
            .filter(|(_, registered)| machine_function(registered.definition()).is_none())
            .filter(|(_, registered)| registered.definition().body().is_some())
            .map(|(function, registered)| (*function, registered.definition().as_ref()))
            .collect();
        shuffler.shuffle(layout, &mut units);

        let no_bindings = NO_BINDINGS;
        let mut compiler = Compiler::new(
            layout,
            shuffler,
            Resolve {
                lib_of: &lib_of,
                listed: None,
            },
            &no_bindings,
            Base::default(),
        );
        let codes: Vec<CodeId> = units.iter().map(|_| compiler.reserve()).collect();
        let mut bodies = BTreeMap::new();
        for ((function, definition), code) in units.iter().zip(codes) {
            bodies.insert(*function, code);
            if let Some(body) = definition.body() {
                let params: Vec<Name> = definition
                    .signature
                    .params
                    .iter()
                    .map(|param| param.name.clone())
                    .collect();
                // A helper's body is ordinary code; the body of a function
                // with a native implementation is covered by its formula.
                let unit = Unit::Library(*function);
                let charged = !definition.has_native();
                compiler.unit_code(code, unit, &params, &body.block, charged);
            }
        }
        let mut tables = compiler.finish();
        // A prepared library lives as long as its worker: its tables keep
        // what they hold, not what growing them reserved.
        tables.blocks.shrink_to_fit();
        tables.stmts.shrink_to_fit();
        tables.stream.ops.shrink_to_fit();
        let libs = library
            .iter()
            .map(|(function, registered)| {
                let definition = Arc::clone(registered.definition());
                let run = if let Some(machine) = machine_function(&definition) {
                    LibRun::Machine(machine)
                } else if let Some(native) = registered.native() {
                    LibRun::Native(Arc::clone(native))
                } else {
                    // A function with no native registered has a body, and
                    // the loop above gave every such body a code.
                    LibRun::Body(bodies.get(function).copied().unwrap_or(CodeId(0)))
                };
                let params = &definition.signature.params;
                let charge = Plan::new(&definition.charge, params);
                let limit = definition
                    .guard
                    .as_ref()
                    .map(|guard| Plan::new(&guard.limit, params));
                // A guarded call counts its work, which only the general
                // call does; an odd layout takes the general call always.
                let prepared = match &run {
                    LibRun::Native(native) if limit.is_none() && layout.0.is_multiple_of(2) => {
                        native.prepare()
                    }
                    _ => None,
                };
                let prepared =
                    prepared.map(|entry| (entry, Fast::new(&definition.charge, params, &charge)));
                Lib {
                    id: *function,
                    arity: params.len(),
                    native: definition.has_native(),
                    charge,
                    limit,
                    prepared,
                    definition,
                    run,
                }
            })
            .collect();
        let missing = missing(&registry);
        Self(Arc::new(Prepared {
            registry,
            layout,
            tables,
            libs,
            lib_of,
            missing,
            documents: Default::default(),
        }))
    }
}

/// Each function whose body reaches a function `registry` does not hold,
/// through the bodies of functions with no native implementation, and the
/// function it reaches.
fn missing(registry: &FunctionRegistry) -> BTreeMap<FunctionId, FunctionId> {
    let mut callers: BTreeMap<FunctionId, Vec<FunctionId>> = BTreeMap::new();
    for (function, registered) in registry.iter() {
        if registered.native().is_none()
            && let Some(body) = registered.definition().body()
        {
            for callee in body.functions.keys() {
                callers.entry(*callee).or_default().push(*function);
            }
        }
    }
    let mut pending: Vec<(FunctionId, FunctionId)> = callers
        .iter()
        .filter(|(callee, _)| registry.get(callee).is_none())
        .flat_map(|(absent, callers)| callers.iter().map(|caller| (*caller, *absent)))
        .collect();
    let mut missing = BTreeMap::new();
    while let Some((function, absent)) = pending.pop() {
        if missing.contains_key(&function) {
            continue;
        }
        missing.insert(function, absent);
        if let Some(callers) = callers.get(&function) {
            pending.extend(callers.iter().map(|caller| (*caller, absent)));
        }
    }
    missing
}

/// One run's executable: its document's own code over a prepared library.
/// The document's codes, blocks and statements are numbered after the
/// library's.
pub(crate) struct Executable {
    library: PreparedLibrary,
    document: Arc<Document>,
    own: Arc<DocumentCode>,
    admission: Arc<preparation::Admission>,
}

/// Immutable document tables, independent of any run or owning document.
pub(crate) struct DocumentCode {
    tables: Tables,
    pub(crate) main: CodeId,
    pub(crate) declared: BTreeMap<Name, CodeId>,
}

impl std::ops::Deref for Executable {
    type Target = DocumentCode;

    fn deref(&self) -> &Self::Target {
        &self.own
    }
}

/// The entry `index` names in a library's table followed by a document's.
fn entry<'t, T>(library: &'t [T], own: &'t [T], index: u32) -> Option<&'t T> {
    let index = index as usize;
    match index.checked_sub(library.len()) {
        None => library.get(index),
        Some(own_index) => own.get(own_index),
    }
}

impl Executable {
    pub(crate) fn get_code(&self, id: CodeId) -> Option<&Code> {
        entry(&self.library.0.tables.codes, &self.own.tables.codes, id.0)
    }

    pub(crate) fn get_stmt(&self, id: StmtId) -> Option<&Stmt> {
        entry(&self.library.0.tables.stmts, &self.own.tables.stmts, id.0)
    }

    #[expect(
        clippy::expect_used,
        reason = "every code id the machine holds was handed out by this executable"
    )]
    pub(crate) fn code(&self, id: CodeId) -> &Code {
        self.get_code(id).expect("a code of this executable")
    }

    #[expect(
        clippy::expect_used,
        reason = "every block id the machine holds was handed out by this executable"
    )]
    pub(crate) fn block(&self, id: BlockId) -> &Block {
        entry(&self.library.0.tables.blocks, &self.own.tables.blocks, id.0)
            .expect("a block of this executable")
    }

    #[expect(
        clippy::expect_used,
        reason = "every statement id the machine holds was handed out by this executable"
    )]
    pub(crate) fn stmt(&self, id: StmtId) -> &Stmt {
        self.get_stmt(id).expect("a statement of this executable")
    }

    /// Statement `id`, the stream its ops are in, and where its step
    /// leaves its frame when it completes in its own block.
    pub(crate) fn statement(&self, id: StmtId) -> (&Stream, &Stmt, StepEdge) {
        let library = &self.library.0.tables;
        let index = id.0 as usize;
        let (tables, at) = match index.checked_sub(library.stmts.len()) {
            None => (library, index),
            Some(own) => (&self.own.tables, own),
        };
        (&tables.stream, &tables.stmts[at], tables.edges[at])
    }

    /// Checks the document's statements' ops (`lash_kernel_exec::verify_stream`),
    /// giving how many it checked.
    pub(crate) fn verify_stream(&self) -> Result<usize, KirError> {
        let library = &self.library.0.tables;
        self.own.tables.verify_stream(
            &Base {
                codes: library.codes.len() as u32,
                blocks: library.blocks.len() as u32,
                stmts: library.stmts.len() as u32,
            },
            library.codes.len() + self.own.tables.codes.len(),
            library.blocks.len() + self.own.tables.blocks.len(),
            self.library.0.libs.len(),
        )
    }

    pub(crate) fn lib(&self, id: LibId) -> &Lib {
        &self.library.0.libs[id.0 as usize]
    }

    /// The library function `function`, when the document lists it.
    pub(crate) fn lib_of(&self, function: &FunctionId) -> Option<LibId> {
        if !self.document.manifest.functions.contains_key(function) {
            return None;
        }
        self.library.0.lib_of.get(function).copied()
    }

    /// The tables that hold `unit`'s code: the library's for a function
    /// the document lists, the document's own for its main and functions.
    fn tables_of(&self, unit: &Unit) -> Option<&Tables> {
        match unit {
            Unit::Library(function) => self
                .document
                .manifest
                .functions
                .contains_key(function)
                .then_some(&self.library.0.tables),
            Unit::Main | Unit::Function(_) => Some(&self.own.tables),
        }
    }

    /// The function body or closure body whose site is `site`.
    pub(crate) fn code_at(&self, site: &Site) -> Option<CodeId> {
        self.tables_of(&site.unit)?.code_at.get(site).copied()
    }

    /// The block whose site is `site`.
    pub(crate) fn block_at(&self, site: &Site) -> Option<BlockId> {
        self.tables_of(&site.unit)?.block_at.get(site).copied()
    }

    /// How many codes, library functions and statements the executable
    /// numbers.
    pub(crate) fn code_count(&self) -> usize {
        self.library.0.tables.codes.len() + self.own.tables.codes.len()
    }

    pub(crate) fn lib_count(&self) -> usize {
        self.library.0.libs.len()
    }

    pub(crate) fn stmt_count(&self) -> usize {
        self.library.0.tables.stmts.len() + self.own.tables.stmts.len()
    }

    /// Every block, the library's and the document's.
    pub(crate) fn blocks(&self) -> impl Iterator<Item = &Block> {
        self.library
            .0
            .tables
            .blocks
            .iter()
            .chain(&self.own.tables.blocks)
    }

    /// The registered name of a library function, listed or not.
    pub(crate) fn library_name(&self, function: &FunctionId) -> Option<&doc::FunctionName> {
        self.library
            .0
            .registry
            .get(function)
            .map(|registered| &registered.definition().name)
    }
}

/// One function body: `main`, a declared function, a library body or a
/// closure.
pub(crate) struct Code {
    /// The site of the body: a unit's, or a closure expression's child.
    pub(crate) site: Site,
    pub(crate) params: Vec<Local>,
    pub(crate) body: BlockId,
    pub(crate) slots: Vec<SlotInfo>,
    /// Where each slot sits in a frame.
    pub(crate) positions: Vec<u32>,
    /// The enclosing code's variables this closure shares.
    pub(crate) captures: Vec<Capture>,
    /// Whether the code's forms are charged: those of the body of a library
    /// function with a native implementation are not (`K-CHG-007`). That
    /// body calls only functions with a native implementation (`K-LIB-004`),
    /// so no charged frame runs above it, and a closure it makes is charged.
    pub(crate) charged: bool,
}

pub(crate) struct SlotInfo {
    pub(crate) name: Name,
    /// The node that declares the variable: its `let`, the block it is a
    /// parameter, a loop binding or a `catch` binding of, or, for a
    /// captured variable, the closure's body.
    pub(crate) declared: Site,
    /// A closure shares the variable, so it lives in a heap cell.
    pub(crate) shared: bool,
}

pub(crate) struct Capture {
    /// The slot in the enclosing code.
    pub(crate) outer: Slot,
    /// The slot in this code.
    pub(crate) inner: Slot,
    /// Where the two sit in their frames.
    pub(crate) outer_at: u32,
    pub(crate) inner_at: u32,
}

pub(crate) struct Block {
    pub(crate) site: Site,
    pub(crate) stmts: Vec<StmtId>,
    /// Where the variables the block declares sit in a frame; they end
    /// with it.
    pub(crate) declares: Vec<u32>,
}

pub(crate) struct Lib {
    pub(crate) id: FunctionId,
    pub(crate) definition: Arc<FunctionDefinition>,
    pub(crate) run: LibRun,
    /// How many parameters the definition has, and whether it states a
    /// native implementation.
    pub(crate) arity: usize,
    pub(crate) native: bool,
    /// The definition's charge formula, and its guard's limit.
    pub(crate) charge: Plan,
    pub(crate) limit: Option<Plan>,
    /// An unguarded native's prepared entry and its formula-derived charge.
    /// Odd layouts and heap-dependent natives use the general call.
    pub(crate) prepared: Option<(PreparedNative, Fast)>,
}

pub(crate) enum LibRun {
    Native(Arc<dyn NativeFunction>),
    Body(CodeId),
    Machine(MachineFunction),
}

pub(crate) enum Var {
    Local(Local),
    /// A session binding, looked up by name when it is read
    /// (`K-SES-001`).
    Session(Name),
    /// A name no enclosing scope declares (`K-FORM-003`).
    Unbound(Name),
}

/// The ops of every statement of a library or a document, and the
/// constants, names and fields they index.
#[derive(Default)]
pub(crate) struct Stream {
    pub(crate) ops: Vec<Op>,
    pub(crate) consts: Vec<Value>,
    pub(crate) names: Vec<Name>,
    pub(crate) fields: Vec<String>,
    pub(crate) field_lists: Vec<Box<[String]>>,
}

/// One statement: its ops in its stream, how many registers they use, and
/// what the machine's control needs of its shape.
pub(crate) struct Stmt {
    /// The range a step runs from the statement's start: its expressions,
    /// then the op that completes it or ends its step. An action
    /// statement's action is its first op, and the statement resumes at the
    /// op after it.
    pub(crate) ops: OpRange,
    pub(crate) regs: u32,
    pub(crate) form: Form,
}

pub(crate) enum Form {
    /// A statement its ops run to their end.
    Plain,
    If {
        then_block: BlockId,
        else_block: BlockId,
    },
    For {
        site: Site,
        binding: Local,
        body: BlockId,
    },
    While {
        site: Site,
        /// The condition's ops, which every iteration step runs.
        test: OpRange,
        body: BlockId,
    },
    Try {
        site: Site,
        body: BlockId,
        catch: Option<(Local, BlockId)>,
        finally: Option<BlockId>,
    },
    /// An action statement: `let`, an assignment or `do` of an action.
    Action(Action),
}

impl Stmt {
    /// Where the statement resumes when its action has a value.
    pub(crate) fn resume(&self) -> usize {
        self.ops.start as usize + 1
    }
}

impl Form {
    /// The blocks the statement holds.
    fn blocks(&self) -> Vec<BlockId> {
        match self {
            Form::Plain | Form::Action(_) => Vec::new(),
            Form::If {
                then_block,
                else_block,
            } => vec![*then_block, *else_block],
            Form::For { body, .. } | Form::While { body, .. } => vec![*body],
            Form::Try {
                body,
                catch,
                finally,
                ..
            } => [
                Some(*body),
                catch.as_ref().map(|(_, block)| *block),
                *finally,
            ]
            .into_iter()
            .flatten()
            .collect(),
        }
    }
}

pub(crate) struct Action {
    pub(crate) site: Site,
    pub(crate) kind: ActionKind,
}

pub(crate) enum ActionKind {
    Call {
        callee: Callee,
        args: Vec<Atom>,
    },
    Perform {
        effect: doc::EffectName,
        args: Vec<Atom>,
        result: Type,
    },
    Sleep(Atom),
    Join(Atom),
    JoinMany(JoinMode, Atom),
    Yield,
    Spawn {
        callee: Callee,
        args: Vec<Atom>,
    },
    Cancel(Atom),
}

pub(crate) enum Callee {
    Declared(CodeId),
    Value(Var),
    Library(LibId),
}

pub(crate) enum Atom {
    Var(Var),
    Literal(Value),
}

/// A program the machine cannot compile: a function the registry lacks.
#[derive(Clone, Copy)]
pub(crate) struct Missing(pub(crate) FunctionId);

/// A small deterministic generator for layouts (splitmix64).
struct Shuffler(u64);

impl Shuffler {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^ (z >> 31)
    }

    fn shuffle<T>(&mut self, layout: Layout, items: &mut [T]) {
        if layout.0 == 0 {
            return;
        }
        for last in (1..items.len()).rev() {
            let pick = (self.next() % (last as u64 + 1)) as usize;
            items.swap(last, pick);
        }
    }
}

/// The scopes of one code while it is compiled.
struct Scopes {
    main: bool,
    /// The site of the code's body.
    body: Site,
    /// The innermost block last; in each, the latest declaration last.
    blocks: Vec<Vec<(Name, Declared)>>,
    slots: Vec<SlotInfo>,
    captures: Vec<Capture>,
}

#[derive(Clone, Copy)]
enum Declared {
    Slot(Slot),
    Session,
}

/// The session bindings a library body declares: none.
const NO_BINDINGS: BTreeSet<Name> = BTreeSet::new();

/// How a compiled call names a library function.
#[derive(Clone, Copy)]
struct Resolve<'a> {
    lib_of: &'a BTreeMap<FunctionId, LibId>,
    /// The functions a document lists; a library body's calls resolve
    /// against the whole registry, which its registration checked.
    listed: Option<&'a BTreeMap<FunctionId, doc::FunctionName>>,
}

impl Resolve<'_> {
    fn lib(&self, function: &FunctionId) -> Option<LibId> {
        if self
            .listed
            .is_some_and(|listed| !listed.contains_key(function))
        {
            return None;
        }
        self.lib_of.get(function).copied()
    }
}

/// The first id of each table a compilation adds to: zero for a library,
/// and for a document the length of its library's tables.
#[derive(Clone, Copy, Default)]
struct Base {
    codes: u32,
    blocks: u32,
    stmts: u32,
}

struct Compiler<'a> {
    /// The session bindings `main` keeps private.
    private: &'a BTreeSet<Name>,
    layout: Layout,
    shuffler: Shuffler,
    base: Base,
    codes: Vec<Option<Code>>,
    blocks: Vec<Block>,
    stmts: Vec<Stmt>,
    stream: Stream,
    /// Where each name and field already sits in the stream's pools.
    names: BTreeMap<Name, u32>,
    fields: BTreeMap<String, u32>,
    /// Emptied buffers that statements gather their ops in.
    spare: Vec<Vec<Op>>,
    declared: BTreeMap<Name, CodeId>,
    resolve: Resolve<'a>,
    /// The codes being compiled, the outermost first; each closure adds one.
    scopes: Vec<Scopes>,
    unit: Unit,
    path: Vec<u32>,
}

enum UnitSource<'a> {
    Main,
    Function(&'a Name),
}

/// Compiles `document`'s own code over `library`, which holds every
/// library body it reaches.
fn compile_document(
    document: &Document,
    library: &PreparedLibrary,
) -> Result<DocumentCode, Missing> {
    let prepared = &library.0;
    if !prepared.missing.is_empty() {
        for function in document.manifest.functions.keys() {
            if let Some(absent) = prepared.missing.get(function) {
                return Err(Missing(*absent));
            }
        }
    }
    let layout = prepared.layout;
    let mut shuffler = Shuffler(layout.0);
    let mut units: Vec<UnitSource<'_>> = vec![UnitSource::Main];
    units.extend(document.functions.keys().map(UnitSource::Function));
    shuffler.shuffle(layout, &mut units);
    let mut compiler = Compiler::new(
        layout,
        shuffler,
        Resolve {
            lib_of: &prepared.lib_of,
            listed: Some(&document.manifest.functions),
        },
        &document.private_bindings,
        Base {
            codes: prepared.tables.codes.len() as u32,
            blocks: prepared.tables.blocks.len() as u32,
            stmts: prepared.tables.stmts.len() as u32,
        },
    );
    let codes: Vec<CodeId> = units.iter().map(|_| compiler.reserve()).collect();
    let mut main = codes.first().copied().unwrap_or(CodeId(0));
    for (unit, code) in units.iter().zip(&codes) {
        match unit {
            UnitSource::Main => main = *code,
            UnitSource::Function(name) => {
                compiler.declared.insert((*name).clone(), *code);
            }
        }
    }
    for (unit, code) in units.iter().zip(codes) {
        match unit {
            UnitSource::Main => compiler.unit_code(code, Unit::Main, &[], &document.main, true),
            UnitSource::Function(name) => {
                if let Some(function) = document.functions.get(*name) {
                    let unit = Unit::Function((*name).clone());
                    compiler.unit_code(code, unit, &function.params, &function.body, true);
                }
            }
        }
    }
    let declared = std::mem::take(&mut compiler.declared);
    Ok(DocumentCode {
        tables: compiler.finish(),
        main,
        declared,
    })
}

impl<'a> Compiler<'a> {
    fn new(
        layout: Layout,
        shuffler: Shuffler,
        resolve: Resolve<'a>,
        private: &'a BTreeSet<Name>,
        base: Base,
    ) -> Self {
        Self {
            private,
            layout,
            shuffler,
            base,
            codes: Vec::new(),
            blocks: Vec::new(),
            stmts: Vec::new(),
            stream: Stream::default(),
            names: BTreeMap::new(),
            fields: BTreeMap::new(),
            spare: Vec::new(),
            declared: BTreeMap::new(),
            resolve,
            scopes: Vec::new(),
            unit: Unit::Main,
            path: Vec::new(),
        }
    }

    /// Takes the next code id, for a unit compiled later.
    fn reserve(&mut self) -> CodeId {
        let id = CodeId(self.base.codes + self.codes.len() as u32);
        self.codes.push(None);
        id
    }

    /// The compiled tables, with every body and block found by its site.
    fn finish(self) -> Tables {
        let base = self.base;
        let codes: Vec<Code> = self.codes.into_iter().flatten().collect();
        let code_at = codes
            .iter()
            .enumerate()
            .map(|(index, code)| (code.site.clone(), CodeId(base.codes + index as u32)))
            .collect();
        let block_at = self
            .blocks
            .iter()
            .enumerate()
            .map(|(index, block)| (block.site.clone(), BlockId(base.blocks + index as u32)))
            .collect();
        let edges = step_edges(&codes, &self.blocks, &self.stmts, base);
        Tables {
            codes,
            blocks: self.blocks,
            stmts: self.stmts,
            stream: self.stream,
            edges,
            code_at,
            block_at,
        }
    }
}

/// The step edge of a statement that is not the last of its block: the
/// next statement starts the next step.
const ON: StepEdge = StepEdge {
    leaves: 0,
    next: BoundaryKind::StatementStart,
};

/// The step edge of the last statement of a function's, a loop's or a
/// `try`'s block: the block's end is a step of its own.
const TO_END: StepEdge = StepEdge {
    leaves: 0,
    next: BoundaryKind::BlockEnd,
};

/// Each statement's step edge, by statement: where the statement's step
/// leaves its frame when it completes in its own block. Only an `if` arm
/// sits right inside another block of its frame, so only the step of an
/// arm's last statement leaves blocks: the arm, and whatever the step of
/// the `if` itself leaves.
fn step_edges(codes: &[Code], blocks: &[Block], stmts: &[Stmt], base: Base) -> Vec<StepEdge> {
    let mut edges = vec![ON; stmts.len()];
    // A block, and the edge of its last statement.
    let mut pending: Vec<(BlockId, StepEdge)> =
        codes.iter().map(|code| (code.body, TO_END)).collect();
    while let Some((block, last)) = pending.pop() {
        let ids = &blocks[(block.0 - base.blocks) as usize].stmts;
        for (index, id) in ids.iter().enumerate() {
            let edge = if index + 1 == ids.len() { last } else { ON };
            let at = (id.0 - base.stmts) as usize;
            edges[at] = edge;
            match &stmts[at].form {
                Form::If {
                    then_block,
                    else_block,
                } => {
                    let arm = StepEdge {
                        leaves: edge.leaves + 1,
                        next: edge.next,
                    };
                    pending.extend([(*then_block, arm), (*else_block, arm)]);
                }
                Form::For { body, .. } | Form::While { body, .. } => {
                    pending.push((*body, TO_END));
                }
                Form::Try {
                    body,
                    catch,
                    finally,
                    ..
                } => {
                    pending.push((*body, TO_END));
                    pending.extend(catch.map(|(_, block)| (block, TO_END)));
                    pending.extend(finally.map(|block| (block, TO_END)));
                }
                _ => {}
            }
        }
    }
    edges
}

impl Compiler<'_> {
    fn site(&self) -> Site {
        Site::new(self.unit.clone(), self.path.clone())
    }

    /// Compiles one unit's body into the code slot reserved for it.
    fn unit_code(
        &mut self,
        code: CodeId,
        unit: Unit,
        params: &[Name],
        body: &doc::Block,
        charged: bool,
    ) {
        let main = unit == Unit::Main;
        self.unit = unit;
        self.path.clear();
        let compiled = self.code(params, body, main, charged);
        self.codes[(code.0 - self.base.codes) as usize] = Some(compiled);
    }

    /// Compiles a function body in a scope of its own. The path addresses
    /// the body block.
    fn code(&mut self, params: &[Name], body: &doc::Block, main: bool, charged: bool) -> Code {
        let site = self.site();
        self.scopes.push(Scopes {
            main,
            body: site.clone(),
            blocks: Vec::new(),
            slots: Vec::new(),
            captures: Vec::new(),
        });
        let (body, params) = self.block(body, params);
        let scopes = self.scopes.pop().unwrap_or(Scopes {
            main,
            body: site.clone(),
            blocks: Vec::new(),
            slots: Vec::new(),
            captures: Vec::new(),
        });
        let mut positions: Vec<u32> = (0..scopes.slots.len() as u32).collect();
        self.shuffler.shuffle(self.layout, &mut positions);
        let placer = Placer {
            positions: &positions,
            slots: &scopes.slots,
        };
        let params = params
            .into_iter()
            .map(|slot| placer.placed(Local::unplaced(slot)))
            .collect();
        let mut captures = scopes.captures;
        for capture in &mut captures {
            capture.inner_at = positions[capture.inner as usize];
        }
        // Lowered, every variable already sits at its slot and is bound as
        // unshared: a code whose layout keeps the slots in place and that
        // shares none is placed as it is.
        let moved = positions
            .iter()
            .enumerate()
            .any(|(slot, at)| slot as u32 != *at);
        if moved || scopes.slots.iter().any(|slot| slot.shared) {
            self.place(body, &placer);
        }
        Code {
            site,
            params,
            body,
            slots: scopes.slots,
            positions,
            captures,
            charged,
        }
    }

    /// Writes where each variable of a code sits into the ops and the
    /// shapes of its statements, which its blocks hold, and into the
    /// captures of the closures it makes.
    fn place(&mut self, body: BlockId, placer: &Placer<'_>) {
        let mut blocks = vec![body];
        let mut closures = Vec::new();
        while let Some(block) = blocks.pop() {
            let block = &mut self.blocks[(block.0 - self.base.blocks) as usize];
            for declared in &mut block.declares {
                *declared = placer.positions[*declared as usize];
            }
            for stmt in &block.stmts {
                let stmt = &mut self.stmts[(stmt.0 - self.base.stmts) as usize];
                placer.form(&mut stmt.form, &mut blocks);
                let mut ranges = [Some(stmt.ops), None];
                if let Form::While { test, .. } = stmt.form {
                    ranges[1] = Some(test);
                }
                for range in ranges.into_iter().flatten() {
                    let ops = &mut self.stream.ops[range.start as usize..range.end() as usize];
                    placer.ops(ops, &mut closures);
                }
            }
        }
        for closure in closures {
            if let Some(Some(code)) = self.codes.get_mut((closure.0 - self.base.codes) as usize) {
                for capture in &mut code.captures {
                    capture.outer_at = placer.positions[capture.outer as usize];
                }
            }
        }
    }

    /// Declares a variable at the node being compiled.
    fn declare(&mut self, name: &Name) -> Slot {
        let declared = self.site();
        let Some(scopes) = self.scopes.last_mut() else {
            return 0;
        };
        let slot = scopes.slots.len() as Slot;
        scopes.slots.push(SlotInfo {
            name: name.clone(),
            declared,
            shared: false,
        });
        if let Some(block) = scopes.blocks.last_mut() {
            block.push((name.clone(), Declared::Slot(slot)));
        }
        slot
    }

    /// Compiles a block, declaring `bound` in its scope first: a function's
    /// parameters, a loop's binding, a catch's binding.
    fn block(&mut self, block: &doc::Block, bound: &[Name]) -> (BlockId, Vec<Slot>) {
        if let Some(scopes) = self.scopes.last_mut() {
            scopes.blocks.push(Vec::new());
        }
        let bound: Vec<Slot> = bound.iter().map(|name| self.declare(name)).collect();
        let mut stmts = Vec::with_capacity(block.len());
        for (index, stmt) in block.iter().enumerate() {
            self.path.push(index as u32);
            let stmt = self.stmt(stmt);
            self.path.pop();
            stmts.push(stmt);
        }
        let declared = self
            .scopes
            .last_mut()
            .and_then(|scopes| scopes.blocks.pop())
            .unwrap_or_default();
        let declares = declared
            .into_iter()
            .filter_map(|(_, declared)| match declared {
                Declared::Slot(slot) => Some(slot),
                Declared::Session => None,
            })
            .collect();
        // Statement and block ids are taken in the order the layout visits
        // the units, so they differ between layouts.
        let id = BlockId(self.base.blocks + self.blocks.len() as u32);
        self.blocks.push(Block {
            site: self.site(),
            stmts: Vec::new(),
            declares,
        });
        let ids = stmts
            .into_iter()
            .map(|stmt| {
                let id = StmtId(self.base.stmts + self.stmts.len() as u32);
                self.stmts.push(stmt);
                id
            })
            .collect();
        self.blocks[(id.0 - self.base.blocks) as usize].stmts = ids;
        (id, bound)
    }

    fn child<T>(&mut self, index: u32, compile: impl FnOnce(&mut Self) -> T) -> T {
        self.path.push(index);
        let compiled = compile(self);
        self.path.pop();
        compiled
    }

    fn resolve(&mut self, name: &Name) -> Var {
        match self.scopes.len() {
            0 => Var::Unbound(name.clone()),
            depth => self.resolve_in(depth - 1, name),
        }
    }

    /// Resolves `name` as code `level` sees it. A variable of an enclosing
    /// code becomes a capture of every closure between the two.
    fn resolve_in(&mut self, level: usize, name: &Name) -> Var {
        let scopes = &self.scopes[level];
        let declared = scopes
            .blocks
            .iter()
            .rev()
            .flat_map(|block| block.iter().rev())
            .find(|(declared, _)| declared == name)
            .map(|(_, declared)| *declared);
        match declared {
            Some(Declared::Slot(slot)) => return Var::Local(Local::unplaced(slot)),
            Some(Declared::Session) => return Var::Session(name.clone()),
            None => {}
        }
        if level == 0 {
            return if scopes.main {
                Var::Session(name.clone())
            } else {
                Var::Unbound(name.clone())
            };
        }
        // A capture already taken for this name is the same variable: the
        // closure sees its defining scope as it was at the closure.
        let taken = scopes
            .captures
            .iter()
            .find(|capture| scopes.slots[capture.inner as usize].name == *name)
            .map(|capture| capture.inner);
        if let Some(inner) = taken {
            return Var::Local(Local::unplaced(inner));
        }
        match self.resolve_in(level - 1, name) {
            Var::Local(Local { slot: outer, .. }) => {
                self.scopes[level - 1].slots[outer as usize].shared = true;
                let scopes = &mut self.scopes[level];
                let inner = scopes.slots.len() as Slot;
                scopes.slots.push(SlotInfo {
                    name: name.clone(),
                    declared: scopes.body.clone(),
                    shared: true,
                });
                scopes.captures.push(Capture {
                    outer,
                    inner,
                    outer_at: outer,
                    inner_at: inner,
                });
                Var::Local(Local::unplaced(inner))
            }
            other => other,
        }
    }
}

/// Where the variables of one compiled code sit in its frames.
struct Placer<'p> {
    positions: &'p [u32],
    slots: &'p [SlotInfo],
}

impl Placer<'_> {
    fn placed(&self, local: Local) -> Local {
        Local {
            slot: local.slot,
            at: self.positions[local.slot as usize],
            shared: self.slots[local.slot as usize].shared,
        }
    }

    fn local(&self, local: &mut Local) {
        *local = self.placed(*local);
    }

    fn var(&self, var: &mut Var) {
        if let Var::Local(local) = var {
            self.local(local);
        }
    }

    /// Places a statement's ops' variables, a binding of a variable a
    /// closure shares as its cell, and collects the closures they make.
    fn ops(&self, ops: &mut [Op], closures: &mut Vec<CodeId>) {
        for op in ops {
            match op {
                Op::Local { local, .. }
                | Op::ChargeLocal { local, .. }
                | Op::WriteLocal { local, .. } => {
                    local.at = self.positions[local.slot as usize];
                }
                Op::Bind { local, src } => {
                    let (local, src) = (self.place_ref(*local), *src);
                    *op = if self.slots[local.slot as usize].shared {
                        Op::BindShared { local, src }
                    } else {
                        Op::Bind { local, src }
                    };
                }
                Op::Closure { code, .. } => closures.push(CodeId(code.0)),
                _ => {}
            }
        }
    }

    fn place_ref(&self, local: LocalRef) -> LocalRef {
        LocalRef {
            slot: local.slot,
            at: self.positions[local.slot as usize],
        }
    }

    /// Places the variables a statement's shape names, and collects the
    /// blocks it holds.
    fn form(&self, form: &mut Form, blocks: &mut Vec<BlockId>) {
        match form {
            Form::Plain => {}
            Form::If {
                then_block,
                else_block,
            } => blocks.extend([*then_block, *else_block]),
            Form::For { binding, body, .. } => {
                self.local(binding);
                blocks.push(*body);
            }
            Form::While { body, .. } => blocks.push(*body),
            Form::Try {
                body,
                catch,
                finally,
                ..
            } => {
                blocks.push(*body);
                if let Some((binding, block)) = catch {
                    self.local(binding);
                    blocks.push(*block);
                }
                blocks.extend(*finally);
            }
            Form::Action(action) => self.action(action),
        }
    }

    fn action(&self, action: &mut Action) {
        let (callee, atoms): (Option<&mut Callee>, &mut [Atom]) = match &mut action.kind {
            ActionKind::Call { callee, args } | ActionKind::Spawn { callee, args } => {
                (Some(callee), args)
            }
            ActionKind::Perform { args, .. } => (None, args),
            ActionKind::Sleep(atom)
            | ActionKind::Join(atom)
            | ActionKind::JoinMany(_, atom)
            | ActionKind::Cancel(atom) => (None, std::slice::from_mut(atom)),
            ActionKind::Yield => (None, &mut []),
        };
        if let Some(Callee::Value(var)) = callee {
            self.var(var);
        }
        for atom in atoms {
            if let Atom::Var(var) = atom {
                self.var(var);
            }
        }
    }
}

fn literal_value(literal: &doc::Literal) -> Value {
    match literal {
        doc::Literal::Null => Value::Null,
        doc::Literal::Absent => Value::Absent,
        doc::Literal::Bool(flag) => Value::Bool(*flag),
        doc::Literal::Int(integer) => Value::Int(integer.clone()),
        doc::Literal::Float(float) => Value::Float(*float),
        doc::Literal::Text(text) => Value::text(text.as_str()),
        doc::Literal::Bytes(bytes) => Value::Bytes(bytes.clone()),
        doc::Literal::Function(name) => Value::Function(name.clone()),
    }
}
