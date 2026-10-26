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
//! graph edges, including recursive ones. Every valid schema is presentable;
//! refinements a type cannot express and resource cuts retain validation notes.
//! Only malformed schemas return [`ShapeImportRefusal`]. Value refinements remain explicit
//! notes even when the base type is [`ShapeKind::Unknown`]. Validation remains
//! the contract's responsibility.

mod document;
mod import;
mod rules;

pub use document::{CanonicalIdentity, DefinitionId, SchemaDefinition, SchemaDocument};
pub use import::{ImportLimits, ShapeImportRefusal};

use serde::{Deserialize, Serialize};
use serde_json::{Map, Number, Value};

/// The JSON Schema keyword that carries the types JSON Schema cannot say.
///
/// JSON Schema describes data only: it has no vocabulary for a function a
/// program hands over or a callable process, so a tool contract that
/// traffics in one spells it under this extension keyword. A contract that
/// does not carry it reads as plain JSON Schema, and a slot it does not mark
/// admits no function.
pub const X_LASH_KEYWORD: &str = "x-lash";

/// The lash-only half of a type, as it rides inside a JSON Schema.
///
/// [`XLashType::decode`] is its one reader: the kernel's projection of a
/// schema and every presenter match the decoded kind, so a kind neither
/// maps does not compile.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum XLashType {
    /// A function the program passes, crossing as a function value; with
    /// the signature it is called under, when the contract states one.
    Function {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        signature: Option<XLashSignature>,
    },
    /// A process with an authoritative call signature.
    Process { signature: XLashSignature },
    /// A process the host can only describe as callable.
    ProcessUnknown,
    /// Decimal nanoseconds on the JSON boundary.
    Timestamp,
    /// Lowercase hexadecimal bytes on the JSON boundary.
    Bytes,
}

impl XLashType {
    /// The `x-lash` declaration `value` states, or `None` when it states
    /// none this vocabulary has.
    #[must_use]
    pub fn decode(value: &Value) -> Option<Self> {
        Self::deserialize(value).ok()
    }

    /// The call signature the declaration states.
    #[must_use]
    pub fn signature(&self) -> Option<&XLashSignature> {
        match self {
            Self::Function { signature } => signature.as_ref(),
            Self::Process { signature } => Some(signature),
            Self::ProcessUnknown | Self::Timestamp | Self::Bytes => None,
        }
    }
}

/// An ordered call signature in schema form.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct XLashSignature {
    pub params: Vec<XLashParam>,
    pub output: Box<Value>,
}

/// One named call parameter in schema form.
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
/// # use lash_kernel_value::tool_shape::is_named_type_reference;
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

/// The JSON pointer a local `$ref` names within its schema's root: the
/// part after `#`, the root itself when empty. `None` for any other
/// reference, which no reader resolves.
///
/// ```
/// # use lash_kernel_value::tool_shape::local_reference_pointer;
/// assert_eq!(local_reference_pointer("#/$defs/Node"), Some("/$defs/Node"));
/// assert_eq!(local_reference_pointer("#"), Some(""));
/// assert_eq!(local_reference_pointer("other.json#/a"), None);
/// ```
#[must_use]
pub fn local_reference_pointer(reference: &str) -> Option<&str> {
    reference
        .strip_prefix('#')
        .filter(|pointer| pointer.is_empty() || pointer.starts_with('/'))
}

/// Whether a schema below `root` declares its own `$id`. An embedded
/// identifier changes the base a `$ref` resolves against; without a URI
/// resolver, every reader leaves the local references of such a schema to
/// the tool's validation rather than bind one to the wrong root.
///
/// ```
/// # use lash_kernel_value::tool_shape::has_scoped_references;
/// assert!(has_scoped_references(&serde_json::json!({"$defs": {"a": {"$id": "a"}}})));
/// assert!(!has_scoped_references(&serde_json::json!({"$id": "root"})));
/// ```
#[must_use]
pub fn has_scoped_references(root: &Value) -> bool {
    let mut pending = Vec::new();
    if let Some(map) = root.as_object() {
        pending.extend(map.values());
    }
    while let Some(value) = pending.pop() {
        match value {
            Value::Object(map) => {
                if map.contains_key("$id") {
                    return true;
                }
                pending.extend(map.values());
            }
            Value::Array(items) => pending.extend(items),
            _ => {}
        }
    }
    false
}

/// One schema node as a prompt surface shows it.
#[derive(Clone, Debug, PartialEq)]
pub struct SchemaShape {
    pub kind: ShapeKind,
    /// A schema title, retained for readable union arm ownership.
    pub title: Option<String>,
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
    /// All members apply at once, without distributing over unions.
    Intersection(Vec<SchemaShape>),
    /// A host data type referenced by name.
    Named(String),
    /// A process value; `None` when the host only says it is callable.
    Process(Option<ProcessShape>),
    /// A function the program passes; `None` when the contract states no
    /// signature for it.
    Function(Option<ProcessShape>),
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
    /// Rules no language type can show, stated at this node's path.
    pub rules: Vec<String>,
}

