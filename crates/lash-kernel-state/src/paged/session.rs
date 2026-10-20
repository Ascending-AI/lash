//! A carried session: how a cell runs over it, and what a host reads of
//! its pages without opening its heap.
//!
//! A cell's touched names are slots over its cold session root. Tree edits
//! happen only at projection; the census visits the overlay as working roots.
//!
//! A host reads whether names are bound, and the session's per-name notes.
//! The notes are a keyed table beside the bindings (FIG-6418). A row
//! exists only while a name has a note: why an earlier cell's binding of
//! it is not carried (`K-SES-003`), or the cell whose end froze what the
//! saved function of the name reads (FIG-5799). The rows cost nothing:
//! they are host bookkeeping, not guest memory.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::rc::Rc;

use lash_kernel_doc::Name;
use lash_kernel_doc::graph_tokens::{TokenCodec, TokenError, TokenReader, TokenWriter};
use lash_kernel_doc::heap::{HeapFault, PageGrant, PageRef, PageValue};

use super::heap_impl::PagedHeap;
use super::metadata::metadata_key;
use super::source::grant;
use super::tree::{Access, Cell, SealWork, Tree};
use super::{
    BindingRecord, BoundRecord, HeapWriteError, KeyedRoot, NameRef, PageSource, SealBudget,
    SemanticHeap, SessionRoot,
};

#[derive(Default)]
pub(super) struct SessionSlots {
    names: HashMap<Rc<[u8]>, usize>,
    slots: Vec<Slot>,
    dirty: Vec<usize>,
}
struct Slot {
    key: Rc<[u8]>,
    record: Option<BindingRecord>,
    dirty: bool,
}

impl SessionSlots {
    fn get(&self, key: &[u8]) -> Option<&Option<BindingRecord>> {
        self.names.get(key).map(|slot| &self.slots[*slot].record)
    }
}

impl PagedHeap {
    /// Loads a touched name once. Inspectors can read without installing a
    /// working slot; execution uses this admission before growing the overlay.
    pub fn load_session_binding_at(
        &mut self,
        key: &[u8],
    ) -> Result<Option<PageValue>, HeapWriteError> {
        self.session_slot(key)?;
        self.session_binding_at(key).map_err(Into::into)
    }

    /// [`Self::load_session_binding_at`], the binding unresolved: a shared
    /// variable stays its identity, for the machine's arena to read.
    pub fn load_session_bound_at(
        &mut self,
        key: &[u8],
    ) -> Result<Option<BoundRecord>, HeapWriteError> {
        let slot = self.session_slot(key)?;
        Ok(self.session_slots.slots[slot]
            .record
            .as_ref()
            .map(|record| record.value.clone()))
    }

    fn session_slot(&mut self, key: &[u8]) -> Result<usize, HeapWriteError> {
        if let Some(slot) = self.session_slots.names.get(key) {
            return Ok(*slot);
        }
        let record = self.session.get(key, &self.access)?;
        let slots = &mut self.session_slots;
        let slot = slots.slots.len();
        let key: Rc<[u8]> = Rc::from(key);
        slots.names.insert(Rc::clone(&key), slot);
        slots.slots.push(Slot {
            key,
            record,
            dirty: false,
        });
        Ok(slot)
    }

    /// The value the session binds `name` to, if it binds it.
    pub fn session_binding(
        &self,
        name: &lash_kernel_doc::Name,
    ) -> Result<Option<PageValue>, HeapFault> {
        self.session_binding_at(&super::metadata::metadata_key(name))
    }

    /// Reads the working slot, falling back to the untouched cold root.
    pub fn session_binding_at(&self, key: &[u8]) -> Result<Option<PageValue>, HeapFault> {
        let binding = match self.session_slots.get(key) {
            Some(record) => record.as_ref().map(|record| record.value.clone()),
            None => self
                .session
                .get_with(key, &self.access, |record| record.map(|r| r.value.clone()))?,
        };
        binding
            .map(|binding| match binding {
                BoundRecord::Value(value) => Ok(value),
                BoundRecord::Shared(id) => self.variable_get(id),
            })
            .transpose()
    }

