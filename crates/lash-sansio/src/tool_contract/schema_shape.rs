//! The language-neutral, prompt-facing shape of a JSON Schema.
//!
//! A tool contract's schemas are JSON Schema, and every prompt surface has to
//! show a model what they accept: the TypeScript dialect as a type, the
//! compact contract as signature rows. [`SchemaShape`] is the one reading of a
//! schema those surfaces share. It is imported once, here, and each surface
//! only spells it; no surface reads the raw schema again. Runtime-value
//! inference constructs the same shape directly.
//!
//! The shape is a *view*, not a validator. It keeps what a reader needs: the
//! fields of open objects and whether extra keys are allowed, which fields are
//! required, nested objects and arrays, enums, unions and nullability, field
//! descriptions, and constraints as annotations. Anything it cannot say
//! becomes [`ShapeKind::Unknown`] rather than an error, because a schema that
//! must be refused is refused where it enters the catalog.

use serde::{Deserialize, Serialize};
use serde_json::{Map, Number, Value};

/// How deep the importer follows nested containers and references before it
/// answers [`ShapeKind::Unknown`]. The bound keeps a recursive schema's
/// rendered shape finite.
const MAX_SHAPE_DEPTH: usize = 8;

/// The JSON Schema keyword that carries the types JSON Schema cannot say.
///
/// JSON Schema has no vocabulary for a callable process or for a trigger
/// handle, so a tool contract that traffics in them spells them under this
/// extension keyword. A contract that does not carry it reads as plain JSON
/// Schema.
pub const X_LASH_KEYWORD: &str = "x-lash";

/// The lash-only half of a type, as it rides inside a JSON Schema.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum XLashType {
    /// A process with an authoritative call signature.
    Process { signature: XLashSignature },
    /// A process the host can only describe as callable.
    ProcessUnknown,
    /// A trigger handle over the payload the trigger delivers.
    Handle { payload: Box<Value> },
}

/// An ordered process-call signature in schema form.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct XLashSignature {
    pub params: Vec<XLashParam>,
    pub output: Box<Value>,
}

/// One named process parameter in schema form.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct XLashParam {
    pub name: String,
    pub schema: Value,
}

/// Whether a non-pointer `$ref` names a host data type.
///
/// Named data types are dotted identifiers (`lash.TriggerRegistration`). A URL
/// or a relative file reference is a JSON Schema construct lash cannot
/// resolve.
pub fn is_named_type_reference(reference: &str) -> bool {
    !reference.is_empty()
        && reference.split('.').all(|segment| {
            !segment.is_empty()
                && segment
                    .chars()
                    .all(|character| character.is_ascii_alphanumeric() || character == '_')
        })
        && reference
            .chars()
            .next()
            .is_some_and(|first| first.is_ascii_alphabetic() || first == '_')
}

/// One schema node as a prompt surface shows it.
#[derive(Clone, Debug, PartialEq)]
pub struct SchemaShape {
    pub kind: ShapeKind,
    /// The node's own `description`, trimmed; `None` when absent or blank.
    pub description: Option<String>,
    /// The node's `default`.
    pub default: Option<Value>,
    pub constraints: ShapeConstraints,
}

/// What a schema node accepts.
#[derive(Clone, Debug, PartialEq)]
#[non_exhaustive]
pub enum ShapeKind {
    /// Anything: an empty schema, or a construct the view cannot say.
    Unknown,
    Null,
    Bool,
    Int,
    Float,
    Str,
    /// One of a fixed set of non-null values (`enum` or `const`).
    Literals(Vec<Value>),
    List(Box<SchemaShape>),
    /// A fixed-position array (`prefixItems`, or an `items` array).
    Tuple(Vec<SchemaShape>),
    Object(ObjectShape),
    /// Two or more alternatives, flattened and without duplicates.
    Union(Vec<SchemaShape>),
    /// A host data type referenced by name.
    Named(String),
    /// A process value; `None` when the host only says it is callable.
    Process(Option<ProcessShape>),
    /// A trigger handle over the payload it delivers.
    Handle(Box<SchemaShape>),
}

