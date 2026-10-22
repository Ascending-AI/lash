//! The executable: an admitted document compiled to tables the machine
//! walks.
//!
//! The executable is a cache (`docs/kernel/design.md` §5). It is derived
//! deterministically from the document and the registry, is never saved,
//! and nothing outside this crate sees its shape. Names are resolved to
//! frame slots, declared and library functions to table indexes, and every
//! node that an identity names keeps its site.
//!
//! It is compiled in two parts. A [`PreparedLibrary`] indexes a registry and compiles each library
//! body on first use, once for all runs using that registry. Each immutable document's admission and own code are reused
//! at start and import against that library. Its code is numbered after
//! the library's, so preparation does not grow with the library. A
//! function's identity is its content, so the body compiled under it is
//! the one every document that lists it runs, and the one a parked run
//! that pins it resumes in.
//!
//! A [`Layout`] permutes the tables and the frame slots, and chooses
//! whether a native implementation's fast path answers the calls it can, and
//! whether a native that stands beside a kernel body answers at all. No
//! value, effect identity or charge may depend on it; the laws compile one
//! document under several layouts and compare the runs.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::num::NonZeroU32;
use std::sync::Arc;

use lash_kernel_doc as doc;
use lash_kernel_doc::{Document, FunctionDefinition, FunctionRegistry, JoinMode};

use lash_kernel_exec::{
    ActIx, BoundaryKind, KirError, Op, OpRange, Reg, RegionTable, StepEdge, StmtRanges,
    Tables as StreamTables, verify_stream,
};
use lash_vocabulary::names::FunctionId;
use lash_vocabulary::names::Name;
use lash_vocabulary::sites::Site;
use lash_vocabulary::sites::Unit;
use lash_vocabulary::types::Type;

use crate::costs::Costs;
use crate::functions::{MachineFunction, machine_function, machine_name};

pub(crate) mod expr;
mod formula;
pub(crate) mod islands;
pub(crate) mod kind_test;
mod library;
pub(crate) use library::{Lib, LibRun, PreparedPlan};
mod frame;
mod lazy;
pub use lazy::body_census;
mod lower;
mod occurrences;
mod parameters;
pub(crate) mod plan_regions;
mod preparation;
mod reads;
mod regions;
mod scopes;
mod templates;
pub(crate) use templates::{Entry, INNER, inner as inner_entry};
pub(crate) mod segments;
pub(crate) use reads::{
    Field, PropertySlot, ReadSite, find as find_property, write as write_property,
};

pub(crate) use formula::{Charge, Fast, Plan, ScalarTable, SiteFormula, Source};
pub(crate) use regions::{
    Accumulator, Append, Arms, BARREN, Commit, CompiledBody, CompiledHead, CompiledLoop,
    CompiledStep, Cx, Event, Fresh, GuestCall, Guests, Headroom, Hook, Kept, LoopPlan, Made, Meter,
    NESTING, Node, Place, Ran, RegionSlot, Role, SessionSlot, Slots, Taken, Typed, UNPRODUCTIVE,
    measured_formula, position, scalar, store,
};
#[cfg(test)]
pub(crate) use regions::{Head, Step, Summary};
#[cfg(test)]
pub(crate) use regions::{Lowered, Refused};

/// How the executable is laid out. Layout `0` is the natural order; any
/// other value permutes the code, block, statement and library tables and
/// each frame's slots, and an odd one also calls every native
/// implementation through its general call, never its fast path or an
/// operator opcode. One that
/// leaves 3 when divided by 4 declines for every native that stands beside a
/// kernel body: the body runs every call of such a function. It exists for
/// tests: every layout runs a document the same way. Even layouts keep
/// per-site property caches; one leaving 2 modulo 4 makes every cache miss.
/// Every layout but 0 splits charge regions at its own choice of events. None of
/// this cache state is saved. Bit 62 turns region formation off
/// (`docs/design/kernel-regions.md` §3.3) and changes nothing else: every
/// other choice reads the layout without it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Layout(pub u64);

impl Layout {
    /// The bit that turns region formation off.
    const NO_REGIONS: u64 = 1 << 62;

    /// This layout with region formation off, and everything else equal.
    pub const fn without_regions(self) -> Self {
        Self(self.0 | Self::NO_REGIONS)
    }

    /// Whether lowering forms regions.
    pub(crate) const fn regions(self) -> bool {
        self.0 & Self::NO_REGIONS == 0
    }

    /// The layout every choice but region formation reads.
    const fn shape(self) -> Self {
        Self(self.0 & !Self::NO_REGIONS)
    }

    /// Every native that stands beside a kernel body declines.
    fn declines(self) -> bool {
        self.0 % 4 == 3
    }
}

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
    /// Lexical closure bodies by function and creation site.
    closure_at: BTreeMap<(FunctionId, Site), CodeId>,
    /// Canonical action-site order, independent of the executable layout.
    occurrence_at: BTreeMap<Site, usize>,
    occurrence_keys: Box<[Vec<u8>]>,
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
            nested: stream.nested.len(),
            regions: stream.regions.len(),
            sites: stream.sites as usize,
        };
        verify_stream(&stream.ops, &stmts, &sizes, &stream.regions)?;
        Ok(stmts.len())
    }
}

