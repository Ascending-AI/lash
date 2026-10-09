use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, OnceLock};

use crate::sync::MutexExt;
use crate::{
    JsonSchema, SchemaContract, SchemaDialect, SchemaProjectionOverride, SchemaPurpose,
    ToolCatalogBuildError, ToolDeclaration,
};

/// The contract for executing one logical call, pinned before its first attempt.
#[derive(
    Clone,
    Copy,
    Debug,
    Default,
    PartialEq,
    Eq,
    serde::Serialize,
    serde::Deserialize,
    schemars::JsonSchema,
)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum ExecutionPolicy {
    /// One application attempt, even after a reported pre-effect failure.
    #[default]
    Once,
    /// Repeated execution is part of the tool's declared contract.
    Repeatable { retry: BoundedRetry },
}

#[derive(
    Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize, schemars::JsonSchema,
)]
#[serde(deny_unknown_fields)]
pub struct BoundedRetry {
    /// Counts admitted application ordinals, including the first attempt.
    pub max_attempts: std::num::NonZeroU32,
    pub backoff: Backoff,
}

#[derive(
    Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize, schemars::JsonSchema,
)]
#[serde(deny_unknown_fields)]
pub struct Backoff {
    pub base_delay_ms: u64,
    pub max_delay_ms: u64,
}

#[derive(
    Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize, schemars::JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum LimitCause {
    ExecutionSlice,
    ExecutionTotal,
    WaitDeadline,
}

impl ExecutionPolicy {
    pub fn repeatable(
        max_attempts: std::num::NonZeroU32,
        base_delay_ms: u64,
        max_delay_ms: u64,
    ) -> Self {
        Self::Repeatable {
            retry: BoundedRetry {
                max_attempts,
                backoff: Backoff {
                    base_delay_ms,
                    max_delay_ms,
                },
            },
        }
    }

    pub fn max_attempts(self) -> u32 {
        match self {
            Self::Once => 1,
            Self::Repeatable { retry } => retry.max_attempts.get(),
        }
    }

    /// A current declaration may veto a repeat; it cannot upgrade admission.
    pub fn permits_repeat(self, current: Self, failed_ordinal: u32) -> bool {
        matches!((self, current), (Self::Repeatable { retry }, Self::Repeatable { .. }) if failed_ordinal < retry.max_attempts.get())
    }

    pub fn delay_ms_for_retry(self, retry_index: u32, suggested_delay_ms: Option<u64>) -> u64 {
        match self {
            Self::Once => 0,
            Self::Repeatable { retry } => {
                let multiplier = 1_u64.checked_shl(retry_index).unwrap_or(u64::MAX);
                suggested_delay_ms
                    .unwrap_or_else(|| retry.backoff.base_delay_ms.saturating_mul(multiplier))
                    .min(retry.backoff.max_delay_ms)
            }
        }
    }
}

/// How long a deferring tool's call may stay parked, waiting for its
/// completion: its host's choice, declared beside its
/// [`ToolManifest::execution`] and never inside the closed
/// [`ToolDeclaration`]. The deadline it sets is computed once, when the call
/// is admitted, and pinned with the call's run: a crash or takeover never
/// refreshes it.
#[derive(
    Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize, schemars::JsonSchema,
)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum ParkBound {
    /// The park ends `TimedOut` this long after the call's admission.
    Within(std::time::Duration),
    /// The park has no deadline: it lasts until it resolves or the scope
    /// that owns the call (its turn or its process) ends, which revokes it.
    UntilScopeEnd,
}

/// The bound a tool's manifest must declare, named by
/// [`RegistrationRefused::MissingBound`].
#[derive(
    Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize, schemars::JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum ToolBound {
    /// [`ToolManifest::execution`], which every tool declares.
    Execution,
    /// [`ToolManifest::park`], which every tool that may defer declares.
    Park,
}

impl std::fmt::Display for ToolBound {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::Execution => "execution",
            Self::Park => "park",
        })
    }
}

/// A tool's bounds, as its manifest holds them ([`ToolManifest::bounds`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ToolBounds {
    /// How long one run of the tool's body may take.
    pub execution: std::time::Duration,
    /// How long a call may stay parked; `None` for a tool that never defers.
    pub park: Option<ParkBound>,
}

/// Why registration refused a tool: why its manifest cannot be built, or why
/// a catalog does not take it.
#[derive(
    Clone,
    Debug,
    PartialEq,
    Eq,
    thiserror::Error,
    serde::Serialize,
    serde::Deserialize,
    schemars::JsonSchema,
)]
#[serde(tag = "refusal", rename_all = "snake_case", deny_unknown_fields)]
#[non_exhaustive]
pub enum RegistrationRefused {
    /// The tool's manifest does not declare a bound its host must set: its
    /// `execution`, or the `park` of a tool that may defer.
    #[error("tool `{tool}` declares no {bound} bound; its host must set one")]
    MissingBound { tool: String, bound: ToolBound },
    /// A tool that never defers declares a `park` bound it can never use.
    #[error("tool `{tool}` declares a park bound, but it does not declare `may_defer`")]
    ParkWithoutDeferral { tool: String },
    /// The tool's declaration is itself invalid.
    #[error("tool `{tool}`'s declaration is refused: {cause}")]
    Declaration {
        tool: String,
        cause: crate::DeclarationRefusal,
    },
    /// The tool is declared isolated and names no process engine to run in.
    #[error("tool `{tool}` is declared isolated, but it names no process engine")]
    IsolatedWithoutEngine { tool: String },
    /// A tool that is not isolated names a process engine it never runs in.
    #[error("tool `{tool}` names a process engine, but it does not declare `isolated`")]
    EngineWithoutIsolation { tool: String },
    /// The tool is isolated in a process engine its deployment does not
    /// register: an isolated call never falls back to an inline body.
    #[error("tool `{tool}` is isolated in process engine `{engine}`, which is not registered")]
    UnregisteredIsolationEngine { tool: String, engine: String },
}