/// An object's named fields and its stance on keys it does not name.
#[derive(Clone, Debug, PartialEq)]
pub struct ObjectShape {
    /// Required fields in the schema's `required` order, then the rest in
    /// property order.
    pub fields: Vec<ShapeField>,
    pub extra_keys: ExtraKeys,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ShapeField {
    pub name: String,
    pub required: bool,
    pub shape: SchemaShape,
}

/// Whether an object accepts keys beyond its named fields.
#[derive(Clone, Debug, PartialEq)]
#[non_exhaustive]
pub enum ExtraKeys {
    /// `additionalProperties: false`.
    Closed,
    /// The schema does not say. JSON Schema then allows them, but a schema
    /// that lists its fields and stays silent is describing those fields, so
    /// a surface shows the fields alone.
    Unstated,
    /// The schema allows them, with this shape.
    Open(Box<SchemaShape>),
}

/// The refinements a schema node states, kept as annotations.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ShapeConstraints {
    pub minimum: Option<Number>,
    pub exclusive_minimum: Option<Number>,
    pub maximum: Option<Number>,
    pub exclusive_maximum: Option<Number>,
    pub min_length: Option<u64>,
    pub max_length: Option<u64>,
    pub min_items: Option<u64>,
    pub max_items: Option<u64>,
    pub pattern: Option<String>,
    pub format: Option<String>,
}

/// A process call signature.
#[derive(Clone, Debug, PartialEq)]
pub struct ProcessShape {
    pub params: Vec<ProcessParamShape>,
    pub output: Box<SchemaShape>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ProcessParamShape {
    pub name: String,
    pub shape: SchemaShape,
}

/// One addressable field of a shape, for a surface that lists fields in rows.
///
/// `path` joins object fields with `.` and marks array items with `[]`
/// (`response[].artists[].id`). `shape` is the field's shape with nested
/// fields removed, because those have rows of their own.
#[derive(Clone, Debug, PartialEq)]
pub struct ShapeRow {
    pub path: String,
    pub required: bool,
    pub shape: SchemaShape,
}

impl ShapeConstraints {
    pub fn is_empty(&self) -> bool {
        *self == Self::default()
    }

    /// Each constraint as a short annotation, in a fixed order.
    pub fn notes(&self) -> Vec<String> {
        let mut notes = Vec::new();
        if let Some(value) = &self.minimum {
            notes.push(format!(">= {value}"));
        }
        if let Some(value) = &self.exclusive_minimum {
            notes.push(format!("> {value}"));
        }
        if let Some(value) = &self.maximum {
            notes.push(format!("<= {value}"));
        }
        if let Some(value) = &self.exclusive_maximum {
            notes.push(format!("< {value}"));
        }
        if let Some(value) = self.min_length {
            notes.push(format!("min length {value}"));
        }
        if let Some(value) = self.max_length {
            notes.push(format!("max length {value}"));
        }
        if let Some(value) = self.min_items {
            notes.push(format!("min items {value}"));
        }
        if let Some(value) = self.max_items {
            notes.push(format!("max items {value}"));
        }
        if let Some(value) = &self.pattern {
            notes.push(format!("pattern {value}"));
        }
        if let Some(value) = &self.format {
            notes.push(format!("format {value}"));
        }
        notes
    }

    fn from_schema(schema: &Map<String, Value>) -> Self {
        let number = |key: &str| schema.get(key).and_then(Value::as_number).cloned();
        let count = |key: &str| schema.get(key).and_then(Value::as_u64);
        let text = |key: &str| schema.get(key).and_then(Value::as_str).map(str::to_string);
        Self {
            minimum: number("minimum"),
            exclusive_minimum: number("exclusiveMinimum"),
            maximum: number("maximum"),
            exclusive_maximum: number("exclusiveMaximum"),
            min_length: count("minLength"),
            max_length: count("maxLength"),
            min_items: count("minItems"),
            max_items: count("maxItems"),
            pattern: text("pattern"),
            format: text("format"),
        }
    }

    /// Keeps what is stated here and takes the rest from `inner`.
    fn or(self, inner: Self) -> Self {
        Self {
            minimum: self.minimum.or(inner.minimum),
            exclusive_minimum: self.exclusive_minimum.or(inner.exclusive_minimum),
            maximum: self.maximum.or(inner.maximum),
            exclusive_maximum: self.exclusive_maximum.or(inner.exclusive_maximum),
            min_length: self.min_length.or(inner.min_length),
            max_length: self.max_length.or(inner.max_length),
            min_items: self.min_items.or(inner.min_items),
            max_items: self.max_items.or(inner.max_items),
            pattern: self.pattern.or(inner.pattern),
            format: self.format.or(inner.format),
        }
    }
}

impl From<ShapeKind> for SchemaShape {
    fn from(kind: ShapeKind) -> Self {
        Self {
            kind,
            description: None,
            default: None,
            constraints: ShapeConstraints::default(),
        }
    }
}

impl SchemaShape {
    /// Spells this shape using the compact tool contract's type notation.
    pub fn compact_type(&self) -> String {
        super::schema_docs::compact_type(self)
    }