/// A function registry whose library bodies compile on demand, once, for
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
    charge: crate::Charge,
    alternative: std::sync::OnceLock<PreparedLibrary>,
    registry: Arc<FunctionRegistry>,
    layout: Layout,
    tables: lazy::LibraryTables,
    /// Every registered function, in the layout's order, each prepared
    /// when first asked for.
    libs: doc::FrameArray<std::sync::OnceLock<lazy::LibSlot>>,
    functions: doc::FrameArray<FunctionId>,
    /// Each library id by its function's registry position, when the
    /// layout permutes them; the position itself otherwise.
    lib_at: Option<Box<[u32]>>,
    runs: doc::FrameArray<Runs>,
    plan_sites: doc::FrameArray<u32>,
    /// Each function whose body reaches, through the bodies the machine
    /// runs, a function the registry does not hold: that function.
    missing: BTreeMap<FunctionId, FunctionId>,
    /// Where the library's property read sites, its plans' among them,
    /// end.
    sites: u32,
    documents: std::sync::Mutex<preparation::Documents>,
    frame_index: Option<&'static [u8]>,
}

impl fmt::Debug for PreparedLibrary {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PreparedLibrary")
            .field("functions", &self.0.libs.len())
            .field("bodies", &self.0.tables.compiled())
            .finish_non_exhaustive()
    }
}

impl PreparedLibrary {
    /// Prepares the registry. Each body compiles once, when first needed.
    pub fn new(registry: Arc<FunctionRegistry>) -> Self {
        Self::with_layout(registry, Layout::default())
    }

    /// Prepares code for the run's charge policy. Off emits no charge regions.
    pub fn with_charge(registry: Arc<FunctionRegistry>, charge: crate::Charge) -> Self {
        Self::with_layout_and_charge(registry, Layout::default(), charge)
    }

    pub(crate) fn in_charge(&self, charge: crate::Charge) -> Self {
        if self.0.charge == charge {
            self.clone()
        } else {
            self.0
                .alternative
                .get_or_init(|| match self.0.frame_index {
                    Some(index) => Self::over_frame(self.0.registry.clone(), charge, index)
                        .unwrap_or_else(|error| panic!("library frame allocation: {error}")),
                    None => Self::with_layout_and_charge(
                        Arc::clone(&self.0.registry),
                        self.0.layout,
                        charge,
                    ),
                })
                .clone()
        }
    }

    /// How many segments the library runs, as plans or as their natives'
    /// fallbacks, and each installed one whose sites its body does not
    /// state.
    pub(crate) fn segments(&self) -> (usize, Vec<String>) {
        let mut running = 0;
        let mut unresolved = Vec::new();
        for index in 0..self.0.libs.len() {
            let lib = self.0.lib(LibId(index as u32));
            let plan = match &lib.run {
                LibRun::Plan { plan, .. } => Some(plan),
                LibRun::Native { .. } | LibRun::DeferredPlan { .. } => {
                    self.0.fallback(LibId(index as u32))
                }
                _ => None,
            };
            if plan.is_some_and(|plan| plan.segment.is_some()) {
                running += 1;
            } else if plan.is_none()
                && !self.0.layout.declines()
                && let Err(problem) = self.0.checked_plan(index, false)
            {
                unresolved.push(format!("{}: {problem}", lib.header.name));
            }
        }
        (running, unresolved)
    }

    /// Explicitly verifies every library body, including cold fallbacks.
    pub(crate) fn verify_stream(&self) -> Result<usize, KirError> {
        self.0.tables.verify_stream(&self.0)
    }

    /// How many library functions are prepared.
    #[cfg(test)]
    pub(crate) fn prepared(&self) -> usize {
        self.0.libs.iter().filter(|lib| lib.get().is_some()).count()
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
            Self::with_layout_and_charge(Arc::clone(&self.0.registry), layout, self.0.charge)
        }
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
            for callee in doc::Node::Block(&body.block).functions_within() {
                callers.entry(callee).or_default().push(*function);
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
    pub(crate) instances: BTreeMap<lash_vocabulary::content_ids::LinkId, Arc<Document>>,
    // Immutable census boundaries, beside the document hot path.
    code_base: usize,
    block_base: usize,
    stmt_base: usize,
    library: PreparedLibrary,
    document: Arc<Document>,
    own: Arc<DocumentCode>,
    admission: Arc<preparation::Admission>,
}

/// Immutable document tables, independent of any run or owning document.
pub(crate) struct DocumentCode {
    tables: Tables,
    /// The loop regions lowering formed, by the index their `while` and
    /// `for` statements state (`Form::While::region`, `Form::For::region`).
    pub(crate) regions: Vec<LoopPlan>,
    /// The call islands lowering formed, by code.
    pub(crate) islands: islands::Islands,
    pub(crate) block_ordinals: Vec<Option<u32>>,
    pub(crate) site_count: usize,
    pub(crate) action_ordinals: BTreeMap<Site, u32>,
    pub(crate) condition_ordinals: BTreeMap<u32, u32>,
    pub(crate) main: Option<CodeId>,
    pub(crate) declared: BTreeMap<FunctionId, CodeId>,
    pub(crate) native_starts: BTreeMap<FunctionId, CodeId>,
    /// The functions the bodies of the library functions the document
    /// lists reach where the machine runs them, which a frame of the run
    /// may stand in beside those it lists. Found only when a run asks for
    /// one the document does not list: compiling never walks the library.
    reached: std::sync::OnceLock<BTreeSet<FunctionId>>,
}

impl std::ops::Deref for Executable {
    type Target = DocumentCode;

