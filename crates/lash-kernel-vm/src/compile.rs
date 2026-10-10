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
//! registry. A run compiles only its document's own code, numbered after
//! the library's, so its start does not grow with the library. A
//! function's identity is its content, so the body compiled under it is
//! the one every document that lists it runs, and the one a parked run
//! that pins it resumes in.
//!
//! A [`Layout`] permutes the tables and the frame slots. No value, effect
//! identity or charge may depend on it; the laws compile one document under
//! several layouts and compare the runs.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::sync::Arc;

use lash_kernel_doc as doc;
use lash_kernel_doc::{
    Document, FunctionDefinition, FunctionId, FunctionRegistry, JoinMode, Name, NativeFunction,
    Site, Type, Unit, Value,
};

use crate::functions::{MachineFunction, machine_function};

mod formula;
pub(crate) mod pe;

pub(crate) use formula::{Plan, Source};

/// How the executable is laid out. Layout `0` is the natural order; any
/// other value permutes the code, block, statement and library tables and
/// each frame's slots. It exists for tests: every layout runs a document
/// the same way.
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
    /// Each function body by its site, and each block by its own: how a
    /// parked run's coordinates find their place in this layout.
    code_at: BTreeMap<Site, CodeId>,
    block_at: BTreeMap<Site, BlockId>,
}

