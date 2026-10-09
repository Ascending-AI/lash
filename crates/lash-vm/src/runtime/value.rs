//! Value types: the dynamically-typed `Value` enum, its projection wrapper,
//! the `ImageValue` attachment descriptor, and the projection read vocabulary
//! (`ResourceRef`, `ProjectedReadRequest`, `ProjectedReadResponse`).
//!
//! The `Value` enum is the universal currency of the lash_vm runtime: every
//! load, every binary op, every host-tool argument, every JSON round-trip
//! flows through it. `ProjectedValue` wraps host-side bindings the runtime
//! can read but does not own: a scalar in memory, or a `ResourceRef` read
//! through its type's provider (ADR 0132 §9); field/index access on a projected source
//! propagates the wrapper so downstream consumers can tell that this came
//! from a projected binding.

use std::borrow::Borrow;
use std::borrow::Cow;
use std::fmt;

use std::ops::Deref;
use std::sync::Arc;

use compact_str::CompactString;
use lash_render::{RenderNode, RenderValue};
use rustc_hash::FxHashMap;
use serde::ser::SerializeMap;
use serde::{Deserialize, Deserializer, Serialize, Serializer};

use super::projection_provider::{self, ProjectionReadError, ProjectionReader};
use super::record::Symbol;
use super::{
    HeapId, Name, Record, RuntimeError, RuntimeJson, append_tuple_literal_direct,
    execute_contains_direct, from_json, is_truthy as value_truthy, materialize_value,
    read_field_ref_direct, read_index_ref_direct, stringify_value, value_contains_projected,
    value_len, value_type_name, write_number,
};

impl RenderValue for Value {
    fn node(&self) -> RenderNode<'_> {
        match self {
            Self::Null => RenderNode::Null,
            Self::Undefined => RenderNode::Undefined,
            Self::Bool(value) => RenderNode::Bool(*value),
            Self::Number(value) => {
                let mut number = String::new();
                let _ = write_number(&mut number, *value);
                RenderNode::Number(Cow::Owned(if value.is_finite() {
                    number
                } else {
                    "null".into()
                }))
            }
            Self::String(value) => RenderNode::Text(Cow::Borrowed(value.as_str())),
            Self::Tuple(values) | Self::List(values) => RenderNode::Array(values.len()),
            Self::Record(record) => RenderNode::Object(record.len()),
            Self::Image(image) => {
                RenderNode::Placeholder(Cow::Owned(format!("[Image: {}]", image.id)))
            }
            Self::Resource(resource) => {
                RenderNode::Placeholder(Cow::Owned(format!("[Resource: {}]", resource.alias)))
            }
            Self::Ref(_) => RenderNode::Placeholder(Cow::Borrowed("[unavailable reference]")),
            Self::Projected(projected) => match &projected.kind {
                ProjectedKind::Scalar(value) => value.node(),
                ProjectedKind::Resource {
                    type_name,
                    resource,
                } => {
                    let fallback = || {
                        RenderNode::Placeholder(Cow::Owned(
                            projected
                                .render()
                                .unwrap_or_else(|error| format!("[{error}]")),
                        ))
                    };
                    let len = || match projected.len() {
                        Ok(len) => RenderNode::Array(len),
                        Err(_) => fallback(),
                    };
                    if type_name.as_ref() == "list" {
                        return len();
                    }
                    match projected.read_one(resource, ProjectedReadRequest::Keys) {
                        Ok(Some(ProjectedReadResponse::Keys(keys))) => {
                            RenderNode::Object(keys.len())
                        }
                        Ok(Some(ProjectedReadResponse::Value(Value::List(keys)))) => {
                            RenderNode::Object(keys.len())
                        }
                        _ => len(),
                    }
                }
            },
        }
    }

    fn index(&self, index: usize) -> Option<Cow<'_, Self>> {
        match self {
            Self::Tuple(values) | Self::List(values) => values.get(index).map(Cow::Borrowed),
            Self::Projected(projected) => projected
                .get_index(&Self::Number(index as f64))
                .ok()
                .flatten()
                .map(Cow::Owned),
            _ => None,
        }
    }

    fn fields(&self) -> impl Iterator<Item = (Cow<'_, str>, Cow<'_, Self>)> + '_ {
        let fields: Vec<_> = match self {
            Self::Record(record) => record
                .iter()
                .map(|(key, value)| (Cow::Borrowed(key), Cow::Borrowed(value)))
                .collect(),
            Self::Projected(projected) => {
                let keys = projected.keys().unwrap_or_default();
                match &projected.kind {
                    ProjectedKind::Scalar(value) => match value.as_ref() {
                        Value::Record(record) => keys
                            .into_iter()
                            .filter_map(|key| {
                                let value = record.get(&key).cloned()?;
                                Some((Cow::Owned(key), Cow::Owned(value)))
                            })
                            .collect(),
                        _ => Vec::new(),
                    },
                    ProjectedKind::Resource { resource, .. } => projected
                        .read_fields(resource, keys)
                        .unwrap_or_default()
                        .into_iter()
                        .map(|(key, value)| (Cow::Owned(key), Cow::Owned(value)))
                        .collect(),
                }
            }
            _ => Vec::new(),
        };
        fields.into_iter()
    }
}

pub use lash_sansio::schema_contract::LASH_TYPE_KEY;
pub const LASH_HOST_DESCRIPTOR_TYPE_KEY: &str = "$lash_host_descriptor_type";
pub const LASH_HOST_DESCRIPTOR_VALUE_KEY: &str = "$lash_host_descriptor_value";
pub const LASH_PROCESS_VALUE_KEY: &str = "$lash_process";
pub const LASH_PROCESS_NAME_KEY: &str = "process_name";
pub const LASH_MODULE_REF_KEY: &str = "module_ref";
pub const LASH_PROCESS_REF_KEY: &str = "process_ref";
pub const LASH_HOST_REQUIREMENTS_REF_KEY: &str = "host_requirements_ref";