    fn deref(&self) -> &Self::Target {
        &self.own
    }
}

impl Executable {
    /// Whether the run charges its charged code (`Charge::Exact`); with
    /// charge Off, that code is paced.
    pub(crate) fn charging(&self) -> bool {
        self.library.0.charge == crate::Charge::Exact
    }

    pub(crate) fn block_ordinal(&self, id: BlockId) -> Option<u32> {
        let at = (id.0 as usize).checked_sub(self.block_base)?;
        self.own.block_ordinals.get(at).copied().flatten()
    }

    #[expect(
        clippy::expect_used,
        reason = "every code id the machine holds was handed out by this executable"
    )]
    #[inline]
    pub(crate) fn code(&self, id: CodeId) -> &Code {
        match (id.0 as usize).checked_sub(self.code_base) {
            Some(own) => &self.own.tables.codes[own],
            None => self
                .library
                .0
                .tables
                .code(&self.library.0, id)
                .expect("a code of this executable"),
        }
    }

    #[expect(
        clippy::expect_used,
        reason = "every block id the machine holds was handed out by this executable"
    )]
    #[inline]
    pub(crate) fn block(&self, id: BlockId) -> &Block {
        match (id.0 as usize).checked_sub(self.block_base) {
            Some(own) => &self.own.tables.blocks[own],
            None => self
                .library
                .0
                .tables
                .block(&self.library.0, id)
                .expect("a block of this executable"),
        }
    }

    #[expect(
        clippy::expect_used,
        reason = "every statement id the machine holds was handed out by this executable"
    )]
    #[inline]
    pub(crate) fn stmt(&self, id: StmtId) -> &Stmt {
        match (id.0 as usize).checked_sub(self.stmt_base) {
            Some(own) => &self.own.tables.stmts[own],
            None => self
                .library
                .0
                .tables
                .stmt(&self.library.0, id)
                .expect("a statement of this executable"),
        }
    }

    /// Statement `id`, the stream its ops are in, and where its step
    /// leaves its frame when it completes in its own block.
    #[inline]
    pub(crate) fn document_statement(&self, id: StmtId) -> Option<(&Stream, &Stmt, StepEdge)> {
        let at = (id.0 as usize).checked_sub(self.stmt_base)?;
        let tables = &self.own.tables;
        Some((&tables.stream, &tables.stmts[at], tables.edges[at]))
    }

    /// The document's own stream, which its codes' statements and island
    /// plans index.
    #[inline]
    pub(crate) fn document_stream(&self) -> &Stream {
        &self.own.tables.stream
    }

    #[inline]
    pub(crate) fn statement(&self, id: StmtId) -> (&Stream, &Stmt, StepEdge) {
        self.document_statement(id)
            .unwrap_or_else(|| self.library_statement(id.0 as usize))
    }

    // Keep lazy resolution out of the document's statement loop.
    #[inline(never)]
    fn library_statement(&self, index: usize) -> (&Stream, &Stmt, StepEdge) {
        let library = &self.library.0.tables;
        let body = library.stmts[index];
        let slot = &library.bodies[body];
        let tables = library.tables(&self.library.0, body);
        let at = index - slot.base.stmts as usize;
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
                occurrences: library.occurrence_keys.len(),
                sites: self.library.0.sites,
            },
            library.codes.len() + self.own.tables.codes.len(),
            library.blocks.len() + self.own.tables.blocks.len(),
            self.library.0.libs.len(),
        )
    }

    pub(crate) fn fallback(&self, id: LibId) -> Option<&Arc<PreparedPlan>> {
        self.library.0.fallback(id)
    }

    #[inline]
    pub(crate) fn lib(&self, id: LibId) -> &Lib {
        self.library.0.lib(id)
    }

    #[inline]
    pub(crate) fn shared_params(&self, id: LibId) -> &[usize] {
        &self.library.0.slot(id).shared_params
    }

    /// Resolves a source statement inside one admitted code body.
    pub(crate) fn stmt_at(&self, code: CodeId, site: &Site) -> Option<StmtId> {
        let code = self.code(code);
        if code.site.unit != site.unit || !site.path.starts_with(&code.site.path) {
            return None;
        }
        let mut depth = code.site.path.len();
        let mut block = code.body;
        loop {
            let stmt = *self
                .block(block)
                .stmts
                .get(*site.path.get(depth)? as usize)?;
            if depth + 1 == site.path.len() {
                return Some(stmt);
            }
            let inner = site.path.get(..depth + 2)?;
            block = self
                .stmt(stmt)
                .form
                .blocks()
                .into_iter()
                .find(|block| self.block(*block).site.path == inner)?;
            depth += 2;
        }
    }

    pub(crate) fn authored_unit(&self, unit: &Unit) -> bool {
        match unit {
            Unit::Library(function) => self
                .declared
                .get(function)
                .is_some_and(|code| self.code(*code).charged),
            Unit::Main | Unit::Function(_) => true,
        }
    }

    /// The run-root cleanup of the entry whose body is `code`, when its
    /// start convention declares one (`StartConvention::PyDirect`).
    pub(crate) fn root_cleanup(&self, code: CodeId) -> Option<LinkedHook> {
        let Unit::Library(function) = self.code(code).site.unit else {
            return None;
        };
        let definition = self
            .document
            .exports
            .values()
            .find_map(|export| match export {
                doc::Export::Function(export) if export.function == function => Some(export),
                _ => None,
            })?;
        match &definition.convention {
            doc::StartConvention::PyDirect { cleanup } => match cleanup {
                doc::FunctionTarget::Local(function) => self
                    .declared
                    .get(function)
                    .copied()
                    .map(LinkedHook::Code)
                    .or_else(|| self.lib_of(function).map(LinkedHook::Library)),
                doc::FunctionTarget::Import(slot) => Some(LinkedHook::Import(*slot)),
            },
            _ => None,
        }
    }

    /// The library function `function`, reached or not: a template's
    /// heap path's callee.
    pub(crate) fn library_lib(&self, function: &FunctionId) -> Option<LibId> {
        self.library.0.lib_of(function)
    }

    /// The library function `function`, when the document reaches it.
    pub(crate) fn lib_of(&self, function: &FunctionId) -> Option<LibId> {
        if !self.reaches(function) {
            return None;
        }
        self.library.0.lib_of(function)
    }

    /// Whether a frame of the run may stand in library function
    /// `function`: the document lists it, or the body of one it lists
    /// reaches it where the machine runs that body.
    fn reaches(&self, function: &FunctionId) -> bool {
        let listed = &self.document.functions;
        self.instances
            .values()
            .any(|document| document.functions.contains_key(function))
            || listed.contains_key(function)
            || self
                .own
                .reached
                .get_or_init(|| {
                    let registry = &self.library.0.registry;
                    let mut reached = BTreeSet::new();
                    let mut pending: Vec<FunctionId> = listed.keys().copied().collect();
                    while let Some(function) = pending.pop() {
                        if !reached.insert(function) {
                            continue;
                        }
                        if let Some(registered) = registry.get(&function)
                            && registered.native().is_none()
                            && let Some(body) = registered.definition().body()
                        {
                            pending.extend(body.functions.keys().copied());
                        }
                    }
                    reached
                })
                .contains(function)
    }

    /// The tables that hold `unit`'s code: the library's for a function
    /// the document reaches, the document's own for its main and
    /// functions.
    fn tables_of(&self, unit: &Unit) -> Option<&Tables> {
        match unit {
            Unit::Library(function) => {
                if self.own.declared.contains_key(function)
                    || self.own.native_starts.contains_key(function)
                {
                    return Some(&self.own.tables);
                }
                if !self.reaches(function) {
                    return None;
                }
                let body = self
                    .library
                    .0
                    .tables
                    .body_of(self.library.0.lib_of(function)?)?;
                Some(self.library.0.tables.tables(&self.library.0, body))
            }
            Unit::Main | Unit::Function(_) => Some(&self.own.tables),
        }
    }

    /// The identity of this live executable's code numbering. Every executable
    /// is allocated in an Arc, and the machine and its task storage retain it
    /// for the arena's whole lifetime. Closure code caches never leave that
    /// arena or reach pages, so an address cannot be reused while a cache
    /// could still name it. No process-wide identity allocator is needed.
    pub(crate) fn local(&self) -> u64 {
        std::ptr::from_ref(self).addr() as u64
    }

    /// The function body or closure body whose site is `site`.
    pub(crate) fn code_at(&self, site: &Site) -> Option<CodeId> {
        self.tables_of(&site.unit)?.code_at.get(site).copied()
    }

    /// A closure's lexical body under its defining executable tables.
    pub(crate) fn closure_at(&self, function: &FunctionId, site: &Site) -> Option<CodeId> {
        self.tables_of(&site.unit)?
            .closure_at
            .get(&(*function, site.clone()))
            .copied()
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
            .bodies
            .iter()
            .enumerate()
            .flat_map(|(index, _)| &self.library.0.tables.tables(&self.library.0, index).blocks)
            .chain(&self.own.tables.blocks)
    }

    /// The registered name of a library function, listed or not.
    pub(crate) fn library_name(
        &self,
        function: &FunctionId,
    ) -> Option<&lash_vocabulary::names::FunctionName> {
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
    pub(crate) created: Option<Site>,
    /// The site of the body: a unit's, or a closure expression's child.
    pub(crate) site: Site,
    pub(crate) params: Vec<Local>,
    pub(crate) body: BlockId,
    pub(crate) slots: Vec<SlotInfo>,
    pub(crate) slots_by_at: Vec<u32>,
    /// Where each variable sits, in the order of its name's key: the order
    /// a projection walks a frame's bindings in, whatever the layout
    /// (seam C).
    pub(crate) ats_by_name: Vec<u32>,
    /// The enclosing code's variables this closure shares.
    pub(crate) captures: Vec<Capture>,
    /// The names a document function reads that no scope of it declares,
    /// each at a variable of its own that a call of a saved function binds
    /// to a fresh instance of its frozen capture (`K-SES-004`). A variable
    /// no call binds reads as unbound.
    pub(crate) free: Vec<(Name, Local)>,
    /// Library body forms are covered by the completion formula
    /// (`K-CHG-007`); guest code and closures remain charged.
    pub(crate) charged: bool,
    pub(crate) charging: bool,
    /// Charged code in a run with charge Off: its statements, loop tests
    /// and action atoms count the run's pace, which slices measure, so
    /// that where a slice falls does not depend on the plan.
    pub(crate) paced: bool,
}