fn default_tool_execution_policy() -> ExecutionPolicy {
    ExecutionPolicy::default()
}

fn is_default_tool_execution_policy(policy: &ExecutionPolicy) -> bool {
    *policy == ExecutionPolicy::default()
}

#[derive(
    Clone, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize, schemars::JsonSchema,
)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ToolOutputContract {
    #[default]
    Static,
    FromInputSchema {
        input_field: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        default_schema: Option<JsonSchema>,
    },
}

impl ToolOutputContract {
    pub fn from_input_schema(
        input_field: impl Into<String>,
        default_schema: Option<JsonSchema>,
    ) -> Self {
        Self::FromInputSchema {
            input_field: input_field.into(),
            default_schema,
        }
    }

    pub fn is_static(&self) -> bool {
        matches!(self, Self::Static)
    }

    fn return_type_label(&self, output: &SchemaShape) -> String {
        match self {
            Self::Static => compact_type(output),
            Self::FromInputSchema { .. } => "T".to_string(),
        }
    }

    fn type_parameter_suffix(&self) -> Option<String> {
        match self {
            Self::Static => None,
            Self::FromInputSchema { default_schema, .. } => {
                let default = default_schema
                    .as_ref()
                    .map(|schema| compact_type(&SchemaShape::from_json_schema(schema.as_value())))
                    .unwrap_or_else(|| "any".to_string());
                Some(format!("<T = {default}>"))
            }
        }
    }

    /// The input shape as the compact contract shows it: the field that
    /// carries the output's type is a `TypeSpec<T>` witness, not a record.
    fn witnessed_input(&self, input: &SchemaShape) -> SchemaShape {
        let mut input = input.clone();
        if let Self::FromInputSchema { input_field, .. } = self
            && let ShapeKind::Object(object) = &mut input.kind
            && let Some(field) = object
                .fields
                .iter_mut()
                .find(|field| field.name == *input_field)
        {
            field.shape = SchemaShape {
                kind: ShapeKind::Named("TypeSpec<T>".to_string()),
                description: field.shape.description.take(),
                default: None,
                constraints: ShapeConstraints::default(),
            };
        }
        input
    }

    fn return_fields(&self, output: &SchemaShape) -> Vec<serde_json::Value> {
        match self {
            Self::Static => output
                .rows()
                .iter()
                .map(|row| compact_row(row, "path"))
                .collect(),
            Self::FromInputSchema { .. } => Vec::new(),
        }
    }
}

#[derive(
    Clone, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize, schemars::JsonSchema,
)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ToolArgumentProjectionPolicy {
    #[default]
    MaterializeProjectedValues,
    PreserveProjectedRefsInField {
        field: String,
    },
}

impl ToolArgumentProjectionPolicy {
    pub fn preserve_projected_refs_in_field(field: impl Into<String>) -> Self {
        Self::PreserveProjectedRefsInField {
            field: field.into(),
        }
    }

    pub fn is_materialize_projected_values(&self) -> bool {
        matches!(self, Self::MaterializeProjectedValues)
    }
}

fn is_default_tool_argument_projection_policy(policy: &ToolArgumentProjectionPolicy) -> bool {
    policy.is_materialize_projected_values()
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize)]
#[serde(transparent)]
pub struct ToolId(String);

/// The wire shape is the non-empty string; `ToolId::new`'s emptiness refusal
/// cannot be expressed as a schema assertion.
impl schemars::JsonSchema for ToolId {
    fn inline_schema() -> bool {
        true
    }

    fn schema_name() -> std::borrow::Cow<'static, str> {
        <String as schemars::JsonSchema>::schema_name()
    }

    fn json_schema(generator: &mut schemars::SchemaGenerator) -> schemars::Schema {
        <String as schemars::JsonSchema>::json_schema(generator)
    }
}

impl ToolId {
    pub fn new(id: impl Into<String>) -> Self {
        let id = id.into();
        assert!(!id.trim().is_empty(), "tool id must not be empty");
        Self(id)
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl<'de> serde::Deserialize<'de> for ToolId {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let id = <String as serde::Deserialize>::deserialize(deserializer)?;
        if id.trim().is_empty() {
            return Err(serde::de::Error::custom("tool id must not be empty"));
        }
        Ok(Self(id))
    }
}

impl From<String> for ToolId {
    fn from(id: String) -> Self {
        Self::new(id)
    }
}

impl From<&str> for ToolId {
    fn from(id: &str) -> Self {
        Self::new(id)
    }
}

impl std::fmt::Display for ToolId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// Host-owned discovery operation used to find tools omitted from the prompt.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ToolDiscovery {
    pub operation: String,
}

fn inline_default() -> bool {
    true
}
fn is_inline(value: &bool) -> bool {
    *value
}

