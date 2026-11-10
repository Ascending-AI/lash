//! The run's directory (cutover contract §6): every written object's row,
//! keyed by id, in a copy-on-write keyed tree over the last sealed one.
//!
//! A row holds the object's record and its exact edge set, so a mark over
//! the directory reads rows only. Sealing writes the changed path and
//! leaves every older root's pages as they were: a retained checkpoint's
//! directory never changes.

use std::collections::BTreeSet;

use lash_kernel_doc::heap::{HeapFault, LogicalBytes, PageRef};
use lash_kernel_value::tokens::{TokenCodec, TokenError, TokenReader, TokenWriter};

use super::super::tree::{Access, Cell, SealWork, Tree};
use super::super::{PageSource, SealBudget};
use super::ids::ObjectId;
use super::objects::{Heads, Record};

/// One written object: its record, the heads of the immutables it names
/// and the ids it references.
#[derive(Clone, Debug, PartialEq)]
pub struct Row {
    pub record: Record,
    /// Exactly the heads of the immutables [`Self::record`] names.
    pub heads: Heads,
    /// Ascending, once each: exactly [`Record::edges`].
    pub edges: Vec<ObjectId>,
}

impl Row {
    /// The row of `record` with `heads`, its edges derived.
    pub fn of(record: Record, heads: Heads) -> Self {
        let edges = record.edges();
        Self {
            record,
            heads,
            edges,
        }
    }
}

impl TokenCodec for Row {
    fn encode(&self, out: &mut TokenWriter) {
        out.seq(3);
        self.record.encode(out);
        self.heads.encode(out);
        self.edges.encode(out);
    }
    fn decode(input: &mut TokenReader<'_>) -> Result<Self, TokenError> {
        input.expect_struct(3)?;
        let record = Record::decode(input)?;
        let heads = Heads::decode(input)?;
        heads.check(record.values()).map_err(TokenError::Invalid)?;
        let edges = Vec::<ObjectId>::decode(input)?;
        if edges != record.edges() {
            return Err(TokenError::Invalid(
                "a directory row's edges are not its record's".into(),
            ));
        }
        Ok(Self {
            record,
            heads,
            edges,
        })
    }
}

impl Cell for Row {
    fn cost(&self) -> u64 {
        0
    }
    fn children(&self, out: &mut Vec<PageRef>) {
        match &self.record {
            Record::Chunked(chunked) => {
                out.extend(chunked.parts.iter().map(|part| part.page));
                out.extend(chunked.edges.iter().map(|part| part.page));
            }
            Record::Paged(paged) => out.push(paged.root.page),
            _ => {}
        }
    }
    fn grants(&mut self, children: &[PageRef]) -> Result<(), HeapFault> {
        match &mut self.record {
            Record::Chunked(chunked) => {
                for part in &mut chunked.parts {
                    super::super::tree::grant(&mut part.page, children)?;
                }
                for part in &mut chunked.edges {
                    super::super::tree::grant(&mut part.page, children)?;
                }
            }
            Record::Paged(paged) => super::super::tree::grant(&mut paged.root.page, children)?,
            _ => {}
        }
        Ok(())
    }
}

/// A sealed directory's root: its tree page and row count.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DirectoryRoot {
    pub page: Option<PageRef>,
    pub rows: u64,
}

impl TokenCodec for DirectoryRoot {
    fn encode(&self, out: &mut TokenWriter) {
        out.seq(2);
        self.page.encode(out);
        out.unsigned(self.rows);
    }
    fn decode(input: &mut TokenReader<'_>) -> Result<Self, TokenError> {
        input.expect_struct(2)?;
        let page = Option::<PageRef>::decode(input)?;
        let rows = input.unsigned()?;
        if page.is_none() != (rows == 0) {
            return Err(TokenError::Invalid(
                "a directory root's page and row count disagree".into(),
            ));
        }
        Ok(Self { page, rows })
    }
}

/// The run's current directory and the page port it reads and seals
/// through.
pub struct Directory {
    access: Access<'static>,
    tree: Tree<Row>,
}

pub(super) fn whole() -> SealWork {
    SealWork::new(SealBudget {
        pages: u64::MAX,
        bytes: u64::MAX,
    })
}

impl Directory {
    /// An empty directory over `source`.
    pub fn new(source: Box<dyn PageSource>) -> Self {
        Self {
            access: Access::new(source),
            tree: Tree::new(false),
        }
    }

