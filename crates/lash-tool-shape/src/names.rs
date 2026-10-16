use std::collections::{BTreeMap, BTreeSet};

use crate::{CanonicalIdentity, DefinitionId, SchemaDocument};

/// Append-only readable names, shared across the documents in one listing.
/// Reserve tool, host, inferred and dialect names before allocating. Identical
/// canonical closures share a name; later distinct collisions get `_2`, `_3`,
/// and so on, without changing any earlier name.
///
/// ```
/// # use lash_tool_shape::{ReadableNameAllocator, SchemaDocument, DefinitionId};
/// let doc = SchemaDocument::from_json_schema(&serde_json::json!({"title":"Filter"}), 8)?;
/// let mut names = ReadableNameAllocator::new(["Filter"]);
/// assert_eq!(names.allocate(&doc, DefinitionId(0)), "Filter_2");
/// # Ok::<(), lash_tool_shape::ShapeImportRefusal>(())
/// ```
#[derive(Clone, Debug, Default)]
pub struct ReadableNameAllocator {
    used: BTreeSet<String>,
    identities: BTreeMap<CanonicalIdentity, String>,
}

impl ReadableNameAllocator {
    /// Start a listing with all names already claimed by its calling convention.
    ///
    /// ```
    /// # use lash_tool_shape::ReadableNameAllocator;
    /// let names = ReadableNameAllocator::new(["Process", "tools"]);
    /// ```
    pub fn new(reserved: impl IntoIterator<Item = impl AsRef<str>>) -> Self {
        Self {
            used: reserved
                .into_iter()
                .map(|name| sanitize(name.as_ref()))
                .collect(),
            identities: BTreeMap::new(),
        }
    }

    /// Reserve another host or inferred name before definitions are emitted.
    /// Existing names remain allocated; a false result means it was taken.
    ///
    /// ```
    /// # use lash_tool_shape::ReadableNameAllocator;
    /// let mut names = ReadableNameAllocator::default();
    /// assert!(names.reserve("HostRecord"));
    /// assert!(!names.reserve("HostRecord"));
    /// ```
    pub fn reserve(&mut self, name: &str) -> bool {
        self.used.insert(sanitize(name))
    }

    /// Allocate at first emission, using the decoded pointer basename or title.
    /// Dialects can reserve their keywords through [`Self::new`].
    ///
    /// ```
    /// # use lash_tool_shape::{ReadableNameAllocator, SchemaDocument, DefinitionId};
    /// let doc = SchemaDocument::from_json_schema(&serde_json::json!({}), 8)?;
    /// assert_eq!(ReadableNameAllocator::default().allocate(&doc, DefinitionId(0)), "Root");
    /// # Ok::<(), lash_tool_shape::ShapeImportRefusal>(())
    /// ```
    pub fn allocate(&mut self, document: &SchemaDocument, id: DefinitionId) -> String {
        let identity = document.canonical_identity(id);
        if let Some(name) = self.identities.get(&identity) {
            return name.clone();
        }
        let base = sanitize(&document.definitions[id.0].suggested_name);
        let mut name = base.clone();
        let mut suffix = 2;
        while !self.used.insert(name.clone()) {
            name = format!("{base}_{suffix}");
            suffix += 1;
        }
        self.identities.insert(identity, name.clone());
        name
    }
}

fn sanitize(name: &str) -> String {
    let mut result = String::new();
    for (index, character) in name.chars().enumerate() {
        if index == 0 && character.is_ascii_digit() {
            result.push('_');
        }
        result.push(if character.is_ascii_alphanumeric() || character == '_' {
            character
        } else {
            '_'
        });
    }
    if result.is_empty() {
        result.push_str("Root");
    }
    result
}