/// A process's or a function's call signature.
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
        notes.extend(self.rules.clone());
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
            rules: rules::notes(schema),
            value_notes: VALUE_NOTE_KEYWORDS
                .iter()
                .filter_map(|keyword| {
                    schema
                        .get(*keyword)
                        .filter(|value| *keyword != "not" || rules::simple_not(value).is_none())
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
    /// # use lash_kernel_value::tool_shape::ShapeConstraints;
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
        let mut rules = self.rules;
        for rule in inner.rules {
            if !rules.contains(&rule) {
                rules.push(rule);
            }
        }
        Self {
            rules,
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
            title: None,
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

    /// Note paths for a union's alternatives below `path`. An optional value
    /// (one arm besides `null`) is noted on `path` itself; every other arm is
    /// `path|label`, with a readable, distinct label. Titles win, followed by
    /// a literal discriminant and then a type description.
    pub fn union_arm_paths(&self, path: &str) -> Vec<String> {
        let ShapeKind::Union(members) = &self.kind else {
            return Vec::new();
        };
        let is_null = |member: &Self| matches!(member.kind, ShapeKind::Null);
        let optional = members.len() == 2 && members.iter().filter(|m| is_null(m)).count() == 1;
        let mut used = Vec::<String>::new();
        members
            .iter()
            .map(|member| {
                if optional && !is_null(member) {
                    return path.to_string();
                }
                let base = member.arm_label();
                let mut label = base.clone();
                let mut alternative = 2;
                while used.contains(&label) {
                    label = format!("{base} (alternative {alternative})");
                    alternative += 1;
                }
                let arm = format!("{path}|{label}");
                used.push(label);
                arm
            })
            .collect()
    }

    fn arm_label(&self) -> String {
        if let Some(title) = &self.title {
            return title.clone();
        }
        if let ShapeKind::Object(object) = &self.kind {
            for field in &object.fields {
                if let ShapeKind::Literals(values) = &field.shape.kind
                    && let [value] = values.as_slice()
                {
                    return format!("{}={value}", field.name);
                }
            }
            let required = object
                .fields
                .iter()
                .filter(|field| field.required)
                .map(|field| field.name.as_str())
                .collect::<Vec<_>>();
            if !required.is_empty() {
                return format!("has {}", required.join(", "));
            }
        }
        match &self.kind {
            ShapeKind::Unknown => "any value".into(),
            ShapeKind::Never => "no value".into(),
            ShapeKind::Ref(_) => "referenced type".into(),
            ShapeKind::Null => "null".into(),
            ShapeKind::Bool => "boolean".into(),
            ShapeKind::Int => "integer".into(),
            ShapeKind::Float => "number".into(),
            ShapeKind::Str => "string".into(),
            ShapeKind::Literals(values) => values
                .iter()
                .map(Value::to_string)
                .collect::<Vec<_>>()
                .join(" or "),
            ShapeKind::List(_) => "array".into(),
            ShapeKind::Tuple(_) => "tuple".into(),
            ShapeKind::Object(_) => "object".into(),
            ShapeKind::Union(_) => "union".into(),
            ShapeKind::Intersection(_) => "intersection".into(),
            ShapeKind::Named(name) => name.clone(),
            ShapeKind::Process(_) => "process".into(),
            ShapeKind::Function(_) => "function".into(),
        }
    }

    pub fn is_nullable(&self) -> bool {
        match &self.kind {
            ShapeKind::Null => true,
            ShapeKind::Union(members) => members.iter().any(Self::is_nullable),
            ShapeKind::Intersection(members) => members.iter().all(Self::is_nullable),
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
            ShapeKind::Union(members) | ShapeKind::Intersection(members) => {
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
            ShapeKind::Union(members) | ShapeKind::Intersection(members) => {
                members.iter().any(Self::has_nested_fields)
            }
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
            ShapeKind::Intersection(members) => {
                ShapeKind::Intersection(members.iter().map(Self::without_nested_fields).collect())
            }
            other => other.clone(),
        };
        Self {
            kind,
            title: self.title.clone(),
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
            title: self.shape.title.clone().or(other.shape.title.clone()),
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
        let noted = member.has_notes() || member.title.is_some();
        match member.kind {
            ShapeKind::Union(inner) if !noted => {
                for inner in inner {
                    add(inner, &mut flat);
                }
            }
            kind => add(
                SchemaShape {
                    kind,
                    title: member.title,
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