    /// The directory `root` names, read through `source`; nothing is read
    /// until a row is.
    pub fn open(root: DirectoryRoot, source: Box<dyn PageSource>) -> Self {
        Self::over(root, Access::new(source))
    }

    /// The directory `root` names, read through `access`.
    pub fn over(root: DirectoryRoot, access: Access<'static>) -> Self {
        Self {
            access,
            tree: Tree::open(root.page, root.rows, LogicalBytes(0), false),
        }
    }

    /// The directory's sealed root page, when it has one and no row has
    /// changed since: the page a missing row is reported against.
    pub fn page(&self) -> Option<PageRef> {
        self.tree.page()
    }

    /// How many rows the directory holds.
    pub fn len(&self) -> u64 {
        self.tree.len()
    }

    pub fn is_empty(&self) -> bool {
        self.tree.is_empty()
    }

    /// The row of `id`, if the directory holds one.
    ///
    /// # Errors
    ///
    /// The page port's faults.
    pub fn get(&self, id: ObjectId) -> Result<Option<Row>, HeapFault> {
        self.tree.get(&id.key(), &self.access)
    }

    /// Writes `row` as the row of `id`.
    ///
    /// # Errors
    ///
    /// The page port's faults.
    pub fn put(&mut self, id: ObjectId, row: Row) -> Result<(), HeapFault> {
        self.tree.write(id.key().to_vec(), row, &self.access)
    }

    /// Removes the row of `id`.
    ///
    /// # Errors
    ///
    /// The page port's faults.
    pub fn remove(&mut self, id: ObjectId) -> Result<Option<Row>, HeapFault> {
        self.tree.remove(&id.key(), &self.access)
    }

    /// Every id `roots` reach through the rows' edges, the roots included
    /// when the directory holds them. Reads rows only.
    ///
    /// # Errors
    ///
    /// The page port's faults; [`HeapFault::Malformed`] for an edge to an
    /// id the directory does not hold.
    pub fn reached(
        &self,
        roots: impl IntoIterator<Item = ObjectId>,
    ) -> Result<BTreeSet<ObjectId>, HeapFault> {
        let mut reached = BTreeSet::new();
        let mut work: Vec<ObjectId> = roots.into_iter().collect();
        while let Some(id) = work.pop() {
            if reached.contains(&id) {
                continue;
            }
            let Some(row) = self.get(id)? else {
                continue;
            };
            reached.insert(id);
            for edge in self.edges(&row)? {
                if !reached.contains(&edge) {
                    if self.get(edge)?.is_none() {
                        return Err(HeapFault::Malformed(format!(
                            "object {} references object {}, which the directory does not hold",
                            id.0, edge.0
                        )));
                    }
                    work.push(edge);
                }
            }
        }
        Ok(reached)
    }

    /// The ids `row` references: its edges, or a large object's edge
    /// pages read. Reads no chunk.
    ///
    /// # Errors
    ///
    /// The page port's faults.
    pub fn edges(&self, row: &Row) -> Result<Vec<ObjectId>, HeapFault> {
        match &row.record {
            Record::Chunked(chunked) => super::chunked::edge_ids(&self.access, &chunked.edges),
            _ => Ok(row.edges.clone()),
        }
    }

    /// Drops every row `roots` do not reach through the rows' edges, from
    /// this directory only; gives how many it dropped.
    ///
    /// # Errors
    ///
    /// [`Directory::reached`]'s.
    pub fn prune(&mut self, roots: impl IntoIterator<Item = ObjectId>) -> Result<u64, HeapFault> {
        let reached = self.reached(roots)?;
        let mut dead = Vec::new();
        self.tree.visit(&self.access, &mut |key, _| {
            if let Some(id) = ObjectId::of_key(key)
                && !reached.contains(&id)
            {
                dead.push(id);
            }
            true
        })?;
        for id in &dead {
            self.remove(*id)?;
        }
        Ok(dead.len() as u64)
    }

    /// Seals the changed rows and their paths, giving the new root.
    ///
    /// # Errors
    ///
    /// The page port's faults.
    pub fn seal(&mut self) -> Result<DirectoryRoot, HeapFault> {
        self.tree.seal(&self.access, &mut whole())?;
        Ok(DirectoryRoot {
            page: self.tree.page(),
            rows: self.tree.len(),
        })
    }

    /// The page port, for the root page and the records' own pages.
    pub fn access(&self) -> &Access<'static> {
        &self.access
    }
}
