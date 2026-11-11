//! Library code in an instance (FIG-7688): the process's compiled
//! library bakes in no Link, so a frame of a library unit runs as the
//! Link of the frame that called it. What the Link decides is decided
//! here, as the code runs: the constants a library unit reads, placed
//! once per Link, the closures it makes and the natives it calls.

use std::sync::Arc;

use lash_kernel_doc::Value;
use lash_kernel_doc::arena::native::NewObject;
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

/// The program's own constants, placed in `heap`. A sealed run's are bound
/// to the objects they were placed as (`placed`): a constant is
/// materialized once (contract §2). A run its code does not fit places
/// none.
pub(crate) fn place_constants<H: Heap>(
    heap: &mut H,
    program: &Program,
    placed: Option<&[Value]>,
    unadmitted: bool,
) -> Result<Vec<Value>, Stop> {
    if placed.is_some_and(|placed| placed.len() != program.constants.len()) {
        return Err(Stop::Fault(
            "the run's constants are not its program's".into(),
        ));
    }
    let mut constants = Vec::with_capacity(program.constants.len());
    for (at, constant) in program.constants.iter().enumerate() {
        let object = match constant {
            Constant::Immediate(value) => {
                constants.push(*value);
                continue;
            }
            Constant::Text(text) => NewObject::LongText(text.to_string()),
            Constant::BigInt(int) => NewObject::BigInt(int.clone()),
            Constant::Bytes(bytes) => NewObject::Bytes(bytes.clone()),
            Constant::Function(callable) => NewObject::Function(*callable),
        };
        if unadmitted {
            constants.push(Value::Absent);
            continue;
        }
        if let Some(saved) = placed.map(|placed| placed[at])
            && saved.handle().is_some()
        {
            heap.adopt_constant(saved);
            constants.push(saved);
            continue;
        }
        let value = heap
            .constant(object)
            .map_err(|error| Stop::Fault(error.to_string()))?;
        constants.push(value);
    }
    Ok(constants)
}

impl LibraryPlaced {
    /// The constants it placed.
    pub(crate) fn values(&self) -> impl Iterator<Item = &Value> {
        self.constants.iter().flatten()
    }
}

impl<H: Heap> Vm<H> {
    /// Each library constant placed in the heap, as (Link index, constant
    /// index, value).
    pub(crate) fn placed_library(&self) -> impl Iterator<Item = (u32, u32, Value)> + '_ {
        self.library.iter().enumerate().flat_map(|(link, placed)| {
            placed
                .constants
                .iter()
                .enumerate()
                .filter_map(move |(at, value)| {
                    value
                        .filter(|value| value.handle().is_some())
                        .map(|value| (link as u32, at as u32, value))
                })
        })
    }

    /// Binds library constant `constant` of Link `link` to `value`, the
    /// object a sealed run placed it as.
    pub(crate) fn bind_library(
        &mut self,
        link: u32,
        constant: u32,
        value: Value,
    ) -> Result<(), String> {
        let count = self.program.lib.constants.len();
        if constant as usize >= count || link as usize >= self.program.links.len() {
            return Err(format!(
                "library constant {constant} of Link {link} is not the program's"
            ));
        }
        if self.library.len() <= link as usize {
            self.library
                .resize_with(link as usize + 1, Default::default);
        }
        let placed = &mut self.library[link as usize];
        if placed.constants.len() < count {
            placed.constants.resize(count, None);
        }
        self.heap.adopt_constant(value);
        placed.constants[constant as usize] = Some(value);
        Ok(())
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
        if self.library.len() <= link as usize {
            self.library
                .resize_with(link as usize + 1, Default::default);
        }
        for &n in lib.reads[at].iter() {
            let k = (n & !LIB) as usize;
            let placed = &mut self.library[link as usize];
            if placed.constants.len() <= k {
                placed.constants.resize(lib.constants.len(), None);
            }
            if placed.constants[k].is_some() {
                continue;
            }
            let object = match &lib.constants[k] {
                Constant::Immediate(value) => {
                    placed.constants[k] = Some(*value);
                    continue;
                }
                Constant::Text(text) => NewObject::LongText(text.to_string()),
                Constant::BigInt(int) => NewObject::BigInt(int.clone()),
                Constant::Bytes(bytes) => NewObject::Bytes(bytes.clone()),
                Constant::Function(callable) => NewObject::Function(CallableId {
                    link: program.links[link as usize],
                    function: callable.function,
                }),
            };
            let value = self
                .heap
                .constant(object)
                .map_err(|error| fault(error.to_string()))?;
            self.library[link as usize].constants[k] = Some(value);
        }
        let placed = &mut self.library[link as usize];
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
