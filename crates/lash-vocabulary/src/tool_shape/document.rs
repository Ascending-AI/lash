use std::collections::BTreeMap;

use serde_json::Value;

use crate::tool_shape::{ExtraKeys, SchemaShape, ShapeKind, ShapeRow};

/// A document-local definition index; never compare IDs across documents.
///
/// ```
/// # use lash_vocabulary::tool_shape::DefinitionId;
/// let first = DefinitionId(0);
/// assert_eq!(first.0, 0);
/// ```
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct DefinitionId(pub usize);

/// A reserved reference target and its independently owned annotations.
///
/// ```
/// # use lash_vocabulary::tool_shape::SchemaDocument;
/// let doc = SchemaDocument::from_json_schema(&serde_json::json!({"$ref":"#"}), 8)?;
/// assert_eq!(doc.definitions[0].pointer, "#");
/// # Ok::<(), lash_vocabulary::tool_shape::ShapeImportRefusal>(())
/// ```
#[derive(Clone, Debug, PartialEq)]
pub struct SchemaDefinition {
    pub id: DefinitionId,
    /// Canonical local JSON pointer, including `#`.
    pub pointer: String,
    /// Decoded final pointer segment, or the schema title, before sanitizing.
    pub suggested_name: String,
    pub shape: SchemaShape,
    pub(crate) source: Value,
}

/// A finite schema graph. Roots are anonymous bodies; explicit references
/// point into `definitions`. The root `#` is always reserved as definition 0.
/// No graph is stored in durable contract state.
///
/// ```
/// # use lash_vocabulary::tool_shape::SchemaDocument;
/// let doc = SchemaDocument::from_json_schema(&serde_json::json!(false), 8)?;
/// assert!(matches!(doc.roots[0].kind, lash_vocabulary::tool_shape::ShapeKind::Never));
/// # Ok::<(), lash_vocabulary::tool_shape::ShapeImportRefusal>(())
/// ```
#[derive(Clone, Debug, PartialEq)]
pub struct SchemaDocument {
    pub roots: Vec<SchemaShape>,
    pub definitions: Vec<SchemaDefinition>,
}

/// Canonical bytes of a rooted reachable schema closure, including pointer
/// identities, annotations and constraints, excluding unreachable definitions.
/// Compare these bytes for deduplication, never a rendered type or title.
///
/// ```
/// # use lash_vocabulary::tool_shape::SchemaDocument;
/// let doc = SchemaDocument::from_json_schema(&serde_json::json!({}), 8)?;
/// let identity = doc.canonical_identity(doc.definitions[0].id);
/// assert!(!identity.as_bytes().is_empty());
/// # Ok::<(), lash_vocabulary::tool_shape::ShapeImportRefusal>(())
/// ```
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct CanonicalIdentity(String);

impl CanonicalIdentity {
    /// The deterministic UTF-8 identity bytes.
    ///
    /// ```
    /// # use lash_vocabulary::tool_shape::SchemaDocument;
    /// let doc = SchemaDocument::from_json_schema(&serde_json::json!({}), 8)?;
    /// assert!(!doc.canonical_identity(doc.definitions[0].id).as_bytes().is_empty());
    /// # Ok::<(), lash_vocabulary::tool_shape::ShapeImportRefusal>(())
    /// ```
    pub fn as_bytes(&self) -> &[u8] {
        self.0.as_bytes()
    }
}

impl SchemaDocument {
    /// The original JSON Schema of this document's root, retaining local
    /// definitions and references for validation as well as presentation.
    pub fn root_schema(&self) -> &Value {
        &self.definitions[0].source
    }

    /// Look up a document-local reference. IDs from this document always exist.
    ///
    /// ```
    /// # use lash_vocabulary::tool_shape::{SchemaDocument, DefinitionId};
    /// let doc = SchemaDocument::from_json_schema(&serde_json::json!({}), 8)?;
    /// assert!(doc.definition(DefinitionId(0)).is_some());
    /// # Ok::<(), lash_vocabulary::tool_shape::ShapeImportRefusal>(())
    /// ```
    pub fn definition(&self, id: DefinitionId) -> Option<&SchemaDefinition> {
        self.definitions.get(id.0)
    }