#[derive(Clone, Debug, PartialEq)]
pub struct ListValue {
    values: Arc<Vec<Value>>,
}

impl ListValue {
    pub fn into_vec(self) -> Vec<Value> {
        match Arc::try_unwrap(self.values) {
            Ok(values) => values,
            Err(values) => values.as_ref().clone(),
        }
    }

    pub(crate) fn make_mut(&mut self) -> &mut Vec<Value> {
        Arc::make_mut(&mut self.values)
    }

    pub(crate) fn identity(&self) -> usize {
        Arc::as_ptr(&self.values) as usize
    }
}

impl std::ops::Deref for ListValue {
    type Target = [Value];

    fn deref(&self) -> &Self::Target {
        self.values.as_slice()
    }
}

impl From<Vec<Value>> for ListValue {
    fn from(values: Vec<Value>) -> Self {
        Self {
            values: Arc::new(values),
        }
    }
}

impl FromIterator<Value> for ListValue {
    fn from_iter<T: IntoIterator<Item = Value>>(iter: T) -> Self {
        iter.into_iter().collect::<Vec<_>>().into()
    }
}

/// The string payload inside `Value::String`.
///
/// An inline-capacity string stays a plain `CompactString`: cloning it is a
/// by-value copy, cheaper than managing a shared box. Anything larger lives
/// behind an `Arc`, so cloning a slot's value — or stashing a completion in
/// the VM's `last_value` — is a refcount bump rather than a copy of the text.
///
/// Mutation goes through `make_mut`/`push_str`, which copy-on-write the way
/// `ListValue` does: a buffer that still has aliases is cloned, while one the
/// binding holds alone is appended into. That is what lets `s = s + "x"`
/// reuse its accumulator buffer instead of copying it each step (FIG-3733).
pub struct StringValue(Repr);

enum Repr {
    /// A `CompactString` that fits inline — the `Owned` invariant. An owned
    /// string never holds a heap buffer, so cloning one is a fixed-size copy.
    Owned(CompactString),
    /// Heap-sized contents behind an `Arc`: cloning is a refcount bump, and a
    /// buffer with no other strong references is the one `make_mut` reuses.
    Shared(Arc<CompactString>),
}

impl StringValue {
    fn from_compact(value: CompactString) -> Self {
        if value.is_heap_allocated() {
            Self(Repr::Shared(Arc::new(value)))
        } else {
            Self(Repr::Owned(value))
        }
    }

    /// `left ++ right` in one allocation sized to the result — the concat the
    /// `+` operator and format sites use where no accumulator can be reused.
    pub(crate) fn concatenated(left: &str, right: &str) -> Self {
        let mut value = CompactString::with_capacity(left.len() + right.len());
        value.push_str(left);
        value.push_str(right);
        Self::from_compact(value)
    }

    pub fn as_str(&self) -> &str {
        match &self.0 {
            Repr::Owned(value) => value.as_str(),
            Repr::Shared(value) => value.as_str(),
        }
    }

    /// Mutable access to the bytes: `Arc::make_mut` semantics, so an aliased
    /// buffer is copied before it is touched while a sole-owned one is not.
    ///
    /// Promoting `Owned` into `Shared` is what keeps the invariant: a mutated
    /// string may grow past inline capacity, and only `Shared` may hold heap
    /// contents, since `Clone` on `Owned` must stay a by-value copy.
    pub(crate) fn make_mut(&mut self) -> &mut CompactString {
        if matches!(self.0, Repr::Owned(_)) {
            let Repr::Owned(value) =
                std::mem::replace(&mut self.0, Repr::Owned(CompactString::default()))
            else {
                unreachable!("matched Owned above")
            };
            self.0 = Repr::Shared(Arc::new(value));
        }
        match &mut self.0 {
            Repr::Shared(value) => Arc::make_mut(value),
            Repr::Owned(_) => unreachable!("owned strings promote before mutation"),
        }
    }

    /// Appends `text`, growing geometrically so repeated appends stay
    /// amortised O(1) rather than paying a resize per character.
    pub(crate) fn push_str(&mut self, text: &str) {
        let buffer = self.make_mut();
        if buffer.capacity() - buffer.len() < text.len() {
            buffer.reserve(buffer.len().max(text.len()));
        }
        buffer.push_str(text);
    }

    /// The contents as a fresh owned `CompactString`.
    pub(crate) fn to_compact_string(&self) -> CompactString {
        CompactString::from(self.as_str())
    }
}

impl Clone for StringValue {
    fn clone(&self) -> Self {
        match &self.0 {
            Repr::Owned(value) => {
                debug_assert!(
                    !value.is_heap_allocated(),
                    "an owned string must fit inline storage for clone to stay cheap"
                );
                Self(Repr::Owned(value.clone()))
            }
            Repr::Shared(value) => Self(Repr::Shared(Arc::clone(value))),
        }
    }
}

/// The Owned/Shared split is an implementation detail: a `StringValue` debugs
/// exactly like the `CompactString` it replaced, so `Value`'s Debug output —
/// and every golden that compares it — is unchanged by the representation.
impl fmt::Debug for StringValue {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(self.as_str(), formatter)
    }
}

impl Default for StringValue {
    fn default() -> Self {
        Self(Repr::Owned(CompactString::default()))
    }
}

impl Deref for StringValue {
    type Target = str;

    fn deref(&self) -> &Self::Target {
        self.as_str()
    }
}

impl AsRef<str> for StringValue {
    fn as_ref(&self) -> &str {
        self.as_str()
    }
}

impl Borrow<str> for StringValue {
    fn borrow(&self) -> &str {
        self.as_str()
    }
}

