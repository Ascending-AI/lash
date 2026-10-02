use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, OnceLock};

use crate::sync::MutexExt;
use crate::{
    JsonSchema, SchemaContract, SchemaDialect, SchemaProjectionOverride, SchemaPurpose,
    ToolCatalogBuildError,
};

/// Automatic retry policy for a tool's execution.
///
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
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ToolRetryPolicy {
    /// Never retry automatically.
    #[default]
    Never,
    /// Retry only failures that explicitly report a safe retry disposition.
    Safe {
        max_attempts: u32,
        base_delay_ms: u64,
        max_delay_ms: u64,
    },
    /// Retry only failures that explicitly report a safe retry disposition,
    /// and only when the runtime can provide a stable replay key.
    Idempotent {
        max_attempts: u32,
        base_delay_ms: u64,
        max_delay_ms: u64,
    },
}

impl ToolRetryPolicy {
    pub fn safe(max_attempts: u32, base_delay_ms: u64, max_delay_ms: u64) -> Self {
        Self::Safe {
            max_attempts,
            base_delay_ms,
            max_delay_ms,
        }
    }

    pub(crate) fn idempotent(max_attempts: u32, base_delay_ms: u64, max_delay_ms: u64) -> Self {
        Self::Idempotent {
            max_attempts,
            base_delay_ms,
            max_delay_ms,
        }
    }

    pub(crate) fn max_attempts(self) -> u32 {
        match self {
            Self::Never => 1,
            Self::Safe { max_attempts, .. } | Self::Idempotent { max_attempts, .. } => {
                max_attempts.max(1)
            }
        }
    }

    pub(crate) fn delay_ms_for_retry(
        self,
        retry_index: u32,
        requested_after_ms: Option<u64>,
    ) -> u64 {
        let (base_delay_ms, max_delay_ms) = match self {
            Self::Never => return 0,
            Self::Safe {
                base_delay_ms,
                max_delay_ms,
                ..
            }
            | Self::Idempotent {
                base_delay_ms,
                max_delay_ms,
                ..
            } => (base_delay_ms, max_delay_ms),
        };
        let multiplier = 1_u64.checked_shl(retry_index).unwrap_or(u64::MAX);
        let backoff = base_delay_ms.saturating_mul(multiplier);
        let delay = requested_after_ms.unwrap_or(backoff);
        if max_delay_ms == 0 {
            delay
        } else {
            delay.min(max_delay_ms)
        }
    }
}

fn default_tool_retry_policy() -> ToolRetryPolicy {
    ToolRetryPolicy::default()
}

fn is_default_tool_retry_policy(policy: &ToolRetryPolicy) -> bool {
    *policy == ToolRetryPolicy::default()
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

/// Guidance shared by the tools of one module. Providers give every tool in
/// the module the same metadata; the catalog and hosts present it once.
/// It is recorded with the manifest, independently of execution bindings.
#[derive(
    Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize, schemars::JsonSchema,
)]
pub struct ToolModule {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub instructions: Option<String>,
}

impl ToolModule {
    pub fn render_markdown(&self) -> String {
        let heading = format!("#### {}", self.name);
        match self
            .instructions
            .as_deref()
            .filter(|text| !text.trim().is_empty())
        {
            Some(instructions) => format!("{heading}\n\n{instructions}"),
            None => heading,
        }
    }
}

