//! The language-neutral, prompt-facing shape of a JSON Schema.
//!
//! A tool contract's schemas are JSON Schema, and every tool presenter has to
//! show a model what they accept: the TypeScript presenter as a type, the
//! standard presenter as compact signature rows. [`SchemaShape`] is the one
//! reading of a schema those presenters share. It is imported here, and each
//! presenter only spells it; no presenter reads the raw schema again.
//! Runtime-value inference constructs the same shape directly.
//!
//! Import through [`SchemaDocument::from_json_schema`]. Local references stay
//! graph edges, including recursive ones. Unsupported schemas and exhausted
//! resources return [`ShapeImportRefusal`]. Value refinements remain explicit
//! notes even when the base type is [`ShapeKind::Unknown`]. Validation remains
//! the contract's responsibility.

mod document;
mod import;
mod names;

pub use document::{CanonicalIdentity, DefinitionId, SchemaDefinition, SchemaDocument};
pub use import::{ImportLimits, ShapeImportRefusal};
pub use names::ReadableNameAllocator;

use serde::{Deserialize, Serialize};
use serde_json::{Map, Number, Value};

/// The JSON Schema keyword that carries the types JSON Schema cannot say.
///
/// JSON Schema has no vocabulary for a callable process, so a tool contract
/// that traffics in one spells it under this
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
/// Named data types are dotted identifiers (`lash.ProcessRecord`). A URL
/// or a relative file reference is a JSON Schema construct lash cannot
/// resolve.
///
/// ```
/// # use lash_tool_shape::is_named_type_reference;
/// assert!(is_named_type_reference("lash.ProcessRecord"));
/// assert!(!is_named_type_reference("https://example.com/schema.json"));
/// ```
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
    /// Anything: an explicitly unconstrained schema.
    Unknown,
    /// No value: the false schema.
    Never,
    /// An edge to a definition in the owning document.
    Ref(DefinitionId),
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

// Refinements preserved verbatim because a language type cannot encode them.
const VALUE_NOTE_KEYWORDS: &[&str] = &[
    "minProperties",
    "maxProperties",
    "multipleOf",
    "uniqueItems",
    "contains",
    "minContains",
    "maxContains",
    "propertyNames",
    "patternProperties",
    "dependentRequired",
    "not",
];

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
    /// Value-narrowing JSON Schema keywords, rendered as compact JSON notes.
    /// These constrain the node at its own path, including reference overlays.
    pub value_notes: Vec<(String, Value)>,
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
        notes.extend(
            self.value_notes
                .iter()
                .map(|(keyword, value)| format!("{keyword} {value}")),
        );
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
            value_notes: VALUE_NOTE_KEYWORDS
                .iter()
                .filter_map(|keyword| {
                    schema
                        .get(*keyword)
                        .map(|value| ((*keyword).into(), value.clone()))
                })
                .collect(),
        }
    }

    /// Keeps use-site values stated here, filling absent notes from `inner`.
    /// Both sets of value-refinement notes remain: they apply conjunctively.
    /// Structural assertion intersections must be admitted by the importer.
    ///
    /// ```
    /// # use lash_tool_shape::ShapeConstraints;
    /// let notes = ShapeConstraints::default().overlay(ShapeConstraints::default());
    /// assert!(notes.is_empty());
    /// ```
    pub fn overlay(self, inner: Self) -> Self {
        let mut value_notes = self.value_notes;
        for note in inner.value_notes {
            if !value_notes.contains(&note) {
                value_notes.push(note);
            }
        }
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
            value_notes,
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
            std::mem::take(&mut self.shape.constraints).overlay(other.shape.constraints.clone());
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
/// remaining member is that member; none is [`ShapeKind::Never`].
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
        0 => ShapeKind::Never,
        1 => flat.remove(0).kind,
        _ => ShapeKind::Union(flat),
    }
}

#[cfg(test)]
mod tests;