    /// Reads a JSON Schema document as a shape. Local `#` references are
    /// resolved against `schema` itself.
    pub fn from_json_schema(schema: &Value) -> Self {
        ShapeImporter {
            root: schema,
            resolving: Vec::new(),
        }
        .import(schema, 0)
    }

    pub fn unknown() -> Self {
        ShapeKind::Unknown.into()
    }

    /// The named fields of an object shape; empty for every other kind.
    pub fn fields(&self) -> &[ShapeField] {
        match &self.kind {
            ShapeKind::Object(object) => &object.fields,
            _ => &[],
        }
    }

    pub fn is_nullable(&self) -> bool {
        match &self.kind {
            ShapeKind::Null => true,
            ShapeKind::Union(members) => members.iter().any(Self::is_nullable),
            _ => false,
        }
    }

    /// Whether this node says anything a type alone does not: a description,
    /// a constraint or a default.
    pub fn has_notes(&self) -> bool {
        self.description.is_some() || self.default.is_some() || !self.constraints.is_empty()
    }

    /// Every addressable field below this shape: each leaf, and each
    /// container that carries notes of its own. Alternatives of a union that
    /// reach the same path are merged into one row.
    pub fn rows(&self) -> Vec<ShapeRow> {
        let mut rows = Vec::<ShapeRow>::new();
        self.collect_rows("", true, &mut rows);
        let mut merged = Vec::<ShapeRow>::new();
        for row in rows {
            match merged.iter_mut().find(|existing| existing.path == row.path) {
                Some(existing) => existing.merge(row),
                None => merged.push(row),
            }
        }
        merged
    }

    fn collect_rows(&self, path: &str, required: bool, rows: &mut Vec<ShapeRow>) {
        let push = |rows: &mut Vec<ShapeRow>| {
            if !path.is_empty() {
                rows.push(ShapeRow {
                    path: path.to_string(),
                    required,
                    shape: self.without_nested_fields(),
                });
            }
        };
        match &self.kind {
            ShapeKind::Union(members) => {
                if self.has_notes() {
                    push(rows);
                }
                for member in members {
                    member.collect_rows(path, required, rows);
                }
            }
            ShapeKind::Object(object) if !object.fields.is_empty() => {
                if self.has_notes() {
                    push(rows);
                }
                for field in &object.fields {
                    let path = if path.is_empty() {
                        field.name.clone()
                    } else {
                        format!("{path}.{}", field.name)
                    };
                    field.shape.collect_rows(&path, field.required, rows);
                }
            }
            ShapeKind::List(item) if item.has_nested_fields() => {
                if self.has_notes() {
                    push(rows);
                }
                item.collect_rows(&format!("{path}[]"), true, rows);
            }
            _ => push(rows),
        }
    }

    fn has_nested_fields(&self) -> bool {
        match &self.kind {
            ShapeKind::Object(object) => !object.fields.is_empty(),
            ShapeKind::List(item) => item.has_nested_fields(),
            ShapeKind::Union(members) => members.iter().any(Self::has_nested_fields),
            _ => false,
        }
    }