impl fmt::Display for StringValue {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl PartialEq for StringValue {
    fn eq(&self, other: &Self) -> bool {
        self.as_str() == other.as_str()
    }
}

impl Eq for StringValue {}

impl PartialOrd for StringValue {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for StringValue {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.as_str().cmp(other.as_str())
    }
}

impl std::hash::Hash for StringValue {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.as_str().hash(state);
    }
}

impl PartialEq<str> for StringValue {
    fn eq(&self, other: &str) -> bool {
        self.as_str() == other
    }
}

impl PartialEq<&str> for StringValue {
    fn eq(&self, other: &&str) -> bool {
        self.as_str() == *other
    }
}

impl PartialEq<String> for StringValue {
    fn eq(&self, other: &String) -> bool {
        self.as_str() == other
    }
}

impl PartialEq<StringValue> for String {
    fn eq(&self, other: &StringValue) -> bool {
        self == other.as_str()
    }
}

impl PartialEq<CompactString> for StringValue {
    fn eq(&self, other: &CompactString) -> bool {
        self.as_str() == other.as_str()
    }
}

impl From<&str> for StringValue {
    fn from(value: &str) -> Self {
        Self::from_compact(value.into())
    }
}

impl From<String> for StringValue {
    fn from(value: String) -> Self {
        Self::from_compact(value.into())
    }
}

impl From<&String> for StringValue {
    fn from(value: &String) -> Self {
        Self::from(value.as_str())
    }
}

impl From<CompactString> for StringValue {
    fn from(value: CompactString) -> Self {
        Self::from_compact(value)
    }
}

impl From<&CompactString> for StringValue {
    fn from(value: &CompactString) -> Self {
        Self::from(value.as_str())
    }
}

impl From<StringValue> for CompactString {
    fn from(value: StringValue) -> Self {
        match value.0 {
            Repr::Owned(value) => value,
            Repr::Shared(value) => match Arc::try_unwrap(value) {
                Ok(value) => value,
                Err(value) => value.as_ref().clone(),
            },
        }
    }
}

impl From<StringValue> for String {
    fn from(value: StringValue) -> Self {
        CompactString::from(value).into()
    }
}

impl From<&StringValue> for CompactString {
    fn from(value: &StringValue) -> Self {
        value.to_compact_string()
    }
}

impl From<crate::AstString> for StringValue {
    fn from(value: crate::AstString) -> Self {
        Self::from_compact(value.into())
    }
}

impl Serialize for StringValue {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for StringValue {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        CompactString::deserialize(deserializer).map(Self::from_compact)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ImageValue {
    pub id: String,
    pub mime: crate::MediaType,
    pub label: String,
    pub size: u64,
    pub width: Option<u32>,
    pub height: Option<u32>,
}

impl ImageValue {
    pub fn new(
        id: impl Into<String>,
        mime: crate::MediaType,
        label: impl Into<String>,
        size: u64,
        width: Option<u32>,
        height: Option<u32>,
    ) -> Self {
        Self {
            id: id.into(),
            mime,
            label: label.into(),
            size,
            width,
            height,
        }
    }
}

impl Serialize for ImageValue {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let mut map = serializer.serialize_map(Some(7))?;
        map.serialize_entry("type", "image")?;
        map.serialize_entry("id", &self.id)?;
        map.serialize_entry("mime", &self.mime)?;
        map.serialize_entry("label", &self.label)?;
        map.serialize_entry("size", &self.size)?;
        map.serialize_entry("width", &self.width)?;
        map.serialize_entry("height", &self.height)?;
        map.end()
    }
}

impl<'de> Deserialize<'de> for ImageValue {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        #[derive(Deserialize)]
        struct ImageDescriptor {
            #[serde(rename = "type")]
            kind: String,
            id: String,
            mime: crate::MediaType,
            label: String,
            size: u64,
            #[serde(default)]
            width: Option<u32>,
            #[serde(default)]
            height: Option<u32>,
        }

        let descriptor = ImageDescriptor::deserialize(deserializer)?;
        if descriptor.kind != "image" {
            return Err(serde::de::Error::custom("expected image descriptor"));
        }
        Ok(Self {
            id: descriptor.id,
            mime: descriptor.mime,
            label: descriptor.label,
            size: descriptor.size,
            width: descriptor.width,
            height: descriptor.height,
        })
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResourceHandle {
    pub resource_type: String,
    pub alias: String,
}

impl ResourceHandle {
    pub fn new(resource_type: impl Into<String>, alias: impl Into<String>) -> Self {
        Self {
            resource_type: resource_type.into(),
            alias: alias.into(),
        }
    }
}

#[derive(Clone, Debug)]
pub enum Value {
    Null,
    Undefined,
    Bool(bool),
    Number(f64),
    String(StringValue),
    // Boxed: `ImageValue` is by far the largest variant, and images are rare in
    // value streams. Storing it inline would inflate `size_of::<Value>()` (and
    // therefore every `Vec<Value>`/record allocation) for the common case, so we
    // keep the payload behind a pointer.
    Image(Box<ImageValue>),
    Resource(ResourceHandle),
    Ref(HeapId),
    Tuple(ListValue),
    List(ListValue),
    Record(Arc<Record>),
    Projected(ProjectedValue),
}

impl Value {
    pub fn as_record(&self) -> Option<&Record> {
        match self {
            Self::Record(record) => Some(record.as_ref()),
            _ => None,
        }
    }

    pub fn contains_projected(&self) -> bool {
        value_contains_projected(self)
    }