pub(crate) struct SlotInfo {
    pub(crate) key: Vec<u8>,
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

/// A physical lookup cache; only the name participates in saved state.
#[derive(Clone)]
pub(crate) struct BindingName {
    pub(crate) name: Name,
    pub(crate) key: Arc<[u8]>,
}
impl BindingName {
    pub(crate) fn new(name: Name) -> Self {
        let key = lash_kernel_state::kernel_internals::metadata::metadata_key(&name).into();
        Self { name, key }
    }
}
impl std::ops::Deref for BindingName {
    type Target = Name;
    fn deref(&self) -> &Name {
        &self.name
    }
}

pub(crate) enum Var {
    Local(Local),
    /// A session binding, looked up by name when it is read
    /// (`K-SES-001`).
    Session(BindingName),
    /// A name no enclosing scope declares (`K-FORM-003`).
    Unbound(Name),
}

/// The ops of every statement of a library or a document, and the
/// constants, names and fields they index.
#[derive(Default)]
pub(crate) struct Stream {
    pub(crate) ops: Vec<Op>,
    pub(crate) consts: Vec<Constant>,
    pub(crate) names: Vec<BindingName>,
    pub(crate) fields: Vec<Field>,
    pub(crate) field_lists: Vec<Box<[String]>>,
    /// The resume entries of the stream's nested actions, by [`ActIx`].
    pub(crate) nested: Vec<Nested>,
    /// Each resume entry by its compiled statement and action site: where a
    /// saved continuation finds the entry for this statement's layout.
    pub(crate) nested_at: BTreeMap<(StmtId, Site), ActIx>,
    /// The charge regions the stream's `Op::Region`s head, by
    /// `RegionIx`: the cold side table of its fixed charge events.
    pub(crate) regions: RegionTable,
    /// Where the stream's property read sites end: a document's follow
    /// its library's and its library's plans'.
    pub(crate) sites: u32,
    /// The layout the stream was lowered under, which says whether its
    /// read sites cache.
    pub(crate) layout: Layout,
}

/// An action nested in a statement's expression, or a call inside an
/// expression of a library function that runs its body: what the machine
/// needs to issue it and to resume its statement at the op after it
/// (`K-EVAL-008`).
pub(crate) struct Nested {
    pub(crate) guest: bool,
    /// The action's site: what its waits, its occurrences and its saved
    /// continuation name.
    pub(crate) site: Site,
    /// The action's site as a path under its statement's: what a
    /// continuation saved at it names it by.
    pub(crate) suffix: Suffix,
    pub(crate) occurrence: usize,
    /// The length of its statement's site path: a saved continuation names
    /// the action and its roles by their path suffix under it.
    pub(crate) depth: u32,
    pub(crate) kind: NestedKind,
    /// Whether the call is an operator's (`Op::Operator`): a library call
    /// with a native entry or generated template, with exactly its
    /// parameters, whose entry answers inline what it can.
    pub(crate) operator: bool,
    /// Whether the call may enter an island (`docs/design/kernel-regions.md`
    /// §5 R4): a guest call of a closure, or of a declared function lowering
    /// formed an island of.
    pub(crate) island: bool,
    /// The op its statement resumes at, as an offset from the start of the
    /// statement's range.
    pub(crate) resume: u32,
    /// The register its value goes to.
    pub(crate) dst: Reg,
    /// The operands the statement has evaluated when the action is reached
    /// and will still use, in evaluation order: each the site of the
    /// expression that produced it and the register it is in under this
    /// layout. They are the continuation's roles, `live_roles` of the
    /// action's site.
    pub(crate) roles: Box<[NestedRole]>,
}

/// A site's path under its statement's, as a saved continuation names its
/// action and its roles: built once by the compiler and shared by every
/// continuation the machine saves there, copied out only where a frame's
/// row is written.
pub(crate) type Suffix = Arc<[u32]>;

/// An operand a statement has evaluated when one of its nested actions is
/// reached: a role of the action's continuation (`K-EVAL-008`).
pub(crate) struct NestedRole {
    /// The site of the expression that produced it.
    pub(crate) site: Site,
    /// The site as a path under the statement's.
    pub(crate) suffix: Suffix,
    /// The register it is in under this layout.
    pub(crate) reg: Reg,
}

/// What a nested action does.
pub(crate) enum NestedKind {
    Yield,
    Cancel,
    /// A `call`, `apply` or `invoke`: of a declared function, a closure or
    /// function reference, or a library function, which enters a frame
    /// when it runs its body.
    Call(NestedCallee),
    /// A library call inside an expression of a function that runs its
    /// body here: it enters a frame, as a call does, but it is no action,
    /// so no occurrence of its site is counted.
    Body(LibId),
    Perform {
        slot: lash_vocabulary::names::ImportSlot,
        result: Type,
        then_yield: bool,
    },

