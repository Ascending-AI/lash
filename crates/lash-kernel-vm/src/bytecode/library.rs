//! The compiled library (cutover contract §1): every library body,
//! native and completion formula a run reaches is compiled once per
//! process, validated once and shared by every instance whose registry
//! and build image key it. An instance compiles only its document's own
//! code and links to it.
//!
//! The library is compiled as no Link's: a library body's code is the
//! same whichever Link calls it. What a Link decides is decided where the
//! code runs, as the Link of the frame that runs it: the operation a
//! perform fills, the Link a closure or a function value it makes names,
//! and the occurrence counters its calls and actions advance. So a body
//! compiled once runs in every Link of every instance that reaches it.
//!
//! A [`SharedLibrary`] grows as runs reach bodies it has not compiled: a
//! request compiles them under its lock, and the next start of any
//! instance publishes them, inlined, validated, folded and fused, as a
//! new immutable [`LibCode`]. An instance holds the `LibCode` it was
//! built over; nothing it names ever changes or goes away. Library code
//! is never charged to an instance (§2).
//!
//! The process holds a few libraries, the oldest dropped first, each keyed
//! by the digest of what its code is derived from: the registry's
//! functions (an identity is its definition's content), whether each has
//! a native and the inline answer that native declares for its version,
//! the build image an open compiles under, and [`COMPILER_REVISION`]. A
//! registry or a compiler that differs in any of these never reads
//! another's code. A native's implementation is never part of the code:
//! each instance calls its own registry's.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use lash_kernel_doc::{BuildHelpers, FunctionRegistry};
use lash_kernel_value::names::{FunctionId, ImportSlot};
use lash_kernel_value::sites::Site;

use super::charge_tables::Charge;
use super::compile::{Compiler, Result, Tables, Unsupported};
use super::program::{ActionSite, Constant, NativeEntry, Program, Unit, UnitId, UnitKind};

/// The tag of a library id: a unit, constant, charge or occurrence counter
/// of the library rather than of the document.
pub const LIB: u32 = 1 << 31;

/// The compiler's revision: advanced whenever the code it emits for one
/// registry changes, so a process never runs one revision's library code
/// under another.
pub const COMPILER_REVISION: u32 = 1;

/// The most libraries a process keeps.
const HELD: usize = 8;

/// The library's code at one point: immutable once published.
#[derive(Clone, Default)]
pub struct LibCode {
    pub units: Vec<Arc<Unit>>,
    /// What a frame of each unit holds (contract §2).
    pub frames: Vec<u64>,
    pub constants: Vec<Constant>,
    pub natives: Vec<NativeEntry>,
    pub charges: Vec<Charge>,
    /// The site of each library occurrence counter: a Link's frames count
    /// their own.
    pub occurrences: Vec<Site>,
    /// Each library occurrence counter, by its site.
    pub sites: HashMap<Site, u32>,
    /// Each library closure function's body.
    pub closures: HashMap<FunctionId, UnitId>,
    /// Each library function's trampoline, for a function value naming it
    /// in any Link.
    pub trampolines: HashMap<FunctionId, UnitId>,
    /// Each unit by what a saved frame names it by besides its Link.
    pub keys: HashMap<
        (
            lash_kernel_state::paged::run::continuation::UnitKind,
            Site,
            bool,
        ),
        UnitId,
    >,
    /// Every operation library code performs, by the body whose import
    /// it names.
    pub performs: Vec<(Option<FunctionId>, ImportSlot)>,
    /// Each function's completion formula.
    pub charge_ids: HashMap<FunctionId, u32>,
    /// The frame height and each op's operand depth of every unit a
    /// document may expand inline, for its validation there.
    pub regions: HashMap<UnitId, Region>,
}

/// A unit's frame height and each of its ops' operand depth.
pub type Region = (u32, Box<[Option<u32>]>);

/// What the library compiler holds between requests: the code it has
/// published, and what it has compiled since.
struct Builder {
    code: Arc<LibCode>,
    tables: Tables,
    /// Published code that did not validate: no run reads this library.
    broken: Option<String>,
}

/// One registry's compiled library under one build image.
pub(crate) struct SharedLibrary {
    builder: Mutex<Builder>,
}

impl SharedLibrary {
    fn new() -> Self {
        Self {
            builder: Mutex::new(Builder {
                code: Arc::new(LibCode::default()),
                tables: Tables::default(),
                broken: None,
            }),
        }
    }

