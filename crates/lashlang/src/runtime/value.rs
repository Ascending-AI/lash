//! Value types: the dynamically-typed `Value` enum, its projection wrapper,
//! the `ImageValue` attachment descriptor, and the public projection traits
//! (`ProjectedHostDescriptor`, `ProjectedReadRequest`, `ProjectedFuture`).
//!
//! The `Value` enum is the universal currency of the lashlang runtime: every
//! load, every binary op, every host-tool argument, every JSON round-trip
//! flows through it. `ProjectedValue` wraps host-side bindings the runtime
//! can read but should not own; field/index access on a projected source
//! propagates the wrapper so downstream consumers can tell that this came
//! from a projected binding.

use std::borrow::Borrow;
use std::fmt;
use std::future::Future;
use std::ops::Deref;
use std::pin::Pin;
use std::sync::Arc;

use compact_str::CompactString;
use rustc_hash::FxHashMap;
use serde::ser::SerializeMap;
use serde::{Deserialize, Deserializer, Serialize, Serializer};

use super::record::{Symbol, intern_symbol, symbol_name};
use super::{
    HeapId, Name, Record, RuntimeError, RuntimeJson, append_tuple_literal_direct,
    execute_contains_direct, from_json, is_truthy as value_truthy, materialize_projected_async,
    read_field_ref_direct, read_index_ref_direct, stringify_value_async, value_contains_projected,
    value_len, value_type_name, write_number,
};

/// Marker key that wraps a Type literal at its outermost level so a host-side
/// consumer can tell a Type value apart from a plain record. The inner value
/// is the JSON-Schema representation of the type.
pub const LASH_TYPE_KEY: &str = "$lash_type";
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
        let symbol = intern_symbol(&name);
        if self.bindings.contains_key(&symbol) {
            return Err(ProjectedBindingError::duplicate(name));
        }
        self.bindings.insert(intern_symbol(&name), value);
        Ok(())
    }

    pub(crate) fn get_symbol(&self, symbol: Symbol) -> Option<ProjectedValue> {
        self.bindings.get(&symbol).cloned()
    }

    pub fn get(&self, name: &str) -> Option<ProjectedValue> {
        self.bindings.get(&intern_symbol(name)).cloned()
    }

    pub fn names(&self) -> impl Iterator<Item = String> + '_ {
        self.bindings
            .keys()
            .map(|symbol| symbol_name(*symbol).to_string())
    }
}

#[derive(Clone)]
pub struct ProjectedValue {
    name: Arc<str>,
    kind: ProjectedKind,
    projection_ref: Option<serde_json::Value>,
}

#[derive(Clone)]
enum ProjectedKind {
    Scalar(Arc<Value>),
    Custom(Arc<dyn ProjectedHostDescriptor>),
}

pub type ProjectedFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

#[derive(Clone, Debug, PartialEq)]
pub enum ProjectedReadRequest {
    Len,
    Empty,
    Truthy,
    Field(Arc<str>),
    Index(Value),
    Contains(Value),
    Find {
        needle: Value,
        start: usize,
    },
    GrepText(Value),
    Keys,
    Values,
    StartsWith(Value),
    EndsWith(Value),
    Split(Value),
    Join(Value),
    Trim,
    Slice {
        start: Option<isize>,
        end: Option<isize>,
    },
    Push(Value),
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

/// What a host descriptor answers when it *does* answer.
///
/// "Cannot answer" is not in here: that is `None` from
/// [`ProjectedHostDescriptor::read_one`]. Keeping the two apart is the point of
/// FIG-2863 — a single `Missing` used to mean both, and each consumer picked
/// its own widening for it, so an unanswerable `Contains` read as `false` and an
/// unanswerable `Field` read as `null`.
#[derive(Clone, Debug, PartialEq)]
pub enum ProjectedReadResponse {
    Value(Value),
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

pub trait ProjectedHostDescriptor: Send + Sync {
    fn type_name(&self) -> &str;