    /// References directly inside an anonymous shape, in first-use order,
    /// without following their targets. Repeated edges appear once.
    ///
    /// ```
    /// # use lash_vocabulary::tool_shape::SchemaDocument;
    /// let doc = SchemaDocument::from_json_schema(&serde_json::json!({"$ref":"#"}), 8)?;
    /// assert_eq!(SchemaDocument::references(&doc.roots[0]).len(), 1);
    /// # Ok::<(), lash_vocabulary::tool_shape::ShapeImportRefusal>(())
    /// ```
    pub fn references(shape: &SchemaShape) -> Vec<DefinitionId> {
        let mut ids = Vec::new();
        visit(shape, &mut |shape| {
            if let ShapeKind::Ref(id) = shape.kind
                && !ids.contains(&id)
            {
                ids.push(id);
            }
        });
        ids
    }

    /// Reachable definitions in deterministic first-use order, each once,
    /// including cycles. Start with any subset of roots to budget a listing.
    ///
    /// ```
    /// # use lash_vocabulary::tool_shape::SchemaDocument;
    /// let doc = SchemaDocument::from_json_schema(&serde_json::json!({"$ref":"#"}), 8)?;
    /// assert_eq!(doc.reachable_closure(&doc.roots).len(), 1);
    /// # Ok::<(), lash_vocabulary::tool_shape::ShapeImportRefusal>(())
    /// ```
    pub fn reachable_closure(&self, roots: &[SchemaShape]) -> Vec<DefinitionId> {
        let mut ids = Vec::new();
        for root in roots {
            for id in Self::references(root) {
                if !ids.contains(&id) {
                    ids.push(id);
                }
            }
        }
        let mut index = 0;
        while index < ids.len() {
            for next in Self::references(&self.definitions[ids[index].0].shape) {
                if !ids.contains(&next) {
                    ids.push(next);
                }
            }
            index += 1;
        }
        ids
    }

    /// Identity of one definition's reachable closure. Map keys are sorted;
    /// array order, pointer spellings, annotations and constraints are kept.
    ///
    /// ```
    /// # use lash_vocabulary::tool_shape::SchemaDocument;
    /// let doc = SchemaDocument::from_json_schema(&serde_json::json!({"$ref":"#"}), 8)?;
    /// assert_eq!(doc.canonical_identity(doc.definitions[0].id),
    ///            doc.canonical_identity(doc.definitions[0].id));
    /// # Ok::<(), lash_vocabulary::tool_shape::ShapeImportRefusal>(())
    /// ```
    #[expect(
        clippy::expect_used,
        reason = "the identity contains only JSON values and string map keys, which serialize infallibly"
    )]
    pub fn canonical_identity(&self, id: DefinitionId) -> CanonicalIdentity {
        let root = &self.definitions[id.0];
        let mut ids = self.reachable_closure(&[ShapeKind::Ref(id).into()]);
        ids.sort_by_key(|id| &self.definitions[id.0].pointer);
        let definitions = ids
            .into_iter()
            .map(|id| {
                let definition = &self.definitions[id.0];
                (
                    definition.pointer.clone(),
                    schema_identity(&definition.source),
                )
            })
            .collect::<BTreeMap<_, _>>();
        CanonicalIdentity(
            serde_json::to_string(&(root.pointer.as_str(), definitions))
                .expect("JSON values serialize"),
        )
    }

    /// Finite note rows, qualified by `$index` for roots and JSON pointer for
    /// definitions. Stop at reference edges and visit each reachable definition
    /// once. Union branches retain separate paths, even with identical fields.
    ///
    /// ```
    /// # use lash_vocabulary::tool_shape::SchemaDocument;
    /// let doc = SchemaDocument::from_json_schema(&serde_json::json!({"description":"Any"}), 8)?;
    /// assert_eq!(doc.note_rows()[0].path, "$0");
    /// # Ok::<(), lash_vocabulary::tool_shape::ShapeImportRefusal>(())
    /// ```
    pub fn note_rows(&self) -> Vec<ShapeRow> {
        let mut rows = Vec::new();
        let mut append = |shape: &SchemaShape, path: &str| {
            let mut local = Vec::new();
            collect_notes(shape, path, true, &mut local);
            rows.extend(local);
        };
        for (index, root) in self.roots.iter().enumerate() {
            append(root, &format!("${index}"));
        }
        for id in self.reachable_closure(&self.roots) {
            let definition = &self.definitions[id.0];
            append(&definition.shape, &definition.pointer);
        }
        rows
    }
}