/// Tool metadata exposed to prompts, catalogs, and UI. Catalog membership —
/// being present in a [`ToolProvider`]'s manifest list — is the execution gate;
/// there is no per-manifest tier. The optional compact contract is the
/// catalog-facing projection of the resolved contract; full schemas stay in
/// [`ToolContract`].
#[derive(
    Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize, schemars::JsonSchema,
)]
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
        default = "default_tool_retry_policy",
        skip_serializing_if = "is_default_tool_retry_policy"
    )]
    pub retry_policy: ToolRetryPolicy,
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
    example_limit: usize,
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

    /// The authored examples a prompt shows: the first few, each bounded.
    pub fn compact_examples(&self) -> Vec<String> {
        compact_examples(&self.examples, COMPACT_TOOL_EXAMPLE_LIMIT)
    }

    pub fn compact_contract(&self, manifest: &ToolManifest) -> CompactToolContract {
        self.compact_contract_with_example_limit(manifest, COMPACT_TOOL_EXAMPLE_LIMIT)
    }

    pub fn compact_contract_with_example_limit(
        &self,
        manifest: &ToolManifest,
        example_limit: usize,
    ) -> CompactToolContract {
        self.compact_contract_with_signature_name_and_example_limit(
            manifest,
            &manifest.name,
            example_limit,
        )
    }

    pub fn compact_contract_with_signature_name(
        &self,
        manifest: &ToolManifest,
        signature_name: &str,
    ) -> CompactToolContract {
        self.compact_contract_with_signature_name_and_example_limit(
            manifest,
            signature_name,
            COMPACT_TOOL_EXAMPLE_LIMIT,
        )
    }

    pub fn compact_contract_with_signature_name_and_example_limit(
        &self,
        manifest: &ToolManifest,
        signature_name: &str,
        example_limit: usize,
    ) -> CompactToolContract {
        (*self.compact_contract_shared_with_signature_name_and_example_limit(
            manifest,
            signature_name,
            example_limit,
        ))
        .clone()
    }

    /// Shared handle to the compact projection for `manifest`, memoized on this
    /// contract. Read-only consumers should prefer this over
    /// [`ToolContract::compact_contract`] to avoid the deep `serde_json::Value`
    /// copies behind schema `$ref` resolution.
    pub fn compact_contract_shared(&self, manifest: &ToolManifest) -> Arc<CompactToolContract> {
        self.compact_contract_shared_with_signature_name_and_example_limit(
            manifest,
            &manifest.name,
            COMPACT_TOOL_EXAMPLE_LIMIT,
        )
    }

    /// Shared handle variant of
    /// [`ToolContract::compact_contract_with_signature_name`].
    pub fn compact_contract_shared_with_signature_name(
        &self,
        manifest: &ToolManifest,
        signature_name: &str,
    ) -> Arc<CompactToolContract> {
        self.compact_contract_shared_with_signature_name_and_example_limit(
            manifest,
            signature_name,
            COMPACT_TOOL_EXAMPLE_LIMIT,
        )
    }

    pub fn compact_contract_shared_with_signature_name_and_example_limit(
        &self,
        manifest: &ToolManifest,
        signature_name: &str,
        example_limit: usize,
    ) -> Arc<CompactToolContract> {
        if signature_name == manifest.name
            && example_limit == COMPACT_TOOL_EXAMPLE_LIMIT
            && let Some(stored) = &manifest.compact_contract
            && stored.name == signature_name
            && stored.description == manifest.description.trim()
        {
            return Arc::clone(stored);
        }
        let key = CompactContractKey {
            signature_name: signature_name.to_string(),
            example_limit,
            description: manifest.description.trim().to_string(),
        };
        if let Some(hit) = self.compact_cache.0.lock_recover().get(&key) {
            return Arc::clone(hit);
        }
        let computed = Arc::new(CompactToolContract {
            name: signature_name.to_string(),
            signature: self.input_signature_with_name(manifest, signature_name),
            returns: self.output_summary(),
            parameters: self.parameter_metadata(),
            return_fields: self.output_contract.return_fields(&self.output_shape()),
            description: manifest.description.trim().to_string(),
            examples: compact_examples(&self.examples, example_limit),
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

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ModelTool {
    pub name: String,
    pub description: String,
    pub input_schema: SchemaContract,
    pub output_schema: SchemaContract,
}

const COMPACT_TOOL_EXAMPLE_LIMIT: usize = 2;
const COMPACT_TOOL_EXAMPLE_CHAR_LIMIT: usize = 240;

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

    pub fn render_markdown(&self) -> String {
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
    ) -> Result<Self, ToolCatalogBuildError> {
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
        Ok(Self::new(
            id,
            name,
            description,
            input_schema,
            output_schema,
        ))
    }

    pub fn new(
        id: impl Into<ToolId>,
        name: impl Into<String>,
        description: impl Into<String>,
        input_schema: SchemaContract,
        output_schema: SchemaContract,
    ) -> Self {
        let id = id.into();
        let name = name.into();
        Self {
            manifest: ToolManifest {
                inline: true,
                id: id.clone(),
                name: name.clone(),
                description: description.into(),
                module: None,
                compact_contract: None,
                bindings: std::collections::BTreeMap::new(),
                argument_projection: ToolArgumentProjectionPolicy::default(),
                retry_policy: default_tool_retry_policy(),
            },
            contract: ToolContract {
                identity: Some(ToolContractIdentity { id, name }),
                input_schema,
                output_schema,
                ..ToolContract::default()
            },
        }
    }

    pub fn typed<Args, Output>(
        id: impl Into<ToolId>,
        name: impl Into<String>,
        description: impl Into<String>,
    ) -> Result<Self, ToolCatalogBuildError>
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

    pub fn with_retry_policy(mut self, retry_policy: ToolRetryPolicy) -> Self {
        self.manifest.retry_policy = retry_policy;
        self
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
        self.compact_contract_with_example_limit(COMPACT_TOOL_EXAMPLE_LIMIT)
    }

    pub fn compact_contract_with_example_limit(&self, example_limit: usize) -> CompactToolContract {
        self.contract
            .compact_contract_with_example_limit(&self.manifest, example_limit)
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

    pub fn format_tool_docs(tools: &[ToolDefinition]) -> String {
        Self::format_tool_docs_iter(tools.iter())
    }

    pub fn format_tool_docs_iter<'a>(
        tools: impl IntoIterator<Item = &'a ToolDefinition>,
    ) -> String {
        tools
            .into_iter()
            .map(|tool| tool.compact_contract().render_markdown())
            .collect::<Vec<_>>()
            .join("\n\n")
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

#[cfg(test)]
mod inline_tests {
    use super::*;
    #[test]
    fn manifests_without_inline_keep_the_existing_default() {
        let tool = ToolDefinition::raw(
            "test",
            "test",
            "test",
            serde_json::json!({}),
            serde_json::json!({}),
        )
        .expect("valid declared tool schemas");
        let mut value = serde_json::to_value(tool.manifest()).unwrap();
        assert!(value.get("inline").is_none());
        assert!(
            serde_json::from_value::<ToolManifest>(value.clone())
                .unwrap()
                .inline
        );
        value["inline"] = false.into();
        let hidden = serde_json::from_value::<ToolManifest>(value).unwrap();
        assert!(!hidden.inline);
        assert_eq!(serde_json::to_value(hidden).unwrap()["inline"], false);
    }
}
