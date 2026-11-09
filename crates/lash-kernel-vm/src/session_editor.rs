//! A carried session as its host reads and edits it between cells
//! (`K-SES-001`): the host's view of the bindings a cell hands on, over the
//! same pages, with nothing copied out but what it asks for.
//!
//! The editor opens the session's heap as a cell does: each binding's
//! value a root, each object by proxy, nothing loaded. A binding's value
//! is copied out as data only when the host asks for it, loading what it
//! reaches; a binding the host sets is built from data in the same heap,
//! and the session it seals keeps every row it did not touch as it was.
//!
//! A session is a run session ([`run::session`]); a binding never reaches
//! a function or a task (`K-SES-003`), so a session holds no saved
//! function the editor could list, relink or drop.
//!
//! [`run::session`]: lash_kernel_state::paged::run::session

use std::collections::{BTreeMap, BTreeSet};

use lash_kernel_doc::heap::HeapFault;
use lash_kernel_doc::{Datum, Value};
use lash_kernel_state::arena::Arena;
use lash_kernel_state::paged::run::directory::Directory;
use lash_kernel_state::paged::run::ids::{HighWater, ObjectId};
use lash_kernel_state::paged::run::objects::RunValue;
use lash_kernel_state::paged::run::session::{self, RunFunction, RunSession};
use lash_kernel_state::paged::{PageSource, SessionRoot};
use lash_kernel_value::names::Name;

use crate::data::CopyError;
use crate::heap::slots::SlotHeap;
use crate::heap::{Heap, LoadError};
use crate::vm::{Flow, Run, fault};

mod preview;

/// A binding a host reads: its value as data, or why it is none.
#[derive(Clone, Debug, PartialEq)]
pub enum SessionValue {
    Data(Datum),
    /// A value that is no data (a function's reference): its host lists it
    /// by name.
    Opaque,
    /// The prompt's bounded read stopped before copying this value.
    /// Text and bytes count bytes, containers count members, integers bits.
    Unpreviewed {
        kind: &'static str,
        extent: u64,
    },
}

/// The run session page `session` names; a session in a retired form is
/// refused.
fn run_page(session: &SessionRoot) -> Result<lash_kernel_doc::heap::PageRef, HeapFault> {
    session.run_page().ok_or_else(|| {
        HeapFault::Malformed("a paged-engine session is retired and never reopened".into())
    })
}

/// A carried session opened for its host.
pub struct SessionEditor {
    heap: SlotHeap,
    bindings: BTreeMap<Name, Value>,
    /// The session's saved functions and their objects, sealed as they
    /// came (`K-SES-004`).
    saved: Vec<(RunFunction, Value)>,
    /// The deepest value a copy out reads (`K-VAL-034`).
    depth: usize,
    changed: bool,
}

impl SessionEditor {
    /// The session `session` carries, read through `source`; an empty one
    /// for `None`.
    ///
    /// # Errors
    ///
    /// The heap's faults reading the session's root; a session in a
    /// retired form.
    pub fn open(
        session: Option<&SessionRoot>,
        source: Box<dyn PageSource>,
    ) -> Result<Self, HeapFault> {
        let (heap, bindings, saved) = match session {
            None => (
                SlotHeap::durable(
                    u64::MAX,
                    crate::heap::open::Durable {
                        directory: Directory::new(source),
                        high_water: HighWater(0),
                    },
                ),
                BTreeMap::new(),
                Vec::new(),
            ),
            Some(session) => {
                let (session, directory) = session::open(source, run_page(session)?)?;
                let (heap, values) =
                    SlotHeap::open(u64::MAX, &session.heap, directory).map_err(loaded)?;
                let mut values = values.into_iter();
                let bindings = session.names.into_iter().zip(&mut values).collect();
                (
                    heap,
                    bindings,
                    session.functions.into_iter().zip(values).collect(),
                )
            }
        };
        Ok(Self {
            heap,
            bindings,
            saved,
            depth: lash_kernel_value::data_limits::KernelDataLimits::standard().value_depth(),
            changed: false,
        })
    }