    /// Whether this descriptor is the placeholder a durable wire decodes to
    /// before the live binding is re-supplied. Reads on such a projection refuse
    /// with `RuntimeError::ProjectedValueUnavailable` instead of answering
    /// (FIG-2865); host descriptors never override it.
    fn unavailable_after_restore(&self) -> bool {
        false
    }

    /// Answers one read, or `None` when this descriptor does not answer that
    /// request at all.
    ///
    /// There is deliberately no default: a descriptor states what it answers,
    /// so an unanswered request is a decision rather than an omission
    /// (FIG-2863). Consumers that need an answer refuse with
    /// [`RuntimeError::ProjectedReadUnsupported`]; the string and iteration
    /// helpers treat `None` as "no special implementation" and fall back to
    /// materializing.
    fn read_one(
        &self,
        request: ProjectedReadRequest,
    ) -> ProjectedFuture<'_, Option<ProjectedReadResponse>>;
}

impl ProjectedValue {
    pub fn scalar(name: impl Into<Arc<str>>, value: Value) -> Self {
        Self {
            name: name.into(),
            kind: ProjectedKind::Scalar(Arc::new(value)),
            projection_ref: None,
        }
    }

    pub fn custom(name: impl Into<Arc<str>>, value: Arc<dyn ProjectedHostDescriptor>) -> Self {
        Self::custom_inner(name, value, None)
    }

    pub fn custom_with_projection_ref(
        name: impl Into<Arc<str>>,
        value: Arc<dyn ProjectedHostDescriptor>,
        projection_ref: serde_json::Value,
    ) -> Self {
        Self::custom_inner(name, value, Some(projection_ref))
    }

    fn custom_inner(
        name: impl Into<Arc<str>>,
        value: Arc<dyn ProjectedHostDescriptor>,
        projection_ref: Option<serde_json::Value>,
    ) -> Self {
        Self {
            name: name.into(),
            kind: ProjectedKind::Custom(value),
            projection_ref,
        }
    }

    pub(crate) fn unavailable_after_restore_with_projection_ref(
        name: impl Into<Arc<str>>,
        type_name: impl Into<Arc<str>>,
        projection_ref: Option<serde_json::Value>,
    ) -> Self {
        let name = name.into();
        Self {
            name: name.clone(),
            kind: ProjectedKind::Custom(Arc::new(UnavailableProjection {
                type_name: type_name.into(),
            })),
            projection_ref,
        }
    }

    /// Whether this projection is a placeholder decoded from a durable wire
    /// whose host descriptor has not been re-supplied (FIG-2865).
    pub(crate) fn is_unavailable(&self) -> bool {
        match &self.kind {
            ProjectedKind::Scalar(_) => false,
            ProjectedKind::Custom(value) => value.unavailable_after_restore(),
        }
    }

    /// The one answer a placeholder can give any read.
    fn refusal(&self) -> RuntimeError {
        RuntimeError::ProjectedValueUnavailable {
            name: self.name.to_string(),
            type_name: self.value_type_name().to_string(),
        }
    }

    /// The refusal a consumer raises when this descriptor does not answer a
    /// request it needs an answer to (FIG-2863).
    fn unsupported(&self, request: &ProjectedReadRequest) -> RuntimeError {
        RuntimeError::ProjectedReadUnsupported {
            name: self.name.to_string(),
            type_name: self.value_type_name().to_string(),
            request: request.label().to_string(),
        }
    }

