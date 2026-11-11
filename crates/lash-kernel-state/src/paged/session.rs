//! A carried session: the run session page a cell starts from, and what a
//! host reads of its pages without opening its heap.
//!
//! A host reads whether names are bound, and the session's per-name notes.
//! The notes are a keyed table beside the bindings (FIG-6418). A row
//! exists only while a name has a note: why an earlier cell's binding of
//! it is not carried (`K-SES-003`), or the cell whose end froze what the
//! saved function of the name reads (FIG-5799). The rows cost nothing:
//! they are host bookkeeping, not guest memory.

use std::collections::BTreeMap;

use lash_kernel_doc::heap::{HeapFault, PageGrant, PageRef};
use lash_kernel_value::names::Name;
use {
    lash_kernel_value::tokens::TokenCodec, lash_kernel_value::tokens::TokenError,
    lash_kernel_value::tokens::TokenReader, lash_kernel_value::tokens::TokenWriter,
};

use super::metadata::metadata_key;
use super::source::grant;
use super::tree::{Access, Cell, SealWork, Tree};
use super::{KeyedRoot, NameRef, PageSource, SealBudget, SessionRoot};

impl SessionRoot {
    /// The bytecode core's session sealed under run session page `page`.
    #[must_use]
    pub fn run(page: PageRef) -> Self {
        Self(page)
    }

    /// The run session page.
    #[must_use]
    pub fn page(&self) -> PageRef {
        self.0
    }

    /// The pages this session names: its run session page.
    #[must_use]
    pub fn pages(&self) -> Vec<PageRef> {
        vec![self.0]
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
        self.0.grant = grant(self.0)?;
        Ok(())
    }

    /// Points each page it names at the one `children`, the child table of
    /// the page it was read from, authenticates.
    ///
    /// # Errors
    ///
    /// [`HeapFault::Corrupt`] for a page `children` does not declare.
    pub fn grant_children(&mut self, children: &[PageRef]) -> Result<(), HeapFault> {
        grant(&mut self.0, children)
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
