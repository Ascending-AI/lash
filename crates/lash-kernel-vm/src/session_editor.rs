//! A carried session as its host reads and edits it between cells
//! (`K-SES-001`): the host's view of the bindings a cell hands on, over the
//! same pages, with nothing copied out but what it asks for.
//!
//! The editor opens the session's heap header and nothing else. A binding
//! is read by its name's key, and copied out of the heap as data only when
//! the host asks for its value; a binding the host sets is frozen in from
//! data, and the session it seals names every page it did not touch as it
//! was.

use std::collections::{BTreeMap, BTreeSet};

use lash_kernel_doc::heap::HeapFault;
use lash_kernel_doc::heap::PageValue;
use lash_kernel_doc::{Datum, Name};
use lash_kernel_state::kernel_internals::PagedHeap;
use lash_kernel_state::kernel_internals::census_impl::Census;
use lash_kernel_state::kernel_internals::family_impl::FrozenBase;
use lash_kernel_state::kernel_internals::metadata::metadata_key;
use lash_kernel_state::kernel_internals::tree::Tree;
use lash_kernel_state::paged::{
    AllocationCensus, BindingRecord, BoundRecord, CarryRoots, EditEpoch, FunctionRecord,
    HeapLifecycle, HeapWriteError, KeyedRoot, NameRef, PageSource, SemanticHeap, SessionRoot,
    SiteRef,
};

/// A binding a host reads: its value as data, or why it is none.
#[derive(Clone, Debug, PartialEq)]
pub enum SessionValue {
    Data(Datum),
    /// A value that is no data (a function's reference): its host lists it
    /// by name.
    Opaque,
}

/// A carried session opened for its host.
pub struct SessionEditor {
    heap: PagedHeap,
    bindings: Tree<BindingRecord>,
    functions: KeyedRoot,
    changed: bool,
}

impl SessionEditor {
    /// The session `session` carries, read through `source`; an empty one
    /// for `None`.
    ///
    /// # Errors
    ///
    /// The heap's faults reading the session's header.
    pub fn open(
        session: Option<&SessionRoot>,
        source: Box<dyn PageSource>,
    ) -> Result<Self, HeapFault> {
        let mut heap = match session {
            Some(session) => PagedHeap::open_header(
                lash_kernel_state::kernel_internals::session_heap(session),
                source,
                EditEpoch(0),
            )?,
            None => PagedHeap::fresh(source, EditEpoch(0)),
        };
        let census = Census::restore(heap.certificate())
            .map_err(|fault| HeapFault::Malformed(format!("the session's census: {fault}")))?;
        heap.set_census(Box::new(census))?;
        heap.set_family_base(Box::new(FrozenBase::default()));
        let (bindings, functions) = match session {
            Some(session) => (
                Tree::of_keyed(&heap.adopt(session)?),
                lash_kernel_state::kernel_internals::session_functions(session),
            ),
            None => (Tree::new(false), KeyedRoot::Empty),
        };
        Ok(Self {
            heap,
            bindings,
            functions,
            changed: false,
        })
    }

    /// Every binding's name, in name order. Reads every page of the
    /// bindings: a host keeps what it read.
    ///
    /// # Errors
    ///
    /// The heap's faults.
    pub fn names(&self) -> Result<BTreeSet<Name>, HeapFault> {
        let mut names = Vec::new();
        self.bindings.visit(self.heap.pages(), &mut |_, record| {
            names.push(record.name.clone());
            true
        })?;
        names
            .iter()
            .map(|name| name.resolve(self.heap.pages()))
            .collect()
    }

    /// The names whose bindings differ between `older` and `newer`, two
    /// versions of one session (`older` none for a session that held
    /// nothing), each with whether `older` and `newer` bind it: read over
    /// only the pages the two do not share.
    ///
    /// # Errors
    ///
    /// The heap's faults.
    pub fn changed(
        &self,
        older: Option<&SessionRoot>,
        newer: &SessionRoot,
    ) -> Result<BTreeMap<Name, (bool, bool)>, HeapFault> {
        let older = Tree::<BindingRecord>::of_keyed(&older.map_or(KeyedRoot::Empty, |older| {
            lash_kernel_state::kernel_internals::session_bindings(older)
        }));
        let newer = Tree::<BindingRecord>::of_keyed(
            &lash_kernel_state::kernel_internals::session_bindings(newer),
        );
        let mut changed = Vec::new();
        older.diff(&newer, self.heap.pages(), &mut |(_, old, new)| {
            if let Some(record) = new.or(old) {
                changed.push((record.name.clone(), (old.is_some(), new.is_some())));
            }
        })?;
        changed
            .into_iter()
            .map(|(name, held)| Ok((name.resolve(self.heap.pages())?, held)))
            .collect()
    }