pub(crate) fn visit(shape: &SchemaShape, f: &mut impl FnMut(&SchemaShape)) {
    f(shape);
    match &shape.kind {
        ShapeKind::List(item) => visit(item, f),
        ShapeKind::Tuple(items) | ShapeKind::Union(items) | ShapeKind::Intersection(items) => {
            for item in items {
                visit(item, f);
            }
        }
        ShapeKind::Object(object) => {
            for field in &object.fields {
                visit(&field.shape, f);
            }
            if let ExtraKeys::Open(extra) = &object.extra_keys {
                visit(extra, f);
            }
        }
        ShapeKind::Process(Some(process)) | ShapeKind::Function(Some(process)) => {
            for param in &process.params {
                visit(&param.shape, f);
            }
            visit(&process.output, f);
        }
        _ => {}
    }
}

// Strip definition registries only in schema positions. A default or const
// containing a data key called "$defs" remains part of identity verbatim.
#[expect(
    clippy::expect_used,
    reason = "the importer admitted these schema arrays and the result contains only JSON values"
)]
pub(crate) fn schema_identity(schema: &Value) -> Value {
    let Some(map) = schema.as_object() else {
        return schema.clone();
    };
    let mut result = BTreeMap::new();
    for (key, value) in map {
        if matches!(key.as_str(), "$defs" | "definitions") {
            continue;
        }
        let value = match key.as_str() {
            "properties" => value
                .as_object()
                .map(|map| {
                    Value::Object(
                        map.iter()
                            .map(|(key, value)| (key.clone(), schema_identity(value)))
                            .collect(),
                    )
                })
                .unwrap_or_else(|| value.clone()),
            "items" if value.is_array() => Value::Array(
                value
                    .as_array()
                    .expect("array")
                    .iter()
                    .map(schema_identity)
                    .collect(),
            ),
            "items" | "additionalProperties" => schema_identity(value),
            "allOf" | "anyOf" | "oneOf" | "prefixItems" => Value::Array(
                value
                    .as_array()
                    .expect("admitted schema array")
                    .iter()
                    .map(schema_identity)
                    .collect(),
            ),
            "x-lash" => {
                let mut value = value.clone();
                if let Some(params) = value
                    .pointer_mut("/signature/params")
                    .and_then(Value::as_array_mut)
                {
                    for param in params {
                        param["schema"] = schema_identity(&param["schema"]);
                    }
                }
                if let Some(output) = value.pointer_mut("/signature/output") {
                    *output = schema_identity(output);
                }
                value
            }
            _ => value.clone(),
        };
        result.insert(key.clone(), canonical_json(&value));
    }
    serde_json::to_value(result).expect("JSON map serializes")
}

#[expect(
    clippy::expect_used,
    reason = "canonicalization serializes only JSON values with string map keys"
)]
fn canonical_json(value: &Value) -> Value {
    match value {
        Value::Object(map) => serde_json::to_value(
            map.iter()
                .map(|(key, value)| (key, canonical_json(value)))
                .collect::<BTreeMap<_, _>>(),
        )
        .expect("JSON map"),
        Value::Array(items) => Value::Array(items.iter().map(canonical_json).collect()),
        other => other.clone(),
    }
}

// Path-only notes keep branch ownership and never expand a Ref.
fn collect_notes(shape: &SchemaShape, path: &str, required: bool, rows: &mut Vec<ShapeRow>) {
    if shape.has_notes() {
        rows.push(ShapeRow {
            path: path.to_string(),
            required,
            shape: shape.clone(),
        });
    }
    match &shape.kind {
        ShapeKind::Object(object) => {
            for field in &object.fields {
                collect_notes(
                    &field.shape,
                    &format!("{path}.{}", field.name),
                    field.required,
                    rows,
                );
            }
            if let ExtraKeys::Open(extra) = &object.extra_keys {
                collect_notes(extra, &format!("{path}.*"), false, rows);
            }
        }
        ShapeKind::List(item) => collect_notes(item, &format!("{path}[]"), true, rows),
        ShapeKind::Tuple(items) => {
            for (index, item) in items.iter().enumerate() {
                collect_notes(item, &format!("{path}[{index}]"), true, rows);
            }
        }
        ShapeKind::Union(items) => {
            for (item, arm) in items.iter().zip(shape.union_arm_paths(path)) {
                collect_notes(item, &arm, required, rows);
            }
        }
        ShapeKind::Intersection(items) => {
            for item in items {
                collect_notes(item, path, required, rows);
            }
        }
        ShapeKind::Process(Some(process)) | ShapeKind::Function(Some(process)) => {
            for param in &process.params {
                collect_notes(&param.shape, &format!("{path}({})", param.name), true, rows);
            }
            collect_notes(&process.output, &format!("{path}.return"), true, rows);
        }
        _ => {}
    }
}
