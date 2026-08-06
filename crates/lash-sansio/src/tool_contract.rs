use crate::{SchemaContract, SchemaProjectionOverride};

/// Automatic retry policy for a tool's execution.
///
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ToolRetryPolicy {
    /// Never retry automatically. This is the default for every tool.
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

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolActivation {
    #[default]
    Always,
    Internal,
}

fn is_default_tool_activation(activation: &ToolActivation) -> bool {
    *activation == ToolActivation::default()
}

#[derive(Clone, Debug, Default, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ToolOutputContract {
    #[default]
    Static,
    FromInputSchema {
        input_field: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        default_schema: Option<serde_json::Value>,
    },
}

impl ToolOutputContract {
    pub fn from_input_schema(
        input_field: impl Into<String>,
        default_schema: Option<serde_json::Value>,
    ) -> Self {
        Self::FromInputSchema {
            input_field: input_field.into(),
            default_schema,
        }
    }

    pub fn is_static(&self) -> bool {
        matches!(self, Self::Static)
    }

    fn return_type_label(&self, static_schema: &serde_json::Value) -> String {
        match self {
            Self::Static => compact_schema_label(static_schema),
            Self::FromInputSchema { .. } => "T".to_string(),
        }
    }

    fn type_parameter_suffix(&self) -> Option<String> {
        match self {
            Self::Static => None,
            Self::FromInputSchema { default_schema, .. } => {
                let default = default_schema
                    .as_ref()
                    .map(compact_schema_label)
                    .unwrap_or_else(|| "any".to_string());
                Some(format!("<T = {default}>"))
            }
        }
    }

    fn apply_type_witness_parameter(&self, params: &mut [ParameterDoc]) {
        let Self::FromInputSchema { input_field, .. } = self else {
            return;
        };
        if let Some(param) = params.iter_mut().find(|param| param.name == *input_field) {
            param.type_label = "TypeSpec<T>".to_string();
            param.nullable = false;
            param.default_value = None;
            param.enum_values.clear();
            param.minimum = None;
            param.maximum = None;
            param.min_length = None;
            param.max_length = None;
            param.min_items = None;
            param.max_items = None;
            param.item_type = None;
        }
    }