    /// This value with every compound in it rebuilt, so that none of them is
    /// a tree some heap exported.
    ///
    /// A heap resolves an inline compound it exported back to the object it
    /// came from, by the compound's identity. A value from outside the VM has
    /// to come in as new objects instead, whatever its host built it from:
    /// see [`AbilityOutcome::into_fresh`](super::AbilityOutcome).
    pub(crate) fn into_fresh(self) -> Self {
        match self {
            Self::Tuple(values) => Self::Tuple(fresh_list(values)),
            Self::List(values) => Self::List(fresh_list(values)),
            Self::Record(record) => {
                let mut record = Arc::unwrap_or_clone(record);
                for entry in &mut record.entries {
                    entry.value = std::mem::replace(&mut entry.value, Self::Null).into_fresh();
                }
                Self::Record(Arc::new(record))
            }
            scalar @ (Self::Null
            | Self::Undefined
            | Self::Bool(_)
            | Self::Number(_)
            | Self::String(_)
            | Self::Image(_)
            | Self::Resource(_)
            | Self::Ref(_)
            | Self::Projected(_)) => scalar,
        }
    }
}

fn fresh_list(values: ListValue) -> ListValue {
    values
        .into_vec()
        .into_iter()
        .map(Value::into_fresh)
        .collect()
}

impl PartialEq for Value {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Null, Self::Null) => true,
            (Self::Undefined, Self::Undefined) => true,
            (Self::Bool(left), Self::Bool(right)) => left == right,
            (Self::Number(left), Self::Number(right)) => left == right,
            (Self::String(left), Self::String(right)) => left == right,
            (Self::Image(left), Self::Image(right)) => left == right,
            (Self::Resource(left), Self::Resource(right)) => left == right,
            (Self::Ref(left), Self::Ref(right)) => left == right,
            (Self::Tuple(left), Self::Tuple(right)) => left == right,
            (Self::List(left), Self::List(right)) => left == right,
            (Self::Record(left), Self::Record(right)) => left == right,
            (Self::Projected(left), Self::Projected(right)) => left == right,
            (Self::Projected(left), right) => {
                matches!(left.materialize(), Ok(left) if left == *right)
            }
            (left, Self::Projected(right)) => {
                matches!(right.materialize(), Ok(right) if *left == right)
            }
            _ => false,
        }
    }
}

impl Serialize for Value {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        RuntimeJson(self).serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for Value {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        serde_json::Value::deserialize(deserializer).map(from_json)
    }
}

#[derive(Clone, Default)]
pub struct ProjectedBindings {
    bindings: FxHashMap<Symbol, ProjectedValue>,
    reader: Option<Arc<dyn ProjectionReader>>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProjectedBindingError {
    name: String,
}

impl ProjectedBindingError {
    pub fn duplicate(name: impl Into<String>) -> Self {
        Self { name: name.into() }
    }

    pub fn name(&self) -> &str {
        &self.name
    }
}

impl std::fmt::Display for ProjectedBindingError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "projected binding `{}` is already bound", self.name)
    }
}

impl std::error::Error for ProjectedBindingError {}

impl ProjectedBindings {
    pub fn new() -> Self {
        Self::default()
    }

    #[expect(
        clippy::expect_used,
        reason = "insert is the panicking half of the pair: try_insert is the fallible twin, per the message"
    )]
    pub fn insert(&mut self, name: impl Into<String>, value: ProjectedValue) {
        let name = name.into();
        self.try_insert(name, value)
            .expect("projected binding should not be inserted twice");
    }

    pub fn try_insert(
        &mut self,
        name: impl Into<String>,
        value: ProjectedValue,
    ) -> Result<(), ProjectedBindingError> {
        let name = name.into();
        if self.bindings.contains_key(name.as_str()) {
            return Err(ProjectedBindingError::duplicate(name));
        }
        self.bindings.insert(Symbol::new(&name), value);
        Ok(())
    }

    pub(crate) fn get_symbol(&self, symbol: &Symbol) -> Option<ProjectedValue> {
        self.bindings.get(symbol).cloned()
    }

    pub fn get(&self, name: &str) -> Option<ProjectedValue> {
        self.bindings.get(name).cloned()
    }

    pub fn names(&self) -> impl Iterator<Item = String> + '_ {
        self.bindings
            .keys()
            .map(|symbol| symbol.as_str().to_string())
    }

    /// Answer this execution's projection reads through `reader`: the
    /// worker's wire, or a catalog for an in-process execution. A projection
    /// value holds no reader; the execution that holds it does.
    pub fn with_reader(mut self, reader: Arc<dyn ProjectionReader>) -> Self {
        self.reader = Some(reader);
        self
    }

    /// The reader this execution's projection reads go through.
    pub fn reader(&self) -> Option<Arc<dyn ProjectionReader>> {
        self.reader.clone()
    }
}

#[derive(Clone)]
pub struct ProjectedValue {
    name: Arc<str>,
    kind: ProjectedKind,
}

#[derive(Clone)]
enum ProjectedKind {
    Scalar(Arc<Value>),
    /// The VM's whole hold on a host view: its declared type and its
    /// resource, plain data that snapshots with the heap (ADR 0132 §9).
    Resource {
        type_name: Arc<str>,
        resource: Arc<ResourceRef>,
    },
}

/// A projection's kind, borrowed: see [`ProjectedValue::form`].
pub(crate) enum ProjectedForm<'a> {
    Scalar(&'a Value),
    Resource {
        type_name: &'a str,
        resource: &'a ResourceRef,
    },
}

/// A projection type: the name its provider is registered under.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ProjectionType(Arc<str>);

impl ProjectionType {
    /// Name a projection type.
    pub fn new(name: impl Into<Arc<str>>) -> Self {
        Self(name.into())
    }

