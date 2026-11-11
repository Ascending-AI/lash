//! Library code in an instance (FIG-7688): the process's compiled
//! library bakes in no Link, so a frame of a library unit runs as the
//! Link of the frame that called it. What the Link decides is decided
//! here, as the code runs: the constants a library unit reads, placed
//! once per Link, the closures it makes and the natives it calls.

use std::collections::HashMap;
use std::sync::Arc;

use lash_kernel_doc::Value;
use lash_kernel_doc::arena::native::NewObject;
use lash_kernel_state::paged::run::continuation::ConstantKey;
use lash_kernel_value::LinkId;
use lash_kernel_value::names::CallableId;

use super::{Run, Stop, Vm, fault};
use crate::bytecode::program::{Constant, LIB, Program, UnitId};
use crate::heap::Heap;

/// What one Link's frames of library code read in an instance.
#[derive(Default)]
pub(crate) struct LibraryPlaced {
    /// Each library constant, once placed.
    constants: Vec<Option<Value>>,
    /// Each library unit whose constants are placed.
    units: Vec<bool>,
}

/// The object constants a sealed run placed and this instance has not
/// placed again, by Link and content: placing an equal constant for that
/// Link binds the saved object instead of a copy (contract §2). A table
/// position is not a durable name: the process library grows in the
/// order its code is first compiled, and the library bodies a program
/// inlines bring their constants into its own table.
pub(crate) type Saved = HashMap<(LinkId, ConstantKey), Value>;

/// Every constant the instance's Links placed, and those a sealed run
/// placed that it has not placed again.
#[derive(Default)]
pub(crate) struct Library {
    links: Vec<LibraryPlaced>,
    pub(crate) saved: Saved,
}

impl Library {
    /// No constant placed yet, beside those `saved` holds.
    pub(crate) fn of(saved: Saved) -> Self {
        Self {
            links: Vec::new(),
            saved,
        }
    }

    /// Every placed or saved constant: collection roots.
    pub(crate) fn values(&self) -> impl Iterator<Item = &Value> {
        self.links
            .iter()
            .flat_map(|placed| placed.constants.iter().flatten())
            .chain(self.saved.values())
    }
}

/// The durable name of object constant `object`: its kind and content.
/// An immediate has none: it is its value.
pub(crate) fn key(object: &NewObject) -> ConstantKey {
    let mut out = lash_kernel_value::tokens::TokenWriter::default();
    match object {
        NewObject::LongText { text, .. } => {
            out.sum(0, 1);
            out.text(text);
        }
        NewObject::BigInt(int) => {
            out.sum(1, 1);
            out.text(&int.to_string());
        }
        NewObject::Bytes(bytes) => {
            out.sum(2, 1);
            out.bytes(bytes);
        }
        NewObject::Function(callable) => {
            out.sum(3, 2);
            out.bytes(callable.link.as_bytes());
            out.bytes(callable.function.as_bytes());
        }
        _ => unreachable!("a constant is text, an integer, bytes or a function"),
    }
    ConstantKey(lash_kernel_value::tokens::logical_digest(&out.into_bytes()))
}

/// The object constant `constant` places, its function value naming
/// Link `link` when library code of that Link reads it; none for an
/// immediate.
fn object(constant: &Constant, link: Option<LinkId>) -> Option<NewObject> {
    Some(match constant {
        Constant::Immediate(_) => return None,
        Constant::Text(text) => NewObject::long_text(text.to_string()),
        Constant::BigInt(int) => NewObject::BigInt(int.clone()),
        Constant::Bytes(bytes) => NewObject::Bytes(bytes.clone()),
        Constant::Function(callable) => NewObject::Function(CallableId {
            link: link.unwrap_or(callable.link),
            function: callable.function,
        }),
    })
}

/// Object constant `object` of Link `link` placed in `heap`: the object a
/// sealed run placed it as, when `saved` has one.
fn place<H: Heap>(
    heap: &mut H,
    saved: &mut Saved,
    link: LinkId,
    object: NewObject,
) -> Result<Value, String> {
    if !saved.is_empty()
        && let Some(value) = saved.remove(&(link, key(&object)))
    {
        heap.adopt_constant(value);
        return Ok(value);
    }
    heap.constant(object).map_err(|error| error.to_string())
}

