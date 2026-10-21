//! Kernel types of JSON schemas: a tool's input and result as the kernel
//! signature a document performs it under, and whether a tool fits an
//! effect (R7, D4).
//!
//! An object schema with `properties` is a record of its declared fields.
//! Its rest is what `additionalProperties` states: `Json` when it states
//! nothing or `true` (the open objects MCP servers and pydantic write), the
//! projected schema when it is one, and no rest when it is `false`. A
//! tool's result is projected **open**: a closed object schema is a record
//! whose rest is `Json` too, so a tool that adds an output field still
//! serves a document checked against its earlier result. Input stays as
//! the schema states: it is contravariant, and a tool accepts only what its
//! schema states. A manually authored closed kernel result stays closed, and
//! an open provided result does not serve it.
//!
//! A local `$ref` (`#`, `#/$defs/...`, `#/definitions/...`) is the type of
//! its target, met with the keywords beside it. A reference into a target
//! it is already resolving is a cycle, which the kernel grammar has no
//! recursive type for: the projection stops there at `Json` when nothing
//! the cycle reaches carries an `x-lash` marker, and at `Any` otherwise, so
//! a recursive slot that may hold a function still admits one. Any other
//! reference, and every reference of a schema that declares a nested `$id`,
//! is `Json`. A `const` is its literal's type: a one-text enum, or the
//! scalar type of any other literal.
//!
//! Compositions are types too. `anyOf`, `oneOf` and a `type` list are the
//! union of their members; `allOf`, and every keyword beside another in one
//! schema, is the meet of its members. `oneOf` is typed as `anyOf`: that
//! exactly one member matches is a fact about values no kernel type states,
//! and every value of the `oneOf` is a value of the union, so the union is
//! the tightest type that admits them all; the tool validates exclusivity.
//! A meet the grammar cannot state refuses the schema, naming where.
//!
//! A projection is bounded. A meet distributes over a union, so two unions
//! meet as their product: a meet that would hold more type nodes than both
//! its sides together and more than [`MAX_PROJECTED_TYPE_NODES`] refuses
//! the schema, naming the conjunction, before the product is built. A
//! projection resolves at most [`MAX_RESOLVED_REFERENCES`] local
//! references, each resolution counted again, so definitions a schema
//! shares down many paths project in bounded work; a reference past that
//! budget is typed as one at a cycle is.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

use crate::tool_shape::{
    X_LASH_KEYWORD, XLashType, has_scoped_references, local_reference_pointer,
};
use crate::{Name, Param, RecordType, RecordTypeField, Signature, Type, print_type};
use serde_json::Value;

use crate::type_relations::{data_only, union};

/// The most type nodes a meet may hold beyond its two sides together.
pub const MAX_PROJECTED_TYPE_NODES: usize = 4096;

/// The most local `$ref`s one projection resolves, each resolution of a
/// reference counted again; a reference past them is typed as one at a
/// cycle is.
pub const MAX_RESOLVED_REFERENCES: usize = 4096;

/// The kernel type a JSON schema describes. A JSON schema describes data:
/// a form the grammar has no exact type for is [`Type::Json`], which
/// admits no function, and only a slot an `x-lash` declaration marks
/// admits one. The tool still validates its own input.
///
/// # Errors
///
/// [`SchemaProjectionRefusal`]: a composition with no member, or members
/// whose meet no kernel type states or grows past its bound.
pub fn type_of_schema(schema: &Value) -> Result<Type, SchemaProjectionRefusal> {
    Projector::of(schema).project(schema, Openness::Closed, &Pointer::Root)
}

/// The kernel type a tool's result schema describes: [`type_of_schema`],
/// each record of it open (D4).
///
/// # Errors
///
/// As [`type_of_schema`].
pub fn result_type_of_schema(schema: &Value) -> Result<Type, SchemaProjectionRefusal> {
    Projector::of(schema).project(schema, Openness::Open, &Pointer::Root)
}

/// The signature a tool is performed under, from its input's
/// [`type_of_schema`] and its result's [`result_type_of_schema`]: it takes
/// its input as one record and gives its result open. The input may be
/// omitted when `input_omittable`, a fact the schema layer decides (the
/// input schema accepts the empty object), and an omitted input is
/// performed as `{}`.
#[must_use]
pub fn tool_signature(input: Type, input_omittable: bool, result: Type) -> Signature {
    Signature {
        params: vec![Param {
            name: Name::new("input"),
            ty: input,
            optional: input_omittable,
        }],
        result,
    }
}

