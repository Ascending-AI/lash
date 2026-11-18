//! A relinked saved function's new callable, published through every
//! reference to it (schemasmash K01, RELINK-CLOSURE).
//!
//! A function is an immutable: every row that holds one copies its head,
//! the callable itself, and a tuple's content commits to it. Rewriting the
//! function's own row would leave each holder naming the old callable. The
//! relink therefore runs before the seal, in the heap: the function becomes
//! a new function object of the new callable, each mutable holder takes it
//! in place, each tuple holding it is built anew, and the seal
//! writes every changed row, head and commitment as it writes any edit.
//! Only the objects whose rows reach a relinked function are loaded.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

use lash_kernel_doc::arena::native::NewObject;
use lash_kernel_doc::arena::{ArenaHandle, Body, ErrorBody};
use lash_kernel_doc::heap::HeapFault;
use lash_kernel_doc::{NativeError, Retry, Value};
use lash_kernel_state::arena::Arena;
use lash_kernel_state::paged::run::ids::ObjectId;
use lash_kernel_value::CallableId;

use super::{SessionEditor, loaded};
use crate::heap::Heap;

/// The walk's state: each relinked function's object and the new
/// function standing for it, the objects whose rows reach one, what each
/// immutable becomes, and the objects already walked.
struct Republish {
    targets: BTreeMap<ObjectId, (CallableId, Option<Value>)>,
    reaching: BTreeSet<ObjectId>,
    images: HashMap<u32, Value>,
    walked: HashSet<u32>,
}

impl SessionEditor {
    /// Gives each relinked saved function (not a closure, whose callable
    /// is its own row's alone) its new callable through the saved root,
    /// every binding and every holder its rows reach, and clears its
    /// relink.
    ///
    /// # Errors
    ///
    /// The heap's faults.
    pub(super) fn republish(&mut self) -> Result<(), HeapFault> {
        let targets: BTreeMap<ObjectId, (CallableId, Option<Value>)> = self
            .saved
            .iter()
            .filter(|saved| matches!(saved.value, Value::Function(_)))
            .filter_map(|saved| Some((saved.object, (saved.relinked.as_ref()?.callable, None))))
            .collect();
        if targets.is_empty() {
            return Ok(());
        }
        let roots = self
            .sealed
            .values()
            .filter_map(lash_kernel_state::paged::run::objects::RunValue::id)
            .chain(self.saved.iter().map(|saved| saved.object));
        let reaching = self.reaching(roots, targets.keys().copied().collect())?;
        let mut walk = Republish {
            targets,
            reaching,
            images: HashMap::new(),
            walked: HashSet::new(),
        };
        for index in 0..self.saved.len() {
            let value = self.saved[index].value;
            self.walk(&mut walk, value)?;
            self.saved[index].value = self.image(&mut walk, value)?;
            if matches!(value, Value::Function(_)) {
                self.saved[index].relinked = None;
            }
        }
        let names: Vec<_> = self.bindings.keys().cloned().collect();
        for name in names {
            let value = self.bindings[&name];
            self.walk(&mut walk, value)?;
            let value = self.image(&mut walk, value)?;
            self.bindings.insert(name, value);
        }
        Ok(())
    }

    /// The objects `roots` reach whose rows reach one of `targets`, the
    /// targets included. Reads rows, never a chunk.
    fn reaching(
        &self,
        roots: impl IntoIterator<Item = ObjectId>,
        targets: BTreeSet<ObjectId>,
    ) -> Result<BTreeSet<ObjectId>, HeapFault> {
        let directory = self.directory()?;
        let mut holders: BTreeMap<ObjectId, Vec<ObjectId>> = BTreeMap::new();
        for id in directory.reached(roots)? {
            let Some(row) = directory.held(id)? else {
                continue;
            };
            for edge in directory.edges(&row)? {
                holders.entry(edge).or_default().push(id);
            }
        }
        let mut reaching = BTreeSet::new();
        let mut work: Vec<ObjectId> = targets.into_iter().collect();
        while let Some(id) = work.pop() {
            if reaching.insert(id) {
                work.extend(holders.get(&id).into_iter().flatten().copied());
            }
        }
        Ok(reaching)
    }

    /// The object `value` names when its row reaches a relinked function.
    fn reaches(&self, walk: &Republish, value: Value) -> Option<(ArenaHandle, ObjectId)> {
        if matches!(value, Value::ObjectRef(_)) {
            return None;
        }
        let handle = value.handle()?;
        // An object the host built this edit has no id and holds only data.
        let id = crate::heap::open::id_of(self.heap.arena(), handle).ok()?;
        walk.reaching.contains(&id).then_some((handle, id))
    }