/// The program's own constants, placed in `heap`, binding those `saved`
/// holds. A run its code does not fit places none.
pub(crate) fn place_constants<H: Heap>(
    heap: &mut H,
    program: &Program,
    saved: &mut Saved,
    unadmitted: bool,
) -> Result<Vec<Value>, Stop> {
    let mut constants = Vec::with_capacity(program.constants.len());
    for constant in &program.constants {
        let Some(object) = object(constant, None) else {
            let Constant::Immediate(value) = constant else {
                unreachable!()
            };
            constants.push(*value);
            continue;
        };
        if unadmitted {
            constants.push(Value::Absent);
            continue;
        }
        constants.push(place(heap, saved, program.link, object).map_err(Stop::Fault)?);
    }
    Ok(constants)
}

impl<H: Heap> Vm<H> {
    /// Every object constant placed in the heap, by its Link and content.
    pub(crate) fn placed_constants(&self) -> Vec<(LinkId, ConstantKey, Value)> {
        let program = &self.program;
        let own = program
            .constants
            .iter()
            .zip(&self.constants)
            .filter(|(_, value)| value.handle().is_some())
            .filter_map(|(constant, value)| {
                object(constant, None).map(|object| (program.link, key(&object), *value))
            });
        let library = self
            .library
            .links
            .iter()
            .enumerate()
            .flat_map(|(at, placed)| {
                let link = program.links[at];
                placed
                    .constants
                    .iter()
                    .enumerate()
                    .filter_map(move |(k, value)| {
                        let value = value.filter(|value| value.handle().is_some())?;
                        object(&program.lib.constants[k], Some(link))
                            .map(|object| (link, key(&object), value))
                    })
            });
        // A saved constant of a Link the program does not load is no longer
        // its code's: its object lives on only while a value references it.
        let saved = self
            .library
            .saved
            .iter()
            .filter(|((link, _), _)| program.link_index(link).is_some())
            .map(|((link, key), value)| (*link, *key, *value));
        own.chain(library).chain(saved).collect()
    }
    /// Whether a function value naming `callable` can enter the run: the
    /// Link it names is loaded and declares it.
    pub(crate) fn callable(&self, callable: &CallableId) -> bool {
        self.program.callable(callable)
    }

    /// The Link the running frame runs as.
    #[inline(always)]
    pub(crate) fn running_link(&self) -> u32 {
        self.cx.frames.last().map_or(0, |frame| frame.link)
    }

    /// The Link a frame of `unit` runs as: an own unit's, or `caller`'s
    /// for library code.
    #[inline(always)]
    pub(crate) fn link_of(&self, unit: UnitId, caller: u32) -> u32 {
        match self.program.own_index(unit) {
            Some(at) => self.links[at],
            None => caller,
        }
    }

    /// The index of Link `link` among the program's, if it is loaded.
    pub(crate) fn link_index(&self, link: &lash_kernel_value::LinkId) -> Option<u32> {
        self.program.link_index(link)
    }

    /// What a frame of `unit` holds (contract §2).
    #[inline(always)]
    pub(crate) fn frame_cost(&self, unit: UnitId) -> u64 {
        match self.program.own_index(unit) {
            Some(at) => self.frames[at],
            None => self.program.lib.frames[(unit & !LIB) as usize],
        }
    }

    /// Constant `n` as the running frame reads it.
    #[inline(always)]
    pub(crate) fn konst(&self, n: u32) -> Value {
        if n & LIB == 0 {
            return self.constants[n as usize];
        }
        self.library_constant(n)
    }

    /// Library constant `n` as the running frame's Link placed it: out of
    /// line, so that the dispatch loop holds only the own constant's read.
    #[cold]
    #[inline(never)]
    fn library_constant(&self, n: u32) -> Value {
        self.library
            .links
            .get(self.running_link() as usize)
            .and_then(|placed| placed.constants.get((n & !LIB) as usize).copied().flatten())
            .unwrap_or(Value::Absent)
    }

