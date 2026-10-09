//! What became of each node across edits.
//!
//! Node ids are minted from structural paths, so an insertion renumbers every
//! later sibling and an edit inside a lifted process renames its owner.
//! Continuity is therefore never read off ids or positions: it is the record
//! of what each edit and each normalization did to each draft handle.

use std::collections::{BTreeMap, BTreeSet};

use super::{WorkflowDraftHandle, WorkflowDraftRevision};
use crate::workflow_graph::WorkflowNodeId;

/// Where a node that was not in the base came from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WorkflowNodeSource {
    /// An edit authored it.
    Authored,
    /// An edit cloned it from this node.
    Clone { of: WorkflowDraftHandle },
    /// Normalization derived it from this node: a statement of a region the
    /// node's new expression owns, or the process a literal it holds lifts
    /// to.
    Derived { from: WorkflowDraftHandle },
}

/// The outcome of one node between a base and a later document.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum WorkflowCorrespondenceEntry {
    /// The node is where it was. Its id may still have changed, because ids
    /// follow positions.
    Retained {
        handle: WorkflowDraftHandle,
        from: WorkflowNodeId,
        to: WorkflowNodeId,
    },
    /// An edit moved the node.
    Moved {
        handle: WorkflowDraftHandle,
        from: WorkflowNodeId,
        to: WorkflowNodeId,
    },
    Inserted {
        handle: WorkflowDraftHandle,
        to: WorkflowNodeId,
        source: WorkflowNodeSource,
    },
    Deleted {
        handle: WorkflowDraftHandle,
        from: WorkflowNodeId,
    },
    /// Normalization spelled the node as several statements; each has a
    /// handle of its own and the old one is retired.
    Split {
        handle: WorkflowDraftHandle,
        from: WorkflowNodeId,
        into: Vec<(WorkflowDraftHandle, WorkflowNodeId)>,
    },
    /// The node's provenance was lost: nothing says which node of the new
    /// document it is, and none is guessed.
    Unmatched {
        handle: WorkflowDraftHandle,
        from: WorkflowNodeId,
    },
}

/// Every node of a base document and of the document edits made of it, each
/// with its outcome. Base nodes come first, in handle order, then the nodes
/// only the new document has.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WorkflowCorrespondence {
    pub base: WorkflowDraftRevision,
    pub revision: WorkflowDraftRevision,
    pub entries: Vec<WorkflowCorrespondenceEntry>,
}

impl WorkflowCorrespondence {
    /// The new id of the node that had `from` in the base, when the node
    /// survives as one node.
    pub fn successor(&self, from: &WorkflowNodeId) -> Option<&WorkflowNodeId> {
        self.entries.iter().find_map(|entry| match entry {
            WorkflowCorrespondenceEntry::Retained { from: base, to, .. }
            | WorkflowCorrespondenceEntry::Moved { from: base, to, .. }
                if base == from =>
            {
                Some(to)
            }
            _ => None,
        })
    }

    /// This correspondence carried through an admission of the document it
    /// ends at: `admitted` names the admitted id of each node of that
    /// document, and every outcome ends at the admitted node instead. A
    /// surviving node admission does not name is [`Unmatched`]; an inserted
    /// one it does not name has no entry.
    ///
    /// [`Unmatched`]: WorkflowCorrespondenceEntry::Unmatched
    #[must_use]
    pub fn through(&self, admitted: &BTreeMap<WorkflowNodeId, WorkflowNodeId>) -> Self {
        use WorkflowCorrespondenceEntry as Entry;
        let mut entries = Vec::with_capacity(self.entries.len());
        for entry in &self.entries {
            match entry.clone() {
                Entry::Retained { handle, from, to } => entries.push(match admitted.get(&to) {
                    Some(to) => Entry::Retained {
                        handle,
                        from,
                        to: to.clone(),
                    },
                    None => Entry::Unmatched { handle, from },
                }),
                Entry::Moved { handle, from, to } => entries.push(match admitted.get(&to) {
                    Some(to) => Entry::Moved {
                        handle,
                        from,
                        to: to.clone(),
                    },
                    None => Entry::Unmatched { handle, from },
                }),
                Entry::Inserted { handle, to, source } => {
                    if let Some(to) = admitted.get(&to) {
                        entries.push(Entry::Inserted {
                            handle,
                            to: to.clone(),
                            source,
                        });
                    }
                }
                Entry::Split { handle, from, into } => {
                    let into = into
                        .into_iter()
                        .filter_map(|(piece, id)| Some((piece, admitted.get(&id)?.clone())))
                        .collect::<Vec<_>>();
                    entries.push(if into.is_empty() {
                        Entry::Unmatched { handle, from }
                    } else {
                        Entry::Split { handle, from, into }
                    });
                }
                unchanged @ (Entry::Deleted { .. } | Entry::Unmatched { .. }) => {
                    entries.push(unchanged);
                }
            }
        }
        Self {
            base: self.base,
            revision: self.revision,
            entries,
        }
    }
}