    /// Prepares and admits a slot replacement without editing a heap tree.
    /// `None` leaves a missing assignment untouched.
    pub fn write_session_binding(
        &mut self,
        key: &[u8],
        write: impl FnOnce(
            &mut Self,
            Option<&BindingRecord>,
        ) -> Result<Option<BindingRecord>, HeapWriteError>,
    ) -> Result<bool, HeapWriteError> {
        let slot = self.session_slot(key)?;
        let previous = self.session_slots.slots[slot].record.clone();
        let Some(record) = write(self, previous.as_ref())? else {
            return Ok(false);
        };
        let old_tasks = previous.as_ref().map_or(0, Cell::task_edges);
        let new_tasks = record.task_edges();
        self.replace_holding(old_tasks, new_tasks, |heap| {
            let slots = &mut heap.session_slots;
            slots.slots[slot].record = Some(record);
            // Updating an already dirty slot allocates nothing.
            if !slots.slots[slot].dirty {
                slots.slots[slot].dirty = true;
                slots.dirty.push(slot);
            }
        })?;
        Ok(true)
    }

    /// Gives the slot of `key` the record of the value the machine held in
    /// memory since it wrote the binding, at a projection point (FIG-6748):
    /// the task edges the run holds follow the record.
    ///
    /// # Errors
    ///
    /// The faults of the pages, and a population that does not add up.
    pub fn project_session_binding(
        &mut self,
        key: &[u8],
        record: BindingRecord,
    ) -> Result<(), HeapWriteError> {
        let slot = self.session_slot(key)?;
        let slots = &mut self.session_slots;
        let previous = slots.slots[slot].record.take();
        let old_tasks = previous.as_ref().map_or(0, Cell::task_edges);
        self.population.task_edges = self
            .population
            .task_edges
            .checked_sub(old_tasks)
            .and_then(|edges| edges.checked_add(record.task_edges()))
            .ok_or_else(|| HeapFault::Malformed("session task-edge population overflow".into()))?;
        slots.slots[slot].record = Some(record);
        if !slots.slots[slot].dirty {
            slots.slots[slot].dirty = true;
            slots.dirty.push(slot);
        }
        self.header = None;
        Ok(())
    }

    /// Projects dirty slots in canonical key order. Clean slots never edit
    /// the tree.
    pub(super) fn project_session(&mut self) -> Result<(), HeapFault> {
        if self.session_slots.slots.is_empty() {
            return Ok(());
        }
        let mut root = self.session.clone();
        self.session_slots.dirty.sort_unstable_by(|a, b| {
            self.session_slots.slots[*a]
                .key
                .cmp(&self.session_slots.slots[*b].key)
        });
        for slot in &self.session_slots.dirty {
            let slot = &self.session_slots.slots[*slot];
            if let Some(record) = &slot.record {
                root.write(slot.key.to_vec(), record.clone(), &self.access)?;
            }
        }
        self.session = root;
        self.session_slots = SessionSlots::default();
        Ok(())
    }

    /// [`Self::visit_session_bindings`], but for those of `written`'s keys.
    pub(super) fn visit_session_bindings_over(
        &self,
        written: &[Vec<u8>],
        visit: &mut dyn FnMut(&BindingRecord) -> Result<(), HeapFault>,
    ) -> Result<(), HeapFault> {
        let replaced: HashSet<&[u8]> = written.iter().map(|key| &**key).collect();
        let mut result = Ok(());
        self.session.visit(&self.access, &mut |key, record| {
            if !self.session_slots.names.contains_key(key) && !replaced.contains(key) {
                result = visit(record);
            }
            result.is_ok()
        })?;
        result?;
        for slot in &self.session_slots.slots {
            if let Some(record) = &slot.record
                && !replaced.contains(&*slot.key)
            {
                visit(record)?;
            }
        }
        Ok(())
    }

    pub(super) fn visit_session_bindings(
        &self,
        visit: &mut dyn FnMut(&BindingRecord) -> Result<(), HeapFault>,
    ) -> Result<(), HeapFault> {
        let mut result = Ok(());
        self.session.visit(&self.access, &mut |key, record| {
            if !self.session_slots.names.contains_key(key) {
                result = visit(record);
            }
            result.is_ok()
        })?;
        result?;
        for slot in &self.session_slots.slots {
            if let Some(record) = &slot.record {
                visit(record)?;
            }
        }
        Ok(())
    }
}