/// Whether a function of signature `provided` stands wherever one of
/// `expected` is performed (K-ADM-002): it takes every argument list
/// `expected` allows, and returns only what `expected` promises.
#[must_use]
pub fn signature_serves(provided: &Signature, expected: &Signature) -> bool {
    signature_mismatch(provided, expected).is_none()
}

/// Why a JSON schema has no kernel type: the JSON pointer of the
/// composition that has none, or of the schema whose projection passed a
/// bound, and why.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("the schema at `{path}` has no kernel type: {conflict}")]
pub struct SchemaProjectionRefusal {
    /// The JSON pointer, from the schema's root, of the composition keyword
    /// whose members have no kernel type, or of the schema or `$ref` whose
    /// projection passed a bound; empty for the root itself. Within a
    /// local reference's target, it points from the root to that target.
    pub path: String,
    pub conflict: SchemaConflict,
}

/// What a composition's members cannot be.
#[non_exhaustive]
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SchemaConflict {
    /// A composition or `type` list with no member, which no value is of.
    NoMembers,
    /// No kernel type is both `left` and `right`, at `fields` within them.
    Meet {
        fields: Vec<String>,
        left: Type,
        right: Type,
    },
    /// One member requires `field`, at `fields` within them, and another
    /// member's closed record has no room for it.
    ClosedOut { fields: Vec<String>, field: String },
    /// The meet of the members would hold more than `limit` type nodes:
    /// the larger of [`MAX_PROJECTED_TYPE_NODES`] and its two sides
    /// together.
    TooLarge { limit: usize },
}

impl fmt::Display for SchemaConflict {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let within = |f: &mut fmt::Formatter<'_>, fields: &[String]| {
            if fields.is_empty() {
                Ok(())
            } else {
                write!(f, "at field `{}`, ", fields.join("."))
            }
        };
        match self {
            Self::NoMembers => f.write_str("it has no member, so no value is of it"),
            Self::Meet {
                fields,
                left,
                right,
            } => {
                within(f, fields)?;
                write!(
                    f,
                    "no kernel type is both `{}` and `{}`",
                    print_type(left),
                    print_type(right)
                )
            }
            Self::ClosedOut { fields, field } => {
                within(f, fields)?;
                write!(
                    f,
                    "one member requires field `{field}` and another's closed record has no room for it"
                )
            }
            Self::TooLarge { limit } => write!(
                f,
                "the meet of its members would hold more than {limit} type nodes"
            ),
        }
    }
}

#[derive(Clone, Copy)]
enum Openness {
    Closed,
    Open,
}

/// Where a schema stands within its root, written out only for a refusal.
enum Pointer<'a> {
    Root,
    /// A local reference's target, as the reference spelled its pointer.
    Target(&'a str),
    Key(&'a Pointer<'a>, &'a str),
    Index(&'a Pointer<'a>, usize),
}

impl<'a> Pointer<'a> {
    fn key(&'a self, key: &'a str) -> Self {
        Self::Key(self, key)
    }

    fn index(&'a self, index: usize) -> Self {
        Self::Index(self, index)
    }

    fn refuse(&self, conflict: SchemaConflict) -> SchemaProjectionRefusal {
        SchemaProjectionRefusal {
            path: self.to_string(),
            conflict,
        }
    }
}

impl fmt::Display for Pointer<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Root => Ok(()),
            Self::Target(pointer) => f.write_str(pointer),
            Self::Key(parent, key) => {
                write!(f, "{parent}/{}", key.replace('~', "~0").replace('/', "~1"))
            }
            Self::Index(parent, index) => write!(f, "{parent}/{index}"),
        }
    }
}

type Projected = Result<Type, SchemaProjectionRefusal>;

/// One projection of a schema: its root, which every local `$ref` within
/// it resolves against, and what resolving them has claimed so far.
struct Projector<'a> {
    root: &'a Value,
    /// A nested `$id` moves the base references resolve against
    /// ([`has_scoped_references`]): none of this root's is resolved.
    scoped: bool,
    /// The local pointers being resolved, outermost first, the root's own
    /// (`""`) at the bottom: a reference to one of them is a cycle.
    resolving: Vec<&'a str>,
    /// How many local references this projection has resolved.
    resolved: usize,
    /// Whether the schema at each pointer reaches an `x-lash` marker
    /// through its keywords and local references, as computed once.
    marked: BTreeMap<&'a str, bool>,
}

