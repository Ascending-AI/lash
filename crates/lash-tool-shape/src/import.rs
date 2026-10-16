use std::collections::BTreeMap;
use std::fmt;

use serde_json::{Map, Value};

use crate::{
    DefinitionId, ExtraKeys, ObjectShape, ProcessParamShape, ProcessShape, SchemaDefinition,
    SchemaDocument, SchemaShape, ShapeConstraints, ShapeField, ShapeKind, X_LASH_KEYWORD,
    XLashType, is_named_type_reference, union_kind,
};

/// Hard limits on anonymous nesting and total imported nodes, not repeated
/// walks around a reference cycle. Reference targets are imported separately.
///
/// ```
/// # use lash_tool_shape::ImportLimits;
/// let limits = ImportLimits { max_depth: 32, max_nodes: 10_000 };
/// assert_eq!(limits.max_depth, 32);
/// ```
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ImportLimits {
    pub max_depth: usize,
    pub max_nodes: usize,
}

/// S1: a shape importer refuses information loss, with a typed cause.
///
/// ```
/// # use lash_tool_shape::{SchemaDocument, ShapeImportRefusal};
/// let result = SchemaDocument::from_json_schema(&serde_json::json!({"$ref":"#/missing"}), 8);
/// assert!(matches!(result, Err(ShapeImportRefusal::MissingReference { .. })));
/// ```
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ShapeImportRefusal {
    MissingReference { reference: String },
    UnsupportedReference { reference: String },
    UnsupportedConstruct { keyword: String },
    InvalidSchema { keyword: String },
    ResourceExhausted { resource: String, limit: usize },
}

impl fmt::Display for ShapeImportRefusal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MissingReference { reference } => {
                write!(f, "missing schema reference {reference}")
            }
            Self::UnsupportedReference { reference } => {
                write!(f, "unsupported schema reference {reference}")
            }
            Self::UnsupportedConstruct { keyword } => {
                write!(f, "unsupported schema construct {keyword}")
            }
            Self::InvalidSchema { keyword } => write!(f, "invalid schema keyword {keyword}"),
            Self::ResourceExhausted { resource, limit } => {
                write!(f, "schema {resource} limit {limit} exhausted")
            }
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
    /// # use lash_tool_shape::SchemaDocument;
    /// let doc = SchemaDocument::from_json_schema(&serde_json::json!({"type":"string"}), 8)?;
    /// assert_eq!(doc.roots.len(), 1);
    /// # Ok::<(), lash_tool_shape::ShapeImportRefusal>(())
    /// ```
    ///
    /// # Errors
    /// Refuses unsupported constructs, missing references and exhausted limits.
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
    /// # use lash_tool_shape::{SchemaDocument, ImportLimits};
    /// let doc = SchemaDocument::import(&serde_json::json!({}),
    ///     ImportLimits { max_depth: 8, max_nodes: 100 })?;
    /// assert_eq!(doc.roots.len(), 1);
    /// # Ok::<(), lash_tool_shape::ShapeImportRefusal>(())
    /// ```
    ///
    /// # Errors
    /// Refuses unsupported constructs, missing references and exhausted limits.
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
            limits,
            nodes: 0,
            pointers: BTreeMap::new(),
            definitions: Vec::new(),
        };
        importer.reserve("#")?;
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
    limits: ImportLimits,
    nodes: usize,
    pointers: BTreeMap<String, DefinitionId>,
    definitions: Vec<SchemaDefinition>,
}

fn invalid(keyword: &str) -> ShapeImportRefusal {
    ShapeImportRefusal::InvalidSchema {
        keyword: keyword.to_string(),
    }
}
fn unsupported(keyword: &str) -> ShapeImportRefusal {
    ShapeImportRefusal::UnsupportedConstruct {
        keyword: keyword.to_string(),
    }
}

