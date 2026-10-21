use std::collections::BTreeMap;
use std::fmt;

use serde_json::{Map, Value};

use crate::tool_shape::{
    DefinitionId, ExtraKeys, ObjectShape, ProcessParamShape, ProcessShape, SchemaDefinition,
    SchemaDocument, SchemaShape, ShapeConstraints, ShapeField, ShapeKind, X_LASH_KEYWORD,
    XLashType, has_scoped_references, is_named_type_reference, local_reference_pointer, union_kind,
};

/// Presentation limits on anonymous nesting and total imported nodes, not repeated
/// walks around a reference cycle. Beyond them the type is unknown with a note.
/// Reference targets are imported separately.
///
/// ```
/// # use lash_vocabulary::tool_shape::ImportLimits;
/// let limits = ImportLimits { max_depth: 32, max_nodes: 10_000 };
/// assert_eq!(limits.max_depth, 32);
/// ```
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ImportLimits {
    pub max_depth: usize,
    pub max_nodes: usize,
}

/// A non-schema value or a malformed JSON Schema keyword.
/// Valid schemas are projected with notes when a type cannot express a rule.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ShapeImportRefusal {
    InvalidSchema { keyword: String },
}

impl fmt::Display for ShapeImportRefusal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidSchema { keyword } => write!(f, "invalid schema keyword {keyword}"),
        }
    }
}
impl std::error::Error for ShapeImportRefusal {}

type Result<T> = std::result::Result<T, ShapeImportRefusal>;

impl SchemaDocument {
    /// Import one root, preserving local reference edges and annotations.
    /// The node limit is 10,000; use [`Self::import`] to choose both limits.
    ///
    /// ```
    /// # use lash_vocabulary::tool_shape::SchemaDocument;
    /// let doc = SchemaDocument::from_json_schema(&serde_json::json!({"type":"string"}), 8)?;
    /// assert_eq!(doc.roots.len(), 1);
    /// # Ok::<(), lash_vocabulary::tool_shape::ShapeImportRefusal>(())
    /// ```
    ///
    /// # Errors
    /// Refuses non-schema values and malformed schema keywords. Presentation limits keep an honest type with a validation note.
    pub fn from_json_schema(
        schema: &Value,
        max_depth: usize,
    ) -> std::result::Result<Self, ShapeImportRefusal> {
        Self::import(
            schema,
            ImportLimits {
                max_depth,
                max_nodes: 10_000,
            },
        )
    }

    /// Import with explicit resource limits. Reserve each definition before
    /// reading its body; a queue makes even long reference chains stack bounded.
    ///
    /// ```
    /// # use lash_vocabulary::tool_shape::{SchemaDocument, ImportLimits};
    /// let doc = SchemaDocument::import(&serde_json::json!({}),
    ///     ImportLimits { max_depth: 8, max_nodes: 100 })?;
    /// assert_eq!(doc.roots.len(), 1);
    /// # Ok::<(), lash_vocabulary::tool_shape::ShapeImportRefusal>(())
    /// ```
    ///
    /// # Errors
    /// Refuses non-schema values and malformed schema keywords. Presentation limits keep an honest type with a validation note.
    #[expect(
        clippy::expect_used,
        reason = "every queued pointer was resolved against the same immutable document before reservation"
    )]
    pub fn import(
        schema: &Value,
        limits: ImportLimits,
    ) -> std::result::Result<Self, ShapeImportRefusal> {
        let mut importer = Importer {
            root: schema,
            scoped_references: has_scoped_references(schema),
            limits,
            nodes: 0,
            pointers: BTreeMap::new(),
            definitions: Vec::new(),
        };
        importer.reserve("#");
        let mut index = 0;
        while index < importer.definitions.len() {
            let pointer = importer.definitions[index].pointer.clone();
            let target = schema.pointer(&pointer[1..]).expect("reserved pointer");
            importer.definitions[index].shape = importer.shape(target, 0)?;
            index += 1;
        }
        Ok(Self {
            roots: vec![importer.definitions[0].shape.clone()],
            definitions: importer.definitions,
        })
    }
}

struct Importer<'a> {
    root: &'a Value,
    scoped_references: bool,
    limits: ImportLimits,
    nodes: usize,
    pointers: BTreeMap<String, DefinitionId>,
    definitions: Vec<SchemaDefinition>,
}

fn projection_note(note: &str) -> SchemaShape {
    let mut shape = SchemaShape::unknown();
    shape.constraints.rules.push(note.into());
    shape
}