    Spawn(NestedCallee),
    Join,
    JoinMany(JoinMode),
}

impl NestedKind {
    /// Whether the task takes one more turn once the action's outcome is
    /// in (`K-TASK-005`).
    pub(crate) fn then_yield(&self) -> bool {
        matches!(
            self,
            Self::Perform {
                then_yield: true,
                ..
            }
        )
    }
}

/// What a nested call or `spawn` runs.
pub(crate) enum NestedCallee {
    Import(lash_vocabulary::names::ImportSlot, Option<LibId>),
    Declared(CodeId),
    Library(LibId),
    /// A closure or function reference held in a variable, read into the
    /// action's first register before its arguments (`K-EVAL-004`): a role
    /// of the actions in them, named by the action's own site.
    Read,
}

/// One statement: its ops in its stream, how many registers they use, and
/// what the machine's control needs of its shape.
pub(crate) struct Stmt {
    /// Its document ordinal, or None for a library statement. The main body
    /// owns ordinal zero, so a statement's rank is always nonzero. Keeping
    /// this in the metadata already read at dispatch avoids another table
    /// load and fits the statement's existing alignment padding.
    pub(crate) ordinal: Option<NonZeroU32>,
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
    ReturnVia {
        site: Site,
        policy: LinkedHook,
    },
    If {
        then_block: BlockId,
        else_block: BlockId,
    },
    For {
        metered: u64,
        site: Site,
        binding: Local,
        /// The protocol hooks the loop opens and steps its value with
        /// (`K-ITER-005`).
        via: Option<Via>,
        body: BlockId,
        /// What runs when the loop ends but by `break`.
        else_block: Option<BlockId>,
        /// The loop region it runs as, in the executable's plans.
        region: Option<u32>,
    },
    While {
        metered: u64,
        site: Site,
        /// The condition's ops, which every iteration step runs.
        test: OpRange,
        body: BlockId,
        else_block: Option<BlockId>,
        /// The loop region it runs as, in the executable's plans.
        region: Option<u32>,
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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum LinkedHook {
    Library(LibId),
    Code(CodeId),
    Import(lash_vocabulary::names::ImportSlot),
}
impl LinkedHook {
    pub(crate) fn library(self) -> Option<LibId> {
        match self {
            Self::Library(lib) => Some(lib),
            _ => None,
        }
    }
}

/// A loop protocol's hooks, compiled (`K-ITER-005`).
#[derive(Clone, Copy)]
pub(crate) struct Via {
    pub(crate) open: LinkedHook,
    pub(crate) next: Option<LinkedHook>,
    pub(crate) item: Option<LinkedHook>,
    pub(crate) check: Option<LinkedHook>,
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
            Form::Plain | Form::ReturnVia { .. } | Form::Action(_) => Vec::new(),
            Form::If {
                then_block,
                else_block,
            } => vec![*then_block, *else_block],
            Form::For {
                body, else_block, ..
            }
            | Form::While {
                body, else_block, ..
            } => std::iter::once(*body).chain(*else_block).collect(),
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
    pub(crate) guest: bool,
    pub(crate) site: Site,
    pub(crate) occurrence: usize,
    pub(crate) kind: ActionKind,
}

pub(crate) enum ActionKind {
    Call {
        callee: Callee,
        args: Vec<Atom>,
    },
    Perform {
        slot: lash_vocabulary::names::ImportSlot,
        args: Vec<Atom>,
        result: Type,
        then_yield: bool,
    },

    Join(Atom),
    JoinMany(JoinMode, Atom),
    Yield,
    Spawn {
        callee: Callee,
        args: Vec<Atom>,
    },
    Cancel(Atom),
}

impl ActionKind {
    /// Whether the task takes one more turn once the action's outcome is
    /// in (`K-TASK-005`).
    pub(crate) fn then_yield(&self) -> bool {
        matches!(
            self,
            Self::Perform {
                then_yield: true,
                ..
            }
        )
    }
}

pub(crate) enum Callee {
    Declared(CodeId),
    Value(Var),
    Library(LibId),
    Import(lash_vocabulary::names::ImportSlot, Option<LibId>),
}

impl Callee {
    pub(crate) fn library(&self) -> Option<LibId> {
        match self {
            Self::Library(lib) | Self::Import(_, Some(lib)) => Some(*lib),
            _ => None,
        }
    }
}

pub(crate) enum Atom {
    Var(Var),
    Literal(Constant),
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
    charged: bool,
    main: bool,
    /// The site of the code's body.
    body: Site,
    /// The innermost block last; in each, the latest declaration last.
    blocks: Vec<Vec<(Name, Declared)>>,
    slots: Vec<SlotInfo>,
    captures: Vec<Capture>,
    /// A document function's free names, each with its variable; `None`
    /// for any other code, whose free names are unbound.
    free: Option<Vec<(Name, Slot)>>,
}

#[derive(Clone, Copy)]
enum Declared {
    Slot(Slot),
    ScopedSlot(Slot),
    Session,
}

/// The session bindings a library body declares: none.
const NO_BINDINGS: BTreeSet<Name> = BTreeSet::new();

/// How a compiled call names a library function.
#[derive(Clone, Copy)]
struct Resolve<'a> {
    /// The library the calls resolve against: each function's id, how it
    /// runs and whether it is an operator (`Op::Operator`).
    library: &'a Prepared,
    /// The functions a document lists; a library body's calls resolve
    /// against the whole registry, which its registration checked.
    listed: Option<&'a BTreeMap<FunctionId, lash_vocabulary::names::FunctionName>>,
}

/// How a library function runs when it is called inside an expression.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Runs {
    /// It has no kernel body: it always answers at once.
    Answers,
    /// It has a body, which runs in a frame of its own: always, or when
    /// its native declines the call (`K-LIB-006`).
    Body,
}

impl Resolve<'_> {
    /// How `lib` runs when it is called.
    /// Whether a call of `lib` with `given` arguments is an operator's.
    fn operator(&self, lib: LibId, given: usize) -> bool {
        self.library.operator(lib) == Some(given)
    }

