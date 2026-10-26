use std::collections::{BTreeMap, BTreeSet};

use lash_kernel_value::tool_shape::{CanonicalIdentity, DefinitionId, SchemaDocument};

/// Append-only readable names, shared across the documents in one listing.
/// Reserve tool, host, inferred and dialect names before allocating. Identical
/// canonical closures share a name; later distinct collisions get `_2`, `_3`,
/// and so on, without changing any earlier name.
///
/// ```
/// # use lash_vocabulary::shape_names::ReadableNameAllocator;
/// # use lash_kernel_value::tool_shape::{SchemaDocument, DefinitionId};
/// let doc = SchemaDocument::from_json_schema(&serde_json::json!({"title":"Filter"}), 8)?;
/// let mut names = ReadableNameAllocator::new(["Filter"]);
/// assert_eq!(names.allocate(&doc, DefinitionId(0)), "Filter_2");
/// # Ok::<(), lash_kernel_value::tool_shape::ShapeImportRefusal>(())
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
    /// # use lash_vocabulary::shape_names::ReadableNameAllocator;
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
    /// # use lash_vocabulary::shape_names::ReadableNameAllocator;
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
    /// # use lash_vocabulary::shape_names::ReadableNameAllocator;
    /// # use lash_kernel_value::tool_shape::{SchemaDocument, DefinitionId};
    /// let doc = SchemaDocument::from_json_schema(&serde_json::json!({}), 8)?;
    /// assert_eq!(ReadableNameAllocator::default().allocate(&doc, DefinitionId(0)), "Root");
    /// # Ok::<(), lash_kernel_value::tool_shape::ShapeImportRefusal>(())
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

#[cfg(test)]
mod tests {
    use super::*;
    use lash_kernel_value::tool_shape::*;
    use serde_json::{Value, json};
    fn document(schema: Value) -> SchemaDocument {
        SchemaDocument::from_json_schema(&schema, 8).expect("representable schema")
    }
    fn field<'a>(shape: &'a SchemaShape, name: &str) -> &'a SchemaShape {
        &shape
            .fields()
            .iter()
            .find(|field| field.name == name)
            .expect("field")
            .shape
    }
    fn reference(shape: &SchemaShape) -> DefinitionId {
        let ShapeKind::Ref(id) = shape.kind else {
            panic!("expected reference: {shape:?}")
        };
        id
    }

    #[test]
    fn arbitrary_local_pointers_decode_tilde_and_slash_escapes() {
        let doc = document(json!({"type":"object", "properties":{
            "a~/b":{"type":"string"},
            "copy":{"$ref":"#/properties/a~0~1b"}
        }}));
        let id = reference(field(&doc.roots[0], "copy"));
        assert_eq!(doc.definitions[id.0].suggested_name, "a~/b");
        assert_eq!(doc.definitions[id.0].shape.kind, ShapeKind::Str);
        assert_eq!(ReadableNameAllocator::default().allocate(&doc, id), "a__b");
    }

    #[test]
    fn readable_names_deduplicate_closures_and_preserve_the_append_prefix() {
        let one = document(json!({"$ref":"#/$defs/Filter", "$defs":{"Filter":{"type":"string"}}}));
        let two = document(json!({"$ref":"#/$defs/Filter", "$defs":{"Filter":{"type":"number"}}}));
        let three =
            document(json!({"$ref":"#/$defs/Filter", "$defs":{"Filter":{"type":"boolean"}}}));
        let mut names = ReadableNameAllocator::new(["Filter", "Filter_2", "Host", "tools"]);
        let first = names.allocate(&one, reference(&one.roots[0]));
        assert_eq!(first, "Filter_3");
        assert_eq!(names.allocate(&two, reference(&two.roots[0])), "Filter_4");
        assert_eq!(
            names.allocate(&three, reference(&three.roots[0])),
            "Filter_5"
        );
        assert_eq!(names.allocate(&one, reference(&one.roots[0])), first);
        let unrelated = document(json!({"$ref":"#/$defs/Filter", "$defs":{
            "Filter":{"type":"string"}, "Unused":{}
        }}));
        assert_eq!(
            names.allocate(&unrelated, reference(&unrelated.roots[0])),
            first
        );
    }
}