fn invalid(keyword: &str) -> ShapeImportRefusal {
    ShapeImportRefusal::InvalidSchema {
        keyword: keyword.to_string(),
    }
}
impl<'a> Importer<'a> {
    fn reserve(&mut self, reference: &str) -> Option<DefinitionId> {
        if let Some(id) = self.pointers.get(reference) {
            return Some(*id);
        }
        let pointer = local_reference_pointer(reference)?;
        let source = self.root.pointer(pointer)?;
        if self.definitions.len() >= self.limits.max_nodes.max(1) {
            return None;
        }
        let suggested_name = pointer
            .rsplit('/')
            .next()
            .filter(|segment| !segment.is_empty())
            .map(|segment| segment.replace("~1", "/").replace("~0", "~"))
            .or_else(|| {
                source
                    .get("title")
                    .and_then(Value::as_str)
                    .map(str::to_string)
            })
            .unwrap_or_else(|| "Root".into());
        let id = DefinitionId(self.definitions.len());
        self.pointers.insert(reference.to_string(), id);
        self.definitions.push(SchemaDefinition {
            id,
            pointer: reference.to_string(),
            suggested_name,
            shape: SchemaShape::unknown(),
            source: source.clone(),
        });
        Some(id)
    }

    fn shape(&mut self, schema: &'a Value, depth: usize) -> Result<SchemaShape> {
        if depth >= self.limits.max_depth || self.nodes >= self.limits.max_nodes {
            return Ok(projection_note(
                "Further structure is validated by the tool.",
            ));
        }
        self.nodes += 1;
        if let Value::Bool(value) = schema {
            return Ok(if *value {
                ShapeKind::Unknown
            } else {
                ShapeKind::Never
            }
            .into());
        }
        let map = schema.as_object().ok_or_else(|| invalid("schema"))?;
        let remaining = self.limits.max_nodes.saturating_sub(self.nodes);
        if [
            "properties",
            "required",
            "prefixItems",
            "allOf",
            "anyOf",
            "oneOf",
        ]
        .iter()
        .filter_map(|key| map.get(*key))
        .any(|value| {
            value
                .as_object()
                .map(Map::len)
                .or_else(|| value.as_array().map(Vec::len))
                .unwrap_or(0)
                > remaining
        }) {
            return Ok(projection_note(
                "Further structure is validated by the tool.",
            ));
        }
        let mut inner = self.kind(map, depth)?;
        let local = ShapeConstraints::from_schema(map);
        let mut local_type_notes = local.clone();
        local_type_notes.value_notes.clear();
        let mut inherited_type_notes = inner.constraints.clone();
        inherited_type_notes.value_notes.clear();
        // A singleton composition can carry refinements too. Keep its notes
        // under the same path rather than overriding them with sibling bounds.
        if !local_type_notes.is_empty() && !inherited_type_notes.is_empty() {
            inner = ShapeKind::Intersection(vec![inner]).into();
        }
        let mut constraints = local.overlay(inner.constraints);
        if matches!(inner.kind, ShapeKind::Int | ShapeKind::Float) {
            constraints.format = None;
        }
        Ok(SchemaShape {
            kind: inner.kind,
            title: map
                .get("title")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|title| !title.is_empty())
                .map(str::to_string)
                .or(inner.title),
            description: map
                .get("description")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|text| !text.is_empty())
                .map(str::to_string)
                .or(inner.description),
            default: map.get("default").cloned().or(inner.default),
            constraints,
        })
    }

    fn kind(&mut self, map: &'a Map<String, Value>, depth: usize) -> Result<SchemaShape> {
        // A lash type is what the slot is: the JSON Schema beside it
        // describes nothing a value of it could be.
        if let Some(declaration) = map.get(X_LASH_KEYWORD) {
            return Ok(self.lash_type(declaration, depth).unwrap_or_else(|_| {
                projection_note("The x-lash declaration is validated by the tool.")
            }));
        }
        // Keep conjunctions factored: the shared object is imported once,
        // never copied into each alternative. References stay graph edges.
        let mut conjuncts = Vec::new();
        if let Some(reference) = map.get("$ref") {
            let reference = reference.as_str().ok_or_else(|| invalid("$ref"))?;
            conjuncts.push(if reference.starts_with('#') && self.scoped_references {
                projection_note(&format!("Reference {reference} has a nested identifier scope and is validated by the tool."))
            } else if reference.starts_with('#') {
                self.reserve(reference).map(|id| ShapeKind::Ref(id).into())
                    .unwrap_or_else(|| projection_note(&format!("Reference {reference} is validated by the tool; type details are unavailable.")))
            } else if is_named_type_reference(reference) {
                ShapeKind::Named(reference.into()).into()
            } else {
                projection_note(&format!("Reference {reference} is validated by the tool; type details are unavailable."))
            });
        }
        if let Some(branches) = map.get("allOf") {
            for branch in branches.as_array().ok_or_else(|| invalid("allOf"))? {
                conjuncts.push(self.shape(branch, depth + 1)?);
            }
        }
        for keyword in ["anyOf", "oneOf"] {
            if let Some(branches) = map.get(keyword) {
                let mut members = branches
                    .as_array()
                    .ok_or_else(|| invalid(keyword))?
                    .iter()
                    .map(|branch| self.shape(branch, depth + 1))
                    .collect::<Result<Vec<_>>>()?;
                conjuncts.push(if members.len() == 1 {
                    members.remove(0)
                } else {
                    union_kind(members).into()
                });
            }
        }
        let base = self.base_kind(map, depth)?;
        if !matches!(base.kind, ShapeKind::Unknown) {
            conjuncts.insert(0, base);
        }
        Ok(match conjuncts.len() {
            0 => ShapeKind::Unknown.into(),
            1 => conjuncts.remove(0),
            _ => ShapeKind::Intersection(conjuncts).into(),
        })
    }

    fn base_kind(&mut self, map: &'a Map<String, Value>, depth: usize) -> Result<SchemaShape> {
        if let Some(values) = map.get("enum") {
            let values = values.as_array().ok_or_else(|| invalid("enum"))?;
            return Ok(literals_kind(values, map).into());
        }
        if let Some(value) = map.get("const") {
            return Ok(literals_kind(std::slice::from_ref(value), map).into());
        }
        let kind = match map.get("type") {
            Some(Value::String(name)) => self.named_type(name, map, depth)?,
            Some(Value::Array(names)) => {
                let members = names
                    .iter()
                    .map(|name| {
                        self.named_type(name.as_str().ok_or_else(|| invalid("type"))?, map, depth)
                            .map(Into::into)
                    })
                    .collect::<Result<Vec<_>>>()?;
                union_kind(members)
            }
            Some(_) => return Err(invalid("type")),
            None if ["properties", "required", "additionalProperties"]
                .iter()
                .any(|key| map.contains_key(*key))
                || (map.contains_key("patternProperties")
                    && !["$ref", "allOf", "anyOf", "oneOf"]
                        .iter()
                        .any(|key| map.contains_key(*key))) =>
            {
                self.object(map, depth)?
            }
            None if map.contains_key("items") || map.contains_key("prefixItems") => {
                self.array(map, depth)?
            }
            None => ShapeKind::Unknown,
        };
        Ok(kind.into())
    }

    /// The shape an `x-lash` declaration states: each kind of the one
    /// decode, matched before any other rule.
    fn lash_type(&mut self, declaration: &'a Value, depth: usize) -> Result<SchemaShape> {
        let declared = XLashType::decode(declaration).ok_or_else(|| invalid("x-lash"))?;
        let signature = match declared.signature() {
            Some(_) => Some(self.call_shape(declaration, depth)?),
            None => None,
        };
        Ok(match declared {
            XLashType::Timestamp | XLashType::Bytes => ShapeKind::Str,
            XLashType::Function { .. } => ShapeKind::Function(signature),
            XLashType::Process { .. } | XLashType::ProcessUnknown => ShapeKind::Process(signature),
        }
        .into())
    }

    /// The call signature a decoded `x-lash` declaration states, each
    /// parameter and the output imported as schemas.
    #[expect(
        clippy::expect_used,
        reason = "XLashType::decode established the signature parameter and output paths"
    )]
    fn call_shape(&mut self, declaration: &'a Value, depth: usize) -> Result<ProcessShape> {
        let params = declaration
            .pointer("/signature/params")
            .and_then(Value::as_array)
            .expect("typed signature")
            .iter()
            .map(|param| {
                Ok(ProcessParamShape {
                    name: param["name"].as_str().expect("typed signature").to_owned(),
                    shape: self.shape(&param["schema"], depth + 1)?,
                })
            })
            .collect::<Result<Vec<_>>>()?;
        let output = Box::new(
            self.shape(
                declaration
                    .pointer("/signature/output")
                    .expect("typed signature"),
                depth + 1,
            )?,
        );
        Ok(ProcessShape { params, output })
    }

    fn named_type(
        &mut self,
        name: &str,
        map: &'a Map<String, Value>,
        depth: usize,
    ) -> Result<ShapeKind> {
        Ok(match name {
            "string" => ShapeKind::Str,
            "integer" => ShapeKind::Int,
            "number" => ShapeKind::Float,
            "boolean" => ShapeKind::Bool,
            "null" => ShapeKind::Null,
            "object" => self.object(map, depth)?,
            "array" => self.array(map, depth)?,
            _ => return Err(invalid("type")),
        })
    }

    fn object(&mut self, map: &'a Map<String, Value>, depth: usize) -> Result<ShapeKind> {
        let required = match map.get("required") {
            Some(value) => value
                .as_array()
                .ok_or_else(|| invalid("required"))?
                .iter()
                .map(|name| name.as_str().ok_or_else(|| invalid("required")))
                .collect::<Result<Vec<_>>>()?,
            None => Vec::new(),
        };
        let mut fields = Vec::new();
        if let Some(properties) = map.get("properties") {
            for (name, schema) in properties
                .as_object()
                .ok_or_else(|| invalid("properties"))?
            {
                fields.push(ShapeField {
                    name: name.clone(),
                    required: required.contains(&name.as_str()),
                    shape: self.shape(schema, depth + 1)?,
                });
            }
        }
        // A required-only arm refines fields declared by a sibling schema.
        for name in &required {
            if !fields.iter().any(|field| field.name == *name) {
                fields.push(ShapeField {
                    name: (*name).into(),
                    required: true,
                    shape: SchemaShape::unknown(),
                });
            }
        }
        fields.sort_by_key(|field| {
            required
                .iter()
                .position(|name| *name == field.name)
                .unwrap_or(usize::MAX)
        });
        let extra_keys = match map.get("additionalProperties") {
            // Pattern-matching keys remain possible even when unmatched keys
            // are closed; the note carries their conditional value constraints.
            _ if map.contains_key("patternProperties") => {
                ExtraKeys::Open(Box::new(SchemaShape::unknown()))
            }
            Some(Value::Bool(false)) => ExtraKeys::Closed,
            Some(schema) => ExtraKeys::Open(Box::new(self.shape(schema, depth + 1)?)),
            None => ExtraKeys::Unstated,
        };
        Ok(ShapeKind::Object(ObjectShape { fields, extra_keys }))
    }

    fn array(&mut self, map: &'a Map<String, Value>, depth: usize) -> Result<ShapeKind> {
        let positions = map
            .get("prefixItems")
            .or_else(|| map.get("items").filter(|value| value.is_array()));
        if let Some(positions) = positions {
            let positions = positions.as_array().ok_or_else(|| invalid("prefixItems"))?;
            let tail = if map.contains_key("prefixItems") {
                map.get("items")
            } else {
                map.get("additionalItems")
            };
            if tail != Some(&Value::Bool(false))
                || map.get("minItems").and_then(Value::as_u64) != Some(positions.len() as u64)
            {
                return Ok(ShapeKind::List(Box::new(SchemaShape::unknown())));
            }
            return Ok(ShapeKind::Tuple(
                positions
                    .iter()
                    .map(|schema| self.shape(schema, depth + 1))
                    .collect::<Result<Vec<_>>>()?,
            ));
        }
        Ok(ShapeKind::List(Box::new(match map.get("items") {
            Some(schema) => self.shape(schema, depth + 1)?,
            None => SchemaShape::unknown(),
        })))
    }
}