impl<'a> Projector<'a> {
    fn of(root: &'a Value) -> Self {
        Self {
            root,
            scoped: has_scoped_references(root),
            resolving: vec![""],
            resolved: 0,
            marked: BTreeMap::new(),
        }
    }

    /// Every keyword of a schema at once: its `x-lash` declaration alone
    /// when it has one, otherwise the meet of its own type, its local
    /// reference and its compositions.
    fn project(&mut self, schema: &'a Value, openness: Openness, at: &Pointer<'_>) -> Projected {
        let Some(object) = schema.as_object() else {
            return Ok(Type::Json);
        };
        if let Some(declaration) = object.get(X_LASH_KEYWORD) {
            return self.lash_type(declaration, &at.key(X_LASH_KEYWORD));
        }
        let mut ty = self.own_type(object, openness, at)?;
        if let Some(reference) = object.get("$ref").and_then(Value::as_str) {
            let at = at.key("$ref");
            let target = self.reference(reference, openness)?;
            ty = meet(ty, target, &mut Vec::new()).map_err(|conflict| at.refuse(conflict))?;
        }
        for keyword in ["allOf", "anyOf", "oneOf"] {
            let Some(members) = object.get(keyword) else {
                continue;
            };
            let at = at.key(keyword);
            let Some(members) = members.as_array() else {
                continue;
            };
            if members.is_empty() {
                return Err(at.refuse(SchemaConflict::NoMembers));
            }
            let mut projected = Vec::with_capacity(members.len());
            for (index, member) in members.iter().enumerate() {
                projected.push(self.project(member, openness, &at.index(index))?);
            }
            let composed = if keyword == "allOf" {
                projected
                    .into_iter()
                    .try_fold(Type::Json, |left, right| meet(left, right, &mut Vec::new()))
            } else {
                Ok(alternatives(projected))
            };
            ty = composed
                .and_then(|composed| meet(ty, composed, &mut Vec::new()))
                .map_err(|conflict| at.refuse(conflict))?;
        }
        Ok(ty)
    }

    /// The type a local `$ref` names: its target's, and `Json` for any
    /// other reference. At a cycle, and past [`MAX_RESOLVED_REFERENCES`],
    /// it is `Json`, or `Any` when its target may carry a marked slot.
    fn reference(&mut self, reference: &'a str, openness: Openness) -> Projected {
        let Some(pointer) = local_reference_pointer(reference).filter(|_| !self.scoped) else {
            return Ok(Type::Json);
        };
        let Some(target) = self.root.pointer(pointer) else {
            return Ok(Type::Json);
        };
        if self.resolving.contains(&pointer) || self.resolved == MAX_RESOLVED_REFERENCES {
            return Ok(if self.reaches_marker(pointer) {
                Type::Any
            } else {
                Type::Json
            });
        }
        self.resolved += 1;
        self.resolving.push(pointer);
        let projected = self.project(target, openness, &Pointer::Target(pointer));
        self.resolving.pop();
        projected
    }

    /// Whether the schema at local `pointer` carries an `x-lash` marker
    /// anywhere within it or within a local reference it reaches.
    fn reaches_marker(&mut self, pointer: &'a str) -> bool {
        if let Some(marked) = self.marked.get(pointer) {
            return *marked;
        }
        let mut seen = BTreeSet::from([pointer]);
        let mut pending: Vec<&'a Value> = self.root.pointer(pointer).into_iter().collect();
        let mut marked = false;
        while let Some(value) = pending.pop() {
            match value {
                Value::Object(map) => {
                    if map
                        .get(X_LASH_KEYWORD)
                        .and_then(XLashType::decode)
                        .is_some()
                    {
                        marked = true;
                        break;
                    }
                    if let Some(next) = map
                        .get("$ref")
                        .and_then(Value::as_str)
                        .and_then(local_reference_pointer)
                        .filter(|next| seen.insert(next))
                    {
                        pending.extend(self.root.pointer(next));
                    }
                    pending.extend(map.values());
                }
                Value::Array(items) => pending.extend(items),
                _ => {}
            }
        }
        self.marked.insert(pointer, marked);
        marked
    }