    fn runs(&self, lib: LibId) -> Runs {
        self.library
            .runs
            .get(lib.0 as usize)
            .copied()
            .unwrap_or(Runs::Answers)
    }

    fn lib(&self, function: &FunctionId) -> Option<LibId> {
        if self
            .listed
            .is_some_and(|listed| !listed.contains_key(function))
        {
            return None;
        }
        self.library.lib_of(function)
    }
}

/// The first id of each table a compilation adds to: zero for a library,
/// and for a document the length of its library's tables.
#[derive(Clone, Copy, Default)]
struct Base {
    codes: u32,
    blocks: u32,
    stmts: u32,
    occurrences: usize,
    sites: u32,
}

struct Compiler<'a> {
    metered: bool,
    /// The session bindings `main` keeps private.
    private: &'a BTreeSet<Name>,
    layout: Layout,
    shuffler: Shuffler,
    base: Base,
    codes: Vec<Option<Code>>,
    blocks: Vec<Block>,
    stmts: Vec<Stmt>,
    stream: Stream,
    /// Where each name already sits in the stream's pool.
    names: BTreeMap<Name, u32>,
    /// Emptied buffers that statements gather their ops in.
    spare: Vec<Vec<Op>>,
    /// Emptied buffers that statements gather their charge regions in.
    spare_regions: Vec<lower::Regions>,
    /// The next property read site.
    sites: u32,
    declared: BTreeMap<FunctionId, CodeId>,
    resolve: Resolve<'a>,
    imports: BTreeMap<(Unit, lash_vocabulary::ImportSlot), LibId>,
    /// The codes being compiled, the outermost first; each closure adds one.
    scopes: Vec<Scopes>,
    unit: Unit,
    path: Vec<u32>,
    /// The operands of the statement being compiled that are evaluated and
    /// still to be used, each by its parent's path length, its index among
    /// the parent's children and its register: the roles of a nested action
    /// compiled now. Each parent is an ancestor of what is compiled while
    /// its entries stand, so `path` still holds its prefix.
    live: Vec<(u32, u32, Reg)>,
    /// The site path length of the statement being compiled.
    stmt_depth: u32,
}