/// The module a tool belongs to: a grouping name the catalog and hosts
/// present its tools under. Guidance about a module is a prompt section its
/// plugin registers, never manifest metadata.
#[derive(
    Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize, schemars::JsonSchema,
)]
#[serde(deny_unknown_fields)]
pub struct ToolModule {
    pub name: String,
}

/// Tool metadata exposed to prompts, catalogs, and UI. Catalog membership —
/// being present in a [`ToolProvider`]'s manifest list — is the execution gate;
/// there is no per-manifest tier. The optional compact contract is the
/// catalog-facing projection of the resolved contract; full schemas stay in
/// [`ToolContract`].
///
/// A manifest is complete by construction: it holds its host-set bounds and a
/// valid declaration, and one built or decoded without them is refused
/// ([`RegistrationRefused`]). Nothing that reads a manifest checks them again.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, schemars::JsonSchema)]
pub struct ToolManifest {
    #[serde(default = "inline_default", skip_serializing_if = "is_inline")]
    pub inline: bool,
    pub id: ToolId,
    pub name: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub description: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub module: Option<Arc<ToolModule>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub compact_contract: Option<Arc<CompactToolContract>>,
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub bindings: std::collections::BTreeMap<String, serde_json::Value>,
    #[serde(
        default,
        skip_serializing_if = "is_default_tool_argument_projection_policy"
    )]
    pub argument_projection: ToolArgumentProjectionPolicy,
    #[serde(
        default = "default_tool_execution_policy",
        skip_serializing_if = "is_default_tool_execution_policy"
    )]
    pub execution_policy: ExecutionPolicy,
    /// How long one run of the tool's body may take, set by its host.
    execution: std::time::Duration,
    /// How long a call may stay parked waiting for its completion, set by
    /// its host. Held exactly by a tool whose declaration may defer.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    park: Option<ParkBound>,
    /// The author's three-capability declaration. Admission records it with
    /// this manifest; dispatch, recovery and replay read the recorded answer,
    /// never the live provider.
    #[serde(default, skip_serializing_if = "ToolDeclaration::is_default")]
    declaration: ToolDeclaration,
    /// [`ProcessEngine::kind`] of the engine an isolated tool's calls run in.
    /// Held exactly by a tool whose declaration is isolated.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    isolation_engine: Option<String>,
}

/// A manifest as it is stored, before its bounds and declaration are checked.
#[derive(serde::Deserialize)]
struct ManifestRecord {
    #[serde(default = "inline_default")]
    inline: bool,
    id: ToolId,
    name: String,
    #[serde(default)]
    description: String,
    #[serde(default)]
    module: Option<Arc<ToolModule>>,
    #[serde(default)]
    compact_contract: Option<Arc<CompactToolContract>>,
    #[serde(default)]
    bindings: std::collections::BTreeMap<String, serde_json::Value>,
    #[serde(default)]
    argument_projection: ToolArgumentProjectionPolicy,
    #[serde(default = "default_tool_execution_policy")]
    execution_policy: ExecutionPolicy,
    #[serde(default)]
    execution: Option<std::time::Duration>,
    #[serde(default)]
    park: Option<ParkBound>,
    #[serde(default)]
    declaration: ToolDeclaration,
    #[serde(default)]
    isolation_engine: Option<String>,
}

impl<'de> serde::Deserialize<'de> for ToolManifest {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let record = ManifestRecord::deserialize(deserializer)?;
        let missing_execution = || RegistrationRefused::MissingBound {
            tool: record.name.clone(),
            bound: ToolBound::Execution,
        };
        let execution = record
            .execution
            .ok_or_else(missing_execution)
            .map_err(serde::de::Error::custom)?;
        Self {
            inline: record.inline,
            id: record.id,
            name: record.name,
            description: record.description,
            module: record.module,
            compact_contract: record.compact_contract,
            bindings: record.bindings,
            argument_projection: record.argument_projection,
            execution_policy: record.execution_policy,
            execution,
            park: None,
            declaration: ToolDeclaration::default(),
            isolation_engine: None,
        }
        .declared(record.declaration, record.park, record.isolation_engine)
        .map_err(serde::de::Error::custom)
    }
}

impl ToolManifest {
    /// How long one run of the tool's body may take.
    #[must_use]
    pub fn execution(&self) -> std::time::Duration {
        self.execution
    }

    /// How long a call may stay parked; `None` for a tool that never defers.
    #[must_use]
    pub fn park(&self) -> Option<ParkBound> {
        self.park
    }

    /// The tool's declaration.
    #[must_use]
    pub fn declaration(&self) -> &ToolDeclaration {
        &self.declaration
    }

    /// The process engine an isolated tool's calls run in; `None` for a tool
    /// that is not isolated.
    #[must_use]
    pub fn isolation_engine(&self) -> Option<&str> {
        self.isolation_engine.as_deref()
    }

    /// The bounds this manifest holds.
    #[must_use]
    pub fn bounds(&self) -> ToolBounds {
        ToolBounds {
            execution: self.execution,
            park: self.park,
        }
    }

    /// This manifest with another execution bound.
    #[must_use]
    pub fn with_execution(mut self, execution: std::time::Duration) -> Self {
        self.execution = execution;
        self
    }

