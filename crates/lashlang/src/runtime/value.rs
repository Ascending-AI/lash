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

use std::fmt;
use std::future::Future;
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
    String(CompactString),
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
            (Self::Projected(left), right) => left.materialize() == *right,
            (left, Self::Projected(right)) => *left == right.materialize(),
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

#[derive(Clone, Debug, PartialEq)]
pub enum ProjectedReadResponse {
    Missing,
    Value(Value),
    Text(String),
    Bool(bool),
    Len(usize),
    Keys(Vec<String>),
}

pub trait ProjectedHostDescriptor: Send + Sync {
    fn type_name(&self) -> &str;

    fn read_one(
        &self,
        _request: ProjectedReadRequest,
    ) -> ProjectedFuture<'_, ProjectedReadResponse> {
        Box::pin(async { ProjectedReadResponse::Missing })
    }

    fn read_many(
        &self,
        requests: Vec<ProjectedReadRequest>,
    ) -> ProjectedFuture<'_, Vec<ProjectedReadResponse>> {
        Box::pin(async move {
            let mut responses = Vec::with_capacity(requests.len());
            for request in requests {
                responses.push(self.read_one(request).await);
            }
            responses
        })
    }
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
            kind: ProjectedKind::Custom(Arc::new(UnavailableProjectedValue::new(name, type_name))),
            projection_ref,
        }
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

    /// Wrap a derived value as a `ProjectedValue` carrying a path-extended name
    /// (e.g. `parent.field`). Pass-through if the inner value is already a
    /// `Value::Projected` so we never double-wrap. Used by field/index access on
    /// projected sources to keep "this came from a projected source" alive
    /// across path expressions; non-path operations (binary ops, builtins,
    /// formatters) auto-strip via their existing materialise-and-evaluate code
    /// paths and so naturally lose the wrapper.
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

    pub(crate) async fn len(&self) -> usize {
        match &self.kind {
            ProjectedKind::Scalar(value) => value_len(value).unwrap_or(0),
            ProjectedKind::Custom(value) => match value.read_one(ProjectedReadRequest::Len).await {
                ProjectedReadResponse::Len(value) => value,
                ProjectedReadResponse::Value(value) => value_len(&value).unwrap_or(0),
                ProjectedReadResponse::Missing
                | ProjectedReadResponse::Text(_)
                | ProjectedReadResponse::Bool(_)
                | ProjectedReadResponse::Keys(_) => 0,
            },
        }
    }

    pub(crate) async fn empty(&self) -> Option<bool> {
        match &self.kind {
            ProjectedKind::Scalar(value) => value_len(value).map(|len| len == 0),
            ProjectedKind::Custom(value) => match value.read_one(ProjectedReadRequest::Empty).await
            {
                ProjectedReadResponse::Bool(value) => Some(value),
                ProjectedReadResponse::Value(Value::Bool(value)) => Some(value),
                ProjectedReadResponse::Value(value) => Some(value_truthy(&value)),
                ProjectedReadResponse::Missing
                | ProjectedReadResponse::Text(_)
                | ProjectedReadResponse::Len(_)
                | ProjectedReadResponse::Keys(_) => None,
            },
        }
    }

    pub(crate) async fn truthy(&self) -> bool {
        match &self.kind {
            ProjectedKind::Scalar(value) => value_truthy(value),
            ProjectedKind::Custom(value) => {
                match value.read_one(ProjectedReadRequest::Truthy).await {
                    ProjectedReadResponse::Bool(value) => value,
                    ProjectedReadResponse::Value(value) => value_truthy(&value),
                    _ => false,
                }
            }
        }
    }

    pub(crate) async fn get_index(&self, index: &Value) -> Result<Value, RuntimeError> {
        let index = materialize_projected_async(index.clone()).await;
        match &self.kind {
            ProjectedKind::Scalar(value) => read_index_ref_direct(value, &index),
            ProjectedKind::Custom(value) => {
                match value.read_one(ProjectedReadRequest::Index(index)).await {
                    ProjectedReadResponse::Value(value) => Ok(value),
                    _ => Ok(Value::Null),
                }
            }
        }
    }

    pub(crate) async fn get_field(&self, field: &Name) -> Result<Value, RuntimeError> {
        match &self.kind {
            ProjectedKind::Scalar(value) => read_field_ref_direct(value, field),
            ProjectedKind::Custom(value) => match value
                .read_one(ProjectedReadRequest::Field(field.text.clone()))
                .await
            {
                ProjectedReadResponse::Value(value) => Ok(value),
                _ => Ok(Value::Null),
            },
        }
    }

    pub(crate) async fn contains(&self, needle: &Value) -> Result<bool, RuntimeError> {
        match &self.kind {
            ProjectedKind::Scalar(value) => execute_contains_direct(value, needle),
            ProjectedKind::Custom(value) => Ok(
                match value
                    .read_one(ProjectedReadRequest::Contains(needle.clone()))
                    .await
                {
                    ProjectedReadResponse::Bool(value) => value,
                    ProjectedReadResponse::Value(value) => value_truthy(&value),
                    _ => false,
                },
            ),
        }
    }

    pub(crate) async fn find(&self, needle: Value, start: usize) -> Option<Value> {
        self.custom_read_or_missing(ProjectedReadRequest::Find { needle, start })
            .await
    }

    pub(crate) async fn grep_text(&self, needle: Value) -> Option<Value> {
        self.custom_read_or_missing(ProjectedReadRequest::GrepText(needle))
            .await
    }

    pub(crate) async fn keys(&self) -> Vec<String> {
        match &self.kind {
            ProjectedKind::Scalar(value) => match value.as_ref() {
                Value::Record(record) => record.keys().map(ToString::to_string).collect(),
                _ => Vec::new(),
            },
            ProjectedKind::Custom(value) => {
                match value.read_one(ProjectedReadRequest::Keys).await {
                    ProjectedReadResponse::Keys(value) => value,
                    ProjectedReadResponse::Value(Value::List(values)) => values
                        .iter()
                        .filter_map(|value| match value {
                            Value::String(value) => Some(value.to_string()),
                            _ => None,
                        })
                        .collect(),
                    _ => Vec::new(),
                }
            }
        }
    }

    pub(crate) async fn values(&self) -> Option<Value> {
        match &self.kind {
            ProjectedKind::Scalar(value) => match value.as_ref() {
                Value::Record(record) => Some(Value::List(
                    record.values().cloned().collect::<Vec<_>>().into(),
                )),
                Value::Null => Some(Value::List(Vec::new().into())),
                _ => None,
            },
            ProjectedKind::Custom(_) => {
                self.custom_read_or_missing(ProjectedReadRequest::Values)
                    .await
            }
        }
    }

    pub(crate) async fn starts_with(&self, prefix: Value) -> Option<Value> {
        self.custom_read_or_missing(ProjectedReadRequest::StartsWith(prefix))
            .await
    }

    pub(crate) async fn ends_with(&self, suffix: Value) -> Option<Value> {
        self.custom_read_or_missing(ProjectedReadRequest::EndsWith(suffix))
            .await
    }

    pub(crate) async fn split(&self, needle: Value) -> Option<Value> {
        self.custom_read_or_missing(ProjectedReadRequest::Split(needle))
            .await
    }

    pub(crate) async fn join(&self, sep: Value) -> Option<Value> {
        self.custom_read_or_missing(ProjectedReadRequest::Join(sep))
            .await
    }

    pub(crate) async fn trim(&self) -> Option<Value> {
        self.custom_read_or_missing(ProjectedReadRequest::Trim)
            .await
    }

    pub(crate) async fn slice(&self, start: Option<isize>, end: Option<isize>) -> Option<Value> {
        self.custom_read_or_missing(ProjectedReadRequest::Slice { start, end })
            .await
    }

    pub(crate) async fn push(&self, item: Value) -> Option<Value> {
        self.custom_read_or_missing(ProjectedReadRequest::Push(item))
            .await
    }

    pub(crate) async fn to_number(&self) -> Option<Value> {
        self.custom_read_or_missing(ProjectedReadRequest::ToNumber)
            .await
    }

    pub(crate) async fn json_parse(&self) -> Option<Value> {
        self.custom_read_or_missing(ProjectedReadRequest::JsonParse)
            .await
    }

    pub(crate) async fn slice_bound(&self) -> Option<Value> {
        self.custom_read_or_missing(ProjectedReadRequest::SliceBound)
            .await
    }

    pub(crate) async fn range_bound(&self) -> Option<Value> {
        self.custom_read_or_missing(ProjectedReadRequest::RangeBound)
            .await
    }

    async fn custom_read_or_missing(&self, request: ProjectedReadRequest) -> Option<Value> {
        match &self.kind {
            ProjectedKind::Scalar(_) => None,
            ProjectedKind::Custom(value) => match value.read_one(request).await {
                ProjectedReadResponse::Value(value) => Some(value),
                ProjectedReadResponse::Bool(value) => Some(Value::Bool(value)),
                ProjectedReadResponse::Len(value) => Some(Value::Number(value as f64)),
                ProjectedReadResponse::Text(value) => Some(Value::String(value.into())),
                ProjectedReadResponse::Keys(values) => Some(Value::List(
                    values
                        .into_iter()
                        .map(|value| Value::String(value.into()))
                        .collect::<Vec<_>>()
                        .into(),
                )),
                ProjectedReadResponse::Missing => None,
            },
        }
    }

    pub async fn render(&self) -> String {
        match &self.kind {
            ProjectedKind::Scalar(value) => stringify_value_async(value).await.unwrap_or_default(),
            ProjectedKind::Custom(value) => {
                match value.read_one(ProjectedReadRequest::Render).await {
                    ProjectedReadResponse::Text(value) => value,
                    ProjectedReadResponse::Value(value) => {
                        stringify_value_async(&value).await.unwrap_or_default()
                    }
                    _ => String::new(),
                }
            }
        }
    }

    pub async fn materialize_async(&self) -> Value {
        match &self.kind {
            ProjectedKind::Scalar(value) => (**value).clone(),
            ProjectedKind::Custom(value) => {
                match value.read_one(ProjectedReadRequest::Materialize).await {
                    ProjectedReadResponse::Value(value) => value,
                    ProjectedReadResponse::Text(value) => Value::String(value.into()),
                    ProjectedReadResponse::Bool(value) => Value::Bool(value),
                    ProjectedReadResponse::Len(value) => Value::Number(value as f64),
                    ProjectedReadResponse::Keys(values) => Value::List(
                        values
                            .into_iter()
                            .map(|value| Value::String(value.into()))
                            .collect::<Vec<_>>()
                            .into(),
                    ),
                    ProjectedReadResponse::Missing => Value::Null,
                }
            }
        }
    }

    pub fn materialize(&self) -> Value {
        futures_executor::block_on(self.materialize_async())
    }
}

struct UnavailableProjectedValue {
    name: Arc<str>,
    type_name: Arc<str>,
}

impl UnavailableProjectedValue {
    fn new(name: Arc<str>, type_name: impl Into<Arc<str>>) -> Self {
        Self {
            name,
            type_name: type_name.into(),
        }
    }

    fn message(&self) -> String {
        format!(
            "projected host descriptor `{}` ({}) is unavailable after snapshot restore; rerun the producing tool to recreate it",
            self.name, self.type_name
        )
    }
}

impl ProjectedHostDescriptor for UnavailableProjectedValue {
    fn type_name(&self) -> &str {
        &self.type_name
    }

    fn read_one(
        &self,
        request: ProjectedReadRequest,
    ) -> ProjectedFuture<'_, ProjectedReadResponse> {
        Box::pin(async move {
            match request {
                ProjectedReadRequest::Render => ProjectedReadResponse::Text(self.message()),
                ProjectedReadRequest::Materialize => {
                    ProjectedReadResponse::Value(Value::String(self.message().into()))
                }
                _ => ProjectedReadResponse::Missing,
            }
        })
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
        self.materialize() == other.materialize()
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