impl SessionRoot {
    /// The pages this session names, in its order: its heap header, then
    /// the first page of its bindings and of its saved functions, where it
    /// holds any.
    #[must_use]
    pub fn pages(&self) -> Vec<PageRef> {
        let mut pages = vec![self.heap];
        pages.extend(self.bindings.page());
        pages.extend(self.functions.page());
        pages
    }

    /// The first page of its bindings, none when it binds nothing: where a
    /// host that reads every binding starts fetching.
    #[must_use]
    pub fn bindings_page(&self) -> Option<PageRef> {
        self.bindings.page()
    }

    /// Sets the grant of each page it names to what `grant` gives it, in
    /// [`Self::pages`]' order.
    ///
    /// # Errors
    ///
    /// The first of `grant`'s errors.
    pub fn grant_pages<E>(
        &mut self,
        mut grant: impl FnMut(PageRef) -> Result<PageGrant, E>,
    ) -> Result<(), E> {
        self.heap.grant = grant(self.heap)?;
        for page in [self.bindings.page_mut(), self.functions.page_mut()]
            .into_iter()
            .flatten()
        {
            page.grant = grant(*page)?;
        }
        Ok(())
    }

    /// Points each page it names at the one `children`, the child table of
    /// the page it was read from, authenticates.
    ///
    /// # Errors
    ///
    /// [`HeapFault::Corrupt`] for a page `children` does not declare.
    pub fn grant_children(&mut self, children: &[PageRef]) -> Result<(), HeapFault> {
        grant(&mut self.heap, children)?;
        for page in [self.bindings.page_mut(), self.functions.page_mut()]
            .into_iter()
            .flatten()
        {
            grant(page, children)?;
        }
        Ok(())
    }
}

/// A session name's notes as stored: the reason as a paged text.
#[derive(Clone, Debug)]
struct NoteRecord {
    name: NameRef,
    not_carried: Option<NameRef>,
    saved: Option<u64>,
}

impl TokenCodec for NoteRecord {
    fn encode(&self, out: &mut TokenWriter) {
        out.seq(3);
        self.name.encode(out);
        self.not_carried.encode(out);
        self.saved.encode(out);
    }

    fn decode(input: &mut TokenReader<'_>) -> Result<Self, TokenError> {
        input.expect_struct(3)?;
        Ok(Self {
            name: NameRef::decode(input)?,
            not_carried: Option::decode(input)?,
            saved: Option::decode(input)?,
        })
    }
}

impl Cell for NoteRecord {
    fn cost(&self) -> u64 {
        0
    }

    fn children(&self, out: &mut Vec<PageRef>) {
        self.name.children(out);
        if let Some(not_carried) = &self.not_carried {
            not_carried.children(out);
        }
    }

    fn grants(&mut self, children: &[PageRef]) -> Result<(), HeapFault> {
        self.name.grants(children)?;
        if let Some(not_carried) = &mut self.not_carried {
            not_carried.grants(children)?;
        }
        Ok(())
    }
}

/// A session's notes table, as the descriptor beside its bindings names
/// it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SessionNotes(KeyedRoot);

impl SessionNotes {
    /// The table that holds no note.
    pub const EMPTY: Self = Self(KeyedRoot::Empty);

    /// The table's first page, none when it holds no note.
    #[must_use]
    pub fn page(&self) -> Option<PageRef> {
        self.0.page()
    }

    /// Points the table's page at the one `children`, the child table of
    /// the page it was read from, authenticates.
    ///
    /// # Errors
    ///
    /// [`HeapFault::Corrupt`] for a page `children` does not declare.
    pub fn grant_children(&mut self, children: &[PageRef]) -> Result<(), HeapFault> {
        match self.0.page_mut() {
            Some(page) => grant(page, children),
            None => Ok(()),
        }
    }
}

impl TokenCodec for SessionNotes {
    fn encode(&self, out: &mut TokenWriter) {
        self.0.encode(out);
    }

    fn decode(input: &mut TokenReader<'_>) -> Result<Self, TokenError> {
        KeyedRoot::decode(input).map(Self)
    }
}