    /// This shape with the named fields of nested objects removed.
    fn without_nested_fields(&self) -> Self {
        let kind = match &self.kind {
            ShapeKind::Object(object) if !object.fields.is_empty() => {
                ShapeKind::Object(ObjectShape {
                    fields: Vec::new(),
                    extra_keys: ExtraKeys::Unstated,
                })
            }
            ShapeKind::List(item) => ShapeKind::List(Box::new(item.without_nested_fields())),
            ShapeKind::Union(members) => {
                union_kind(members.iter().map(Self::without_nested_fields).collect())
            }
            other => other.clone(),
        };
        Self {
            kind,
            description: self.description.clone(),
            default: self.default.clone(),
            constraints: self.constraints.clone(),
        }
    }
}

impl ShapeRow {
    fn merge(&mut self, other: Self) {
        self.required |= other.required;
        let description = self
            .shape
            .description
            .take()
            .or(other.shape.description.clone());
        let default = self.shape.default.take().or(other.shape.default.clone());
        let constraints =
            std::mem::take(&mut self.shape.constraints).or(other.shape.constraints.clone());
        let bare = |shape: &SchemaShape| SchemaShape::from(shape.kind.clone());
        self.shape = SchemaShape {
            kind: union_kind(vec![bare(&self.shape), bare(&other.shape)]),
            description,
            default,
            constraints,
        };
    }
}

/// Flattens nested unions, drops duplicates and puts `null` last. One
/// remaining member is that member; none is [`ShapeKind::Unknown`].
fn union_kind(members: Vec<SchemaShape>) -> ShapeKind {
    fn add(member: SchemaShape, flat: &mut Vec<SchemaShape>) {
        if !flat.contains(&member) {
            flat.push(member);
        }
    }
    let mut flat = Vec::<SchemaShape>::new();
    for member in members {
        // A nested union with notes of its own stays one member, so the
        // notes keep the alternatives they describe.
        let noted = member.has_notes();
        match member.kind {
            ShapeKind::Union(inner) if !noted => {
                for inner in inner {
                    add(inner, &mut flat);
                }
            }
            kind => add(
                SchemaShape {
                    kind,
                    description: member.description,
                    default: member.default,
                    constraints: member.constraints,
                },
                &mut flat,
            ),
        }
    }
    flat.sort_by_key(|member| matches!(member.kind, ShapeKind::Null));
    match flat.len() {
        0 => ShapeKind::Unknown,
        1 => flat.remove(0).kind,
        _ => ShapeKind::Union(flat),
    }
}

struct ShapeImporter<'a> {
    root: &'a Value,
    /// The `#` references being expanded, so a cycle ends in
    /// [`ShapeKind::Unknown`] instead of recursing.
    resolving: Vec<&'a str>,
}