use document_code::compile_document;
mod document_code;

/// Forms and verifies the loop region of every `while` and `for` of the
/// document's own `tables` that has one, against `library`, and has each
/// such loop state its plan (`docs/design/kernel-regions.md` §1).
fn form_regions(
    tables: &mut Tables,
    library: &Prepared,
    loop_test: u64,
    island: &dyn Fn(CodeId) -> bool,
) -> Vec<LoopPlan> {
    let (blocks, stmts) = (
        library.tables.blocks.len() as u32,
        library.tables.stmts.len() as u32,
    );
    let mut formed = Vec::new();
    {
        let block = |id: BlockId| &tables.blocks[(id.0 - blocks) as usize];
        let stmt = |id: StmtId| &tables.stmts[(id.0 - stmts) as usize];
        let lib = |id: LibId| library.lib(id);
        let lib_of = |function: &FunctionId| library.lib_of(function);
        let lowered = || regions::Lowered {
            stream: &tables.stream,
            block: &block,
            stmt: &stmt,
            lib: &lib,
            lib_of: &lib_of,
            island,
        };
        let formation = regions::Formation {
            lowered: lowered(),
            loop_test,
        };
        for code in &tables.codes {
            let mut pending = vec![code.body];
            while let Some(id) = pending.pop() {
                for id in &block(id).stmts {
                    let statement = stmt(*id);
                    pending.extend(statement.form.blocks());
                    if !matches!(statement.form, Form::While { .. } | Form::For { .. }) {
                        continue;
                    }
                    if let Some(plan) = formation.form(code, *id)
                        && plan.verify(&lowered(), loop_test).is_ok()
                    {
                        formed.push(plan);
                    }
                }
            }
        }
    }
    for (index, plan) in formed.iter().enumerate() {
        if let Form::While { region, .. } | Form::For { region, .. } =
            &mut tables.stmts[(plan.stmt.0 - stmts) as usize].form
        {
            *region = Some(index as u32);
        }
    }
    formed
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
            metered: true,
            layout,
            shuffler,
            base,
            codes: Vec::new(),
            blocks: Vec::new(),
            stmts: Vec::new(),
            stream: Stream::default(),
            names: BTreeMap::new(),
            spare: Vec::new(),
            spare_regions: Vec::new(),
            sites: base.sites,
            declared: BTreeMap::new(),
            resolve,
            imports: BTreeMap::new(),
            scopes: Vec::new(),
            unit: Unit::Main,
            path: Vec::new(),
            live: Vec::new(),
            stmt_depth: 0,
        }
    }

    /// Takes the next code id, for a unit compiled later.
    fn reserve(&mut self) -> CodeId {
        let id = CodeId(self.base.codes + self.codes.len() as u32);
        self.codes.push(None);
        id
    }

    /// The compiled tables, with every body and block found by its site.
    fn finish(mut self) -> Tables {
        self.stream.sites = self.sites;
        self.stream.layout = self.layout;
        // A content-addressed function can be compiled both as a declaration
        // and as a lexical closure. Resume entries belong to the compiled
        // statement, whose register and slot layout they describe.
        for (index, stmt) in self.stmts.iter().enumerate() {
            let mut ranges = vec![stmt.ops];
            if let Form::While { test, .. } = stmt.form {
                ranges.push(test);
            }
            for range in ranges {
                for op in &self.stream.ops[range.start as usize..range.end() as usize] {
                    if let Op::Nested { act, .. } = op {
                        let site = self.stream.nested[act.ix()].site.clone();
                        self.stream
                            .nested_at
                            .insert((StmtId(self.base.stmts + index as u32), site), *act);
                    }
                }
            }
        }
        let (occurrence_at, occurrence_keys) = self.index_occurrences();
        let base = self.base;
        let codes: Vec<Code> = self.codes.into_iter().flatten().collect();
        let code_at = codes
            .iter()
            .enumerate()
            .map(|(index, code)| (code.site.clone(), CodeId(base.codes + index as u32)))
            .collect();
        let closure_at = codes
            .iter()
            .enumerate()
            .filter_map(|(index, code)| {
                let Unit::Library(function) = code.site.unit else {
                    return None;
                };
                Some((
                    (function, code.created.clone()?),
                    CodeId(base.codes + index as u32),
                ))
            })
            .collect();
        let edges = step_edges(&codes, &self.blocks, &self.stmts, base);
        Tables {
            codes,
            blocks: self.blocks,
            stmts: self.stmts,
            stream: self.stream,
            edges,
            code_at,
            closure_at,
            occurrence_at,
            occurrence_keys,
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
                // The `else` runs once the loop has left the frame's
                // control: like an `if` arm, it is left with what the
                // loop's own step leaves.
                Form::For {
                    body, else_block, ..
                }
                | Form::While {
                    body, else_block, ..
                } => {
                    pending.push((*body, TO_END));
                    pending.extend(else_block.map(|block| {
                        (
                            block,
                            StepEdge {
                                leaves: edge.leaves + 1,
                                next: edge.next,
                            },
                        )
                    }));
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

/// An immediate literal is a compiled value; one an arena object holds is
/// built in the run's arena when the operation reads it.
pub(crate) enum Constant {
    Resident(doc::Value),
    Deferred(doc::Literal),
}
impl From<doc::Literal> for Constant {
    fn from(literal: doc::Literal) -> Self {
        use doc::arena::ShortText;
        let resident = match &literal {
            doc::Literal::Null => Some(doc::Value::Null),
            doc::Literal::Absent => Some(doc::Value::Absent),
            doc::Literal::Bool(value) => Some(doc::Value::Bool(*value)),
            doc::Literal::Float(value) => Some(doc::Value::Float(*value)),
            doc::Literal::Int(value) => value.word().map(doc::Value::Int),
            doc::Literal::Text(text) => ShortText::new(text).map(doc::Value::Text),
            doc::Literal::Function(_) => None,
            doc::Literal::Bytes(_) => None,
        };
        resident.map_or_else(|| Self::Deferred(literal), Self::Resident)
    }
}

/// The public result kinds checked without reading or warming an arena object.
pub(crate) fn import_result_check(ty: &lash_vocabulary::Type) -> Option<fn(doc::Value) -> bool> {
    use doc::Value;
    use lash_vocabulary::Type;
    Some(match ty {
        Type::Any => |_| true,
        Type::Null => |v| matches!(v, Value::Null),
        Type::Absent => |v| matches!(v, Value::Absent),
        Type::Bool => |v| matches!(v, Value::Bool(_)),
        Type::Int => |v| matches!(v, Value::Int(_)),
        Type::Float => |v| matches!(v, Value::Float(_)),
        Type::Number => |v| matches!(v, Value::Int(_) | Value::Float(_)),
        _ => return None,
    })
}

pub(crate) fn import_result_fits(ty: &lash_vocabulary::Type, value: doc::Value) -> bool {
    import_result_check(ty).is_some_and(|fits| fits(value))
}