    /// The saved functions whose records differ between `older` and
    /// `newer`, two versions of one session (`older` none for a session
    /// that held nothing), by declared name: each with its description in
    /// `newer`, none for one `newer` does not hold. Reads only the pages
    /// the two do not share.
    ///
    /// # Errors
    ///
    /// The heap's faults, and a description that is not bytes.
    pub fn changed_functions(
        &self,
        older: Option<&SessionRoot>,
        newer: &SessionRoot,
    ) -> Result<BTreeMap<Name, Option<Vec<u8>>>, HeapFault> {
        let older = Tree::<FunctionRecord>::of_keyed(&older.map_or(KeyedRoot::Empty, |older| {
            lash_kernel_state::kernel_internals::session_functions(older)
        }));
        let newer = Tree::<FunctionRecord>::of_keyed(
            &lash_kernel_state::kernel_internals::session_functions(newer),
        );
        let mut changed = Vec::new();
        older.diff(&newer, self.heap.pages(), &mut |(_, old, new)| {
            if let Some(record) = new.or(old) {
                changed.push((record.name.clone(), new.cloned()));
            }
        })?;
        changed
            .into_iter()
            .map(|(name, record)| {
                let name = name.resolve(self.heap.pages())?;
                let meta = record.map(|record| self.meta(&name, &record)).transpose()?;
                Ok((name, meta))
            })
            .collect()
    }

    fn meta(&self, name: &Name, record: &FunctionRecord) -> Result<Vec<u8>, HeapFault> {
        match crate::data::copy_page_out(&self.heap, &record.meta) {
            Ok(Datum::Bytes(meta)) => Ok(meta.as_slice().to_vec()),
            Err(crate::data::CopyError::Heap(fault)) => Err(fault),
            Ok(_) | Err(crate::data::CopyError::Raised(_)) => Err(HeapFault::Malformed(format!(
                "saved function `{name}`'s description is not bytes"
            ))),
        }
    }

    /// The binding `name`'s value, copied out as data.
    ///
    /// # Errors
    ///
    /// The heap's faults.
    pub fn get(&mut self, name: &Name) -> Result<Option<SessionValue>, HeapFault> {
        let Some(record) = self.bindings.get(&metadata_key(name), self.heap.pages())? else {
            return Ok(None);
        };
        let value = match record.value {
            BoundRecord::Value(value) => value,
            BoundRecord::Shared(id) => self.heap.variable_get(id)?,
        };
        Ok(Some(match crate::data::copy_page_out(&self.heap, &value) {
            Ok(datum) => SessionValue::Data(datum),
            Err(crate::data::CopyError::Heap(fault)) => return Err(fault),
            Err(crate::data::CopyError::Raised(_)) => SessionValue::Opaque,
        }))
    }

    /// Whether the value bound to `binding` refers to the saved function
    /// `function` anywhere in it: what a cell start asks before dropping
    /// the binding that held a function saved under a generated name.
    ///
    /// # Errors
    ///
    /// The heap's faults.
    pub fn refers(&self, binding: &Name, function: &Name) -> Result<bool, HeapFault> {
        use lash_kernel_doc::heap::{Element, PageReader as _};
        use lash_kernel_doc::snapshot::{members, text_of};
        let Some(record) = self
            .bindings
            .get(&metadata_key(binding), self.heap.pages())?
        else {
            return Ok(false);
        };
        let mut pending = vec![match record.value {
            BoundRecord::Value(value) => value,
            BoundRecord::Shared(id) => self.heap.variable_get(id)?,
        }];
        let mut objects = BTreeSet::new();
        while let Some(value) = pending.pop() {
            match &value {
                PageValue::Function(name) => {
                    if text_of(&self.heap, name)? == function.as_str() {
                        return Ok(true);
                    }
                }
                PageValue::Tuple(tuple) => pending.extend(members(&self.heap, tuple)?),
                PageValue::List(id)
                | PageValue::Map(id)
                | PageValue::Set(id)
                | PageValue::Record(id)
                    if objects.insert(*id) =>
                {
                    self.heap.visit(*id, &mut |element| {
                        match element {
                            Element::Item(item) => pending.push(item.clone()),
                            Element::Entry { key, value } => {
                                pending.push(key.clone());
                                pending.push(value.clone());
                            }
                            Element::Field { value, .. } => pending.push(value.clone()),
                        }
                        std::ops::ControlFlow::Continue(())
                    })?;
                }
                _ => {}
            }
        }
        Ok(false)
    }