impl<'a> Importer<'a> {
    fn reserve(&mut self, reference: &str) -> Result<DefinitionId> {
        if let Some(id) = self.pointers.get(reference) {
            return Ok(*id);
        }
        let pointer = reference
            .strip_prefix('#')
            .filter(|pointer| pointer.is_empty() || pointer.starts_with('/'))
            .ok_or_else(|| ShapeImportRefusal::UnsupportedReference {
                reference: reference.to_string(),
            })?;
        // JSON Pointer has only these two escapes. Reject malformed spellings
        // instead of letting a permissive pointer parser silently accept them.
        for segment in pointer.split('/') {
            let mut chars = segment.chars();
            while let Some(c) = chars.next() {
                if c == '~' && !matches!(chars.next(), Some('0' | '1')) {
                    return Err(ShapeImportRefusal::UnsupportedReference {
                        reference: reference.to_string(),
                    });
                }
            }
        }
        let source =
            self.root
                .pointer(pointer)
                .ok_or_else(|| ShapeImportRefusal::MissingReference {
                    reference: reference.to_string(),
                })?;
        if self.definitions.len() >= self.limits.max_nodes {
            return Err(ShapeImportRefusal::ResourceExhausted {
                resource: "nodes".into(),
                limit: self.limits.max_nodes,
            });
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
        Ok(id)
    }

    fn shape(&mut self, schema: &'a Value, depth: usize) -> Result<SchemaShape> {
        if depth >= self.limits.max_depth {
            return Err(ShapeImportRefusal::ResourceExhausted {
                resource: "depth".into(),
                limit: self.limits.max_depth,
            });
        }
        self.nodes += 1;
        if self.nodes > self.limits.max_nodes {
            return Err(ShapeImportRefusal::ResourceExhausted {
                resource: "nodes".into(),
                limit: self.limits.max_nodes,
            });
        }
        if let Value::Bool(value) = schema {
            return Ok(if *value {
                ShapeKind::Unknown
            } else {
                ShapeKind::Never
            }
            .into());
        }
        let map = schema.as_object().ok_or_else(|| invalid("schema"))?;
        self.check_keywords(map)?;
        let inner = self.kind(map, depth)?;
        let mut constraints = ShapeConstraints::from_schema(map).overlay(inner.constraints);
        if matches!(inner.kind, ShapeKind::Int | ShapeKind::Float) {
            constraints.format = None;
        }
        Ok(SchemaShape {
            kind: inner.kind,
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

    fn check_keywords(&self, map: &Map<String, Value>) -> Result<()> {
        for keyword in map.keys() {
            if !matches!(
                keyword.as_str(),
                "$schema"
                    | "$comment"
                    | "$defs"
                    | "definitions"
                    | "title"
                    | "description"
                    | "default"
                    | "examples"
                    | "deprecated"
                    | "readOnly"
                    | "writeOnly"
                    | "$ref"
                    | "x-lash"
                    | "allOf"
                    | "anyOf"
                    | "oneOf"
                    | "enum"
                    | "const"
                    | "type"
                    | "properties"
                    | "required"
                    | "additionalProperties"
                    | "items"
                    | "prefixItems"
                    | "additionalItems"
                    | "minimum"
                    | "maximum"
                    | "exclusiveMinimum"
                    | "exclusiveMaximum"
                    | "minLength"
                    | "maxLength"
                    | "minItems"
                    | "maxItems"
                    | "pattern"
                    | "format"
            ) {
                return Err(unsupported(keyword));
            }
        }
        // A reference or composition with assertion siblings is an
        // intersection. No supported shape spells an arbitrary intersection.
        let composition = ["$ref", "allOf", "anyOf", "oneOf", "x-lash"]
            .iter()
            .filter(|key| map.contains_key(**key))
            .copied()
            .collect::<Vec<_>>();
        if composition.len() > 1 {
            return Err(unsupported("composition siblings"));
        }
        if let Some(primary) = composition.first() {
            for key in map.keys() {
                if key != primary
                    && matches!(
                        key.as_str(),
                        "type"
                            | "properties"
                            | "required"
                            | "additionalProperties"
                            | "items"
                            | "prefixItems"
                            | "enum"
                            | "const"
                            | "minimum"
                            | "maximum"
                            | "exclusiveMinimum"
                            | "exclusiveMaximum"
                            | "minLength"
                            | "maxLength"
                            | "minItems"
                            | "maxItems"
                            | "pattern"
                            | "format"
                    )
                {
                    return Err(unsupported("assertion siblings"));
                }
            }
        }
        Ok(())
    }

    fn kind(&mut self, map: &'a Map<String, Value>, depth: usize) -> Result<SchemaShape> {
        if let Some(reference) = map.get("$ref") {
            let reference = reference.as_str().ok_or_else(|| invalid("$ref"))?;
            return Ok(if reference.starts_with('#') {
                ShapeKind::Ref(self.reserve(reference)?)
            } else if is_named_type_reference(reference) {
                ShapeKind::Named(reference.into())
            } else {
                return Err(ShapeImportRefusal::UnsupportedReference {
                    reference: reference.into(),
                });
            }
            .into());
        }
        if let Some(declaration) = map.get(X_LASH_KEYWORD) {
            return self.lash_type(declaration, depth).map(Into::into);
        }
        if let Some(branches) = map.get("allOf") {
            let branches = branches.as_array().ok_or_else(|| invalid("allOf"))?;
            return match branches.as_slice() {
                [branch] => self.shape(branch, depth + 1),
                _ => Err(unsupported("allOf")),
            };
        }
        if let Some(branches) = map.get("anyOf").or_else(|| map.get("oneOf")) {
            let mut members = branches
                .as_array()
                .ok_or_else(|| invalid("union"))?
                .iter()
                .map(|branch| self.shape(branch, depth + 1))
                .collect::<Result<Vec<_>>>()?;
            return Ok(if members.len() == 1 {
                members.remove(0)
            } else {
                union_kind(members).into()
            });
        }
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
                .any(|key| map.contains_key(*key)) =>
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

    #[expect(
        clippy::expect_used,
        reason = "XLashType deserialization established the signature parameter and output paths"
    )]
    fn lash_type(&mut self, declaration: &'a Value, depth: usize) -> Result<ShapeKind> {
        match serde_json::from_value::<XLashType>(declaration.clone())
            .map_err(|_| invalid("x-lash"))?
        {
            XLashType::ProcessUnknown => Ok(ShapeKind::Process(None)),
            XLashType::Process { signature } => {
                let params = signature
                    .params
                    .iter()
                    .enumerate()
                    .map(|(index, param)| {
                        Ok(ProcessParamShape {
                            name: param.name.clone(),
                            shape: self.shape(
                                declaration
                                    .pointer(&format!("/signature/params/{index}/schema"))
                                    .expect("typed signature"),
                                depth + 1,
                            )?,
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
                Ok(ShapeKind::Process(Some(ProcessShape { params, output })))
            }
        }
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
            _ => return Err(unsupported("type")),
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
        if required
            .iter()
            .any(|name| !fields.iter().any(|field| field.name == *name))
        {
            return Err(unsupported("required without property"));
        }
        fields.sort_by_key(|field| {
            required
                .iter()
                .position(|name| *name == field.name)
                .unwrap_or(usize::MAX)
        });
        let extra_keys = match map.get("additionalProperties") {
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
                return Err(unsupported("variable-length tuple"));
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