/// A function registry with every library body in it compiled, once, for
/// all the runs of all the documents that use it. An embedder prepares
/// its registry once and hands the same library to every [`Program`]
/// (`crate::Program`); a run's start then compiles only its document.
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
    /// The residual variants of each library code, by code id: the first
    /// whose guards an activation's inputs pass runs instead of the code
    /// (FIG-5863 spike). Empty unless the library was specialized.
    variants: Vec<Box<[pe::Variant]>>,
    /// What partial evaluation made, for the spike's report.
    report: Vec<pe::CodeReport>,
    /// Whether activations count the variant they run.
    counting: bool,
    /// Whether activations count every variant whose guards they pass and
    /// run the generic code.
    profiling: bool,
    /// The first residual code id: every code before it is generic.
    generic_codes: u32,
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
    /// (`K-MACH-008`).
    pub(crate) fn with_layout(registry: Arc<FunctionRegistry>, layout: Layout) -> Self {
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
            .filter(|(_, registered)| machine_function(&registered.definition).is_none())
            .filter(|(_, registered)| registered.definition.body().is_some())
            .map(|(function, registered)| (*function, registered.definition.as_ref()))
            .collect();
        shuffler.shuffle(layout, &mut units);

        let mut compiler = Compiler::new(
            layout,
            shuffler,
            Resolve {
                lib_of: &lib_of,
                listed: None,
            },
            &NO_BINDINGS,
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
        let tables = compiler.finish();
        let libs = library
            .iter()
            .map(|(function, registered)| {
                let definition = Arc::clone(&registered.definition);
                let run = if let Some(machine) = machine_function(&definition) {
                    LibRun::Machine(machine)
                } else if let Some(native) = &registered.native {
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
                Lib {
                    id: *function,
                    arity: params.len(),
                    native: definition.has_native(),
                    charge,
                    limit,
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
            variants: Vec::new(),
            report: Vec::new(),
            counting: false,
            profiling: false,
            generic_codes: u32::MAX,
        }))
    }

    /// This library with residual variants of its hot library bodies and
    /// closures, made by partial evaluation over the prepared form
    /// (FIG-5863 spike). The variants are private derivatives of the same
    /// bodies: a run is the same whichever runs.
    pub fn specialized(registry: Arc<FunctionRegistry>, options: &pe::Options) -> Self {
        let plain = Self::with_layout(registry, Layout::default());
        let Ok(mut prepared) = Arc::try_unwrap(plain.0) else {
            unreachable!("a library just prepared has one owner");
        };
        prepared.generic_codes = prepared.tables.codes.len() as u32;
        let (variants, report) = pe::specialize(&mut prepared.tables, &prepared.libs, options);
        prepared.variants = variants;
        prepared.report = report;
        prepared.counting = options.count;
        prepared.profiling = matches!(options.selection, pe::Selection::Profiling);
        Self(Arc::new(prepared))
    }

    /// What partial evaluation made (FIG-5863 spike).
    pub fn pe_report(&self) -> &[pe::CodeReport] {
        &self.0.report
    }
}

/// Each function whose body reaches a function `registry` does not hold,
/// through the bodies of functions with no native implementation, and the
/// function it reaches.
fn missing(registry: &FunctionRegistry) -> BTreeMap<FunctionId, FunctionId> {
    let mut callers: BTreeMap<FunctionId, Vec<FunctionId>> = BTreeMap::new();
    for (function, registered) in registry.iter() {
        if registered.native.is_none()
            && let Some(body) = registered.definition.body()
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
    own: Tables,
    pub(crate) main: CodeId,
    pub(crate) declared: BTreeMap<Name, CodeId>,
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
        entry(&self.library.0.tables.codes, &self.own.codes, id.0)
    }

    pub(crate) fn get_stmt(&self, id: StmtId) -> Option<&Stmt> {
        entry(&self.library.0.tables.stmts, &self.own.stmts, id.0)
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
        entry(&self.library.0.tables.blocks, &self.own.blocks, id.0)
            .expect("a block of this executable")
    }

    #[expect(
        clippy::expect_used,
        reason = "every statement id the machine holds was handed out by this executable"
    )]
    pub(crate) fn stmt(&self, id: StmtId) -> &Stmt {
        self.get_stmt(id).expect("a statement of this executable")
    }

    pub(crate) fn lib(&self, id: LibId) -> &Lib {
        &self.library.0.libs[id.0 as usize]
    }

    /// Whether `code` is a residual variant of a library code.
    pub(crate) fn is_residual(&self, code: CodeId) -> bool {
        code.0 >= self.library.0.generic_codes
            && (code.0 as usize) < self.library.0.tables.codes.len()
    }

    pub(crate) fn profiles_variants(&self) -> bool {
        self.library.0.profiling
    }

    pub(crate) fn counts_variants(&self) -> bool {
        self.library.0.counting
    }

    /// The residual variants of `code`, best first.
    #[inline]
    pub(crate) fn variants(&self, code: CodeId) -> &[pe::Variant] {
        self.library
            .0
            .variants
            .get(code.0 as usize)
            .map_or(&[], |variants| &variants[..])
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
            Unit::Main | Unit::Function(_) => Some(&self.own),
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

#[derive(Clone)]
pub(crate) struct SlotInfo {
    pub(crate) name: Name,
    /// The node that declares the variable: its `let`, the block it is a
    /// parameter, a loop binding or a `catch` binding of, or, for a
    /// captured variable, the closure's body.
    pub(crate) declared: Site,
    /// A closure shares the variable, so it lives in a heap cell.
    pub(crate) shared: bool,
}

#[derive(Clone)]
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
}

pub(crate) enum LibRun {
    Native(Arc<dyn NativeFunction>),
    Body(CodeId),
    Machine(MachineFunction),
}

#[derive(Clone)]
pub(crate) enum Var {
    Local(Local),
    /// A session binding, looked up by name when it is read
    /// (`K-SES-001`).
    Session(Name),
    /// A name no enclosing scope declares (`K-FORM-003`).
    Unbound(Name),
}

#[derive(Clone)]
pub(crate) enum Stmt {
    Let {
        target: Target,
        value: Rhs,
    },
    Assign {
        place: Place,
        value: Rhs,
    },
    Remove(Member),
    Do(Action),
    If {
        condition: Expr,
        then_block: BlockId,
        else_block: BlockId,
    },
    For {
        site: Site,
        binding: Local,
        iterable: Expr,
        body: BlockId,
    },
    While {
        site: Site,
        condition: Expr,
        body: BlockId,
    },
    Break,
    Continue,
    Return(Expr),
    Try {
        site: Site,
        body: BlockId,
        catch: Option<(Local, BlockId)>,
        finally: Option<BlockId>,
    },
    Throw(Expr),
    Print(Expr),
    Finish(Expr),
    Fail(Expr),
}