    /// Every binding's name, in name order.
    ///
    /// # Errors
    ///
    /// None: the names are the session root's.
    pub fn names(&self) -> Result<BTreeSet<Name>, HeapFault> {
        Ok(self.bindings.keys().cloned().collect())
    }

    /// The names whose bindings differ between `older` and `newer`, two
    /// versions of one session (`older` none for a session that held
    /// nothing), each with whether `older` and `newer` bind it.
    ///
    /// Conservative: a binding is reported when its value is another
    /// object or immutable, and when any object its value reaches in
    /// `newer` has another row than in `older`. An object mutated in place
    /// is reported under every binding that reaches it, even where the
    /// change is not one the binding's reader would see; a binding is
    /// never left out when it differs.
    ///
    /// # Errors
    ///
    /// The heap's faults reading either root or a row; a session in a
    /// retired form.
    pub fn changed(
        &self,
        older: Option<&SessionRoot>,
        newer: &SessionRoot,
    ) -> Result<BTreeMap<Name, (bool, bool)>, HeapFault> {
        let access = self.directory()?.access();
        let older = older.map(|older| self.read(older)).transpose()?;
        let newer = self.read(newer)?;
        let bound = |session: &RunSession| -> BTreeMap<Name, RunValue> {
            session
                .names
                .iter()
                .cloned()
                .zip(session.heap.roots.iter().cloned())
                .collect()
        };
        let old_bindings = older.as_ref().map(bound).unwrap_or_default();
        let new_bindings = bound(&newer);
        let rows = older.as_ref().map(|older| {
            (
                Directory::over(older.heap.directory, access.clone()),
                Directory::over(newer.heap.directory, access.clone()),
            )
        });
        let same_rows = older
            .as_ref()
            .is_some_and(|older| older.heap.directory == newer.heap.directory);
        let mut dirty = BTreeMap::new();
        let mut changed = BTreeMap::new();
        for name in old_bindings.keys().chain(new_bindings.keys()) {
            if changed.contains_key(name) {
                continue;
            }
            let (old, new) = (old_bindings.get(name), new_bindings.get(name));
            let differs = match (old, new, &rows) {
                (Some(old), Some(new), Some((old_rows, new_rows))) if old == new => {
                    !same_rows
                        && match new.id() {
                            Some(id) => mutated(id, old_rows, new_rows, &mut dirty)?,
                            None => false,
                        }
                }
                _ => old != new,
            };
            if differs {
                changed.insert(name.clone(), (old.is_some(), new.is_some()));
            }
        }
        Ok(changed)
    }

    /// The run session `session` names.
    fn read(&self, session: &SessionRoot) -> Result<RunSession, HeapFault> {
        session::read(self.directory()?.access(), run_page(session)?)
    }

    fn directory(&self) -> Result<&Directory, HeapFault> {
        self.heap
            .directory()
            .ok_or_else(|| HeapFault::Malformed("a session heap has no directory".into()))
    }

    /// The saved functions whose records differ between `older` and
    /// `newer`, by declared name: a session holds none (`K-SES-003`).
    ///
    /// # Errors
    ///
    /// A session in a retired form.
    pub fn changed_functions(
        &self,
        older: Option<&SessionRoot>,
        newer: &SessionRoot,
    ) -> Result<BTreeMap<Name, Option<Vec<u8>>>, HeapFault> {
        if let Some(older) = older {
            run_page(older)?;
        }
        run_page(newer)?;
        Ok(BTreeMap::new())
    }