fn literals_kind(values: &[Value], map: &Map<String, Value>) -> ShapeKind {
    let values = values
        .iter()
        .filter(|value| {
            map.get("const").is_none_or(|constant| constant == *value)
                && map.get("type").is_none_or(|kind| match kind {
                    Value::String(name) => literal_has_type(value, name),
                    Value::Array(names) => names.iter().any(|name| {
                        name.as_str()
                            .is_some_and(|name| literal_has_type(value, name))
                    }),
                    _ => false,
                })
        })
        .cloned()
        .collect::<Vec<_>>();
    let nullable = values.iter().any(Value::is_null);
    let literals = values
        .into_iter()
        .filter(|value| !value.is_null())
        .collect::<Vec<_>>();
    match (literals.is_empty(), nullable) {
        (true, true) => ShapeKind::Null,
        (true, false) => ShapeKind::Never,
        (false, false) => ShapeKind::Literals(literals),
        (false, true) => ShapeKind::Union(vec![
            ShapeKind::Literals(literals).into(),
            ShapeKind::Null.into(),
        ]),
    }
}
fn literal_has_type(value: &Value, name: &str) -> bool {
    match name {
        "null" => value.is_null(),
        "boolean" => value.is_boolean(),
        "string" => value.is_string(),
        "integer" => value.as_f64().is_some_and(|number| number.fract() == 0.0),
        "number" => value.is_number(),
        "object" => value.is_object(),
        "array" => value.is_array(),
        _ => false,
    }
}