#[derive(Clone)]
pub(crate) enum Target {
    Slot(Local),
    Session(Name),
}

#[derive(Clone)]
pub(crate) enum Rhs {
    Expr(Expr),
    Action(Action),
}

#[derive(Clone)]
pub(crate) enum Place {
    Var(Var),
    Member(Member),
}

#[derive(Clone)]
pub(crate) enum Member {
    Field(Expr, String),
    Index(Expr, Expr),
}

#[derive(Clone)]
pub(crate) struct Action {
    pub(crate) site: Site,
    pub(crate) kind: ActionKind,
}

#[derive(Clone)]
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

#[derive(Clone)]
pub(crate) enum Callee {
    Declared(CodeId),
    Value(Var),
    Library(LibId),
}

#[derive(Clone)]
pub(crate) enum Atom {
    Var(Var),
    Literal(Value),
}

#[derive(Clone)]
pub(crate) enum Expr {
    Literal(Value),
    Var(Var),
    Tuple(Vec<Expr>),
    List(Vec<Expr>),
    Map(Vec<(Expr, Expr)>),
    Set(Vec<Expr>),
    Record(Vec<(String, Expr)>),
    Member(Box<Member>),
    Closure(CodeId),
    Call {
        lib: LibId,
        args: Vec<Expr>,
    },
    Clock,
    Random,
    Read(Box<(Expr, Expr)>),
    /// A subexpression partial evaluation computed (FIG-5863 spike): its
    /// value is known, and evaluating it replays the charges and pins the
    /// original expression made, in order, without doing its work.
    Folded(Box<Folded>),
}

/// What a folded expression does when it is evaluated.
#[derive(Clone, Debug)]
pub(crate) struct Folded {
    pub(crate) events: Box<[Event]>,
    pub(crate) value: Value,
}

/// One observable event of a folded expression, in the order the original
/// expression made it.
#[derive(Clone, Debug)]
pub(crate) enum Event {
    /// Charges `total`, the sum of `parts`, which the original charged one
    /// at a time: a run that crosses its bound charges them one at a time,
    /// so it stops at the part that crosses, as the original did. A part
    /// charged inside a library call names that function when it crosses.
    Charge {
        total: u64,
        parts: Box<[u64]>,
        lib: Option<LibId>,
    },
    /// Keeps a native call's result live until the statement ends, as the
    /// call did, accounting its bytes.
    Pin { value: Value, lib: LibId },
}