impl<'a> ShapeImporter<'a> {
    fn import(&mut self, schema: &'a Value, depth: usize) -> SchemaShape {
        let Some(map) = schema.as_object().filter(|map| !map.is_empty()) else {
            return SchemaShape::unknown();
        };
        if depth >= MAX_SHAPE_DEPTH {
            return SchemaShape::unknown();
        }
        let inner = self.import_kind(map, depth);
        let mut constraints = ShapeConstraints::from_schema(map).or(inner.constraints);
        // On a number, `format` is a machine width (`uint8`, `double`), which
        // tells a reader nothing the bounds do not.
        if matches!(inner.kind, ShapeKind::Int | ShapeKind::Float) {
            constraints.format = None;
        }
        // The node's own annotations win over the ones a `$ref` or a single
        // `allOf` branch brought with it, per JSON Schema annotation rules.
        SchemaShape {
            kind: inner.kind,
            description: map
                .get("description")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|description| !description.is_empty())
                .map(str::to_string)
                .or(inner.description),
            default: map.get("default").cloned().or(inner.default),
            constraints,
        }
    }

    fn import_kind(&mut self, map: &'a Map<String, Value>, depth: usize) -> SchemaShape {
        if let Some(declaration) = map.get(X_LASH_KEYWORD) {
            return self.import_lash_type(declaration, depth).into();
        }
        if let Some(reference) = map.get("$ref") {
            return self.import_reference(reference, depth);
        }
        if let Some([branch]) = map
            .get("allOf")
            .and_then(Value::as_array)
            .map(Vec::as_slice)
        {
            return self.import(branch, depth + 1);
        }
        if let Some(branches) = map
            .get("anyOf")
            .or_else(|| map.get("oneOf"))
            .and_then(Value::as_array)
        {
            let members = branches
                .iter()
                .map(|branch| self.import(branch, depth + 1))
                .collect();
            return union_kind(members).into();
        }
        if let Some(values) = map.get("enum").and_then(Value::as_array) {
            return literals_kind(values).into();
        }
        if let Some(value) = map.get("const") {
            return literals_kind(std::slice::from_ref(value)).into();
        }
        match map.get("type") {
            Some(Value::String(name)) => self.import_type(name, map, depth).into(),
            Some(Value::Array(names)) => union_kind(
                names
                    .iter()
                    .map(|name| match name.as_str() {
                        Some(name) => self.import_type(name, map, depth).into(),
                        None => SchemaShape::unknown(),
                    })
                    .collect(),
            )
            .into(),
            Some(_) => SchemaShape::unknown(),
            None if [
                "properties",
                "required",
                "additionalProperties",
                "patternProperties",
            ]
            .iter()
            .any(|key| map.contains_key(*key)) =>
            {
                self.import_object(map, depth).into()
            }
            None if map.contains_key("items") || map.contains_key("prefixItems") => {
                self.import_array(map, depth).into()
            }
            None => SchemaShape::unknown(),
        }
    }

    fn import_reference(&mut self, reference: &'a Value, depth: usize) -> SchemaShape {
        let Some(reference) = reference.as_str() else {
            return SchemaShape::unknown();
        };
        let Some(pointer) = reference.strip_prefix('#') else {
            return if is_named_type_reference(reference) {
                ShapeKind::Named(reference.to_string()).into()
            } else {
                SchemaShape::unknown()
            };
        };
        if self.resolving.contains(&reference) {
            return SchemaShape::unknown();
        }
        let Some(target) = self.root.pointer(pointer) else {
            return SchemaShape::unknown();
        };
        self.resolving.push(reference);
        let shape = self.import(target, depth);
        self.resolving.pop();
        shape
    }

    fn import_lash_type(&mut self, declaration: &'a Value, depth: usize) -> ShapeKind {
        // The declaration is decoded into its typed form and its nested
        // schemas are then read from the document itself, so they borrow from
        // it like every other node.
        match XLashType::deserialize(declaration) {
            Ok(XLashType::ProcessUnknown) => ShapeKind::Process(None),
            Ok(XLashType::Handle { .. }) => {
                ShapeKind::Handle(Box::new(self.import_at(declaration, "/payload", depth + 1)))
            }
            Ok(XLashType::Process { signature }) => ShapeKind::Process(Some(ProcessShape {
                params: signature
                    .params
                    .iter()
                    .enumerate()
                    .map(|(index, param)| ProcessParamShape {
                        name: param.name.clone(),
                        shape: self.import_at(
                            declaration,
                            &format!("/signature/params/{index}/schema"),
                            depth + 1,
                        ),
                    })
                    .collect(),
                output: Box::new(self.import_at(declaration, "/signature/output", depth + 1)),
            })),
            Err(_) => ShapeKind::Unknown,
        }
    }

    fn import_at(&mut self, parent: &'a Value, pointer: &str, depth: usize) -> SchemaShape {
        match parent.pointer(pointer) {
            Some(schema) => self.import(schema, depth),
            None => SchemaShape::unknown(),
        }
    }

    fn import_type(&mut self, name: &str, map: &'a Map<String, Value>, depth: usize) -> ShapeKind {
        match name {
            "string" => ShapeKind::Str,
            "integer" => ShapeKind::Int,
            "number" => ShapeKind::Float,
            "boolean" => ShapeKind::Bool,
            "null" => ShapeKind::Null,
            "object" => self.import_object(map, depth),
            "array" => self.import_array(map, depth),
            _ => ShapeKind::Unknown,
        }
    }

    fn import_object(&mut self, map: &'a Map<String, Value>, depth: usize) -> ShapeKind {
        let required = map
            .get("required")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .collect::<Vec<_>>();
        let mut fields = map
            .get("properties")
            .and_then(Value::as_object)
            .into_iter()
            .flatten()
            .map(|(name, schema)| ShapeField {
                name: name.clone(),
                required: required.contains(&name.as_str()),
                shape: self.import(schema, depth + 1),
            })
            .collect::<Vec<_>>();
        // A stable sort: required fields move ahead in `required` order and
        // the optional ones keep their property order behind them.
        fields.sort_by_key(|field| {
            required
                .iter()
                .position(|name| *name == field.name)
                .unwrap_or(usize::MAX)
        });
        let extra_keys = match map.get("additionalProperties") {
            _ if map.contains_key("patternProperties") => {
                ExtraKeys::Open(Box::new(SchemaShape::unknown()))
            }
            Some(Value::Bool(false)) => ExtraKeys::Closed,
            Some(schema) => ExtraKeys::Open(Box::new(self.import(schema, depth + 1))),
            None => ExtraKeys::Unstated,
        };
        ShapeKind::Object(ObjectShape { fields, extra_keys })
    }

    fn import_array(&mut self, map: &'a Map<String, Value>, depth: usize) -> ShapeKind {
        let positions = map
            .get("prefixItems")
            .and_then(Value::as_array)
            .or_else(|| map.get("items").and_then(Value::as_array));
        if let Some(positions) = positions {
            return ShapeKind::Tuple(
                positions
                    .iter()
                    .map(|schema| self.import(schema, depth + 1))
                    .collect(),
            );
        }
        ShapeKind::List(Box::new(match map.get("items") {
            Some(schema) => self.import(schema, depth + 1),
            None => SchemaShape::unknown(),
        }))
    }
}