    /// The binding `name`'s value, copied out as data.
    ///
    /// # Errors
    ///
    /// The heap's faults.
    pub fn get(&mut self, name: &Name) -> Result<Option<SessionValue>, HeapFault> {
        let Some(value) = self.bindings.get(name).copied() else {
            return Ok(None);
        };
        let depth = self.depth;
        match self.warm(value, |arena| crate::data::copy_out(arena, depth, &value)) {
            Ok(datum) => Ok(Some(SessionValue::Data(datum))),
            Err(CopyError::Raised(_)) => Ok(Some(SessionValue::Opaque)),
            Err(CopyError::Heap(fault)) => Err(fault),
            Err(CopyError::Cold { .. }) => {
                Err(HeapFault::Malformed("a loaded chunk is still cold".into()))
            }
        }
    }

    /// Runs `read` over the arena, loading each chunk it finds cold, with
    /// `value` and the bindings kept through any collection a load makes.
    fn warm<T>(
        &mut self,
        value: Value,
        mut read: impl FnMut(&Arena) -> Result<T, CopyError>,
    ) -> Result<T, CopyError> {
        loop {
            match read(self.heap.arena()) {
                Err(CopyError::Cold { object, chunk }) => {
                    let bindings = &self.bindings;
                    self.heap
                        .load(object, chunk, &mut |visit| {
                            visit(&value);
                            bindings.values().for_each(&mut *visit);
                        })
                        .map_err(|error| CopyError::Heap(loaded(error)))?;
                }
                other => return other,
            }
        }
    }

    /// Whether the value bound to `binding` refers to the saved function
    /// `function`: a session's binding never reaches one (`K-SES-003`).
    ///
    /// # Errors
    ///
    /// None.
    pub fn refers(
        &self,
        _binding: &Name,
        _environment: &lash_kernel_doc::heap::EntityId,
    ) -> Result<bool, HeapFault> {
        Ok(false)
    }

    /// Binds `name` to `value`.
    ///
    /// # Errors
    ///
    /// A value the heap refuses.
    pub fn set(&mut self, name: &Name, value: &Datum) -> Result<(), HeapFault> {
        let mut host = Host {
            heap: &mut self.heap,
            held: Vec::new(),
        };
        let value = crate::vm::datum::copy_in(&mut host, value).map_err(refused)?;
        self.bindings.insert(name.clone(), value);
        self.changed = true;
        Ok(())
    }

    /// Unbinds `name`. Answers whether it was bound.
    ///
    /// # Errors
    ///
    /// None.
    pub fn remove(&mut self, name: &Name) -> Result<bool, HeapFault> {
        let removed = self.bindings.remove(name).is_some();
        self.changed |= removed;
        Ok(removed)
    }

    /// Every saved function the session holds, by its declared name, with
    /// the bytes its dialect described it in: none (`K-SES-003`).
    ///
    /// # Errors
    ///
    /// None.
    pub fn functions(&self) -> Result<BTreeMap<Name, Vec<u8>>, HeapFault> {
        Ok(BTreeMap::new())
    }

    /// Rewrites the saved function `name` as relinked against its next
    /// cell's Namespaces (K-LIB-009). Answers whether the session held it:
    /// a session holds none (`K-SES-003`).
    ///
    /// # Errors
    ///
    /// None.
    pub fn relink_function(
        &mut self,
        _name: &Name,
        _description: &[u8],
        _closure: lash_kernel_doc::heap::PageRef,
        _callable: lash_kernel_value::CallableId,
    ) -> Result<bool, HeapFault> {
        Ok(false)
    }

    /// Carries the saved function `name` through a proved kernel rewrite.
    /// Answers whether the session held it: a session holds none
    /// (`K-SES-003`).
    ///
    /// # Errors
    ///
    /// None.
    pub fn succeed_function(
        &mut self,
        _name: &Name,
        _description: &[u8],
        _closure: lash_kernel_doc::heap::PageRef,
        _callable: lash_kernel_value::CallableId,
        _correspondence: &lash_kernel_doc::Correspondence,
    ) -> Result<bool, HeapFault> {
        Ok(false)
    }