    /// This manifest declaring `declaration`, with what its host sets beside
    /// it: the `park` bound of a tool that may defer, and the process
    /// `isolation_engine` of an isolated one.
    ///
    /// # Errors
    ///
    /// An invalid declaration; a deferring tool without a park bound, or a
    /// park bound on a tool that never defers; an isolated tool without an
    /// engine, or an engine on a tool that is not isolated.
    pub fn declared(
        mut self,
        declaration: ToolDeclaration,
        park: Option<ParkBound>,
        isolation_engine: Option<String>,
    ) -> Result<Self, RegistrationRefused> {
        let tool = || self.name.clone();
        declaration
            .validate()
            .map_err(|cause| RegistrationRefused::Declaration {
                tool: tool(),
                cause,
            })?;
        match (declaration.may_defer, park) {
            (true, None) => {
                return Err(RegistrationRefused::MissingBound {
                    tool: tool(),
                    bound: ToolBound::Park,
                });
            }
            (false, Some(_)) => {
                return Err(RegistrationRefused::ParkWithoutDeferral { tool: tool() });
            }
            (true, Some(_)) | (false, None) => {}
        }
        match (declaration.isolated, &isolation_engine) {
            (true, None) => {
                return Err(RegistrationRefused::IsolatedWithoutEngine { tool: tool() });
            }
            (false, Some(_)) => {
                return Err(RegistrationRefused::EngineWithoutIsolation { tool: tool() });
            }
            (true, Some(_)) | (false, None) => {}
        }
        self.declaration = declaration;
        self.park = park;
        self.isolation_engine = isolation_engine;
        Ok(self)
    }
}

/// Heavy tool contract resolved only when a prompt or call needs schemas/docs.
#[derive(
    Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize, schemars::JsonSchema,
)]
pub struct ToolContract {
    #[serde(skip)]
    identity: Option<ToolContractIdentity>,
    #[serde(skip)]
    compact_cache: CompactContractCache,
    #[serde(skip)]
    shapes: ShapeCache,
    #[serde(default = "ToolContract::default_input_schema_contract")]
    pub input_schema: SchemaContract,
    #[serde(default)]
    pub output_schema: SchemaContract,
    #[serde(default, skip_serializing_if = "ToolOutputContract::is_static")]
    pub output_contract: ToolOutputContract,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub examples: Vec<String>,
}

/// Memoized [`CompactToolContract`] projections, keyed by the inputs that
/// actually shape the compact output (the signature name, the example limit,
/// and the manifest description). Schema `$ref` resolution behind the compact
/// contract deep-copies the schema tree; without this memo that copy ran once
/// per doc render per tool.
///
/// Cloning or comparing a contract never carries or observes the memo, so the
/// cache cannot leak into wire/persisted state or equality semantics.
#[derive(Debug, Default)]
struct CompactContractCache(Mutex<BTreeMap<CompactContractKey, Arc<CompactToolContract>>>);

impl Clone for CompactContractCache {
    fn clone(&self) -> Self {
        Self::default()
    }
}

impl PartialEq for CompactContractCache {
    fn eq(&self, _other: &Self) -> bool {
        true
    }
}

impl Eq for CompactContractCache {}

/// The contract's schemas read once as [`SchemaShape`]s.
///
/// Like [`CompactContractCache`], the memo is invisible to cloning, equality
/// and the wire.
#[derive(Debug, Default)]
struct ShapeCache {
    input: OnceLock<Arc<SchemaShape>>,
    output: OnceLock<Arc<SchemaShape>>,
}

impl Clone for ShapeCache {
    fn clone(&self) -> Self {
        Self::default()
    }
}

impl PartialEq for ShapeCache {
    fn eq(&self, _other: &Self) -> bool {
        true
    }
}

impl Eq for ShapeCache {}