    /// Places what a frame of library unit `unit` reads as Link `link`,
    /// once for each.
    #[inline(always)]
    pub(crate) fn prepare(&mut self, unit: UnitId, link: u32) -> Run<()> {
        if unit & LIB == 0 {
            return Ok(());
        }
        let placed = self
            .library
            .links
            .get(link as usize)
            .and_then(|placed| placed.units.get((unit & !LIB) as usize).copied());
        if placed == Some(true) {
            return Ok(());
        }
        self.place_library(unit, link)
    }

    #[cold]
    fn place_library(&mut self, unit: UnitId, link: u32) -> Run<()> {
        let program = Arc::clone(&self.program);
        let lib = &program.lib;
        let at = (unit & !LIB) as usize;
        let library = &mut self.library;
        if library.links.len() <= link as usize {
            library
                .links
                .resize_with(link as usize + 1, Default::default);
        }
        let id = program.links[link as usize];
        for &n in lib.reads[at].iter() {
            let k = (n & !LIB) as usize;
            let placed = &mut library.links[link as usize];
            if placed.constants.len() <= k {
                placed.constants.resize(lib.constants.len(), None);
            }
            if placed.constants[k].is_some() {
                continue;
            }
            let value = match object(&lib.constants[k], Some(id)) {
                None => {
                    let Constant::Immediate(value) = &lib.constants[k] else {
                        unreachable!()
                    };
                    *value
                }
                Some(object) => {
                    place(&mut self.heap, &mut library.saved, id, object).map_err(fault)?
                }
            };
            library.links[link as usize].constants[k] = Some(value);
        }
        let placed = &mut library.links[link as usize];
        if placed.units.len() <= at {
            placed.units.resize(lib.units.len(), false);
        }
        placed.units[at] = true;
        Ok(())
    }

    /// The body unit of a closure of `callable`.
    pub(crate) fn closure_unit(&self, callable: &CallableId) -> Option<UnitId> {
        if let Some(unit) = self.closures.get(callable) {
            return Some(*unit);
        }
        self.link_index(&callable.link)?;
        self.program.lib.closures.get(&callable.function).copied()
    }

    /// The trampoline a function value naming `callable` applies.
    pub(crate) fn function_unit(&self, callable: &CallableId) -> Option<UnitId> {
        if let Some(unit) = self.functions.get(callable) {
            return Some(*unit);
        }
        self.link_index(&callable.link)?;
        self.program
            .lib
            .trampolines
            .get(&callable.function)
            .copied()
    }

    /// Native `native`'s implementation in this instance's registry.
    pub(crate) fn implementation(
        &mut self,
        native: u32,
    ) -> Run<Arc<dyn lash_kernel_doc::NativeFunction>> {
        let (table, at) = (usize::from(native & LIB != 0), (native & !LIB) as usize);
        if let Some(Some(found)) = self.implementations[table].get(at) {
            return Ok(Arc::clone(found));
        }
        let function = self.program.native(native).function;
        let found = self
            .crossing
            .program
            .library
            .registry()
            .get(&function)
            .and_then(|registered| registered.native().cloned())
            .ok_or_else(|| fault(format!("native {function} is not linked in this build")))?;
        let table = &mut self.implementations[table];
        if table.len() <= at {
            table.resize(at + 1, None);
        }
        table[at] = Some(Arc::clone(&found));
        Ok(found)
    }

    /// The identity of closure site `n` of `unit`, made by a frame
    /// running as Link `link`: library code's names that Link.
    pub(crate) fn identity(
        &mut self,
        unit: UnitId,
        n: u32,
        link: u32,
    ) -> Arc<lash_kernel_doc::arena::ClosureIdentity> {
        let site = &self.program.unit(unit).closures[n as usize];
        if unit & LIB == 0 {
            return Arc::clone(&site.identity);
        }
        if let Some(identity) = self.identities.get(&(link, unit, n)) {
            return Arc::clone(identity);
        }
        let at = self.program.links[link as usize];
        let made = &site.identity;
        let identity = Arc::new(lash_kernel_doc::arena::ClosureIdentity {
            capture_names: made.capture_names.clone(),
            callable: CallableId {
                link: at,
                function: made.callable.function,
            },
            site: lash_kernel_doc::linked_site::LinkedSite {
                link: at,
                site: made.site.site.clone(),
            },
        });
        self.identities
            .insert((link, unit, n), Arc::clone(&identity));
        identity
    }
}