    /// The type a schema's own `enum`, `const` or `type` states, before its
    /// reference and compositions: `Json` when it states none the grammar
    /// has. An object schema that states no `type` but declares
    /// `properties` also admits every non-object value, so it is the
    /// record beside `Json`.
    fn own_type(
        &mut self,
        object: &'a serde_json::Map<String, Value>,
        openness: Openness,
        at: &Pointer<'_>,
    ) -> Projected {
        if let Some(values) = object.get("enum").and_then(Value::as_array) {
            return Ok(enum_type(values));
        }
        if let Some(value) = object.get("const") {
            return Ok(const_type(value));
        }
        match object.get("type") {
            Some(Value::String(name)) => self.named_type(name, object, openness, at),
            Some(Value::Array(names)) => {
                if names.is_empty() {
                    return Err(at.key("type").refuse(SchemaConflict::NoMembers));
                }
                let mut members = Vec::with_capacity(names.len());
                for name in names {
                    members.push(match name.as_str() {
                        Some(name) => self.named_type(name, object, openness, at)?,
                        None => Type::Json,
                    });
                }
                Ok(alternatives(members))
            }
            None if object.contains_key("properties") => Ok(alternatives(vec![
                Type::Json,
                self.record_of(object, openness, at)?,
            ])),
            _ => Ok(Type::Json),
        }
    }

    fn named_type(
        &mut self,
        name: &str,
        object: &'a serde_json::Map<String, Value>,
        openness: Openness,
        at: &Pointer<'_>,
    ) -> Projected {
        Ok(match name {
            "null" => Type::Null,
            "boolean" => Type::Bool,
            "integer" => Type::Int,
            "number" => Type::Number,
            "string" => Type::Text,
            "array" => Type::List(Box::new(match object.get("items") {
                Some(items) => self.project(items, openness, &at.key("items"))?,
                None => Type::Json,
            })),
            "object" => self.record_of(object, openness, at)?,
            _ => Type::Json,
        })
    }

    /// An object schema as a record of its declared `properties`, its rest
    /// what `additionalProperties` states (open in an open result); an
    /// object schema that states neither is data.
    fn record_of(
        &mut self,
        object: &'a serde_json::Map<String, Value>,
        openness: Openness,
        at: &Pointer<'_>,
    ) -> Projected {
        let properties = object.get("properties").and_then(Value::as_object);
        let additional = object.get("additionalProperties");
        let rest = match additional {
            // A key a pattern matches may hold what that pattern's schema
            // states, whatever the rest is.
            _ if object.contains_key("patternProperties") => Some(Type::Json),
            Some(Value::Bool(false)) => match openness {
                Openness::Closed => None,
                Openness::Open => Some(Type::Json),
            },
            Some(schema @ Value::Object(_)) => {
                Some(self.project(schema, openness, &at.key("additionalProperties"))?)
            }
            _ => Some(Type::Json),
        };
        let Some(properties) = properties else {
            return Ok(match (additional, rest) {
                (Some(Value::Bool(false) | Value::Object(_)), rest) => Type::Record(RecordType {
                    fields: Vec::new(),
                    rest: rest.map(Box::new),
                }),
                _ => Type::Json,
            });
        };
        let required: BTreeSet<&str> = object
            .get("required")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .collect();
        let properties_at = at.key("properties");
        let mut fields = Vec::with_capacity(properties.len());
        for (field, schema) in properties {
            fields.push(RecordTypeField {
                name: field.clone(),
                ty: self.project(schema, openness, &properties_at.key(field))?,
                optional: !required.contains(field.as_str()),
            });
        }
        Ok(Type::Record(RecordType {
            fields,
            rest: rest.map(Box::new),
        }))
    }