/// What edits and normalizations did to handles.
#[derive(Clone, Debug, Default)]
pub(super) struct Journal {
    pub(super) minted: BTreeMap<WorkflowDraftHandle, WorkflowNodeSource>,
    pub(super) deleted: BTreeSet<WorkflowDraftHandle>,
    pub(super) split: BTreeMap<WorkflowDraftHandle, Vec<WorkflowDraftHandle>>,
    pub(super) moved: BTreeSet<WorkflowDraftHandle>,
    pub(super) unmatched: BTreeSet<WorkflowDraftHandle>,
    /// Nodes an edit moved to a body of another process or of `main`.
    pub(super) reframed: BTreeSet<WorkflowDraftHandle>,
}

impl Journal {
    /// Appends a later journal to this one.
    pub(super) fn append(&mut self, later: Self) {
        for (handle, source) in later.minted {
            self.minted.entry(handle).or_insert(source);
        }
        self.deleted.extend(later.deleted);
        self.split.extend(later.split);
        self.moved.extend(later.moved);
        self.unmatched.extend(later.unmatched);
        self.reframed.extend(later.reframed);
    }

    /// The correspondence from the nodes of `base` to the nodes of `now`.
    pub(super) fn correspondence(
        &self,
        base: &BTreeMap<WorkflowDraftHandle, WorkflowNodeId>,
        now: &BTreeMap<WorkflowDraftHandle, WorkflowNodeId>,
    ) -> Vec<WorkflowCorrespondenceEntry> {
        let mut entries = Vec::new();
        let mut accounted = BTreeSet::new();
        for (handle, from) in base {
            let (handle, from) = (*handle, from.clone());
            entries.push(if let Some(to) = now.get(&handle) {
                let to = to.clone();
                if self.moved.contains(&handle) {
                    WorkflowCorrespondenceEntry::Moved { handle, from, to }
                } else {
                    WorkflowCorrespondenceEntry::Retained { handle, from, to }
                }
            } else if self.split.contains_key(&handle) {
                let mut into = Vec::new();
                self.pieces(handle, now, &mut into);
                if into.is_empty() {
                    WorkflowCorrespondenceEntry::Deleted { handle, from }
                } else {
                    accounted.extend(into.iter().map(|(piece, _)| *piece));
                    WorkflowCorrespondenceEntry::Split { handle, from, into }
                }
            } else if self.unmatched.contains(&handle) {
                WorkflowCorrespondenceEntry::Unmatched { handle, from }
            } else {
                WorkflowCorrespondenceEntry::Deleted { handle, from }
            });
        }
        for (handle, to) in now {
            if base.contains_key(handle) || accounted.contains(handle) {
                continue;
            }
            entries.push(WorkflowCorrespondenceEntry::Inserted {
                handle: *handle,
                to: to.clone(),
                source: self
                    .minted
                    .get(handle)
                    .copied()
                    .unwrap_or(WorkflowNodeSource::Authored),
            });
        }
        entries
    }

    /// The surviving statements a split node became, through later splits.
    fn pieces(
        &self,
        handle: WorkflowDraftHandle,
        now: &BTreeMap<WorkflowDraftHandle, WorkflowNodeId>,
        into: &mut Vec<(WorkflowDraftHandle, WorkflowNodeId)>,
    ) {
        for piece in self.split.get(&handle).into_iter().flatten() {
            match now.get(piece) {
                Some(id) => into.push((*piece, id.clone())),
                None => self.pieces(*piece, now, into),
            }
        }
    }
}
