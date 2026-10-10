//! Which node of a document became which node of the document edited from
//! it.

use lash_kernel_doc::{DocumentId, Site};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// The nodes of one document that survive into another, each with the site
/// it has there (`K-EDIT-004`).
///
/// A node of `base` that is not listed did not survive. A node of `result`
/// that no entry leads to is new.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Correspondence {
    pub base: DocumentId,
    pub result: DocumentId,
    /// Ordered by `from`; no two share a `from` or a `to`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    entries: Vec<Survivor>,
}

/// One surviving node.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Survivor {
    /// Its site in the base document.
    pub from: Site,
    /// Its site in the result.
    pub to: Site,
    /// An edit wrote the node itself: its statement, expression, action or
    /// a name it holds. A node only moved, or changed only beneath, is not
    /// edited.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub edited: bool,
}

impl Correspondence {
    pub(crate) fn new(base: DocumentId, result: DocumentId, mut entries: Vec<Survivor>) -> Self {
        entries.sort_by(|a, b| a.from.cmp(&b.from));
        Self {
            base,
            result,
            entries,
        }
    }

    /// The correspondence `entries` state between two documents, or `None`
    /// when two of them share a `from` or a `to`. A kernel version's
    /// document migration answers one (`K-VER-004`).
    pub fn of(base: DocumentId, result: DocumentId, entries: Vec<Survivor>) -> Option<Self> {
        let distinct = {
            let mut from = std::collections::BTreeSet::new();
            let mut to = std::collections::BTreeSet::new();
            entries
                .iter()
                .all(|entry| from.insert(&entry.from) && to.insert(&entry.to))
        };
        distinct.then(|| Self::new(base, result, entries))
    }

    /// Every node of `document` to itself.
    pub(crate) fn identity(document: DocumentId, sites: impl IntoIterator<Item = Site>) -> Self {
        let entries = sites
            .into_iter()
            .map(|site| Survivor {
                from: site.clone(),
                to: site,
                edited: false,
            })
            .collect();
        Self::new(document, document, entries)
    }

    /// The surviving nodes, ordered by their site in the base.
    pub fn entries(&self) -> &[Survivor] {
        &self.entries
    }

    /// What became of the node at `from`, when it survived.
    pub fn survivor(&self, from: &Site) -> Option<&Survivor> {
        self.entries
            .binary_search_by(|entry| entry.from.cmp(from))
            .ok()
            .map(|index| &self.entries[index])
    }

    /// The site in the result of the node that was at `from`.
    pub fn successor(&self, from: &Site) -> Option<&Site> {
        self.survivor(from).map(|entry| &entry.to)
    }

    /// The site in the base of the node that is at `to`.
    pub fn predecessor(&self, to: &Site) -> Option<&Site> {
        self.entries
            .iter()
            .find(|entry| &entry.to == to)
            .map(|entry| &entry.from)
    }

    /// The correspondence from this one's base to `next`'s result, or
    /// `None` when `next` does not start where this one ends.
    pub fn then(&self, next: &Correspondence) -> Option<Correspondence> {
        if self.result != next.base {
            return None;
        }
        let entries = self
            .entries
            .iter()
            .filter_map(|first| {
                let second = next.survivor(&first.to)?;
                Some(Survivor {
                    from: first.from.clone(),
                    to: second.to.clone(),
                    edited: first.edited || second.edited,
                })
            })
            .collect();
        Some(Self::new(self.base, next.result, entries))
    }
}