/// A session name's notes.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Note {
    /// Why an earlier cell's binding of the name is not carried
    /// (`K-SES-003`).
    pub not_carried: Option<String>,
    /// The cell whose end froze what the saved function of this name
    /// reads.
    pub saved: Option<u64>,
}

/// Every note the notes table `notes` holds, by name, read through
/// `source`.
///
/// # Errors
///
/// The heap's faults reading the table's pages.
pub fn read_notes(
    notes: &SessionNotes,
    source: Box<dyn PageSource>,
) -> Result<BTreeMap<Name, Note>, HeapFault> {
    let access = Access::new(source);
    let tree = Tree::<NoteRecord>::of_keyed(&notes.0);
    let mut records = Vec::new();
    tree.visit(&access, &mut |_, record| {
        records.push(record.clone());
        true
    })?;
    let mut notes = BTreeMap::new();
    for record in records {
        let name = record.name.resolve(&access)?;
        let not_carried = record
            .not_carried
            .map(|why| why.resolve(&access))
            .transpose()?
            .map(|why| why.to_string());
        notes.insert(
            name,
            Note {
                not_carried,
                saved: record.saved,
            },
        );
    }
    Ok(notes)
}

/// The notes table under edit: each name's note is changed in place, a
/// note left empty is removed, and the table is sealed once.
pub struct NotesEdit<'a> {
    tree: Tree<NoteRecord>,
    access: Access<'a>,
}

impl NotesEdit<'_> {
    /// `name` is carried: its not-carried reason dies.
    ///
    /// # Errors
    ///
    /// The heap's faults reading or writing the table's pages.
    pub fn carried(&mut self, name: &Name) -> Result<(), HeapFault> {
        self.note(name, |record| record.not_carried = None)
    }

    /// `name` is not carried, for the reason `why`.
    ///
    /// # Errors
    ///
    /// The heap's faults reading or writing the table's pages.
    pub fn not_carried(&mut self, name: &Name, why: &str) -> Result<(), HeapFault> {
        let why = NameRef::new(&Name::new(why), &self.access)?;
        self.note(name, |record| record.not_carried = Some(why))
    }

    /// The cell whose end froze what the saved function of `name` reads,
    /// none when no saved function of it is held.
    ///
    /// # Errors
    ///
    /// The heap's faults reading or writing the table's pages.
    pub fn saved(&mut self, name: &Name, cell: Option<u64>) -> Result<(), HeapFault> {
        self.note(name, |record| record.saved = cell)
    }

    fn note(&mut self, name: &Name, f: impl FnOnce(&mut NoteRecord)) -> Result<(), HeapFault> {
        let key = metadata_key(name);
        let mut record = match self.tree.get(&key, &self.access)? {
            Some(record) => record,
            None => NoteRecord {
                name: NameRef::new(name, &self.access)?,
                not_carried: None,
                saved: None,
            },
        };
        f(&mut record);
        if record.not_carried.is_none() && record.saved.is_none() {
            self.tree.remove(&key, &self.access)?;
        } else {
            self.tree.write(key, record, &self.access)?;
        }
        Ok(())
    }

    fn seal(mut self) -> Result<SessionNotes, HeapFault> {
        let mut work = SealWork::new(SealBudget {
            pages: u64::MAX,
            bytes: u64::MAX,
        });
        self.tree.seal(&self.access, &mut work)?;
        self.tree.keyed_root().map(SessionNotes)
    }
}

/// Edits the notes table `notes` through `f`, and seals it into `source`.
/// Unchanged subtrees keep their pages: the sealed table shares them with
/// `notes`.
///
/// # Errors
///
/// The heap's faults, and `f`'s.
pub fn edit_notes(
    notes: &SessionNotes,
    source: Box<dyn PageSource>,
    f: impl FnOnce(&mut NotesEdit<'_>) -> Result<(), HeapFault>,
) -> Result<SessionNotes, HeapFault> {
    let access = Access::new(source);
    let tree = Tree::<NoteRecord>::of_keyed(&notes.0);
    let mut edit = NotesEdit { tree, access };
    f(&mut edit)?;
    edit.seal()
}