    /// What `value` is once every relinked function it reaches is its new
    /// one. A relinked function is its new function, and a tuple holding
    /// one is built anew; every other value, a mutable object included, is
    /// itself. Immutables hold no cycle, so this recursion ends.
    fn image(&mut self, walk: &mut Republish, value: Value) -> Result<Value, HeapFault> {
        let Some((handle, id)) = self.reaches(walk, value) else {
            return Ok(value);
        };
        if let Some(image) = walk.images.get(&handle.index()) {
            return Ok(*image);
        }
        let image = if let Some((callable, new)) = walk.targets.get_mut(&id) {
            match new {
                Some(function) => *function,
                None => {
                    let function = self.create(NewObject::Function(*callable))?;
                    *new = Some(function);
                    function
                }
            }
        } else {
            match value {
                Value::Tuple(tuple) => {
                    let members =
                        self.arena_read(|arena| arena.sequence(tuple).map(<[Value]>::to_vec))?;
                    let mut images = Vec::with_capacity(members.len());
                    for member in &members {
                        images.push(self.image(walk, *member)?);
                    }
                    if members
                        .iter()
                        .zip(&images)
                        .any(|(old, new)| moved(*old, *new))
                    {
                        self.create(NewObject::Tuple(images))?
                    } else {
                        value
                    }
                }
                Value::Error(error) => {
                    // An error's data is the record its construction
                    // built: a mutable object, which keeps its identity.
                    let data = self.error_data(error)?;
                    if moved(data, self.image(walk, data)?) {
                        return Err(HeapFault::Malformed(format!(
                            "error object {} holds a relinked function in immutable data",
                            id.0
                        )));
                    }
                    value
                }
                _ => value,
            }
        };
        walk.images.insert(handle.index(), image);
        Ok(image)
    }

    /// Gives every mutable object `value` reaches the image of each member
    /// it holds, in place. A key stays as it is: the save refuses a saved
    /// function a key reaches.
    fn walk(&mut self, walk: &mut Republish, value: Value) -> Result<(), HeapFault> {
        let Some((handle, _)) = self.reaches(walk, value) else {
            return Ok(());
        };
        if !walk.walked.insert(handle.index()) {
            return Ok(());
        }
        match value {
            Value::List(list) => {
                let len = self.arena_read(|arena| arena.len(list))?;
                for index in 0..len {
                    let Some(member) = self.arena_read(|arena| arena.item(list, index))? else {
                        continue;
                    };
                    self.walk(walk, member)?;
                    let image = self.image(walk, member)?;
                    if moved(member, image) {
                        self.arena_write(|arena| arena.list_set(list, index, image))?;
                    }
                }
            }
            Value::Map(table) | Value::Record(table) => {
                let entries =
                    self.arena_read(|arena| arena.entries(table).map(<[Value]>::to_vec))?;
                for pair in entries.as_chunks::<2>().0 {
                    let [key, member] = *pair;
                    self.walk(walk, member)?;
                    let image = self.image(walk, member)?;
                    if moved(member, image) {
                        self.arena_write(|arena| arena.table_write(table, key, image))?;
                    }
                }
            }
            Value::Closure(closure) => {
                let captures = self.arena_read(|arena| {
                    arena
                        .closure(closure)
                        .map(|(_, captures, _)| captures.to_vec())
                })?;
                for cell in captures {
                    let Ok(id) = crate::heap::open::id_of(self.heap.arena(), cell) else {
                        continue;
                    };
                    if !walk.reaching.contains(&id) || !walk.walked.insert(cell.index()) {
                        continue;
                    }
                    let held = self.arena_read(|arena| arena.cell(cell))?;
                    self.walk(walk, held)?;
                    let image = self.image(walk, held)?;
                    if moved(held, image) {
                        self.arena_write(|arena| arena.cell_set(cell, image))?;
                    }
                }
            }
            Value::Tuple(tuple) => {
                let members =
                    self.arena_read(|arena| arena.sequence(tuple).map(<[Value]>::to_vec))?;
                for member in members {
                    self.walk(walk, member)?;
                }
            }
            Value::Error(error) => {
                let data = self.error_data(error)?;
                self.walk(walk, data)?;
            }
            _ => {}
        }
        Ok(())
    }

    /// The data the error `error` holds.
    fn error_data(&mut self, error: ArenaHandle) -> Result<Value, HeapFault> {
        self.arena_read(|arena| match arena.body(error)? {
            Body::Error(ErrorBody { data, .. }) => Ok(*data),
            _ => Err(Retry::Cold {
                object: error,
                chunk: 0,
            }
            .into()),
        })
    }

    /// A new object of `object`, admitted to the editor's unbounded heap.
    fn create(&mut self, object: NewObject) -> Result<Value, HeapFault> {
        self.heap
            .admit(crate::heap::slots::schedule(&object))
            .map_err(|refused| HeapFault::Malformed(format!("{refused:?}")))?;
        self.heap.create(object).map_err(malformed)
    }

    /// Runs `read` over the arena, loading each chunk it finds cold.
    fn arena_read<T>(
        &mut self,
        mut read: impl FnMut(&Arena) -> Result<T, NativeError>,
    ) -> Result<T, HeapFault> {
        self.arena_write(|arena| read(arena))
    }

    /// Runs `write` over the arena, loading each chunk it finds cold.
    fn arena_write<T>(
        &mut self,
        mut write: impl FnMut(&mut Arena) -> Result<T, NativeError>,
    ) -> Result<T, HeapFault> {
        loop {
            match write(self.heap.arena_mut()) {
                Err(NativeError::Retry(Retry::Cold { object, chunk })) => {
                    // The editor's heap is unbounded: a load never
                    // collects, so nothing the walk holds is freed.
                    self.heap.load(object, chunk, &mut |_| {}).map_err(loaded)?;
                }
                other => return other.map_err(malformed),
            }
        }
    }
}

fn malformed(error: NativeError) -> HeapFault {
    HeapFault::Malformed(error.to_string())
}

/// Whether publishing gave `old` another value: only an object becomes
/// another, so the handles decide.
fn moved(old: Value, new: Value) -> bool {
    old.handle() != new.handle()
}