#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
struct CompactContractKey {
    signature_name: String,
    presentation: ToolPresentationConfig,
    description: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct ToolContractIdentity {
    id: ToolId,
    name: String,
}

impl Default for ToolContract {
    fn default() -> Self {
        Self {
            identity: None,
            compact_cache: CompactContractCache::default(),
            shapes: ShapeCache::default(),
            input_schema: Self::default_input_schema_contract(),
            output_schema: JsonSchema::any().into(),
            output_contract: ToolOutputContract::Static,
            examples: Vec::new(),
        }
    }
}

impl ToolContract {
    #[expect(
        clippy::expect_used,
        reason = "the default input schema is a fixed object schema"
    )]
    fn default_input_schema_contract() -> SchemaContract {
        SchemaContract::admit(Self::default_input_schema()).expect("valid default input schema")
    }

    pub fn default_input_schema() -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {},
            "additionalProperties": true
        })
    }

    /// Whether this contract was resolved from the exact manifest identity.
    ///
    /// Contracts produced through [`ToolDefinition::contract`] carry this
    /// process-local identity. Standalone/deserialized contracts do not and
    /// must use a provider's authoritative by-id resolution path instead.
    pub fn matches_manifest_identity(&self, manifest: &ToolManifest) -> bool {
        self.identity
            .as_ref()
            .is_some_and(|identity| identity.id == manifest.id && identity.name == manifest.name)
    }

    /// The prompt-facing shape of the canonical input schema. Every surface
    /// that shows a model what this tool accepts spells this shape.
    pub fn input_shape(&self) -> Arc<SchemaShape> {
        Arc::clone(
            self.shapes.input.get_or_init(|| {
                Arc::new(SchemaShape::from_json_schema(self.input_schema.canonical()))
            }),
        )
    }

    /// The prompt-facing shape of the canonical output schema.
    pub fn output_shape(&self) -> Arc<SchemaShape> {
        Arc::clone(self.shapes.output.get_or_init(|| {
            Arc::new(SchemaShape::from_json_schema(
                self.output_schema.canonical(),
            ))
        }))
    }

    /// The input schema under the host's prompt depth bound.
    pub fn input_shape_with(&self, config: &ToolPresentationConfig) -> Arc<SchemaShape> {
        if config.schema_depth == ToolPresentationConfig::standard().schema_depth {
            return self.input_shape();
        }
        Arc::new(SchemaShape::from_json_schema_with_depth(
            self.input_schema.canonical(),
            config.schema_depth,
        ))
    }
    /// The output schema under the host's prompt depth bound.
    pub fn output_shape_with(&self, config: &ToolPresentationConfig) -> Arc<SchemaShape> {
        if config.schema_depth == ToolPresentationConfig::standard().schema_depth {
            return self.output_shape();
        }
        Arc::new(SchemaShape::from_json_schema_with_depth(
            self.output_schema.canonical(),
            config.schema_depth,
        ))
    }
    /// Authored examples under the host's prompt cuts.
    pub fn compact_examples_with(&self, config: &ToolPresentationConfig) -> Vec<String> {
        schema_docs::compact_examples_with_chars(
            &self.examples,
            config.example_limit,
            config.example_chars,
        )
    }

    /// The authored examples a prompt shows: the first few, each bounded.
    pub fn compact_examples(&self) -> Vec<String> {
        compact_examples(&self.examples, COMPACT_TOOL_EXAMPLE_LIMIT)
    }

    pub fn compact_contract(&self, manifest: &ToolManifest) -> CompactToolContract {
        (*self.compact_contract_shared(manifest)).clone()
    }

    pub fn compact_contract_with_signature_name(
        &self,
        manifest: &ToolManifest,
        signature_name: &str,
    ) -> CompactToolContract {
        (*self.compact_contract_shared_with_signature_name(manifest, signature_name)).clone()
    }

    /// Shared standard projection, avoiding deep schema copies for read-only consumers.
    pub fn compact_contract_shared(&self, manifest: &ToolManifest) -> Arc<CompactToolContract> {
        self.compact_contract_shared_with_signature_name(manifest, &manifest.name)
    }

    /// Shared standard projection with a host-selected signature name.
    pub fn compact_contract_shared_with_signature_name(
        &self,
        manifest: &ToolManifest,
        signature_name: &str,
    ) -> Arc<CompactToolContract> {
        self.compact_contract_with_presentation(
            manifest,
            signature_name,
            &ToolPresentationConfig::standard(),
        )
    }

    /// A memoized compact contract under the host's complete presentation policy.
    pub fn compact_contract_with_presentation(
        &self,
        manifest: &ToolManifest,
        signature_name: &str,
        config: &ToolPresentationConfig,
    ) -> Arc<CompactToolContract> {
        if signature_name == manifest.name
            && *config == ToolPresentationConfig::standard()
            && let Some(stored) = &manifest.compact_contract
            && stored.name == signature_name
            && stored.description == manifest.description.trim()
        {
            return Arc::clone(stored);
        }
        let key = CompactContractKey {
            signature_name: signature_name.to_string(),
            presentation: *config,
            description: manifest.description.trim().to_string(),
        };
        if let Some(hit) = self.compact_cache.0.lock_recover().get(&key) {
            return Arc::clone(hit);
        }
        let input = self.input_shape_with(config);
        let output = self.output_shape_with(config);
        let witnessed_input = self.output_contract.witnessed_input(&input);
        let computed = Arc::new(CompactToolContract {
            name: signature_name.to_string(),
            signature: format!(
                "{}{}({})",
                signature_name,
                self.output_contract
                    .type_parameter_suffix()
                    .unwrap_or_default(),
                compact_arguments(&witnessed_input)
            ),
            returns: self.output_contract.return_type_label(&output),
            parameters: witnessed_input
                .rows()
                .iter()
                .map(|row| compact_row(row, "name"))
                .collect(),
            return_fields: self.output_contract.return_fields(&output),
            description: manifest.description.trim().to_string(),
            examples: self.compact_examples_with(config),
        });
        self.compact_cache
            .0
            .lock_recover()
            .insert(key, Arc::clone(&computed));
        computed
    }

    pub fn input_signature(&self, manifest: &ToolManifest) -> String {
        self.input_signature_with_name(manifest, &manifest.name)
    }

    pub fn input_signature_with_name(
        &self,
        _manifest: &ToolManifest,
        signature_name: &str,
    ) -> String {
        let body = compact_arguments(&self.output_contract.witnessed_input(&self.input_shape()));
        format!(
            "{}{}({})",
            signature_name,
            self.output_contract
                .type_parameter_suffix()
                .unwrap_or_default(),
            body
        )
    }

    pub fn output_summary(&self) -> String {
        self.output_contract.return_type_label(&self.output_shape())
    }

    pub fn parameter_metadata(&self) -> Vec<serde_json::Value> {
        self.output_contract
            .witnessed_input(&self.input_shape())
            .rows()
            .iter()
            .map(|row| compact_row(row, "name"))
            .collect()
    }

    pub fn model_tool(&self, manifest: &ToolManifest) -> ModelTool {
        ModelTool {
            name: manifest.name.clone(),
            description: manifest.description.clone(),
            input_schema: self.input_schema.clone(),
            output_schema: self.output_schema.clone(),
        }
    }
}