    /// The kernel type of an `x-lash` declaration, one for each kind of the
    /// one decode the presenters read too. A process value is a handle
    /// record, which a cell calls through its dialect, so a process slot
    /// stays open. A declaration this vocabulary has no kind for is data.
    fn lash_type(&mut self, declaration: &'a Value, at: &Pointer<'_>) -> Projected {
        let Some(declared) = XLashType::decode(declaration) else {
            return Ok(Type::Json);
        };
        Ok(match declared {
            XLashType::Timestamp => Type::Timestamp,
            XLashType::Bytes => Type::Bytes,
            XLashType::Function { signature: Some(_) } => {
                // The decode above read the signature out of `declaration`;
                // its schemas are projected where they stand in the root.
                let at = at.key("signature");
                let params_at = at.key("params");
                let params_json = declaration
                    .pointer("/signature/params")
                    .and_then(Value::as_array)
                    .map(Vec::as_slice)
                    .unwrap_or_default();
                let mut params = Vec::with_capacity(params_json.len());
                for (index, param) in params_json.iter().enumerate() {
                    params.push(Param {
                        name: Name::new(param["name"].as_str().unwrap_or_default()),
                        ty: self.project(
                            &param["schema"],
                            Openness::Closed,
                            &params_at.index(index).key("schema"),
                        )?,
                        optional: false,
                    });
                }
                let output = declaration
                    .pointer("/signature/output")
                    .unwrap_or(&Value::Null);
                Type::Function(Box::new(Signature {
                    params,
                    result: self.project(output, Openness::Open, &at.key("output"))?,
                }))
            }
            XLashType::Function { signature: None }
            | XLashType::Process { .. }
            | XLashType::ProcessUnknown => Type::Any,
        })
    }
}

/// A text enum, nullable when `null` is among its values; any other enum
/// is data.
fn enum_type(values: &[Value]) -> Type {
    let nullable = values.iter().any(Value::is_null);
    let texts: Option<Vec<String>> = values
        .iter()
        .filter(|value| !value.is_null())
        .map(|value| value.as_str().map(str::to_owned))
        .collect();
    match texts {
        Some(texts) if !texts.is_empty() && nullable => {
            Type::Union(vec![Type::Enum(texts), Type::Null])
        }
        Some(texts) if !texts.is_empty() => Type::Enum(texts),
        Some(_) if nullable => Type::Null,
        _ => Type::Json,
    }
}

/// A `const`'s literal type: a text is the enum of that one text, a
/// number an `Int` when it is spelled as an integer and a `Number`
/// otherwise; a list or an object literal is data.
fn const_type(value: &Value) -> Type {
    match value {
        Value::Null => Type::Null,
        Value::Bool(_) => Type::Bool,
        Value::Number(number) if number.is_i64() || number.is_u64() => Type::Int,
        Value::Number(_) => Type::Number,
        Value::String(text) => Type::Enum(vec![text.clone()]),
        Value::Array(_) | Value::Object(_) => Type::Json,
    }
}

/// How many type nodes `ty` holds.
fn type_nodes(ty: &Type) -> usize {
    1 + match ty {
        Type::Tuple(members) | Type::Union(members) => members.iter().map(type_nodes).sum(),
        Type::List(item) | Type::Set(item) | Type::Task(item) => type_nodes(item),
        Type::Map(map) => type_nodes(&map.key) + type_nodes(&map.value),
        Type::Record(record) => {
            record
                .fields
                .iter()
                .map(|field| type_nodes(&field.ty))
                .sum::<usize>()
                + record.rest.as_deref().map_or(0, type_nodes)
        }
        Type::Function(signature) => {
            signature
                .params
                .iter()
                .map(|param| type_nodes(&param.ty))
                .sum::<usize>()
                + type_nodes(&signature.result)
        }
        Type::Any
        | Type::Json
        | Type::Null
        | Type::Absent
        | Type::Bool
        | Type::Int
        | Type::Float
        | Type::Number
        | Type::Text
        | Type::Bytes
        | Type::Timestamp
        | Type::Enum(_)
        | Type::Error
        | Type::Handle(_) => 0,
    }
}

/// The union of a composition's members, through the union normalizer.
/// `Json` already holds every member whose every value is data, so it
/// absorbs them: what is left beside it may hold a function.
fn alternatives(members: Vec<Type>) -> Type {
    if members.contains(&Type::Json) {
        union(
            members
                .into_iter()
                .filter(|member| *member == Type::Json || !data_only(member)),
        )
    } else {
        union(members)
    }
}