    /// Binds `name` to `value`, declared at `main` of no cell.
    ///
    /// # Errors
    ///
    /// The heap's faults, and a value the heap refuses.
    pub fn set(&mut self, name: &Name, value: &Datum) -> Result<(), HeapFault> {
        let frozen = self.heap.freeze_datum(value).map_err(write_fault)?;
        let value: PageValue = self.heap.instantiate(&frozen).map_err(write_fault)?;
        let record = BindingRecord {
            name: NameRef::new(name, self.heap.pages())?,
            declared: SiteRef::new(
                &lash_kernel_doc::Site::new(lash_kernel_doc::Unit::Main, Vec::new()),
                self.heap.pages(),
            )?,
            value: BoundRecord::Value(value),
        };
        self.bindings
            .write(metadata_key(name), record, self.heap.pages())?;
        self.changed = true;
        Ok(())
    }

    /// Unbinds `name`. Answers whether it was bound.
    ///
    /// # Errors
    ///
    /// The heap's faults.
    pub fn remove(&mut self, name: &Name) -> Result<bool, HeapFault> {
        let removed = self
            .bindings
            .remove(&metadata_key(name), self.heap.pages())?
            .is_some();
        self.changed |= removed;
        Ok(removed)
    }

    /// Every saved function the session holds, by its declared name, with
    /// the bytes its dialect described it in.
    ///
    /// # Errors
    ///
    /// The heap's faults, and a description that is not bytes.
    pub fn functions(&self) -> Result<BTreeMap<Name, Vec<u8>>, HeapFault> {
        let mut records = Vec::new();
        Tree::<FunctionRecord>::of_keyed(&self.functions).visit(
            self.heap.pages(),
            &mut |_, record| {
                records.push(record.clone());
                true
            },
        )?;
        let mut functions = BTreeMap::new();
        for record in records {
            let name = record.name.resolve(self.heap.pages())?;
            let meta = self.meta(&name, &record)?;
            functions.insert(name, meta);
        }
        Ok(functions)
    }

    /// Rewrites the saved function `name` as relinked to its next cell's
    /// package set (K-LIB-009): its description, now `description`, and
    /// the execution closure its code calls into, now `closure`. Its
    /// captures, token and binding stay as they are. Answers whether the
    /// session held it.
    ///
    /// # Errors
    ///
    /// The heap's faults.
    pub fn relink_function(
        &mut self,
        name: &Name,
        description: &[u8],
        closure: lash_kernel_doc::heap::PageRef,
    ) -> Result<bool, HeapFault> {
        let mut functions = Tree::<FunctionRecord>::of_keyed(&self.functions);
        let key = metadata_key(name);
        let Some(mut record) = functions.get(&key, self.heap.pages())? else {
            return Ok(false);
        };
        record.meta = PageValue::Bytes(self.heap.bytes(description)?);
        record.closure = closure;
        functions.write(key, record, self.heap.pages())?;
        self.functions = sealed(functions, &mut self.heap)?;
        self.changed = true;
        Ok(true)
    }

    /// Drops the saved function `name` and the binding of its name: what a
    /// cell start does with one its package set cannot serve
    /// (RELINK-CLOSURE). Answers whether the session held it.
    ///
    /// # Errors
    ///
    /// The heap's faults.
    pub fn drop_function(&mut self, name: &Name) -> Result<bool, HeapFault> {
        let mut functions = Tree::<FunctionRecord>::of_keyed(&self.functions);
        if functions
            .remove(&metadata_key(name), self.heap.pages())?
            .is_none()
        {
            return Ok(false);
        }
        self.functions = sealed(functions, &mut self.heap)?;
        self.remove(name)?;
        self.changed = true;
        Ok(true)
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
        let mut work = lash_kernel_state::kernel_internals::tree::SealWork::new(
            lash_kernel_state::paged::SealBudget {
                pages: u64::MAX,
                bytes: u64::MAX,
            },
        );
        self.bindings.seal(self.heap.pages(), &mut work)?;
        let bindings = self.bindings.keyed_root()?;
        self.heap.carry(CarryRoots {
            bindings,
            functions: self.functions,
        })
    }
}

/// `tree` sealed, as the root a session names.
fn sealed(mut tree: Tree<FunctionRecord>, heap: &mut PagedHeap) -> Result<KeyedRoot, HeapFault> {
    let mut work = lash_kernel_state::kernel_internals::tree::SealWork::new(
        lash_kernel_state::paged::SealBudget {
            pages: u64::MAX,
            bytes: u64::MAX,
        },
    );
    tree.seal(heap.pages(), &mut work)?;
    tree.keyed_root()
}

fn write_fault(error: HeapWriteError) -> HeapFault {
    match error {
        HeapWriteError::Fault(fault) => fault,
        other => HeapFault::Malformed(format!("the session refused a host value: {other}")),
    }
}