/// Static authoring helper for tools.
///
/// Composes the runtime [`ToolManifest`] and [`ToolContract`] projections. They
/// serialize under the explicit `manifest` and `contract` keys: the definition
/// is reachable from the persisted RLM execution-state envelope, whose
/// canonical-encoding invariant bans `#[serde(flatten)]` because a flattened
/// subtree has no declaration order a structural pre-pass can validate.
#[derive(
    Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize, schemars::JsonSchema,
)]
pub struct ToolDefinition {
    pub manifest: ToolManifest,
    pub contract: ToolContract,
}

/// A tool as its author defines it, before its host sets its execution
/// bound. Lash supplies no bound: [`ToolDraft::with_execution`] makes the
/// [`ToolDefinition`] a provider registers, and nothing else does.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ToolDraft {
    id: ToolId,
    name: String,
    description: String,
    bindings: std::collections::BTreeMap<String, serde_json::Value>,
    contract: ToolContract,
}

impl ToolDraft {
    /// Tool identity.
    pub fn id(&self) -> &ToolId {
        &self.id
    }

    /// Tool name.
    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn with_examples(mut self, examples: Vec<String>) -> Self {
        self.contract.examples = examples;
        self
    }

    /// The tool's contract: its schemas and examples.
    pub fn contract(&self) -> &ToolContract {
        &self.contract
    }

    /// The tool as a model is offered it.
    pub fn model_tool(&self) -> ModelTool {
        ModelTool {
            name: self.name.clone(),
            description: self.description.clone(),
            input_schema: self.contract.input_schema.clone(),
            output_schema: self.contract.output_schema.clone(),
        }
    }

    /// Sets how long one run of the tool's body may take: the bound every
    /// tool's host sets, which makes the draft a definition.
    pub fn with_execution(self, execution: std::time::Duration) -> ToolDefinition {
        ToolDefinition {
            manifest: ToolManifest {
                inline: true,
                id: self.id,
                name: self.name,
                description: self.description,
                module: None,
                compact_contract: None,
                bindings: self.bindings,
                argument_projection: ToolArgumentProjectionPolicy::default(),
                execution_policy: default_tool_execution_policy(),
                execution,
                park: None,
                declaration: ToolDeclaration::default(),
                isolation_engine: None,
            },
            contract: self.contract,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ModelTool {
    pub name: String,
    pub description: String,
    pub input_schema: SchemaContract,
    pub output_schema: SchemaContract,
}

/// Prompt-facing schema and example cuts; validation always uses the full schema.
#[derive(
    Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, serde::Serialize, serde::Deserialize,
)]
#[serde(default, deny_unknown_fields)]
pub struct ToolPresentationConfig {
    pub example_limit: usize,
    pub example_chars: usize,
    pub schema_depth: usize,
}
impl Default for ToolPresentationConfig {
    fn default() -> Self {
        Self::standard()
    }
}
impl ToolPresentationConfig {
    /// Standard preset: two examples, 240 characters each, eight schema
    /// container levels. Historical cuts with no universal workload measurement.
    pub const fn standard() -> Self {
        Self {
            example_limit: 2,
            example_chars: 240,
            schema_depth: 8,
        }
    }
}
const COMPACT_TOOL_EXAMPLE_LIMIT: usize = ToolPresentationConfig::standard().example_limit;
const COMPACT_TOOL_EXAMPLE_CHAR_LIMIT: usize = ToolPresentationConfig::standard().example_chars;

#[derive(
    Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize, schemars::JsonSchema,
)]
pub struct CompactToolContract {
    pub name: String,
    pub signature: String,
    pub returns: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub parameters: Vec<serde_json::Value>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub return_fields: Vec<serde_json::Value>,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub description: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub examples: Vec<String>,
}

impl CompactToolContract {
    pub(crate) fn render_signature_head(&self) -> String {
        format!("{} -> {}", self.signature.trim(), self.returns.trim())
    }

    pub fn render_signature(&self) -> String {
        let mut sections = vec![self.render_signature_head()];
        let parameter_lines = self
            .parameters
            .iter()
            .filter_map(compact_doc_line)
            .collect::<Vec<_>>();
        if !parameter_lines.is_empty() {
            sections.push(format!("Parameters:\n{}", parameter_lines.join("\n")));
        }
        let return_field_lines = self
            .return_fields
            .iter()
            .filter_map(compact_doc_line)
            .collect::<Vec<_>>();
        if !return_field_lines.is_empty() {
            sections.push(format!("Return fields:\n{}", return_field_lines.join("\n")));
        }
        sections.join("\n")
    }

    #[cfg(test)]
    pub(crate) fn render_returns(&self) -> String {
        let mut sections = Vec::new();
        let return_field_lines = self
            .return_fields
            .iter()
            .filter_map(compact_doc_line)
            .collect::<Vec<_>>();
        if !return_field_lines.is_empty() {
            sections.push(format!("Return fields:\n{}", return_field_lines.join("\n")));
        }
        sections.join("\n")
    }