    fn lock(&self) -> MutexGuard<'_, Builder> {
        self.builder.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Runs `request` on the library's compiler: what it compiles joins
    /// the library, published at the next [`Self::code`].
    pub(crate) fn request<R>(
        &self,
        registry: &FunctionRegistry,
        available: Option<&BuildHelpers>,
        request: impl FnOnce(&mut Compiler<'_>) -> Result<R>,
    ) -> Result<R> {
        let mut builder = self.lock();
        if let Some(broken) = &builder.broken {
            return Err(Unsupported(broken.clone()));
        }
        let tables = std::mem::take(&mut builder.tables);
        let mut compiler = Compiler::library(registry, available, &builder.code, tables);
        let answer = request(&mut compiler);
        builder.tables = compiler.into_tables();
        answer
    }

    /// The library's code, with everything compiled so far published.
    pub(crate) fn code(&self) -> Result<Arc<LibCode>> {
        let mut builder = self.lock();
        if let Some(broken) = &builder.broken {
            return Err(Unsupported(broken.clone()));
        }
        if builder.tables.is_published() {
            return Ok(Arc::clone(&builder.code));
        }
        let builder = &mut *builder;
        match publish(&builder.code, &mut builder.tables) {
            Ok(code) => {
                builder.code = Arc::new(code);
                Ok(Arc::clone(&builder.code))
            }
            Err(error) => {
                builder.broken = Some(error.0.clone());
                Err(error)
            }
        }
    }
}

/// `held` with what `tables` compiled since added: its new units laid
/// out, inlined, validated, folded and fused once, for every instance.
fn publish(held: &Arc<LibCode>, tables: &mut Tables) -> Result<LibCode> {
    let fresh = tables.take_fresh()?;
    let mut program = Program {
        units: fresh.units,
        constants: fresh.constants,
        natives: fresh.natives,
        charges: fresh.charges,
        tag: LIB,
        lib: Arc::clone(held),
        link: lash_kernel_value::content_ids::LinkId::from_bytes([0; 32]),
        links: Vec::new(),
        functions: Vec::new(),
        declared: Default::default(),
        globals: Vec::new(),
        main: 0,
    };
    for unit in &mut program.units {
        unit.sites = super::site_tables::build(unit);
    }
    super::inline::inline(&mut program);
    let checked = super::validate::checked(&program)
        .map_err(|error| Unsupported(format!("the library does not validate: {error}")))?;
    super::fold::fold(&mut program);
    super::fuse::fuse(&mut program);
    let mut code = LibCode::clone(held);
    let mut performs: std::collections::HashSet<_> = code.performs.iter().copied().collect();
    let units = program.units.into_iter().zip(checked);
    for (at, (unit, (height, depth))) in units.enumerate() {
        let id = LIB | (held.units.len() + at) as UnitId;
        code.frames.push(crate::heap::meter::frame(&unit, height));
        if super::inline::expandable(&unit) {
            code.regions.insert(id, (height, depth.into_boxed_slice()));
        }
        if let UnitKind::Closure(function) = unit.kind {
            code.closures.entry(function).or_insert(id);
        }
        let saved = crate::durable::save_kind(&unit.kind);
        code.keys
            .insert((saved, unit.site.clone(), unit.covered), id);
        for action in &unit.actions {
            if let ActionSite::Perform { owner, slot, .. } = action
                && performs.insert((*owner, *slot))
            {
                code.performs.push((*owner, *slot));
            }
        }
        code.units.push(Arc::new(unit));
    }
    code.constants.extend(program.constants);
    code.natives.extend(program.natives);
    for charge in program.charges {
        if let Some(function) = charge.function {
            code.charge_ids
                .insert(function, LIB | code.charges.len() as u32);
        }
        code.charges.push(charge);
    }
    for (site, at) in fresh.occurrences {
        debug_assert_eq!(code.occurrences.len() as u32, at & !LIB);
        code.sites.insert(site.clone(), at);
        code.occurrences.push(site);
    }
    for (function, unit) in fresh.trampolines {
        code.trampolines.insert(function, unit);
    }
    Ok(code)
}

/// The library of `registry` under build image `available`, kept for the
/// process.
pub(crate) fn shared(
    registry: &FunctionRegistry,
    available: Option<&BuildHelpers>,
) -> Arc<SharedLibrary> {
    held(key(&digest(registry), available))
}

/// The library keyed `key`, made now if the process holds none.
pub(crate) fn held(key: [u8; 32]) -> Arc<SharedLibrary> {
    static LIBRARIES: Mutex<Vec<([u8; 32], Arc<SharedLibrary>)>> = Mutex::new(Vec::new());
    let mut libraries = LIBRARIES.lock().unwrap_or_else(PoisonError::into_inner);
    if let Some(at) = libraries.iter().position(|(held, _)| *held == key) {
        let found = libraries.remove(at);
        let library = Arc::clone(&found.1);
        libraries.push(found);
        return library;
    }
    if libraries.len() == HELD {
        libraries.remove(0);
    }
    let library = Arc::new(SharedLibrary::new());
    libraries.push((key, Arc::clone(&library)));
    library
}

/// The digest of what a registry's library code is derived from: each
/// function's identity, whether it has a native and the inline answer
/// that native declares for the function's version.
pub(crate) fn digest(registry: &FunctionRegistry) -> [u8; 32] {
    let mut bytes = Vec::with_capacity(registry.len() * 41);
    for (function, registered) in registry.iter() {
        bytes.extend_from_slice(function.as_bytes());
        match registered.native() {
            None => bytes.push(0),
            Some(native) => {
                bytes.push(1);
                let version = registered.definition().native_version;
                let inline = native
                    .specialization()
                    .inline
                    .and_then(|inline| inline.for_version(version));
                if let Some(inline) = inline {
                    let declared = format!("{inline:?}");
                    bytes.extend_from_slice(&(declared.len() as u32).to_le_bytes());
                    bytes.extend_from_slice(declared.as_bytes());
                }
                bytes.push(0xff);
            }
        }
    }
    lash_kernel_value::tokens::logical_digest(&bytes)
}

/// The key of the library of a registry of digest `registry` under build
/// image `available`, at this compiler revision.
pub(crate) fn key(registry: &[u8; 32], available: Option<&BuildHelpers>) -> [u8; 32] {
    key_at(registry, available, COMPILER_REVISION)
}

/// [`key`] at compiler revision `revision`.
pub(crate) fn key_at(
    registry: &[u8; 32],
    available: Option<&BuildHelpers>,
    revision: u32,
) -> [u8; 32] {
    let mut bytes = Vec::with_capacity(72);
    bytes.extend_from_slice(registry);
    match available {
        None => bytes.push(0),
        Some(image) => {
            bytes.push(1);
            bytes.extend_from_slice(image.identity().as_bytes());
        }
    }
    bytes.extend_from_slice(&revision.to_le_bytes());
    lash_kernel_value::tokens::logical_digest(&bytes)
}