    /// Drops the saved function `name` and the binding of its name
    /// (RELINK-CLOSURE). Answers whether the session held it: a session
    /// holds none (`K-SES-003`).
    ///
    /// # Errors
    ///
    /// None.
    pub fn drop_function(&mut self, _name: &Name) -> Result<bool, HeapFault> {
        Ok(false)
    }

    /// Whether an edit changed the session.
    #[must_use]
    pub fn edited(&self) -> bool {
        self.changed
    }

    /// The session as edited, sealed into pages through the source.
    ///
    /// # Errors
    ///
    /// The heap's faults.
    pub fn seal(mut self) -> Result<SessionRoot, HeapFault> {
        let (names, mut values): (Vec<Name>, Vec<Value>) =
            std::mem::take(&mut self.bindings).into_iter().unzip();
        let (functions, saved): (Vec<RunFunction>, Vec<Value>) =
            std::mem::take(&mut self.saved).into_iter().unzip();
        values.extend(saved);
        let heap = self.heap.seal(&values).map_err(|error| match error {
            crate::paged::SealError::Heap(fault) => fault,
            other => HeapFault::Malformed(other.to_string()),
        })?;
        let page = session::write(
            self.directory()?.access(),
            &RunSession {
                heap,
                names,
                functions,
            },
        )?;
        Ok(SessionRoot::run(page))
    }
}

/// The session's heap as a datum is built in it: unbounded, as the host's
/// own edits are, and never collected while it builds.
struct Host<'a> {
    heap: &'a mut SlotHeap,
    held: Vec<Value>,
}

impl crate::vm::datum::Build for Host<'_> {
    fn held(&mut self) -> &mut Vec<Value> {
        &mut self.held
    }
    fn admit(&mut self, units: u64) -> Run<()> {
        self.heap
            .admit(units)
            .map_err(|refused| fault(format!("{refused:?}")))
    }
    fn alloc(&mut self, object: lash_kernel_doc::arena::native::NewObject) -> Run<Value> {
        self.admit(crate::heap::slots::schedule(&object))?;
        self.heap
            .create(object)
            .map_err(|error| fault(error.to_string()))
    }
    fn host_handle(&mut self, handle: &lash_kernel_doc::Handle, units: u64) -> Run<Value> {
        let value = self.heap.arena_mut().host_handle(handle.clone());
        if let Some(placed) = value.handle() {
            self.heap.settle(placed, units);
        }
        Ok(value)
    }
    fn new_error(&mut self, kind: &str, message: &str, data: Value) -> Run<Value> {
        let units = self
            .heap
            .error_units(kind, message, data)
            .map_err(|error| fault(error.to_string()))?;
        self.admit(units)?;
        self.heap
            .error(kind, message, data)
            .map_err(|error| fault(error.to_string()))
    }
}

/// Whether any object `id` reaches in `newer` has another row than in
/// `older`, or none there; `dirty` keeps each id's answer across bindings.
fn mutated(
    id: ObjectId,
    older: &Directory,
    newer: &Directory,
    dirty: &mut BTreeMap<ObjectId, bool>,
) -> Result<bool, HeapFault> {
    let mut work = vec![id];
    let mut seen = BTreeSet::new();
    while let Some(id) = work.pop() {
        if !seen.insert(id) {
            continue;
        }
        let Some(row) = newer.get(id)? else {
            continue;
        };
        let differs = match dirty.get(&id) {
            Some(differs) => *differs,
            None => {
                let differs = older.get(id)?.as_ref() != Some(&row);
                dirty.insert(id, differs);
                differs
            }
        };
        if differs {
            return Ok(true);
        }
        work.extend(newer.edges(&row)?);
    }
    Ok(false)
}

fn loaded(error: LoadError) -> HeapFault {
    match error {
        LoadError::Store(fault) => fault,
        LoadError::Memory(refused) => {
            HeapFault::Malformed(format!("a session heap refused a load: {refused:?}"))
        }
    }
}

fn refused(flow: Flow) -> HeapFault {
    HeapFault::Malformed(format!("the session refused a host value: {flow:?}"))
}