    fn refuse_if_unavailable(&self) -> Result<(), RuntimeError> {
        if self.is_unavailable() {
            return Err(self.refusal());
        }
        Ok(())
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    /// Host descriptor vocabulary for prompt and linker metadata. Scalar
    /// projections report the underlying runtime value type; custom
    /// projections forward the descriptor's declared type name.
    pub fn type_name(&self) -> &str {
        self.value_type_name()
    }

    pub fn projection_ref(&self) -> Option<&serde_json::Value> {
        self.projection_ref.as_ref()
    }

    /// The value behind a *scalar* projection, which is already in memory and so
    /// costs nothing to read through. Path reads use this to resolve field and
    /// index access with the VM's dialect-aware helpers instead of the
    /// dialect-blind `access.rs` reads `get_field` / `get_index` fall back to —
    /// a scalar projection of a string has a `.length`, and a missing key on a
    /// projected record is `undefined` in the TypeScript dialect, exactly as it
    /// is when the same value is not projected. `None` for a custom projection,
    /// whose reads belong to the host descriptor and stay lazy.
    /// Whether this projection is absent, for `??`.
    ///
    /// A scalar projection is nullish exactly when the value behind it is —
    /// `Scalar(Null)` is how this design spells an absent projected value, which
    /// is the whole point of FIG-1479.
    ///
    /// A custom projection stands for a live host view, so it is present, and it
    /// is deliberately not read to find that out. Reading would invert the
    /// answer: an unanswered `ProjectedReadRequest` is `Missing`, which
    /// `materialize_async` maps to `Value::Null`, so every descriptor that does
    /// not implement `Materialize` — the documented minimum is `type_name` alone
    /// — would judge its own view absent and hand `??` the fallback. It would
    /// also be the one read this question must never make, dragging a whole
    /// session view across to decide presence.
    ///
    /// Because nothing is read, `IsNullish` stays completed by the VM's fast path
    /// rather than bailing to the async projected route the way `ToBool` does:
    /// truthiness has a `ProjectedReadRequest::Truthy` to await, and presence has
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
            ProjectedKind::Custom(_) => false,
        }
    }

    pub(crate) fn scalar_value(&self) -> Option<&Value> {
        match &self.kind {
            ProjectedKind::Scalar(value) => Some(value),
            ProjectedKind::Custom(_) => None,
        }
    }

    /// `parent.field`).
    /// Pass-through if the inner value is already a `Value::Projected` so we never
    /// double-wrap.
    /// Used by field/index access on projected sources to keep "this came from a projected
    /// source" alive across path expressions; non-path operations (binary ops, builtins,
    /// formatters) auto-strip via their existing materialise-and-evaluate code paths and so
    /// naturally lose the wrapper.
    pub fn propagate_field(parent_name: &str, field: &str, inner: Value) -> Value {
        match inner {
            Value::Projected(_) => inner,
            other => Value::Projected(ProjectedValue::scalar(
                Arc::<str>::from(format!("{parent_name}.{field}")),
                other,
            )),
        }
    }

    pub fn propagate_index(parent_name: &str, index: &Value, inner: Value) -> Value {
        match inner {
            Value::Projected(_) => inner,
            other => {
                let suffix = match index {
                    Value::String(s) => format!("[{s:?}]"),
                    Value::Number(n) => format!("[{n}]"),
                    other => format!("[{other}]"),
                };
                Value::Projected(ProjectedValue::scalar(
                    Arc::<str>::from(format!("{parent_name}{suffix}")),
                    other,
                ))
            }
        }
    }

    pub(crate) async fn len(&self) -> Result<usize, RuntimeError> {
        self.refuse_if_unavailable()?;
        Ok(match &self.kind {
            ProjectedKind::Scalar(value) => value_len(value).unwrap_or(0),
            ProjectedKind::Custom(value) => match value.read_one(ProjectedReadRequest::Len).await {
                Some(ProjectedReadResponse::Len(value)) => value,
                Some(ProjectedReadResponse::Value(value)) => value_len(&value).unwrap_or(0),
                Some(ProjectedReadResponse::Text(value)) => value.chars().count(),
                Some(ProjectedReadResponse::Keys(values)) => values.len(),
                Some(ProjectedReadResponse::Bool(_)) | None => {
                    return Err(self.unsupported(&ProjectedReadRequest::Len));
                }
            },
        })
    }

    pub(crate) async fn empty(&self) -> Result<Option<bool>, RuntimeError> {
        self.refuse_if_unavailable()?;
        Ok(match &self.kind {
            ProjectedKind::Scalar(value) => value_len(value).map(|len| len == 0),
            ProjectedKind::Custom(value) => match value.read_one(ProjectedReadRequest::Empty).await
            {
                Some(ProjectedReadResponse::Bool(value)) => Some(value),
                Some(ProjectedReadResponse::Value(Value::Bool(value))) => Some(value),
                Some(ProjectedReadResponse::Value(value)) => Some(value_truthy(&value)?),
                Some(ProjectedReadResponse::Len(value)) => Some(value == 0),
                Some(ProjectedReadResponse::Keys(values)) => Some(values.is_empty()),
                Some(ProjectedReadResponse::Text(value)) => Some(value.is_empty()),
                None => return Err(self.unsupported(&ProjectedReadRequest::Empty)),
            },
        })
    }

