//! The session's per-name notes, a keyed paged tree (FIG-6418): what a
//! bounded [`super::graph::SessionDescriptor`] names instead of carrying
//! inline. A row exists only while a name has a note: why an earlier
//! cell's binding of it is not carried (`K-SES-003`), or the cell whose
//! end froze what the saved function of the name reads (FIG-5799). The
//! tree's rows cost nothing: they are host bookkeeping, not guest memory.

use std::collections::BTreeMap;

use lash_kernel_doc::Name;
use lash_kernel_doc::graph_tokens::{TokenCodec, TokenError, TokenReader, TokenWriter};
use lash_kernel_doc::heap::HeapFault;
use lash_kernel_doc::heap::PageRef;
use lash_kernel_state::paged::metadata::metadata_key;
use lash_kernel_state::paged::tree::{Access, Cell, SealWork, Tree};
use lash_kernel_state::paged::{KeyedRoot, NameRef, PageSource, SealBudget};

/// A session name's notes: why an earlier cell's binding of the name is
/// not carried, as a paged text, or the cell whose end froze what the
/// saved function of the name reads.
#[derive(Clone, Debug)]
pub(super) struct NoteRecord {
    pub name: NameRef,
    /// Why an earlier cell's binding of the name is not carried
    /// (`K-SES-003`), a text that pages like a name's.
    pub not_carried: Option<NameRef>,
    /// The cell whose end froze what the saved function of this name
    /// reads.
    pub saved: Option<u64>,
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

/// What a name's note reads back as: the resolved reason text and the
/// cell.
#[derive(Clone, Debug, Default)]
pub(crate) struct Note {
    pub not_carried: Option<String>,
    pub saved: Option<u64>,
}

/// The session's notes tree `root` names, read whole into resolved notes
/// by name: a row per name an earlier cell's not-carried reason or a saved
/// function's freezing cell is recorded for.
///
/// # Errors
///
/// The heap's faults reading the tree's pages.
pub(super) fn read(
    root: &KeyedRoot,
    source: Box<dyn PageSource>,
) -> Result<BTreeMap<Name, Note>, HeapFault> {
    let access = Access::new(source);
    let tree = Tree::<NoteRecord>::of_keyed(root);
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

/// The session's notes tree under edit: rows are written and removed one
/// by one and the tree sealed once, beside the bindings in the turn's
/// spool.
pub(super) struct NotesEdit<'a> {
    tree: Tree<NoteRecord>,
    access: Access<'a>,
}

impl NotesEdit<'_> {
    /// Runs `f` over `name`'s row, creating it; a row left with no field
    /// set is removed.
    ///
    /// # Errors
    ///
    /// The heap's faults reading or sealing the tree's pages.
    pub fn note(&mut self, name: &Name, f: impl FnOnce(&mut NoteRecord)) -> Result<(), HeapFault> {
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

    /// The text as a [`NameRef`], paged when it does not fit inline.
    ///
    /// # Errors
    ///
    /// The heap's faults sealing its pages.
    pub fn name_text(&self, text: &str) -> Result<NameRef, HeapFault> {
        NameRef::new(&Name::new(text), &self.access)
    }

    fn seal(mut self) -> Result<KeyedRoot, HeapFault> {
        let mut work = SealWork::new(SealBudget {
            pages: u64::MAX,
            bytes: u64::MAX,
        });
        self.tree.seal(&self.access, &mut work)?;
        self.tree.keyed_root()
    }
}

/// Edits the notes tree `root` names through `f`, and seals it into
/// `source`. Unchanged subtrees keep their pages: the sealed root shares
/// them with `root`.
///
/// # Errors
///
/// The heap's faults.
pub(super) fn edit(
    root: &KeyedRoot,
    source: Box<dyn PageSource>,
    f: impl FnOnce(&mut NotesEdit<'_>) -> Result<(), HeapFault>,
) -> Result<KeyedRoot, HeapFault> {
    let access = Access::new(source);
    let tree = Tree::<NoteRecord>::of_keyed(root);
    let mut edit = NotesEdit { tree, access };
    f(&mut edit)?;
    edit.seal()
}