    fn return_fields(&self, static_schema: &serde_json::Value) -> Vec<serde_json::Value> {
        match self {
            Self::Static => return_field_metadata(static_schema),
            Self::FromInputSchema { .. } => Vec::new(),
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
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

/// Tool metadata exposed to prompts, catalogs, and UI. Catalog membership —
/// being present in a [`ToolProvider`]'s manifest list — is the execution gate;
/// there is no per-manifest tier. The optional compact contract is the
/// catalog-facing projection of the resolved contract; full schemas stay in
/// [`ToolContract`].
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct ToolManifest {
    pub id: ToolId,
    pub name: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub description: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub compact_contract: Option<CompactToolContract>,
    #[serde(default, skip_serializing_if = "is_default_tool_activation")]
    pub activation: ToolActivation,
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
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct ToolContract {
    #[serde(skip)]
    identity: Option<ToolContractIdentity>,
    #[serde(default = "ToolContract::default_input_schema_contract")]
    pub input_schema: SchemaContract,
    #[serde(default)]
    pub output_schema: SchemaContract,
    #[serde(default, skip_serializing_if = "ToolOutputContract::is_static")]
    pub output_contract: ToolOutputContract,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub examples: Vec<String>,
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
            input_schema: Self::default_input_schema_contract(),
            output_schema: serde_json::Value::Null.into(),
            output_contract: ToolOutputContract::Static,
            examples: Vec::new(),
        }
    }
}

impl ToolContract {
    fn default_input_schema_contract() -> SchemaContract {
        Self::default_input_schema().into()
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
        CompactToolContract {
            name: signature_name.to_string(),
            signature: self.input_signature_with_name(manifest, signature_name),
            returns: self.output_summary(),
            parameters: self.parameter_metadata(),
            return_fields: self
                .output_contract
                .return_fields(self.output_schema.canonical()),
            description: manifest.description.trim().to_string(),
            examples: compact_examples(&self.examples, example_limit),
        }
    }

    pub fn input_signature(&self, manifest: &ToolManifest) -> String {
        self.input_signature_with_name(manifest, &manifest.name)
    }

    pub fn input_signature_with_name(
        &self,
        _manifest: &ToolManifest,
        signature_name: &str,
    ) -> String {
        let params = self
            .parameter_docs()
            .into_iter()
            .map(|p| p.signature_fragment())
            .collect::<Vec<_>>();
        let body = if params.is_empty() {
            "{}".to_string()
        } else {
            format!("{{ {} }}", params.join(", "))
        };
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
        self.output_contract
            .return_type_label(self.output_schema.canonical())
    }

    pub fn parameter_metadata(&self) -> Vec<serde_json::Value> {
        self.parameter_docs()
            .into_iter()
            .map(|param| param.into_value())
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

    fn parameter_docs(&self) -> Vec<ParameterDoc> {
        let mut params = schema_parameter_docs(self.input_schema.canonical());
        self.output_contract
            .apply_type_witness_parameter(&mut params);
        params
    }
}

/// Static authoring helper for tools.
///
/// Composes the runtime [`ToolManifest`] and [`ToolContract`] projections. Both
/// are `#[serde(flatten)]`ed so the serialized JSON shape stays flat (and wire/
/// persistence compatible); the two structs have disjoint field names.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct ToolDefinition {
    #[serde(flatten)]
    pub manifest: ToolManifest,
    #[serde(flatten)]
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

#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
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
    ) -> Self {
        let id = id.into();
        let name = name.into();
        Self {
            manifest: ToolManifest {
                id: id.clone(),
                name: name.clone(),
                description: description.into(),
                compact_contract: None,
                activation: ToolActivation::default(),
                bindings: std::collections::BTreeMap::new(),
                argument_projection: ToolArgumentProjectionPolicy::default(),
                retry_policy: default_tool_retry_policy(),
            },
            contract: ToolContract {
                identity: Some(ToolContractIdentity { id, name }),
                input_schema: input_schema.into(),
                output_schema: output_schema.into(),
                ..ToolContract::default()
            },
        }
    }

    pub fn typed<Args, Output>(
        id: impl Into<ToolId>,
        name: impl Into<String>,
        description: impl Into<String>,
    ) -> Self
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

    pub fn with_activation(mut self, activation: ToolActivation) -> Self {
        self.manifest.activation = activation;
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
        profile: impl Into<String>,
        schema: serde_json::Value,
    ) -> Self {
        let profile = profile.into();
        self.contract
            .input_schema
            .projection
            .set_override(SchemaProjectionOverride::new(profile, schema));
        self
    }

    pub fn with_output_schema_projection(
        mut self,
        profile: impl Into<String>,
        schema: serde_json::Value,
    ) -> Self {
        let profile = profile.into();
        self.contract
            .output_schema
            .projection
            .set_override(SchemaProjectionOverride::new(profile, schema));
        self
    }

    pub fn with_output_from_input_schema(
        self,
        input_field: impl Into<String>,
        default_schema: Option<serde_json::Value>,
    ) -> Self {
        self.with_output_contract(ToolOutputContract::from_input_schema(
            input_field,
            default_schema,
        ))
    }

    pub fn default_input_schema() -> serde_json::Value {
        ToolContract::default_input_schema()
    }

    /// Tool identity. Read very widely, so exposed as a thin accessor over the
    /// composed [`ToolManifest`].
    pub fn id(&self) -> &ToolId {
        &self.manifest.id
    }

    /// Tool name. Read very widely, so exposed as a thin accessor.
    pub fn name(&self) -> &str {
        &self.manifest.name
    }

    /// Tool description. Read very widely, so exposed as a thin accessor.
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
        manifest.compact_contract = Some(self.contract.compact_contract(&manifest));
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
        self.parameter_docs()
            .into_iter()
            .map(|param| param.into_value())
            .collect()
    }

    fn parameter_docs(&self) -> Vec<ParameterDoc> {
        let mut params = schema_parameter_docs(self.contract.input_schema.canonical());
        self.contract
            .output_contract
            .apply_type_witness_parameter(&mut params);
        params
    }
}

mod schema_docs;
pub use schema_docs::schema_for;
use schema_docs::{
    ParameterDoc, compact_doc_line, compact_examples, compact_schema_label, return_field_metadata,
    schema_parameter_docs,
};

mod schema_validation;
pub use schema_validation::{LashSchema, validate_tool_input};

include!("tool_contract/tests.rs");