    /// The type's name.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// A projection value's resource, as the VM holds it: plain data that
/// snapshots with the heap and pins no node (ADR 0132 §9). Reads go to the
/// provider registered for `projection` on whichever node runs the actor. A
/// provider that must answer identically after failover sets `revision`.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ResourceRef {
    /// The provider's projection type.
    pub projection: ProjectionType,
    /// The resource within the provider.
    pub id: String,
    /// The revision or snapshot id reads are pinned to, if any.
    pub revision: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum ProjectedReadRequest {
    Len,
    Empty,
    Truthy,
    Field(Arc<str>),
    Index(#[serde(with = "super::effect_value")] Value),
    Contains(#[serde(with = "super::effect_value")] Value),
    Find {
        #[serde(with = "super::effect_value")]
        needle: Value,
        start: usize,
    },
    GrepText(#[serde(with = "super::effect_value")] Value),
    Keys,
    Values,
    StartsWith(#[serde(with = "super::effect_value")] Value),
    EndsWith(#[serde(with = "super::effect_value")] Value),
    Split(#[serde(with = "super::effect_value")] Value),
    Join(#[serde(with = "super::effect_value")] Value),
    Trim,
    Slice {
        start: Option<isize>,
        end: Option<isize>,
    },
    Push(#[serde(with = "super::effect_value")] Value),
    ToNumber,
    JsonParse,
    SliceBound,
    RangeBound,
    Render,
    Materialize,
}

impl ProjectedReadRequest {
    /// The request's name, for the error a consumer raises when a descriptor
    /// does not answer it.
    pub(crate) fn label(&self) -> &'static str {
        match self {
            Self::Len => "len",
            Self::Empty => "empty",
            Self::Truthy => "truthy",
            Self::Field(_) => "field",
            Self::Index(_) => "index",
            Self::Contains(_) => "contains",
            Self::Find { .. } => "find",
            Self::GrepText(_) => "grep_text",
            Self::Keys => "keys",
            Self::Values => "values",
            Self::StartsWith(_) => "starts_with",
            Self::EndsWith(_) => "ends_with",
            Self::Split(_) => "split",
            Self::Join(_) => "join",
            Self::Trim => "trim",
            Self::Slice { .. } => "slice",
            Self::Push(_) => "push",
            Self::ToNumber => "to_number",
            Self::JsonParse => "json_parse",
            Self::SliceBound => "slice_bound",
            Self::RangeBound => "range_bound",
            Self::Render => "render",
            Self::Materialize => "materialize",
        }
    }
}

/// What a projection provider answers when it *does* answer.
///
/// "Cannot answer" is not in here: that is `None` from
/// [`super::ProjectionProvider::read`]. Keeping the two apart is the point of
/// FIG-2863 — a single `Missing` used to mean both, and each consumer picked
/// its own widening for it, so an unanswerable `Contains` read as `false` and an
/// unanswerable `Field` read as `null`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum ProjectedReadResponse {
    Value(#[serde(with = "super::effect_value")] Value),
    Text(String),
    Bool(bool),
    Len(usize),
    Keys(Vec<String>),
}

impl ProjectedReadResponse {
    /// The answer as a runtime value.
    ///
    /// A descriptor that has no value for a request says so in the dialect's
    /// own terms by answering `Value(Value::Undefined)`; nothing here invents
    /// `Value::Null` (FIG-2863).
    pub(crate) fn into_value(self) -> Value {
        match self {
            Self::Value(value) => value,
            Self::Bool(value) => Value::Bool(value),
            Self::Len(value) => Value::Number(value as f64),
            Self::Text(value) => Value::String(value.into()),
            Self::Keys(values) => Value::List(
                values
                    .into_iter()
                    .map(|value| Value::String(value.into()))
                    .collect::<Vec<_>>()
                    .into(),
            ),
        }
    }
}

impl From<ProjectedReadResponse> for Value {
    fn from(response: ProjectedReadResponse) -> Self {
        response.into_value()
    }
}

/// The length named by a provider's answer, shared by len and rendering.
fn projection_length_answer(answer: &ProjectedReadResponse) -> Option<usize> {
    match answer {
        ProjectedReadResponse::Len(len) => Some(*len),
        ProjectedReadResponse::Value(Value::Number(len)) => {
            (len.is_finite() && *len >= 0.0).then_some(*len as usize)
        }
        ProjectedReadResponse::Value(value) => Some(value_len(value).unwrap_or(0)),
        ProjectedReadResponse::Text(text) => Some(text.chars().count()),
        ProjectedReadResponse::Keys(keys) => Some(keys.len()),
        ProjectedReadResponse::Bool(_) => None,
    }
}

impl ProjectedValue {
    pub fn scalar(name: impl Into<Arc<str>>, value: Value) -> Self {
        Self {
            name: name.into(),
            kind: ProjectedKind::Scalar(Arc::new(value)),
        }
    }

    /// A projection of `resource`, declared as a `type_name` value: plain
    /// data that reads through the provider registered for
    /// `resource.projection` (ADR 0132 §9).
    pub fn resource(
        name: impl Into<Arc<str>>,
        type_name: impl Into<Arc<str>>,
        resource: ResourceRef,
    ) -> Self {
        Self {
            name: name.into(),
            kind: ProjectedKind::Resource {
                type_name: type_name.into(),
                resource: Arc::new(resource),
            },
        }
    }

    /// One read of `resource` through the reader of the execution that holds
    /// this value; `None` when its provider does not answer `request`.
    fn read_one(
        &self,
        resource: &ResourceRef,
        request: ProjectedReadRequest,
    ) -> Result<Option<ProjectedReadResponse>, RuntimeError> {
        projection_provider::read(resource, request).map_err(|error| self.read_error(error))
    }

    fn read_error(&self, error: ProjectionReadError) -> RuntimeError {
        match error {
            ProjectionReadError::Refused(refusal) => RuntimeError::ProjectionRefused {
                name: self.name.to_string(),
                refusal,
            },
            ProjectionReadError::Failed(source) => RuntimeError::ProjectionReadFailed {
                name: self.name.to_string(),
                source,
            },
        }
    }

    /// The refusal a consumer raises when the provider does not answer a
    /// request it needs an answer to (FIG-2863).
    fn unsupported(&self, request: &ProjectedReadRequest) -> RuntimeError {
        RuntimeError::ProjectedReadUnsupported {
            name: self.name.to_string(),
            type_name: self.value_type_name().to_string(),
            request: request.label().to_string(),
        }
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    /// Host descriptor vocabulary for prompt and linker metadata. Scalar
    /// projections report the underlying runtime value type; resource
    /// projections their declared type name.
    pub fn type_name(&self) -> &str {
        self.value_type_name()
    }

    /// What this projection is, for a durable writer: the value a scalar
    /// projection stands for, or a resource projection's type and resource.
    pub(crate) fn form(&self) -> ProjectedForm<'_> {
        match &self.kind {
            ProjectedKind::Scalar(value) => ProjectedForm::Scalar(value),
            ProjectedKind::Resource {
                type_name,
                resource,
            } => ProjectedForm::Resource {
                type_name,
                resource,
            },
        }
    }

    /// The resource a resource projection reads, which is all of it the VM
    /// holds.
    pub fn resource_ref(&self) -> Option<&ResourceRef> {
        match &self.kind {
            ProjectedKind::Resource { resource, .. } => Some(resource),
            ProjectedKind::Scalar(_) => None,
        }
    }

    /// Whether this projection is absent, for `??`.
    ///
    /// A scalar projection is nullish exactly when the value behind it is —
    /// `Scalar(Null)` is how this design spells an absent projected value, which
    /// is the whole point of FIG-1479.
    ///
    /// A resource projection stands for a host view, so it is present, and it
    /// is deliberately not read to find that out. Reading would invert the
    /// answer: a provider that does not answer `Materialize` would judge its
    /// own view absent and hand `??` the fallback. It would also be the one read
    /// this question must never make, dragging a whole session view across to
    /// decide presence.
    ///
    /// Because nothing is read, `IsNullish` stays completed by the VM's fast path
    /// rather than bailing to the async projected route the way `ToBool` does:
    /// truthiness has a `ProjectedReadRequest::Truthy` to ask, and presence has
    /// no counterpart to ask for.
    ///
    /// Matched on the kind rather than through `scalar_value` so that a new
    /// `ProjectedKind` has to state its own answer here instead of inheriting
    /// "present" from a `None`.
    pub(crate) fn is_nullish(&self) -> bool {
        match &self.kind {
            ProjectedKind::Scalar(value) => {
                matches!(value.as_ref(), Value::Null | Value::Undefined)
            }
            ProjectedKind::Resource { .. } => false,
        }
    }

    /// The value behind a *scalar* projection, which is already in memory and so
    /// costs nothing to read through. Path reads use this to resolve field and
    /// index access with the VM's dialect-aware helpers instead of the
    /// dialect-blind `access.rs` reads `get_field` / `get_index` fall back to —
    /// a scalar projection of a string has a `.length`, and a missing key on a
    /// projected record is `undefined` in the TypeScript dialect, exactly as it
    /// is when the same value is not projected. `None` for a resource
    /// projection, whose reads belong to its provider and stay lazy.
    pub fn scalar_value(&self) -> Option<&Value> {
        match &self.kind {
            ProjectedKind::Scalar(value) => Some(value),
            ProjectedKind::Resource { .. } => None,
        }
    }

    /// The value a member read `parent.field` of a projection yields.
    ///
    /// A member read that yields a scalar is that plain value: the read is
    /// done, so nothing is left to project, and every way out of the VM
    /// (`finish`, a tool argument, a snapshot) carries the value itself
    /// (FIG-5197). A compound member stays a projection named by its path, a
    /// narrower view the next read goes through without importing it into the
    /// heap. A member that is itself a projection passes through, so nothing
    /// double-wraps.
    pub fn propagate_field(parent_name: &str, field: &str, inner: Value) -> Value {
        Self::member(inner, || format!("{parent_name}.{field}"))
    }

    /// The value a member read `parent[index]` of a projection yields, by the
    /// rule of [`Self::propagate_field`].
    pub fn propagate_index(parent_name: &str, index: &Value, inner: Value) -> Value {
        Self::member(inner, || match index {
            Value::String(s) => format!("{parent_name}[{s:?}]"),
            Value::Number(n) => format!("{parent_name}[{n}]"),
            other => format!("{parent_name}[{other}]"),
        })
    }

    fn member(inner: Value, path: impl FnOnce() -> String) -> Value {
        match inner {
            compound @ (Value::Tuple(_) | Value::List(_) | Value::Record(_)) => {
                Value::Projected(ProjectedValue::scalar(Arc::<str>::from(path()), compound))
            }
            other => other,
        }
    }

    pub(crate) fn len(&self) -> Result<usize, RuntimeError> {
        Ok(match &self.kind {
            ProjectedKind::Scalar(value) => value_len(value).unwrap_or(0),
            ProjectedKind::Resource { resource, .. } => self
                .read_one(resource, ProjectedReadRequest::Len)?
                .as_ref()
                .and_then(projection_length_answer)
                .ok_or_else(|| self.unsupported(&ProjectedReadRequest::Len))?,
        })
    }

    pub(crate) fn empty(&self) -> Result<Option<bool>, RuntimeError> {
        Ok(match &self.kind {
            ProjectedKind::Scalar(value) => value_len(value).map(|len| len == 0),
            ProjectedKind::Resource { resource, .. } => {
                match self.read_one(resource, ProjectedReadRequest::Empty)? {
                    Some(ProjectedReadResponse::Bool(value)) => Some(value),
                    Some(ProjectedReadResponse::Value(Value::Bool(value))) => Some(value),
                    Some(ProjectedReadResponse::Value(value)) => Some(value_truthy(&value)?),
                    Some(ProjectedReadResponse::Len(value)) => Some(value == 0),
                    Some(ProjectedReadResponse::Keys(values)) => Some(values.is_empty()),
                    Some(ProjectedReadResponse::Text(value)) => Some(value.is_empty()),
                    None => return Err(self.unsupported(&ProjectedReadRequest::Empty)),
                }
            }
        })
    }

    pub(crate) fn truthy(&self) -> Result<bool, RuntimeError> {
        Ok(match &self.kind {
            ProjectedKind::Scalar(value) => value_truthy(value)?,
            ProjectedKind::Resource { resource, .. } => {
                match self.read_one(resource, ProjectedReadRequest::Truthy)? {
                    Some(ProjectedReadResponse::Bool(value)) => value,
                    Some(ProjectedReadResponse::Value(value)) => value_truthy(&value)?,
                    Some(ProjectedReadResponse::Len(value)) => value != 0,
                    Some(ProjectedReadResponse::Keys(values)) => !values.is_empty(),
                    Some(ProjectedReadResponse::Text(value)) => !value.is_empty(),
                    None => return Err(self.unsupported(&ProjectedReadRequest::Truthy)),
                }
            }
        })
    }

    /// Indexes a projected source.
    ///
    /// `None` means the provider does not answer an index read of this key --
    /// which for a container view is the ordinary "no element there". The
    /// caller, which knows the dialect, substitutes its absent value; this layer
    /// does not invent one, because `null` and `undefined` are different answers
    /// in the two dialects (FIG-2863).
    pub(crate) fn get_index(&self, index: &Value) -> Result<Option<Value>, RuntimeError> {
        let index = materialize_value(index.clone())?;
        match &self.kind {
            ProjectedKind::Scalar(value) => read_index_ref_direct(value, &index).map(Some),
            ProjectedKind::Resource { resource, .. } => Ok(self
                .read_one(resource, ProjectedReadRequest::Index(index))?
                .map(ProjectedReadResponse::into_value)),
        }
    }

    /// `None` carries the same meaning as in [`Self::get_index`].
    pub(crate) fn get_field(&self, field: &Name) -> Result<Option<Value>, RuntimeError> {
        match &self.kind {
            ProjectedKind::Scalar(value) => read_field_ref_direct(value, field).map(Some),
            ProjectedKind::Resource { resource, .. } => {
                if let Some(response) =
                    self.read_one(resource, ProjectedReadRequest::Field(field.text.clone()))?
                {
                    return Ok(Some(response.into_value()));
                }
                // `.length` is the count question spelled as a field. A
                // provider answers counts through `Len` and has no reason to
                // also answer a field named `length`, so without this the lazy
                // route silently produced `undefined` where the materializing
                // route (`view.slice(0).length`) produced the count (FIG-3058).
                if field.text.as_ref() == "length"
                    && let Some(ProjectedReadResponse::Len(len)) =
                        self.read_one(resource, ProjectedReadRequest::Len)?
                {
                    return Ok(Some(Value::Number(len as f64)));
                }
                Ok(None)
            }
        }
    }

    /// The named fields of a resource projection, read in one batch: one
    /// frame for all of them rather than one each.
    fn read_fields(
        &self,
        resource: &ResourceRef,
        keys: Vec<String>,
    ) -> Result<Vec<(String, Value)>, RuntimeError> {
        let requests = keys
            .iter()
            .map(|key| ProjectedReadRequest::Field(Arc::from(key.as_str())))
            .collect();
        let responses = projection_provider::read_range(resource, requests)
            .map_err(|error| self.read_error(error))?;
        Ok(keys
            .into_iter()
            .zip(responses)
            .filter_map(|(key, response)| response.map(|response| (key, response.into_value())))
            .collect())
    }

    pub(crate) fn contains(&self, needle: &Value) -> Result<bool, RuntimeError> {
        match &self.kind {
            ProjectedKind::Scalar(value) => execute_contains_direct(value, needle),
            ProjectedKind::Resource { resource, .. } => {
                let request = ProjectedReadRequest::Contains(needle.clone());
                match self.read_one(resource, request.clone())? {
                    Some(ProjectedReadResponse::Bool(value)) => Ok(value),
                    Some(ProjectedReadResponse::Value(value)) => value_truthy(&value),
                    Some(_) | None => Err(self.unsupported(&request)),
                }
            }
        }
    }

    pub(crate) fn find(&self, needle: Value, start: usize) -> Result<Option<Value>, RuntimeError> {
        self.resource_read_or_missing(ProjectedReadRequest::Find { needle, start })
    }

    pub(crate) fn grep_text(&self, needle: Value) -> Result<Option<Value>, RuntimeError> {
        self.resource_read_or_missing(ProjectedReadRequest::GrepText(needle))
    }

    pub(crate) fn keys(&self) -> Result<Vec<String>, RuntimeError> {
        Ok(match &self.kind {
            ProjectedKind::Scalar(value) => match value.as_ref() {
                Value::Record(record) => record.keys().map(ToString::to_string).collect(),
                _ => Vec::new(),
            },
            ProjectedKind::Resource { resource, .. } => {
                match self.read_one(resource, ProjectedReadRequest::Keys)? {
                    Some(ProjectedReadResponse::Keys(value)) => value,
                    Some(ProjectedReadResponse::Value(Value::List(values))) => values
                        .iter()
                        .filter_map(|value| match value {
                            Value::String(value) => Some(value.to_string()),
                            _ => None,
                        })
                        .collect(),
                    Some(_) | None => {
                        return Err(self.unsupported(&ProjectedReadRequest::Keys));
                    }
                }
            }
        })
    }

    pub(crate) fn values(&self) -> Result<Option<Value>, RuntimeError> {
        Ok(match &self.kind {
            ProjectedKind::Scalar(value) => match value.as_ref() {
                Value::Record(record) => Some(Value::List(
                    record.values().cloned().collect::<Vec<_>>().into(),
                )),
                Value::Null => Some(Value::List(Vec::new().into())),
                _ => None,
            },
            ProjectedKind::Resource { resource, .. } => {
                match self.read_one(resource, ProjectedReadRequest::Values)? {
                    Some(response) => Some(response.into_value()),
                    None => return Err(self.unsupported(&ProjectedReadRequest::Values)),
                }
            }
        })
    }

    pub(crate) fn starts_with(&self, prefix: Value) -> Result<Option<Value>, RuntimeError> {
        self.resource_read_or_missing(ProjectedReadRequest::StartsWith(prefix))
    }

    pub(crate) fn ends_with(&self, suffix: Value) -> Result<Option<Value>, RuntimeError> {
        self.resource_read_or_missing(ProjectedReadRequest::EndsWith(suffix))
    }

    pub(crate) fn split(&self, needle: Value) -> Result<Option<Value>, RuntimeError> {
        self.resource_read_or_missing(ProjectedReadRequest::Split(needle))
    }

    pub(crate) fn join(&self, sep: Value) -> Result<Option<Value>, RuntimeError> {
        self.resource_read_or_missing(ProjectedReadRequest::Join(sep))
    }

    pub(crate) fn trim(&self) -> Result<Option<Value>, RuntimeError> {
        self.resource_read_or_missing(ProjectedReadRequest::Trim)
    }

    pub(crate) fn slice(
        &self,
        start: Option<isize>,
        end: Option<isize>,
    ) -> Result<Option<Value>, RuntimeError> {
        self.resource_read_or_missing(ProjectedReadRequest::Slice { start, end })
    }

    pub(crate) fn push(&self, item: Value) -> Result<Option<Value>, RuntimeError> {
        self.resource_read_or_missing(ProjectedReadRequest::Push(item))
    }

    pub(crate) fn to_number(&self) -> Result<Option<Value>, RuntimeError> {
        self.resource_read_or_missing(ProjectedReadRequest::ToNumber)
    }

    pub(crate) fn json_parse(&self) -> Result<Option<Value>, RuntimeError> {
        self.resource_read_or_missing(ProjectedReadRequest::JsonParse)
    }

    pub(crate) fn slice_bound(&self) -> Result<Option<Value>, RuntimeError> {
        self.resource_read_or_missing(ProjectedReadRequest::SliceBound)
    }

    pub(crate) fn range_bound(&self) -> Result<Option<Value>, RuntimeError> {
        self.resource_read_or_missing(ProjectedReadRequest::RangeBound)
    }

    fn resource_read_or_missing(
        &self,
        request: ProjectedReadRequest,
    ) -> Result<Option<Value>, RuntimeError> {
        Ok(match &self.kind {
            ProjectedKind::Scalar(_) => None,
            // `None` here is "no special implementation", not "cannot answer a
            // read of my data": every caller of these helpers falls back to
            // materializing and computing the true answer generically, so
            // refusing would remove a correct result rather than a fabricated
            // one (FIG-2863).
            ProjectedKind::Resource { resource, .. } => {
                self.read_one(resource, request)?.map(Into::into)
            }
        })
    }

    pub fn render(&self) -> Result<String, RuntimeError> {
        Ok(match &self.kind {
            ProjectedKind::Scalar(value) => stringify_value(value).unwrap_or_default(),
            ProjectedKind::Resource { resource, .. } => {
                match self.read_one(resource, ProjectedReadRequest::Render)? {
                    Some(ProjectedReadResponse::Text(value)) => value,
                    Some(response) => stringify_value(&response.into_value()).unwrap_or_default(),
                    None => return Err(self.unsupported(&ProjectedReadRequest::Render)),
                }
            }
        })
    }

    pub fn materialize(&self) -> Result<Value, RuntimeError> {
        Ok(match &self.kind {
            ProjectedKind::Scalar(value) => (**value).clone(),
            ProjectedKind::Resource { resource, .. } => {
                match self.read_one(resource, ProjectedReadRequest::Materialize)? {
                    Some(response) => response.into_value(),
                    None => return Err(self.unsupported(&ProjectedReadRequest::Materialize)),
                }
            }
        })
    }
}

impl fmt::Debug for ProjectedValue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut debug = f.debug_struct("ProjectedValue");
        debug
            .field("name", &self.name)
            .field("kind", &self.value_type_name());
        if let Some(resource) = self.resource_ref() {
            debug.field("resource", resource);
        }
        debug.finish()
    }
}

impl ProjectedValue {
    /// Whether both are projections of one resource as one declared type: one
    /// view, since a resource is pinned to what it answers, so equal without
    /// a read.
    pub(crate) fn same_resource(&self, other: &Self) -> bool {
        matches!(
            (&self.kind, &other.kind),
            (
                ProjectedKind::Resource {
                    type_name: left_type,
                    resource: left,
                },
                ProjectedKind::Resource {
                    type_name: right_type,
                    resource: right,
                },
            ) if left == right && left_type == right_type
        )
    }
}

impl PartialEq for ProjectedValue {
    fn eq(&self, other: &Self) -> bool {
        if self.same_resource(other) {
            return true;
        }
        match (self.materialize(), other.materialize()) {
            (Ok(left), Ok(right)) => left == right,
            _ => false,
        }
    }
}

impl ProjectedValue {
    pub(crate) fn value_type_name(&self) -> &str {
        match &self.kind {
            ProjectedKind::Scalar(value) => value_type_name(value),
            ProjectedKind::Resource { type_name, .. } => type_name,
        }
    }
}
impl fmt::Display for Value {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Null => write!(f, "null"),
            Self::Undefined => write!(f, "undefined"),
            Self::Bool(value) => write!(f, "{value}"),
            Self::Number(value) => write_number(f, *value),
            Self::String(value) => write!(f, "{value}"),
            Self::Tuple(values) => {
                let mut output = String::new();
                append_tuple_literal_direct(&mut output, values).map_err(|_| fmt::Error)?;
                write!(f, "{output}")
            }
            Self::Image(_)
            | Self::Resource(_)
            | Self::Ref(_)
            | Self::List(_)
            | Self::Record(_)
            | Self::Projected(_) => write!(
                f,
                "{}",
                serde_json::to_string(&RuntimeJson(self)).unwrap_or_default()
            ),
        }
    }
}