    /// The contract as one markdown block: the oracle the contract's
    /// rendering laws read. No model-facing text is built from it; a tool's
    /// guidance is a prompt section (ADR 0133).
    #[cfg(test)]
    pub(crate) fn render_markdown(&self) -> String {
        let mut sections = vec![format!("### {}", self.render_signature_head())];
        if !self.description.trim().is_empty() {
            sections.push(self.description.trim().to_string());
        }
        if !self.parameters.is_empty() {
            sections.push(format!(
                "Parameters:\n{}",
                self.parameters
                    .iter()
                    .filter_map(compact_doc_line)
                    .collect::<Vec<_>>()
                    .join("\n")
            ));
        }
        if !self.return_fields.is_empty() {
            sections.push(format!(
                "Return fields:\n{}",
                self.return_fields
                    .iter()
                    .filter_map(compact_doc_line)
                    .collect::<Vec<_>>()
                    .join("\n")
            ));
        }
        if !self.examples.is_empty() {
            sections.push(format!("Examples: {}", self.examples.join("; ")));
        }
        sections.join("\n")
    }
}

impl ToolDefinition {
    pub fn raw(
        id: impl Into<ToolId>,
        name: impl Into<String>,
        description: impl Into<String>,
        input_schema: serde_json::Value,
        output_schema: serde_json::Value,
    ) -> Result<ToolDraft, ToolCatalogBuildError> {
        let id = id.into();
        let name = name.into();
        let admit = |schema, purpose| {
            SchemaContract::admit(schema).map_err(|source| ToolCatalogBuildError::UnusableSchema {
                tool_id: id.clone(),
                name: name.clone(),
                purpose,
                source,
            })
        };
        let input_schema = admit(input_schema, SchemaPurpose::ToolInput)?;
        let output_schema = admit(output_schema, SchemaPurpose::ToolOutput)?;
        let description = description.into();
        Ok(ToolDraft {
            description,
            bindings: std::collections::BTreeMap::new(),
            contract: ToolContract {
                identity: Some(ToolContractIdentity {
                    id: id.clone(),
                    name: name.clone(),
                }),
                input_schema,
                output_schema,
                ..ToolContract::default()
            },
            id,
            name,
        })
    }

    pub fn typed<Args, Output>(
        id: impl Into<ToolId>,
        name: impl Into<String>,
        description: impl Into<String>,
    ) -> Result<ToolDraft, ToolCatalogBuildError>
    where
        Args: schemars::JsonSchema,
        Output: schemars::JsonSchema,
    {
        Self::raw(
            id,
            name,
            description,
            schema_for::<Args>(),
            schema_for::<Output>(),
        )
    }

    pub fn with_examples(mut self, examples: Vec<String>) -> Self {
        self.contract.examples = examples;
        self
    }

    pub fn with_argument_projection(
        mut self,
        argument_projection: ToolArgumentProjectionPolicy,
    ) -> Self {
        self.manifest.argument_projection = argument_projection;
        self
    }

    pub fn with_execution_policy(mut self, execution_policy: ExecutionPolicy) -> Self {
        self.manifest.execution_policy = execution_policy;
        self
    }

    /// Sets another execution bound: how long one run of the tool's body
    /// may take.
    pub fn with_execution(mut self, execution: std::time::Duration) -> Self {
        self.manifest = self.manifest.with_execution(execution);
        self
    }

    /// Declares what the tool's inline body may do beyond a Done result:
    /// return Deferred, or declare Lash intents. `park` is how long a call
    /// may stay parked waiting for its completion, which the host of a tool
    /// that may defer sets, and no other tool has.
    ///
    /// # Errors
    ///
    /// [`RegistrationRefused`]: an invalid declaration, a deferring tool
    /// without a park bound, a park bound on a tool that never defers, or an
    /// isolated declaration, which [`Self::isolated_in`] makes.
    pub fn with_declaration(
        mut self,
        declaration: ToolDeclaration,
        park: Option<ParkBound>,
    ) -> Result<Self, RegistrationRefused> {
        self.manifest = self.manifest.declared(declaration, park, None)?;
        Ok(self)
    }

    /// Declares the tool isolated: every call runs as a process of the
    /// engine registered as `engine`, with no inline body.
    ///
    /// # Errors
    ///
    /// [`RegistrationRefused`]: the tool already declares what only an
    /// inline body does.
    pub fn isolated_in(mut self, engine: impl Into<String>) -> Result<Self, RegistrationRefused> {
        let declaration = ToolDeclaration {
            isolated: true,
            ..self.manifest.declaration.clone()
        };
        let park = self.manifest.park;
        self.manifest = self
            .manifest
            .declared(declaration, park, Some(engine.into()))?;
        Ok(self)
    }

    pub fn with_output_contract(mut self, output_contract: ToolOutputContract) -> Self {
        self.contract.output_contract = output_contract;
        self
    }

    pub fn with_input_schema_projection(
        mut self,
        dialect: SchemaDialect,
        schema: JsonSchema,
    ) -> Self {
        self.contract
            .input_schema
            .projection
            .set_override(SchemaProjectionOverride::new(dialect, schema));
        self
    }

    pub fn with_output_schema_projection(
        mut self,
        dialect: SchemaDialect,
        schema: JsonSchema,
    ) -> Self {
        self.contract
            .output_schema
            .projection
            .set_override(SchemaProjectionOverride::new(dialect, schema));
        self
    }

    pub fn with_output_from_input_schema(
        self,
        input_field: impl Into<String>,
        default_schema: Option<JsonSchema>,
    ) -> Self {
        self.with_output_contract(ToolOutputContract::from_input_schema(
            input_field,
            default_schema,
        ))
    }

    pub fn default_input_schema() -> serde_json::Value {
        ToolContract::default_input_schema()
    }

    /// Tool identity.
    pub fn id(&self) -> &ToolId {
        &self.manifest.id
    }

    /// Tool name.
    pub fn name(&self) -> &str {
        &self.manifest.name
    }