/// A program the machine cannot compile: a function the registry lacks.
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
static NO_BINDINGS: BTreeSet<Name> = BTreeSet::new();

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
pub(crate) fn compile(
    document: &Arc<Document>,
    library: &PreparedLibrary,
) -> Result<Executable, Missing> {
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
    Ok(Executable {
        library: library.clone(),
        document: Arc::clone(document),
        own: compiler.finish(),
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
        Tables {
            codes,
            blocks: self.blocks,
            stmts: self.stmts,
            code_at,
            block_at,
        }
    }
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
        self.place(body, &placer);
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

    /// Writes where each variable of a code sits into the code's blocks
    /// and statements, and into the captures of the closures it makes.
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
                placer.stmt(stmt, &mut blocks, &mut closures);
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

    fn stmt(&mut self, stmt: &doc::Stmt) -> Stmt {
        match stmt {
            doc::Stmt::Let { name, value } => {
                // The right-hand side does not see the variable it binds.
                let value = self.child(0, |c| c.rhs(value));
                let top_level = self.scopes.len() == 1
                    && self
                        .scopes
                        .last()
                        .is_some_and(|scopes| scopes.main && scopes.blocks.len() == 1);
                let target = if top_level && !self.private.contains(name) {
                    if let Some(block) = self.scopes.last_mut().and_then(|s| s.blocks.last_mut()) {
                        block.push((name.clone(), Declared::Session));
                    }
                    Target::Session(name.clone())
                } else {
                    Target::Slot(Local::unplaced(self.declare(name)))
                };
                Stmt::Let { target, value }
            }
            doc::Stmt::Assign { place, value } => {
                let (place, next) = match place {
                    doc::Place::Variable(name) => (Place::Var(self.resolve(name)), 0),
                    doc::Place::Member(member) => {
                        let (member, next) = self.member(member, 0);
                        (Place::Member(member), next)
                    }
                };
                let value = self.child(next, |c| c.rhs(value));
                Stmt::Assign { place, value }
            }
            doc::Stmt::Remove { member } => Stmt::Remove(self.member(member, 0).0),
            doc::Stmt::Do { action } => Stmt::Do(self.child(0, |c| c.action(action))),
            doc::Stmt::If {
                condition,
                then_block,
                else_block,
            } => Stmt::If {
                condition: self.child(0, |c| c.expr(condition)),
                then_block: self.child(1, |c| c.block(then_block, &[]).0),
                else_block: self.child(2, |c| c.block(else_block, &[]).0),
            },
            doc::Stmt::For {
                binding,
                iterable,
                body,
            } => {
                let iterable = self.child(0, |c| c.expr(iterable));
                let (body, bound) = self.child(1, |c| c.block(body, std::slice::from_ref(binding)));
                Stmt::For {
                    site: self.site(),
                    binding: Local::unplaced(bound.first().copied().unwrap_or(0)),
                    iterable,
                    body,
                }
            }
            doc::Stmt::While { condition, body } => Stmt::While {
                site: self.site(),
                condition: self.child(0, |c| c.expr(condition)),
                body: self.child(1, |c| c.block(body, &[]).0),
            },
            doc::Stmt::Break => Stmt::Break,
            doc::Stmt::Continue => Stmt::Continue,
            doc::Stmt::Return { value } => Stmt::Return(self.child(0, |c| c.expr(value))),
            doc::Stmt::Throw { value } => Stmt::Throw(self.child(0, |c| c.expr(value))),
            doc::Stmt::Print { value } => Stmt::Print(self.child(0, |c| c.expr(value))),
            doc::Stmt::Finish { value } => Stmt::Finish(self.child(0, |c| c.expr(value))),
            doc::Stmt::Fail { value } => Stmt::Fail(self.child(0, |c| c.expr(value))),
            doc::Stmt::Try(scope) => {
                let mut next = 0;
                let body = self.child(next, |c| c.block(&scope.body, &[]).0);
                next += 1;
                let catch = scope.catch.as_ref().map(|catch| {
                    let (block, bound) = self.child(next, |c| {
                        c.block(&catch.body, std::slice::from_ref(&catch.binding))
                    });
                    next += 1;
                    (Local::unplaced(bound.first().copied().unwrap_or(0)), block)
                });
                let finally = scope
                    .finally
                    .as_ref()
                    .map(|finally| self.child(next, |c| c.block(finally, &[]).0));
                Stmt::Try {
                    site: self.site(),
                    body,
                    catch,
                    finally,
                }
            }
        }
    }

    fn rhs(&mut self, rhs: &doc::Rhs) -> Rhs {
        match rhs {
            doc::Rhs::Expr(expr) => Rhs::Expr(self.expr(expr)),
            doc::Rhs::Action(action) => Rhs::Action(self.action(action)),
        }
    }

    /// Compiles a member whose first child has index `first`, and returns
    /// the index after its last.
    fn member(&mut self, member: &doc::Member, first: u32) -> (Member, u32) {
        match member {
            doc::Member::Field { target, field } => (
                Member::Field(self.child(first, |c| c.expr(target)), field.clone()),
                first + 1,
            ),
            doc::Member::Index { target, index } => (
                Member::Index(
                    self.child(first, |c| c.expr(target)),
                    self.child(first + 1, |c| c.expr(index)),
                ),
                first + 2,
            ),
        }
    }

    fn action(&mut self, action: &doc::Action) -> Action {
        let kind = match action {
            doc::Action::Call { callee, args } => ActionKind::Call {
                callee: self.callee(callee),
                args: self.atoms(args),
            },
            doc::Action::Spawn { callee, args } => ActionKind::Spawn {
                callee: self.callee(callee),
                args: self.atoms(args),
            },
            doc::Action::Perform {
                effect,
                args,
                result,
            } => ActionKind::Perform {
                effect: effect.clone(),
                args: self.atoms(args),
                result: result.clone(),
            },
            doc::Action::Sleep { duration } => ActionKind::Sleep(self.atom(duration)),
            doc::Action::Join { task } => ActionKind::Join(self.atom(task)),
            doc::Action::JoinMany { mode, tasks } => ActionKind::JoinMany(*mode, self.atom(tasks)),
            doc::Action::Yield => ActionKind::Yield,
            doc::Action::Cancel { task } => ActionKind::Cancel(self.atom(task)),
        };
        Action {
            site: self.site(),
            kind,
        }
    }

    fn callee(&mut self, callee: &doc::Callee) -> Callee {
        match callee {
            doc::Callee::Declared(name) => match self.declared.get(name) {
                Some(code) => Callee::Declared(*code),
                // Validation refuses this; a variable of that name is what
                // is left to mean.
                None => Callee::Value(Var::Unbound(name.clone())),
            },
            doc::Callee::Value(name) => Callee::Value(self.resolve(name)),
            doc::Callee::Library(function) => match self.resolve.lib(function) {
                Some(lib) => Callee::Library(lib),
                None => Callee::Value(Var::Unbound(Name::new(function.to_string()))),
            },
        }
    }

    fn atoms(&mut self, atoms: &[doc::Atom]) -> Vec<Atom> {
        atoms.iter().map(|atom| self.atom(atom)).collect()
    }

    fn atom(&mut self, atom: &doc::Atom) -> Atom {
        match atom {
            doc::Atom::Variable(name) => Atom::Var(self.resolve(name)),
            doc::Atom::Literal(literal) => Atom::Literal(literal_value(literal)),
        }
    }

    fn exprs(&mut self, exprs: &[doc::Expr]) -> Vec<Expr> {
        exprs
            .iter()
            .enumerate()
            .map(|(index, expr)| self.child(index as u32, |c| c.expr(expr)))
            .collect()
    }

    fn expr(&mut self, expr: &doc::Expr) -> Expr {
        match expr {
            doc::Expr::Literal(literal) => Expr::Literal(literal_value(literal)),
            doc::Expr::Variable(name) => Expr::Var(self.resolve(name)),
            doc::Expr::Tuple(items) => Expr::Tuple(self.exprs(items)),
            doc::Expr::List(items) => Expr::List(self.exprs(items)),
            doc::Expr::Set(items) => Expr::Set(self.exprs(items)),
            doc::Expr::Map(entries) => Expr::Map(
                entries
                    .iter()
                    .enumerate()
                    .map(|(index, entry)| {
                        let index = index as u32 * 2;
                        (
                            self.child(index, |c| c.expr(&entry.key)),
                            self.child(index + 1, |c| c.expr(&entry.value)),
                        )
                    })
                    .collect(),
            ),
            doc::Expr::Record(entries) => Expr::Record(
                entries
                    .iter()
                    .enumerate()
                    .map(|(index, entry)| {
                        let value = self.child(index as u32, |c| c.expr(&entry.value));
                        (entry.field.clone(), value)
                    })
                    .collect(),
            ),
            doc::Expr::Member(member) => Expr::Member(Box::new(self.member(member, 0).0)),
            doc::Expr::Closure(closure) => {
                // A closure runs only where it is applied, which the body of
                // a function with a native implementation cannot do
                // (`K-LIB-004`): wherever it runs, it is ordinary code.
                let code = self.child(0, |c| c.code(&closure.params, &closure.body, false, true));
                let id = CodeId(self.base.codes + self.codes.len() as u32);
                self.codes.push(Some(code));
                Expr::Closure(id)
            }
            doc::Expr::Call { function, args } => match self.resolve.lib(function) {
                Some(lib) => Expr::Call {
                    lib,
                    args: self.exprs(args),
                },
                None => Expr::Var(Var::Unbound(Name::new(function.to_string()))),
            },
            doc::Expr::Clock => Expr::Clock,
            doc::Expr::Random => Expr::Random,
            doc::Expr::Read(read) => Expr::Read(Box::new((
                self.child(0, |c| c.expr(&read.handle)),
                self.child(1, |c| c.expr(&read.request)),
            ))),
        }
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
                    outer_at: 0,
                    inner_at: 0,
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

    /// Places a statement's variables, and collects the blocks it holds
    /// and the closures it makes.
    fn stmt(&self, stmt: &mut Stmt, blocks: &mut Vec<BlockId>, closures: &mut Vec<CodeId>) {
        match stmt {
            Stmt::Let { target, value } => {
                if let Target::Slot(local) = target {
                    self.local(local);
                }
                self.rhs(value, closures);
            }
            Stmt::Assign { place, value } => {
                match place {
                    Place::Var(var) => self.var(var),
                    Place::Member(member) => self.member(member, closures),
                }
                self.rhs(value, closures);
            }
            Stmt::Remove(member) => self.member(member, closures),
            Stmt::Do(action) => self.action(action),
            Stmt::If {
                condition,
                then_block,
                else_block,
            } => {
                self.expr(condition, closures);
                blocks.extend([*then_block, *else_block]);
            }
            Stmt::For {
                binding,
                iterable,
                body,
                ..
            } => {
                self.local(binding);
                self.expr(iterable, closures);
                blocks.push(*body);
            }
            Stmt::While {
                condition, body, ..
            } => {
                self.expr(condition, closures);
                blocks.push(*body);
            }
            Stmt::Try {
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
            Stmt::Break | Stmt::Continue => {}
            Stmt::Return(expr)
            | Stmt::Throw(expr)
            | Stmt::Print(expr)
            | Stmt::Finish(expr)
            | Stmt::Fail(expr) => self.expr(expr, closures),
        }
    }

    fn rhs(&self, rhs: &mut Rhs, closures: &mut Vec<CodeId>) {
        match rhs {
            Rhs::Expr(expr) => self.expr(expr, closures),
            Rhs::Action(action) => self.action(action),
        }
    }

    fn member(&self, member: &mut Member, closures: &mut Vec<CodeId>) {
        match member {
            Member::Field(target, _) => self.expr(target, closures),
            Member::Index(target, index) => {
                self.expr(target, closures);
                self.expr(index, closures);
            }
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

    fn expr(&self, expr: &mut Expr, closures: &mut Vec<CodeId>) {
        match expr {
            Expr::Literal(_) | Expr::Clock | Expr::Random | Expr::Folded(_) => {}
            Expr::Var(var) => self.var(var),
            Expr::Tuple(items) | Expr::List(items) | Expr::Set(items) => {
                items.iter_mut().for_each(|item| self.expr(item, closures));
            }
            Expr::Call { args, .. } => args.iter_mut().for_each(|arg| self.expr(arg, closures)),
            Expr::Map(entries) => {
                for (key, value) in entries {
                    self.expr(key, closures);
                    self.expr(value, closures);
                }
            }
            Expr::Record(entries) => {
                for (_, value) in entries {
                    self.expr(value, closures);
                }
            }
            Expr::Member(member) => self.member(member, closures),
            Expr::Closure(code) => closures.push(*code),
            Expr::Read(read) => {
                self.expr(&mut read.0, closures);
                self.expr(&mut read.1, closures);
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