/// The type of every value of both `left` and `right`, the members of one
/// conjunction, at `fields` within it. `Json` is what a projection gives a
/// form it has no exact type for, so it adds nothing to a meet; neither
/// does `Any`, which only a marker gives.
fn meet(left: Type, right: Type, fields: &mut Vec<String>) -> Result<Type, SchemaConflict> {
    let conflict = |fields: &[String], left: Type, right: Type| SchemaConflict::Meet {
        fields: fields.to_vec(),
        left,
        right,
    };
    Ok(match (left, right) {
        (Type::Json, other) | (other, Type::Json) | (Type::Any, other) | (other, Type::Any) => {
            other
        }
        (left, right) if left == right => left,
        (Type::Union(members), other) | (other, Type::Union(members)) => {
            // Each member that meets `other` stays. Two unions meet as their
            // product, so the members met so far stay within the bound as
            // they are met, never only after.
            let limit = MAX_PROJECTED_TYPE_NODES
                .max(1 + members.iter().map(type_nodes).sum::<usize>() + type_nodes(&other));
            let mut spent = 1;
            let mut met = Vec::new();
            for member in &members {
                match meet(member.clone(), other.clone(), fields) {
                    Ok(member) => {
                        spent += type_nodes(&member);
                        if spent > limit {
                            return Err(SchemaConflict::TooLarge { limit });
                        }
                        met.push(member);
                    }
                    Err(too_large @ SchemaConflict::TooLarge { .. }) => return Err(too_large),
                    Err(_) => {}
                }
            }
            if met.is_empty() {
                return Err(conflict(fields, Type::Union(members), other));
            }
            alternatives(met)
        }
        (Type::Number, Type::Int) | (Type::Int, Type::Number) => Type::Int,
        (Type::Number, Type::Float) | (Type::Float, Type::Number) => Type::Float,
        (Type::Text, Type::Enum(texts)) | (Type::Enum(texts), Type::Text) => Type::Enum(texts),
        (Type::Enum(left), Type::Enum(right)) => {
            let shared: Vec<String> = left
                .iter()
                .filter(|text| right.contains(text))
                .cloned()
                .collect();
            if shared.is_empty() {
                return Err(conflict(fields, Type::Enum(left), Type::Enum(right)));
            }
            Type::Enum(shared)
        }
        (Type::List(left), Type::List(right)) => Type::List(Box::new(meet(*left, *right, fields)?)),
        (Type::Record(left), Type::Record(right)) => {
            Type::Record(meet_records(left, right, fields)?)
        }
        (left, right) => return Err(conflict(fields, left, right)),
    })
}

/// Two records' meet: each field either names, typed by both, and required
/// when either requires it. A field one names and the other does not is
/// typed by the other's rest; a closed record has no room for it, so it is
/// dropped when optional and refused when required. The meet is closed
/// when either is.
fn meet_records(
    left: RecordType,
    right: RecordType,
    fields: &mut Vec<String>,
) -> Result<RecordType, SchemaConflict> {
    let mut met = Vec::new();
    let mut within = |field: RecordTypeField,
                      other: Option<RecordTypeField>,
                      other_rest: Option<&Type>,
                      fields: &mut Vec<String>|
     -> Result<(), SchemaConflict> {
        let (ty, optional) = match (other, other_rest) {
            (Some(other), _) => (other.ty, field.optional && other.optional),
            (None, Some(rest)) => (rest.clone(), field.optional),
            (None, None) if field.optional => return Ok(()),
            (None, None) => {
                return Err(SchemaConflict::ClosedOut {
                    fields: fields.clone(),
                    field: field.name,
                });
            }
        };
        fields.push(field.name.clone());
        let ty = meet(field.ty, ty, fields)?;
        fields.pop();
        met.push(RecordTypeField {
            name: field.name,
            ty,
            optional,
        });
        Ok(())
    };
    let mut others = right.fields;
    for field in left.fields {
        let other = others
            .iter()
            .position(|other| other.name == field.name)
            .map(|index| others.remove(index));
        within(field, other, right.rest.as_deref(), fields)?;
    }
    for field in others {
        within(field, None, left.rest.as_deref(), fields)?;
    }
    let rest = match (left.rest, right.rest) {
        (Some(left), Some(right)) => Some(Box::new(meet(*left, *right, fields)?)),
        _ => None,
    };
    Ok(RecordType { fields: met, rest })
}

/// The first part of `provided` that cannot serve `expected`, found by the
/// same relation as [`signature_serves`].
#[must_use]
pub fn signature_mismatch(
    provided: &Signature,
    expected: &Signature,
) -> Option<crate::SignatureMismatch> {
    crate::type_relations::serves(provided, expected)
}