    /// Tool description.
    pub fn description(&self) -> &str {
        &self.manifest.description
    }

    pub fn input_signature(&self) -> String {
        self.contract.input_signature(&self.manifest)
    }

    pub fn output_summary(&self) -> String {
        self.contract.output_summary()
    }

    pub fn signature(&self) -> String {
        format!("{} -> {}", self.input_signature(), self.output_summary())
    }

    pub fn compact_contract(&self) -> CompactToolContract {
        self.contract.compact_contract(&self.manifest)
    }

    pub fn model_tool(&self) -> ModelTool {
        self.contract.model_tool(&self.manifest)
    }

    /// Project the manifest, computing the catalog-facing compact contract from
    /// the resolved [`ToolContract`].
    pub fn manifest(&self) -> ToolManifest {
        let mut manifest = self.manifest.clone();
        manifest.compact_contract = Some(self.contract.compact_contract_shared(&manifest));
        manifest
    }

    pub fn contract(&self) -> ToolContract {
        let mut contract = self.contract.clone();
        contract.identity = Some(ToolContractIdentity {
            id: self.manifest.id.clone(),
            name: self.manifest.name.clone(),
        });
        contract
    }

    /// Recompose a definition from its [`ToolManifest`] and [`ToolContract`]
    /// projections — the inverse of [`ToolDefinition::manifest`]/[`ToolDefinition::contract`].
    pub fn from_parts(manifest: ToolManifest, mut contract: ToolContract) -> Self {
        contract.identity = Some(ToolContractIdentity {
            id: manifest.id.clone(),
            name: manifest.name.clone(),
        });
        Self { manifest, contract }
    }

    pub fn parameter_metadata(&self) -> Vec<serde_json::Value> {
        self.contract.parameter_metadata()
    }
}

/// The one manifest key a tool binding lives under.
///
/// The key is lash's internal projection onto the manifest's opaque `bindings`
/// map: hosts never read or write it. The binding it holds names a module path
/// and operation, not a source language; each dialect spells that binding in
/// its own syntax (ADR 0096).
pub const TOOL_BINDING_KEY: &str = "lash.tool";

/// Dialect-agnostic binding that makes a host tool callable as a module
/// operation during code-mode execution. Which dialect executes the call is
/// lash's decision; the host supplies the module path, operation, and optional
/// authority type and aliases.
#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ToolBinding {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub module_path: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub operation: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub authority_type: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub aliases: Vec<String>,
}

impl ToolBinding {
    pub fn new(
        module_path: impl IntoIterator<Item = impl Into<String>>,
        operation: impl Into<String>,
    ) -> Self {
        Self {
            module_path: module_path.into_iter().map(Into::into).collect(),
            operation: Some(operation.into()),
            authority_type: None,
            aliases: Vec::new(),
        }
    }

    pub fn with_authority_type(mut self, authority_type: impl Into<String>) -> Self {
        self.authority_type = Some(authority_type.into());
        self
    }

    pub fn with_aliases(mut self, aliases: impl IntoIterator<Item = impl Into<String>>) -> Self {
        self.aliases = aliases.into_iter().map(Into::into).collect();
        self
    }
}

/// The one host-facing setter for a tool's executable binding.
///
/// The manifest key the binding is written under
/// ([`TOOL_BINDING_KEY`]) is lash's own projection onto the opaque
/// `bindings` map: hosts never read or write it, and no dialect choice is
/// exposed or implied by calling this setter.
pub trait ToolDefinitionBindingExt {
    fn with_tool_binding(self, tool_binding: ToolBinding) -> Self;
}

impl ToolDefinitionBindingExt for ToolDraft {
    #[expect(
        clippy::expect_used,
        reason = "ToolBinding is a module-owned struct of strings and maps, so serialization into the manifest's JSON bindings map can only fail if the type is widened, which the site's message asserts"
    )]
    fn with_tool_binding(mut self, tool_binding: ToolBinding) -> Self {
        self.bindings.insert(
            TOOL_BINDING_KEY.to_string(),
            serde_json::to_value(&tool_binding).expect("tool binding must serialize to JSON"),
        );
        self
    }
}

impl ToolDefinitionBindingExt for ToolDefinition {
    #[expect(
        clippy::expect_used,
        reason = "ToolBinding is a module-owned struct of strings and maps, so serialization into the manifest's JSON bindings map can only fail if the type is widened, which the site's message asserts"
    )]
    fn with_tool_binding(mut self, tool_binding: ToolBinding) -> Self {
        self.manifest.bindings.insert(
            TOOL_BINDING_KEY.to_string(),
            serde_json::to_value(&tool_binding).expect("tool binding must serialize to JSON"),
        );
        self
    }
}

pub(crate) mod schema_docs;
pub mod schema_shape;
pub use schema_docs::schema_for;
use schema_docs::{
    compact_arguments, compact_doc_line, compact_examples, compact_row, compact_type,
};
pub use schema_shape::{
    ExtraKeys, ObjectShape, ProcessParamShape, ProcessShape, SchemaShape, ShapeConstraints,
    ShapeField, ShapeKind, ShapeRow, X_LASH_KEYWORD, XLashParam, XLashSignature, XLashType,
    is_named_type_reference,
};

#[cfg(feature = "schema-validation")]
mod schema_validation;
#[cfg(feature = "schema-validation")]
pub use schema_validation::validate_tool_input;

#[cfg(test)]
mod tests;