fn literals_kind(values: &[Value]) -> ShapeKind {
    let literals = values
        .iter()
        .filter(|value| !value.is_null())
        .cloned()
        .collect::<Vec<_>>();
    let nullable = values.iter().any(Value::is_null);
    match (literals.is_empty(), nullable) {
        (true, true) => ShapeKind::Null,
        (true, false) => ShapeKind::Unknown,
        (false, false) => ShapeKind::Literals(literals),
        (false, true) => ShapeKind::Union(vec![
            ShapeKind::Literals(literals).into(),
            ShapeKind::Null.into(),
        ]),
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn shape(schema: Value) -> SchemaShape {
        SchemaShape::from_json_schema(&schema)
    }

    fn field<'a>(shape: &'a SchemaShape, name: &str) -> &'a ShapeField {
        shape
            .fields()
            .iter()
            .find(|field| field.name == name)
            .unwrap_or_else(|| panic!("field `{name}` in {shape:?}"))
    }

    #[test]
    fn local_references_resolve_and_a_cycle_ends_in_unknown() {
        let imported = shape(json!({
            "$defs": {
                "Node": {
                    "type": "object",
                    "description": "A node.",
                    "properties": { "next": { "$ref": "#/$defs/Node" } }
                }
            },
            "type": "object",
            "properties": { "head": { "$ref": "#/$defs/Node", "description": "First node." } }
        }));
        let head = &field(&imported, "head").shape;
        assert_eq!(head.description.as_deref(), Some("First node."));
        assert_eq!(field(head, "next").shape.kind, ShapeKind::Unknown);
        assert_eq!(
            shape(json!({ "$ref": "lash.TriggerRegistration" })).kind,
            ShapeKind::Named("lash.TriggerRegistration".to_string())
        );
        assert_eq!(
            shape(json!({ "$ref": "https://example.com/schema.json" })).kind,
            ShapeKind::Unknown
        );
    }

    #[test]
    fn nesting_past_the_depth_bound_ends_in_unknown() {
        let mut schema = json!({ "type": "string" });
        for _ in 0..(MAX_SHAPE_DEPTH + 3) {
            schema = json!({ "type": "object", "properties": { "child": schema } });
        }
        let mut current = shape(schema);
        let mut objects = 0;
        while let ShapeKind::Object(object) = current.kind {
            objects += 1;
            current = object.fields.into_iter().next().expect("child").shape;
        }
        assert_eq!(objects, MAX_SHAPE_DEPTH);
        assert_eq!(current.kind, ShapeKind::Unknown);
    }

    #[test]
    fn lash_types_and_tuples_are_shapes() {
        assert_eq!(
            shape(json!({ "x-lash": { "kind": "process_unknown" } })).kind,
            ShapeKind::Process(None)
        );
        assert_eq!(
            shape(json!({ "x-lash": { "kind": "handle", "payload": { "type": "string" } } })).kind,
            ShapeKind::Handle(Box::new(ShapeKind::Str.into()))
        );
        assert_eq!(
            shape(json!({ "x-lash": {
                "kind": "process",
                "signature": {
                    "params": [{ "name": "event", "schema": { "type": "string" } }],
                    "output": { "type": "integer" }
                }
            } }))
            .kind,
            ShapeKind::Process(Some(ProcessShape {
                params: vec![ProcessParamShape {
                    name: "event".to_string(),
                    shape: ShapeKind::Str.into(),
                }],
                output: Box::new(ShapeKind::Int.into()),
            }))
        );
        assert_eq!(
            shape(json!({ "x-lash": { "kind": "proc" } })).kind,
            ShapeKind::Unknown
        );
        assert_eq!(
            shape(json!({ "type": "array", "prefixItems": [{ "type": "string" }, { "type": "integer" }] }))
                .kind,
            ShapeKind::Tuple(vec![ShapeKind::Str.into(), ShapeKind::Int.into()])
        );
    }
}