    pub(crate) async fn truthy(&self) -> Result<bool, RuntimeError> {
        self.refuse_if_unavailable()?;
        Ok(match &self.kind {
            ProjectedKind::Scalar(value) => value_truthy(value)?,
            ProjectedKind::Custom(value) => {
                match value.read_one(ProjectedReadRequest::Truthy).await {
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
    /// `None` means the descriptor does not answer an index read of this key --
    /// which for a container view is the ordinary "no element there". The
    /// caller, which knows the dialect, substitutes its absent value; this layer
    /// does not invent one, because `null` and `undefined` are different answers
    /// in the two dialects (FIG-2863).
    pub(crate) async fn get_index(&self, index: &Value) -> Result<Option<Value>, RuntimeError> {
        self.refuse_if_unavailable()?;
        let index = materialize_projected_async(index.clone()).await?;
        match &self.kind {
            ProjectedKind::Scalar(value) => read_index_ref_direct(value, &index).map(Some),
            ProjectedKind::Custom(value) => Ok(value
                .read_one(ProjectedReadRequest::Index(index))
                .await
                .map(ProjectedReadResponse::into_value)),
        }
    }

    /// `None` carries the same meaning as in [`Self::get_index`].
    pub(crate) async fn get_field(&self, field: &Name) -> Result<Option<Value>, RuntimeError> {
        self.refuse_if_unavailable()?;
        match &self.kind {
            ProjectedKind::Scalar(value) => read_field_ref_direct(value, field).map(Some),
            ProjectedKind::Custom(value) => {
                if let Some(response) = value
                    .read_one(ProjectedReadRequest::Field(field.text.clone()))
                    .await
                {
                    return Ok(Some(response.into_value()));
                }
                // `.length` is the count question spelled as a field. A
                // descriptor answers counts through `Len` and has no reason to
                // also answer a field named `length`, so without this the lazy
                // route silently produced `undefined` where the materializing
                // route (`view.slice(0).length`) produced the count (FIG-3058).
                if field.text.as_ref() == "length"
                    && let Some(ProjectedReadResponse::Len(len)) =
                        value.read_one(ProjectedReadRequest::Len).await
                {
                    return Ok(Some(Value::Number(len as f64)));
                }
                Ok(None)
            }
        }
    }

    pub(crate) async fn contains(&self, needle: &Value) -> Result<bool, RuntimeError> {
        self.refuse_if_unavailable()?;
        match &self.kind {
            ProjectedKind::Scalar(value) => execute_contains_direct(value, needle),
            ProjectedKind::Custom(value) => {
                let request = ProjectedReadRequest::Contains(needle.clone());
                match value.read_one(request.clone()).await {
                    Some(ProjectedReadResponse::Bool(value)) => Ok(value),
                    Some(ProjectedReadResponse::Value(value)) => value_truthy(&value),
                    Some(_) | None => Err(self.unsupported(&request)),
                }
            }
        }
    }

    pub(crate) async fn find(
        &self,
        needle: Value,
        start: usize,
    ) -> Result<Option<Value>, RuntimeError> {
        self.custom_read_or_missing(ProjectedReadRequest::Find { needle, start })
            .await
    }

    pub(crate) async fn grep_text(&self, needle: Value) -> Result<Option<Value>, RuntimeError> {
        self.custom_read_or_missing(ProjectedReadRequest::GrepText(needle))
            .await
    }

    pub(crate) async fn keys(&self) -> Result<Vec<String>, RuntimeError> {
        self.refuse_if_unavailable()?;
        Ok(match &self.kind {
            ProjectedKind::Scalar(value) => match value.as_ref() {
                Value::Record(record) => record.keys().map(ToString::to_string).collect(),
                _ => Vec::new(),
            },
            ProjectedKind::Custom(value) => {
                match value.read_one(ProjectedReadRequest::Keys).await {
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

    pub(crate) async fn values(&self) -> Result<Option<Value>, RuntimeError> {
        self.refuse_if_unavailable()?;
        Ok(match &self.kind {
            ProjectedKind::Scalar(value) => match value.as_ref() {
                Value::Record(record) => Some(Value::List(
                    record.values().cloned().collect::<Vec<_>>().into(),
                )),
                Value::Null => Some(Value::List(Vec::new().into())),
                _ => None,
            },
            ProjectedKind::Custom(value) => {
                match value.read_one(ProjectedReadRequest::Values).await {
                    Some(response) => Some(response.into_value()),
                    None => return Err(self.unsupported(&ProjectedReadRequest::Values)),
                }
            }
        })
    }

    pub(crate) async fn starts_with(&self, prefix: Value) -> Result<Option<Value>, RuntimeError> {
        self.custom_read_or_missing(ProjectedReadRequest::StartsWith(prefix))
            .await
    }

    pub(crate) async fn ends_with(&self, suffix: Value) -> Result<Option<Value>, RuntimeError> {
        self.custom_read_or_missing(ProjectedReadRequest::EndsWith(suffix))
            .await
    }

    pub(crate) async fn split(&self, needle: Value) -> Result<Option<Value>, RuntimeError> {
        self.custom_read_or_missing(ProjectedReadRequest::Split(needle))
            .await
    }

    pub(crate) async fn join(&self, sep: Value) -> Result<Option<Value>, RuntimeError> {
        self.custom_read_or_missing(ProjectedReadRequest::Join(sep))
            .await
    }

    pub(crate) async fn trim(&self) -> Result<Option<Value>, RuntimeError> {
        self.custom_read_or_missing(ProjectedReadRequest::Trim)
            .await
    }

    pub(crate) async fn slice(
        &self,
        start: Option<isize>,
        end: Option<isize>,
    ) -> Result<Option<Value>, RuntimeError> {
        self.custom_read_or_missing(ProjectedReadRequest::Slice { start, end })
            .await
    }

    pub(crate) async fn push(&self, item: Value) -> Result<Option<Value>, RuntimeError> {
        self.custom_read_or_missing(ProjectedReadRequest::Push(item))
            .await
    }

    pub(crate) async fn to_number(&self) -> Result<Option<Value>, RuntimeError> {
        self.custom_read_or_missing(ProjectedReadRequest::ToNumber)
            .await
    }

    pub(crate) async fn json_parse(&self) -> Result<Option<Value>, RuntimeError> {
        self.custom_read_or_missing(ProjectedReadRequest::JsonParse)
            .await
    }

    pub(crate) async fn slice_bound(&self) -> Result<Option<Value>, RuntimeError> {
        self.custom_read_or_missing(ProjectedReadRequest::SliceBound)
            .await
    }

    pub(crate) async fn range_bound(&self) -> Result<Option<Value>, RuntimeError> {
        self.custom_read_or_missing(ProjectedReadRequest::RangeBound)
            .await
    }

    async fn custom_read_or_missing(
        &self,
        request: ProjectedReadRequest,
    ) -> Result<Option<Value>, RuntimeError> {
        self.refuse_if_unavailable()?;
        Ok(match &self.kind {
            ProjectedKind::Scalar(_) => None,
            // `None` here is "no special implementation", not "cannot answer a
            // read of my data": every caller of these helpers falls back to
            // materializing and computing the true answer generically, so
            // refusing would remove a correct result rather than a fabricated
            // one (FIG-2863).
            ProjectedKind::Custom(value) => value.read_one(request).await.map(Into::into),
        })
    }

    pub async fn render(&self) -> Result<String, RuntimeError> {
        self.refuse_if_unavailable()?;
        Ok(match &self.kind {
            ProjectedKind::Scalar(value) => stringify_value_async(value).await.unwrap_or_default(),
            ProjectedKind::Custom(value) => {
                match value.read_one(ProjectedReadRequest::Render).await {
                    Some(ProjectedReadResponse::Text(value)) => value,
                    Some(response) => stringify_value_async(&response.into_value())
                        .await
                        .unwrap_or_default(),
                    None => return Err(self.unsupported(&ProjectedReadRequest::Render)),
                }
            }
        })
    }

    pub async fn materialize_async(&self) -> Result<Value, RuntimeError> {
        self.refuse_if_unavailable()?;
        Ok(match &self.kind {
            ProjectedKind::Scalar(value) => (**value).clone(),
            ProjectedKind::Custom(value) => {
                match value.read_one(ProjectedReadRequest::Materialize).await {
                    Some(response) => response.into_value(),
                    None => return Err(self.unsupported(&ProjectedReadRequest::Materialize)),
                }
            }
        })
    }

    pub fn materialize(&self) -> Result<Value, RuntimeError> {
        futures_executor::block_on(self.materialize_async())
    }
}

/// A projection that survived a durable wire but lost its host descriptor.
///
/// It carries only the identity both durable writers encode — the binding name
/// and the declared type name — which is what lets a re-supplied binding refresh
/// it in place. Until that happens it answers nothing: reads refuse with
/// [`RuntimeError::ProjectedValueUnavailable`] so the host's missing view can
/// never be substituted by a diagnostic string standing in as data (FIG-2865).
struct UnavailableProjection {
    type_name: Arc<str>,
}

impl ProjectedHostDescriptor for UnavailableProjection {
    fn type_name(&self) -> &str {
        &self.type_name
    }

    fn unavailable_after_restore(&self) -> bool {
        true
    }

    fn read_one(
        &self,
        _request: ProjectedReadRequest,
    ) -> ProjectedFuture<'_, Option<ProjectedReadResponse>> {
        // Unreachable: `ProjectedValue` refuses before asking a placeholder.
        Box::pin(async { None })
    }
}

impl fmt::Debug for ProjectedValue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ProjectedValue")
            .field("name", &self.name)
            .field("kind", &self.value_type_name())
            .finish()
    }
}

impl PartialEq for ProjectedValue {
    fn eq(&self, other: &Self) -> bool {
        // An unavailable placeholder has no value to compare, so it is equal
        // only to the same placeholder. Materializing it would be an error, and
        // silently treating that error as a value is exactly what FIG-2865
        // removes.
        match (self.is_unavailable(), other.is_unavailable()) {
            (true, true) => {
                self.name == other.name && self.value_type_name() == other.value_type_name()
            }
            (true, false) | (false, true) => false,
            (false, false) => match (self.materialize(), other.materialize()) {
                (Ok(left), Ok(right)) => left == right,
                _ => false,
            },
        }
    }
}

impl ProjectedValue {
    pub(crate) fn value_type_name(&self) -> &str {
        match &self.kind {
            ProjectedKind::Scalar(value) => value_type_name(value),
            ProjectedKind::Custom(value) => value.type_name(),
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

#[cfg(test)]
mod tests {
    use super::*;

    fn projected(name: &str) -> Value {
        Value::Projected(ProjectedValue::scalar(name, Value::String("host".into())))
    }

    #[test]
    fn contains_projected_returns_true_for_direct_projected_values() {
        assert!(projected("input").contains_projected());
    }

    #[test]
    fn contains_projected_returns_true_for_nested_projected_values() {
        let mut record = Record::new();
        record.insert("title".to_string(), Value::String("local".into()));
        record.insert(
            "items".to_string(),
            Value::List(vec![Value::Number(1.0), projected("input.items")].into()),
        );

        assert!(Value::Record(Arc::new(record)).contains_projected());
    }

    #[test]
    fn contains_projected_returns_false_for_ordinary_values() {
        let mut record = Record::new();
        record.insert("ok".to_string(), Value::Bool(true));
        record.insert(
            "items".to_string(),
            Value::List(
                vec![
                    Value::Null,
                    Value::Number(2.0),
                    Value::String("plain".into()),
                ]
                .into(),
            ),
        );

        assert!(!Value::Record(Arc::new(record)).contains_projected());
    }
}
