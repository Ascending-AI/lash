//! A session's globals as typed facts (FIG-1438): every name a cell can use
//! or an earlier cell left, the cell whose committed transition last bound
//! it, and what it holds; and how many of them the prompt's bound-variables
//! listing shows under its recorded budget.
//!
//! The provenance is read from the committed cell records' binding changes
//! (FIG-5865), not kept beside the bindings: the record is the one fact of
//! what a cell's transition did. The facts are frozen with the call's prompt
//! cut, so a continuation names the snapshot it pages through and is refused
//! against any other.

/// What one session global holds.
#[derive(Clone, Debug, PartialEq)]
#[non_exhaustive]
pub enum SessionGlobalKind {
    /// Data a cell reads, as the host reads it.
    Data(serde_json::Value),
    /// A carried value that cannot be copied out as host data, such as a
    /// record containing saved functions. It remains bound in the cell.
    Opaque,
    /// A bound value whose contents exceed the prompt's read budget.
    /// Inspect the live value in a cell; this is not a partial data value.
    Unpreviewed {
        /// The kernel value kind.
        kind: String,
        /// Top-level extent: bytes for text/bytes, members for containers,
        /// bits for a large integer, one for other values.
        extent: u64,
    },
    /// A host resource handle. It stays opaque: a cell passes it whole to a
    /// tool and has no field of it to read.
    Handle {
        /// The handle's type.
        kind: String,
        /// The host's identifier for the resource.
        id: String,
    },
    /// A saved function, by its signature.
    Function {
        signature: String,
        /// The session's cell, counted from one, whose end froze what the
        /// function reads; `None` for one seeded before the session ran a
        /// cell.
        frozen_at: Option<u64>,
    },
    /// A name a cell left holding what a session does not carry, with why;
    /// it is not bound.
    NotCarried { why: String },
}

/// One session global.
#[derive(Clone, Debug, PartialEq)]
pub struct SessionGlobal {
    pub name: String,
    /// The id of the cell whose committed transition last bound the name, as
    /// `history` names the cell. `None` when no cell record of the frame names
    /// it: a host seeded it, or the cell that bound it was compacted away or
    /// kept no names past its record's bound.
    pub cell: Option<String>,
    pub kind: SessionGlobalKind,
}

/// Every global of a session at one prompt cut, in name order, and how many
/// of them, from the first, the bound-variables listing shows.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct SessionGlobals {
    entries: Vec<SessionGlobal>,
    listed: usize,
    snapshot: u64,
}

/// Where a page of a session's globals continues: an offset into the
/// globals of one snapshot.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct GlobalsPage {
    snapshot: u64,
    offset: usize,
}

impl GlobalsPage {
    /// The index of the first global the page starts at.
    pub fn offset(&self) -> usize {
        self.offset
    }
}

/// A continuation taken from another snapshot of a session's globals.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
#[error("the globals page continues another snapshot of the session's globals")]
pub struct StaleGlobalsPage;

/// One page of a session's globals and where the next one starts.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct GlobalsPageView<'a> {
    pub entries: &'a [SessionGlobal],
    /// `None` on the last page.
    pub next: Option<GlobalsPage>,
}

impl SessionGlobals {
    /// `entries` in name order, the first `listed` of them shown, under the
    /// digest `snapshot` of what they hold.
    pub(crate) fn new(entries: Vec<SessionGlobal>, listed: usize, snapshot: u64) -> Self {
        debug_assert!(listed <= entries.len());
        debug_assert!(entries.windows(2).all(|pair| pair[0].name < pair[1].name));
        Self {
            entries,
            listed,
            snapshot,
        }
    }

    /// Include the carried root so a change beyond the preview budget
    /// still invalidates a continuation of the earlier prompt cut.
    pub(crate) fn at_revision(&mut self, revision: u64) {
        use std::hash::{Hash, Hasher};
        let mut hash = std::collections::hash_map::DefaultHasher::new();
        self.snapshot.hash(&mut hash);
        revision.hash(&mut hash);
        self.snapshot = hash.finish();
    }

    /// Every global, in name order.
    pub fn entries(&self) -> &[SessionGlobal] {
        &self.entries
    }

    /// The globals the listing shows.
    pub fn listed(&self) -> &[SessionGlobal] {
        &self.entries[..self.listed]
    }

    /// How many globals the listing leaves out.
    pub fn omitted(&self) -> usize {
        self.entries.len() - self.listed
    }

    /// The global named `name`, listed or not.
    pub fn get(&self, name: &str) -> Option<&SessionGlobal> {
        self.entries
            .binary_search_by(|entry| entry.name.as_str().cmp(name))
            .ok()
            .map(|index| &self.entries[index])
    }

    /// Where the globals the listing leaves out start; `None` when it shows
    /// them all.
    pub fn continuation(&self) -> Option<GlobalsPage> {
        (self.listed < self.entries.len()).then_some(GlobalsPage {
            snapshot: self.snapshot,
            offset: self.listed,
        })
    }

    /// At most `count` globals from `from`, and where the rest continue.
    ///
    /// # Errors
    ///
    /// [`StaleGlobalsPage`] when `from` was taken from other globals.
    pub fn page(
        &self,
        from: GlobalsPage,
        count: usize,
    ) -> Result<GlobalsPageView<'_>, StaleGlobalsPage> {
        if from.snapshot != self.snapshot || from.offset > self.entries.len() {
            return Err(StaleGlobalsPage);
        }
        let end = from.offset.saturating_add(count).min(self.entries.len());
        Ok(GlobalsPageView {
            entries: &self.entries[from.offset..end],
            next: (end < self.entries.len()).then_some(GlobalsPage {
                snapshot: self.snapshot,
                offset: end,
            }),
        })
    }
}

#[cfg(test)]
mod tests;
