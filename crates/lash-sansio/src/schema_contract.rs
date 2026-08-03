//! JSON Schema contracts and provider dialect projection.
//!
//! A [`SchemaContract`] keeps the canonical schema used for runtime
//! validation separate from the provider wire schema. Providers declare the
//! dialects they accept for each purpose and resolve contracts lazily at the
//! request boundary.

use serde_json::{Map, Value, json};

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct SchemaContract {
    pub canonical: Value,
    #[serde(default, skip_serializing_if = "SchemaProjectionPolicy::is_default")]
    pub projection: SchemaProjectionPolicy,
}

impl SchemaContract {
    pub fn new(canonical: Value) -> Self {
        Self {
            canonical,
            projection: SchemaProjectionPolicy::default(),
        }
    }

    pub fn with_override(mut self, dialect: impl Into<String>, schema: Value) -> Self {
        self.projection
            .set_override(SchemaProjectionOverride::new(dialect, schema));
        self
    }

    pub fn canonical(&self) -> &Value {
        &self.canonical
    }
}

impl Default for SchemaContract {
    fn default() -> Self {
        Self::new(Value::Null)
    }
}

impl From<Value> for SchemaContract {
    fn from(value: Value) -> Self {
        Self::new(value)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct SchemaProjectionPolicy {
    #[serde(default, skip_serializing_if = "ProjectionMode::is_auto")]
    pub mode: ProjectionMode,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub overrides: Vec<SchemaProjectionOverride>,
}

impl SchemaProjectionPolicy {
    pub(crate) fn is_default(&self) -> bool {
        self.mode == ProjectionMode::Auto && self.overrides.is_empty()
    }

    pub(crate) fn set_override(&mut self, override_schema: SchemaProjectionOverride) {
        self.overrides
            .retain(|projection| projection.dialect != override_schema.dialect);
        self.overrides.push(override_schema);
    }
}

impl Default for SchemaProjectionPolicy {
    fn default() -> Self {
        Self {
            mode: ProjectionMode::Auto,
            overrides: Vec::new(),
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProjectionMode {
    #[default]
    Auto,
    ExplicitOnly,
    Exact,
}

impl ProjectionMode {
    fn is_auto(&self) -> bool {
        *self == Self::Auto
    }
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct SchemaProjectionOverride {
    pub dialect: String,
    pub schema: Value,
}

impl SchemaProjectionOverride {
    pub(crate) fn new(dialect: impl Into<String>, schema: Value) -> Self {
        Self {
            dialect: dialect.into(),
            schema,
        }
    }
}

#[derive(
    Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize, serde::Deserialize,
)]
#[serde(transparent)]
pub struct SchemaDialect(String);

impl SchemaDialect {
    pub const OPENAI_TOOL_PARAMETERS: &'static str = "openai_tool_parameters";
    pub const OPENAI_STRICT_TOOL_PARAMETERS: &'static str = "openai_strict_tool_parameters";
    pub const OPENAI_STRUCTURED_OUTPUT: &'static str = "openai_structured_output";
    pub const ANTHROPIC_TOOL_INPUT: &'static str = "anthropic_tool_input";
    pub const ANTHROPIC_OUTPUT_CONFIG_JSON_SCHEMA: &'static str =
        "anthropic_output_config_json_schema";
    pub const BEDROCK_CLAUDE_OUTPUT_CONFIG_JSON_SCHEMA: &'static str =
        "bedrock_claude_output_config_json_schema";
    pub const GOOGLE_SCHEMA: &'static str = "google_schema";
    pub const JSON_PROMPT_SCHEMA: &'static str = "json_prompt_schema";

    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn openai_tool_parameters() -> Self {
        Self::new(Self::OPENAI_TOOL_PARAMETERS)
    }

    pub fn openai_strict_tool_parameters() -> Self {
        Self::new(Self::OPENAI_STRICT_TOOL_PARAMETERS)
    }

    pub fn openai_structured_output() -> Self {
        Self::new(Self::OPENAI_STRUCTURED_OUTPUT)
    }

    pub fn anthropic_tool_input() -> Self {
        Self::new(Self::ANTHROPIC_TOOL_INPUT)
    }

    pub fn anthropic_output_config_json_schema() -> Self {
        Self::new(Self::ANTHROPIC_OUTPUT_CONFIG_JSON_SCHEMA)
    }

    pub fn bedrock_claude_output_config_json_schema() -> Self {
        Self::new(Self::BEDROCK_CLAUDE_OUTPUT_CONFIG_JSON_SCHEMA)
    }

    pub fn google_schema() -> Self {
        Self::new(Self::GOOGLE_SCHEMA)
    }
}

impl From<&str> for SchemaDialect {
    fn from(value: &str) -> Self {
        Self::new(value)
    }
}

impl From<String> for SchemaDialect {
    fn from(value: String) -> Self {
        Self::new(value)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SchemaPurpose {
    ToolInput,
    ToolOutput,
    StructuredOutput,
    PromptSchema,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ProviderSchemaCapabilities {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tool_input: Vec<SchemaDialect>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tool_output: Vec<SchemaDialect>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub structured_output: Vec<SchemaDialect>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub prompt_schema: Vec<SchemaDialect>,
}

impl ProviderSchemaCapabilities {
    pub fn openai(strict_tools: bool) -> Self {
        Self {
            tool_input: vec![if strict_tools {
                SchemaDialect::openai_strict_tool_parameters()
            } else {
                SchemaDialect::openai_tool_parameters()
            }],
            structured_output: vec![SchemaDialect::openai_structured_output()],
            ..Default::default()
        }
    }

    pub fn anthropic() -> Self {
        Self {
            tool_input: vec![SchemaDialect::anthropic_tool_input()],
            structured_output: vec![SchemaDialect::anthropic_output_config_json_schema()],
            ..Default::default()
        }
    }

    pub fn bedrock_claude() -> Self {
        Self {
            tool_input: vec![SchemaDialect::anthropic_tool_input()],
            structured_output: vec![SchemaDialect::bedrock_claude_output_config_json_schema()],
            ..Default::default()
        }
    }

    pub fn google() -> Self {
        Self {
            tool_input: vec![SchemaDialect::google_schema()],
            structured_output: vec![SchemaDialect::google_schema()],
            ..Default::default()
        }
    }

    pub fn dialects_for(&self, purpose: SchemaPurpose) -> &[SchemaDialect] {
        match purpose {
            SchemaPurpose::ToolInput => &self.tool_input,
            SchemaPurpose::ToolOutput => &self.tool_output,
            SchemaPurpose::StructuredOutput => &self.structured_output,
            SchemaPurpose::PromptSchema => &self.prompt_schema,
        }
    }
}

#[derive(Clone, Debug)]
pub struct SchemaResolutionRequest<'a> {
    pub provider: &'a str,
    pub purpose: SchemaPurpose,
    pub dialects: &'a [SchemaDialect],
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ResolvedSchema {
    pub schema: Value,
    pub dialect: SchemaDialect,
    pub diagnostics: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SchemaResolutionError {
    pub provider: String,
    pub purpose: SchemaPurpose,
    pub dialect: Option<SchemaDialect>,
    pub diagnostics: Vec<String>,
    first_diagnostic: String,
}

impl std::fmt::Display for SchemaResolutionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{} schema resolution for {:?} failed: {}",
            self.provider, self.purpose, self.first_diagnostic
        )
    }
}

impl std::error::Error for SchemaResolutionError {}

impl SchemaResolutionError {
    fn new(
        provider: impl Into<String>,
        purpose: SchemaPurpose,
        dialect: Option<SchemaDialect>,
        diagnostics: Vec<String>,
    ) -> Self {
        let first_diagnostic = diagnostics
            .first()
            .cloned()
            .unwrap_or_else(|| "schema resolution failed".to_string());
        Self {
            provider: provider.into(),
            purpose,
            dialect,
            diagnostics,
            first_diagnostic,
        }
    }

    pub fn first_diagnostic(&self) -> &str {
        &self.first_diagnostic
    }
}

pub fn resolve_schema(
    contract: &SchemaContract,
    request: SchemaResolutionRequest<'_>,
) -> Result<ResolvedSchema, SchemaResolutionError> {
    if request.dialects.is_empty() {
        return Err(SchemaResolutionError::new(
            request.provider,
            request.purpose,
            None,
            vec!["provider declared no schema dialects for this purpose".to_string()],
        ));
    }

    let mut diagnostics = Vec::new();
    for dialect in request.dialects {
        if let Some(override_schema) = contract
            .projection
            .overrides
            .iter()
            .find(|projection| projection.dialect == dialect.as_str())
        {
            let diagnostics = diagnostics;
            return Ok(ResolvedSchema {
                schema: override_schema.schema.clone(),
                dialect: dialect.clone(),
                diagnostics,
            });
        }

        match contract.projection.mode {
            ProjectionMode::ExplicitOnly => diagnostics.push(format!(
                "{}: no explicit projection override for {}",
                dialect.as_str(),
                format_purpose(request.purpose)
            )),
            ProjectionMode::Exact => match project_for_dialect(&contract.canonical, dialect) {
                Ok(projection)
                    if projection.schema == contract.canonical
                        && projection.diagnostics.is_empty() =>
                {
                    let diagnostics = diagnostics;
                    return Ok(ResolvedSchema {
                        schema: contract.canonical.clone(),
                        dialect: dialect.clone(),
                        diagnostics,
                    });
                }
                Ok(projection) => diagnostics.push(format!(
                    "{}: canonical schema is not exact for provider dialect ({})",
                    dialect.as_str(),
                    projection.diagnostics.join("; ")
                )),
                Err(err) => diagnostics.extend(err.diagnostics),
            },
            ProjectionMode::Auto => match project_for_dialect(&contract.canonical, dialect) {
                Ok(projection) => {
                    diagnostics.extend(projection.diagnostics);
                    return Ok(ResolvedSchema {
                        schema: projection.schema,
                        dialect: dialect.clone(),
                        diagnostics,
                    });
                }
                Err(err) => diagnostics.extend(err.diagnostics),
            },
        }
    }

    Err(SchemaResolutionError::new(
        request.provider,
        request.purpose,
        request.dialects.last().cloned(),
        diagnostics,
    ))
}

fn format_purpose(purpose: SchemaPurpose) -> &'static str {
    match purpose {
        SchemaPurpose::ToolInput => "tool input",
        SchemaPurpose::ToolOutput => "tool output",
        SchemaPurpose::StructuredOutput => "structured output",
        SchemaPurpose::PromptSchema => "prompt schema",
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
enum OpenAiSchemaProfile {
    ToolParameters,
    StrictToolParameters,
    StructuredOutput,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SchemaProjection {
    pub schema: Value,
    pub diagnostics: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct SchemaProjectionError {
    profile: OpenAiSchemaProfile,
    diagnostics: Vec<String>,
    first_diagnostic: String,
}

impl std::fmt::Display for SchemaProjectionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "OpenAI schema projection for {:?} failed: {}",
            self.profile, self.first_diagnostic
        )
    }
}

impl std::error::Error for SchemaProjectionError {}

impl SchemaProjectionError {
    fn new(profile: OpenAiSchemaProfile, diagnostics: Vec<String>) -> Self {
        let first_diagnostic = diagnostics
            .first()
            .cloned()
            .unwrap_or_else(|| "schema projection failed".to_string());
        Self {
            profile,
            diagnostics,
            first_diagnostic,
        }
    }
}

fn project_schema(
    schema: &Value,
    profile: OpenAiSchemaProfile,
) -> Result<SchemaProjection, SchemaProjectionError> {
    Projector::new(profile).project(schema)
}

fn project_tool_parameters(schema: &Value) -> Result<SchemaProjection, SchemaProjectionError> {
    project_schema(schema, OpenAiSchemaProfile::ToolParameters)
}

fn project_strict_tool_parameters(
    schema: &Value,
) -> Result<SchemaProjection, SchemaProjectionError> {
    project_schema(schema, OpenAiSchemaProfile::StrictToolParameters)
}

fn project_structured_output(schema: &Value) -> Result<SchemaProjection, SchemaProjectionError> {
    project_schema(schema, OpenAiSchemaProfile::StructuredOutput)
}

pub fn project_for_dialect(
    schema: &Value,
    dialect: &SchemaDialect,
) -> Result<SchemaProjection, SchemaResolutionError> {
    match dialect.as_str() {
        SchemaDialect::OPENAI_TOOL_PARAMETERS => project_tool_parameters(schema).map_err(|err| {
            SchemaResolutionError::new(
                "schema",
                SchemaPurpose::ToolInput,
                Some(dialect.clone()),
                err.diagnostics,
            )
        }),
        SchemaDialect::OPENAI_STRICT_TOOL_PARAMETERS => project_strict_tool_parameters(schema)
            .map_err(|err| {
                SchemaResolutionError::new(
                    "schema",
                    SchemaPurpose::ToolInput,
                    Some(dialect.clone()),
                    err.diagnostics,
                )
            }),
        SchemaDialect::OPENAI_STRUCTURED_OUTPUT => {
            project_structured_output(schema).map_err(|err| {
                SchemaResolutionError::new(
                    "schema",
                    SchemaPurpose::StructuredOutput,
                    Some(dialect.clone()),
                    err.diagnostics,
                )
            })
        }
        SchemaDialect::ANTHROPIC_TOOL_INPUT
        | SchemaDialect::ANTHROPIC_OUTPUT_CONFIG_JSON_SCHEMA
        | SchemaDialect::BEDROCK_CLAUDE_OUTPUT_CONFIG_JSON_SCHEMA => {
            project_anthropic_bedrock_schema(schema, dialect)
        }
        SchemaDialect::GOOGLE_SCHEMA | SchemaDialect::JSON_PROMPT_SCHEMA => Ok(SchemaProjection {
            schema: schema.clone(),
            diagnostics: Vec::new(),
        }),
        other => Err(SchemaResolutionError::new(
            "schema",
            SchemaPurpose::StructuredOutput,
            Some(dialect.clone()),
            vec![format!("unsupported schema dialect {other}")],
        )),
    }
}

pub fn project_anthropic_bedrock_schema(
    schema: &Value,
    dialect: &SchemaDialect,
) -> Result<SchemaProjection, SchemaResolutionError> {
    let mut projected = schema.clone();
    let mut sanitizer = AnthropicBedrockSanitizer {
        diagnostics: Vec::new(),
        errors: Vec::new(),
    };
    sanitizer.sanitize_value(&mut projected, Path::root());
    sanitizer.ensure_object_root(&mut projected);
    if sanitizer.errors.is_empty() {
        Ok(SchemaProjection {
            schema: projected,
            diagnostics: sanitizer.diagnostics,
        })
    } else {
        Err(SchemaResolutionError::new(
            "schema",
            SchemaPurpose::StructuredOutput,
            Some(dialect.clone()),
            sanitizer.errors,
        ))
    }
}

struct Projector {
    profile: OpenAiSchemaProfile,
    diagnostics: Vec<String>,
    errors: Vec<String>,
}

impl Projector {
    fn new(profile: OpenAiSchemaProfile) -> Self {
        Self {
            profile,
            diagnostics: Vec::new(),
            errors: Vec::new(),
        }
    }

    fn project(mut self, schema: &Value) -> Result<SchemaProjection, SchemaProjectionError> {
        let mut projected = schema.clone();
        self.project_value(&mut projected, Path::root(), true);
        self.ensure_object_root(&mut projected);

        if self.errors.is_empty() {
            Ok(SchemaProjection {
                schema: projected,
                diagnostics: self.diagnostics,
            })
        } else {
            Err(SchemaProjectionError::new(self.profile, self.errors))
        }
    }

    fn project_value(&mut self, value: &mut Value, path: Path, is_root: bool) {
        let Some(obj) = value.as_object_mut() else {
            self.errors
                .push(format!("{path}: schema must be a JSON object"));
            return;
        };

        self.flatten_single_all_of(obj, &path);
        self.convert_const(obj, &path);
        self.infer_type(obj, &path, is_root);

        if is_root
            && obj.contains_key("anyOf")
            && self.profile == OpenAiSchemaProfile::StructuredOutput
        {
            self.errors.push(format!(
                "{path}: OpenAI structured outputs do not allow root anyOf"
            ));
        }

        self.reject_unsupported_keywords(obj, &path);

        if schema_type_contains(obj, "object") {
            self.project_object(obj, &path);
        }

        if let Some(items) = obj.get_mut("items") {
            self.project_value(items, path.child("items"), false);
        }

        for key in ["$defs", "definitions"] {
            if let Some(defs) = obj.get_mut(key).and_then(Value::as_object_mut) {
                for (name, schema) in defs {
                    self.project_value(schema, path.child(key).child(name), false);
                }
            }
        }

        if let Some(any_of) = obj.get_mut("anyOf").and_then(Value::as_array_mut) {
            for (idx, schema) in any_of.iter_mut().enumerate() {
                self.project_value(schema, path.child("anyOf").index(idx), false);
            }
        }
    }

    fn ensure_object_root(&mut self, value: &mut Value) {
        let Some(obj) = value.as_object_mut() else {
            return;
        };
        if !schema_type_contains(obj, "object") {
            self.errors
                .push("$: root schema must be an object schema".to_string());
            return;
        }
        obj.entry("properties")
            .or_insert_with(|| Value::Object(Map::new()));
        if !obj.get("properties").is_some_and(Value::is_object) {
            self.errors
                .push("$: object schema properties must be an object".to_string());
        }
    }

    fn project_object(&mut self, obj: &mut Map<String, Value>, path: &Path) {
        let originally_required = required_set(obj);
        match obj.get_mut("properties") {
            Some(Value::Object(properties)) => {
                let property_names = properties.keys().cloned().collect::<Vec<_>>();
                for (name, schema) in properties.iter_mut() {
                    let optional = !originally_required.iter().any(|required| required == name);
                    self.project_value(schema, path.child("properties").child(name), false);
                    if optional && self.requires_strict_objects() {
                        make_nullable(schema);
                    }
                }
                if self.requires_strict_objects() {
                    obj.insert(
                        "required".to_string(),
                        Value::Array(property_names.into_iter().map(Value::String).collect()),
                    );
                }
            }
            Some(_) => self.errors.push(format!(
                "{path}: object schema properties must be an object"
            )),
            None => {
                obj.insert("properties".to_string(), Value::Object(Map::new()));
                self.diagnostics
                    .push(format!("{path}: inserted missing object properties"));
                if self.requires_strict_objects() {
                    obj.insert("required".to_string(), Value::Array(Vec::new()));
                }
            }
        }

        if self.requires_strict_objects() {
            obj.insert("additionalProperties".to_string(), Value::Bool(false));
        }
    }

    fn requires_strict_objects(&self) -> bool {
        matches!(
            self.profile,
            OpenAiSchemaProfile::StrictToolParameters | OpenAiSchemaProfile::StructuredOutput
        )
    }

    fn flatten_single_all_of(&mut self, obj: &mut Map<String, Value>, path: &Path) {
        let Some(all_of) = obj.remove("allOf") else {
            return;
        };
        let Value::Array(mut branches) = all_of else {
            obj.insert("allOf".to_string(), all_of);
            return;
        };
        if branches.len() != 1 {
            obj.insert("allOf".to_string(), Value::Array(branches));
            return;
        }

        let branch = branches.pop().expect("single allOf branch");
        let Value::Object(branch_obj) = branch else {
            obj.insert("allOf".to_string(), Value::Array(vec![branch]));
            self.errors.push(format!(
                "{path}: single-branch allOf must contain an object schema"
            ));
            return;
        };

        let conflicts = branch_obj
            .iter()
            .filter_map(|(key, value)| {
                obj.get(key)
                    .filter(|existing| *existing != value)
                    .map(|_| key.clone())
            })
            .collect::<Vec<_>>();
        if !conflicts.is_empty() {
            obj.insert(
                "allOf".to_string(),
                Value::Array(vec![Value::Object(branch_obj)]),
            );
            self.errors.push(format!(
                "{path}: single-branch allOf conflicts with sibling schema keys: {}",
                conflicts.join(", ")
            ));
            return;
        }

        for (key, value) in branch_obj {
            obj.entry(key).or_insert(value);
        }
        self.diagnostics
            .push(format!("{path}: flattened single-branch allOf"));
    }

    fn convert_const(&mut self, obj: &mut Map<String, Value>, path: &Path) {
        if let Some(value) = obj.remove("const") {
            obj.entry("enum".to_string())
                .or_insert_with(|| Value::Array(vec![value]));
            self.diagnostics
                .push(format!("{path}: converted const to single-value enum"));
        }
    }

    fn infer_type(&mut self, obj: &mut Map<String, Value>, path: &Path, is_root: bool) {
        if obj.contains_key("type") {
            return;
        }
        let inferred = if is_root
            || obj.contains_key("properties")
            || obj.contains_key("required")
            || obj.contains_key("additionalProperties")
        {
            Some("object")
        } else if obj.contains_key("items") {
            Some("array")
        } else if let Some(enum_values) = obj.get("enum").and_then(Value::as_array) {
            infer_enum_type(enum_values)
        } else {
            None
        };
        if let Some(inferred) = inferred {
            obj.insert("type".to_string(), Value::String(inferred.to_string()));
            self.diagnostics
                .push(format!("{path}: inferred missing type as {inferred}"));
        }
    }

    fn reject_unsupported_keywords(&mut self, obj: &Map<String, Value>, path: &Path) {
        let unsupported = [
            "allOf",
            "oneOf",
            "not",
            "dependentRequired",
            "dependentSchemas",
            "if",
            "then",
            "else",
            "patternProperties",
        ];
        for key in unsupported {
            if obj.contains_key(key) {
                self.errors
                    .push(format!("{path}: unsupported JSON Schema keyword `{key}`"));
            }
        }
    }
}

struct AnthropicBedrockSanitizer {
    diagnostics: Vec<String>,
    errors: Vec<String>,
}

impl AnthropicBedrockSanitizer {
    fn sanitize_value(&mut self, value: &mut Value, path: Path) {
        let Some(obj) = value.as_object_mut() else {
            self.errors
                .push(format!("{path}: schema must be a JSON object"));
            return;
        };

        self.convert_const(obj, &path);
        self.infer_rootless_type(obj, &path);
        self.strip_validation_keywords(obj, &path);

        if let Some(properties) = obj.get_mut("properties") {
            let Some(properties) = properties.as_object_mut() else {
                self.errors
                    .push(format!("{path}.properties: properties must be an object"));
                return;
            };
            for (name, property) in properties {
                self.sanitize_value(property, path.child("properties").child(name));
            }
        }

        if let Some(items) = obj.get_mut("items") {
            match items {
                Value::Array(values) => {
                    for (idx, item) in values.iter_mut().enumerate() {
                        self.sanitize_value(item, path.child("items").index(idx));
                    }
                }
                _ => self.sanitize_value(items, path.child("items")),
            }
        }

        for key in ["$defs", "definitions"] {
            if let Some(defs) = obj.get_mut(key).and_then(Value::as_object_mut) {
                for (name, schema) in defs {
                    self.sanitize_value(schema, path.child(key).child(name));
                }
            }
        }

        for key in ["anyOf", "oneOf", "allOf"] {
            if let Some(values) = obj.get_mut(key).and_then(Value::as_array_mut) {
                for (idx, schema) in values.iter_mut().enumerate() {
                    self.sanitize_value(schema, path.child(key).index(idx));
                }
            }
        }
    }

    fn ensure_object_root(&mut self, value: &mut Value) {
        let Some(obj) = value.as_object_mut() else {
            return;
        };
        obj.entry("type".to_string())
            .or_insert_with(|| Value::String("object".to_string()));
        if !schema_type_contains(obj, "object") {
            self.errors
                .push("$: root schema must be an object schema".to_string());
            return;
        }
        obj.entry("properties".to_string())
            .or_insert_with(|| Value::Object(Map::new()));
    }

    fn convert_const(&mut self, obj: &mut Map<String, Value>, path: &Path) {
        if let Some(value) = obj.remove("const") {
            obj.entry("enum".to_string())
                .or_insert_with(|| Value::Array(vec![value]));
            self.diagnostics
                .push(format!("{path}: converted const to single-value enum"));
        }
    }

    fn infer_rootless_type(&mut self, obj: &mut Map<String, Value>, path: &Path) {
        if obj.contains_key("type") {
            return;
        }
        let inferred = if obj.contains_key("properties")
            || obj.contains_key("required")
            || obj.contains_key("additionalProperties")
        {
            Some("object")
        } else if obj.contains_key("items") {
            Some("array")
        } else if let Some(enum_values) = obj.get("enum").and_then(Value::as_array) {
            infer_enum_type(enum_values)
        } else {
            None
        };
        if let Some(inferred) = inferred {
            obj.insert("type".to_string(), Value::String(inferred.to_string()));
            self.diagnostics
                .push(format!("{path}: inferred missing type as {inferred}"));
        }
    }

    fn strip_validation_keywords(&mut self, obj: &mut Map<String, Value>, path: &Path) {
        let strip_keys = [
            "minItems",
            "maxItems",
            "uniqueItems",
            "minLength",
            "maxLength",
            "pattern",
            "format",
            "minimum",
            "maximum",
            "exclusiveMinimum",
            "exclusiveMaximum",
            "multipleOf",
        ];
        let mut stripped = Vec::new();
        for key in strip_keys {
            if let Some(value) = obj.remove(key) {
                stripped.push(format!("{key}={}", compact_value(&value)));
                self.diagnostics.push(format!(
                    "{path}: removed unsupported provider schema keyword `{key}`"
                ));
            }
        }
        if !stripped.is_empty() {
            append_description_constraint(
                obj,
                &format!("Provider compatibility note: {}", stripped.join(", ")),
            );
        }
    }
}

fn append_description_constraint(obj: &mut Map<String, Value>, note: &str) {
    match obj.get_mut("description") {
        Some(Value::String(description)) if !description.trim().is_empty() => {
            if !description.ends_with('.') {
                description.push('.');
            }
            description.push(' ');
            description.push_str(note);
        }
        _ => {
            obj.insert("description".to_string(), Value::String(note.to_string()));
        }
    }
}

fn compact_value(value: &Value) -> String {
    match value {
        Value::String(value) => format!("{value:?}"),
        _ => value.to_string(),
    }
}

#[derive(Clone, Debug)]
struct Path(String);

impl Path {
    fn root() -> Self {
        Self("$".to_string())
    }

    fn child(&self, segment: &str) -> Self {
        Self(format!("{}.{}", self.0, segment))
    }

    fn index(&self, index: usize) -> Self {
        Self(format!("{}[{index}]", self.0))
    }
}

impl std::fmt::Display for Path {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

fn schema_type_contains(obj: &Map<String, Value>, expected: &str) -> bool {
    match obj.get("type") {
        Some(Value::String(value)) => value == expected,
        Some(Value::Array(values)) => values.iter().any(|value| value.as_str() == Some(expected)),
        _ => false,
    }
}

fn required_set(obj: &Map<String, Value>) -> Vec<String> {
    obj.get("required")
        .and_then(Value::as_array)
        .map(|values| {
            values
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

fn infer_enum_type(values: &[Value]) -> Option<&'static str> {
    let mut inferred = None;
    for value in values {
        let value_type = match value {
            Value::String(_) => "string",
            Value::Bool(_) => "boolean",
            Value::Number(number) if number.is_i64() || number.is_u64() => "integer",
            Value::Number(_) => "number",
            Value::Null => continue,
            _ => return None,
        };
        match inferred {
            Some(existing) if existing != value_type => return None,
            Some(_) => {}
            None => inferred = Some(value_type),
        }
    }
    inferred
}

fn make_nullable(schema: &mut Value) {
    let Some(obj) = schema.as_object_mut() else {
        return;
    };
    if let Some(any_of) = obj.get_mut("anyOf").and_then(Value::as_array_mut) {
        if !any_of.iter().any(is_null_schema) {
            any_of.push(json!({ "type": "null" }));
        }
        return;
    }
    match obj.get_mut("type") {
        Some(Value::String(value)) if value != "null" => {
            let original = value.clone();
            obj.insert(
                "type".to_string(),
                Value::Array(vec![
                    Value::String(original),
                    Value::String("null".to_string()),
                ]),
            );
        }
        Some(Value::Array(values))
            if !values.iter().any(|value| value.as_str() == Some("null")) =>
        {
            values.push(Value::String("null".to_string()));
        }
        None => {
            let original = Value::Object(obj.clone());
            obj.clear();
            obj.insert(
                "anyOf".to_string(),
                Value::Array(vec![original, json!({ "type": "null" })]),
            );
        }
        _ => {}
    }
}

fn is_null_schema(value: &Value) -> bool {
    value
        .as_object()
        .is_some_and(|obj| schema_type_contains(obj, "null"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn required_names(schema: &Value) -> Vec<String> {
        let mut names = schema["required"]
            .as_array()
            .unwrap()
            .iter()
            .map(|value| value.as_str().unwrap().to_string())
            .collect::<Vec<_>>();
        names.sort();
        names
    }

    #[test]
    fn projection_does_not_mutate_canonical_schema() {
        let schema = json!({"type": "object"});
        let projected = project_tool_parameters(&schema).unwrap();
        assert_eq!(schema, json!({"type": "object"}));
        assert_eq!(projected.schema["properties"], json!({}));
    }

    #[test]
    fn tool_parameters_repairs_empty_root_object() {
        let projected = project_tool_parameters(&json!({})).unwrap();
        assert_eq!(projected.schema["type"], "object");
        assert_eq!(projected.schema["properties"], json!({}));
        assert!(projected.diagnostics.iter().any(|d| d.contains("inferred")));
    }

    #[test]
    fn tool_parameters_repairs_missing_properties_missing_type_and_const() {
        let schema = json!({
            "properties": {
                "mode": { "const": "fast" }
            }
        });
        let projected = project_tool_parameters(&schema).unwrap();
        assert_eq!(projected.schema["type"], "object");
        assert_eq!(
            projected.schema["properties"]["mode"]["enum"],
            json!(["fast"])
        );
        assert!(
            projected.schema["properties"]["mode"]
                .get("const")
                .is_none()
        );
    }

    #[test]
    fn tool_parameters_infers_array_and_enum_types() {
        let schema = json!({
            "type": "object",
            "properties": {
                "tags": { "items": { "type": "string" } },
                "level": { "enum": [1, 2, 3] }
            }
        });
        let projected = project_tool_parameters(&schema).unwrap();
        assert_eq!(projected.schema["properties"]["tags"]["type"], "array");
        assert_eq!(projected.schema["properties"]["level"]["type"], "integer");
    }

    #[test]
    fn strict_projection_requires_optional_nullable_properties() {
        let schema = json!({
            "type": "object",
            "properties": {
                "required_name": { "type": "string" },
                "optional_count": { "type": "integer" }
            },
            "required": ["required_name"]
        });
        let projected = project_strict_tool_parameters(&schema).unwrap();
        assert_eq!(
            required_names(&projected.schema),
            vec!["optional_count", "required_name"]
        );
        assert_eq!(
            projected.schema["properties"]["optional_count"]["type"],
            json!(["integer", "null"])
        );
        assert_eq!(projected.schema["additionalProperties"], false);
    }

    #[test]
    fn strict_projection_preserves_required_nonnullable_fields() {
        let schema = json!({
            "type": "object",
            "properties": {
                "name": { "type": "string" },
                "age": { "type": "integer" }
            },
            "required": ["name", "age"]
        });
        let projected = project_strict_tool_parameters(&schema).unwrap();
        assert_eq!(projected.schema["properties"]["name"]["type"], "string");
        assert_eq!(projected.schema["properties"]["age"]["type"], "integer");
    }

    #[test]
    fn strict_projection_does_not_duplicate_existing_nullable_type() {
        let schema = json!({
            "type": "object",
            "properties": {
                "name": { "type": ["string", "null"] }
            }
        });
        let projected = project_strict_tool_parameters(&schema).unwrap();
        assert_eq!(
            projected.schema["properties"]["name"]["type"],
            json!(["string", "null"])
        );
    }

    #[test]
    fn strict_projection_adds_null_branch_to_optional_any_of() {
        let schema = json!({
            "type": "object",
            "properties": {
                "value": {
                    "anyOf": [
                        { "type": "string" },
                        { "type": "integer" }
                    ]
                }
            }
        });
        let projected = project_strict_tool_parameters(&schema).unwrap();
        assert_eq!(
            projected.schema["properties"]["value"]["anyOf"][2],
            json!({ "type": "null" })
        );
    }

    #[test]
    fn strict_projection_flattens_single_branch_all_of_before_nullable() {
        let schema = json!({
            "type": "object",
            "properties": {
                "limit": {
                    "description": "Maximum number of results.",
                    "allOf": [
                        {
                            "type": "integer",
                            "minimum": 1,
                            "maximum": 100
                        }
                    ]
                }
            }
        });
        let projected = project_strict_tool_parameters(&schema).unwrap();
        let limit = &projected.schema["properties"]["limit"];
        assert!(limit.get("allOf").is_none());
        assert_eq!(limit["description"], "Maximum number of results.");
        assert_eq!(limit["type"], json!(["integer", "null"]));
        assert_eq!(limit["minimum"], 1);
        assert_eq!(limit["maximum"], 100);
        assert!(
            projected
                .diagnostics
                .iter()
                .any(|diagnostic| diagnostic.contains("flattened single-branch allOf"))
        );
    }

    #[test]
    fn structured_output_enforces_strict_objects() {
        let schema = json!({
            "type": "object",
            "properties": {
                "value": { "type": "string" }
            }
        });
        let projected = project_structured_output(&schema).unwrap();
        assert_eq!(projected.schema["required"], json!(["value"]));
        assert_eq!(projected.schema["additionalProperties"], false);
    }

    #[test]
    fn structured_output_recurses_into_nested_objects_and_arrays() {
        let schema = json!({
            "type": "object",
            "properties": {
                "items": {
                    "type": "array",
                    "items": {
                        "type": "object",
                        "properties": {
                            "title": { "type": "string" },
                            "score": { "type": "number" }
                        },
                        "required": ["title"]
                    }
                }
            }
        });
        let projected = project_structured_output(&schema).unwrap();
        let nested = &projected.schema["properties"]["items"]["items"];
        assert_eq!(nested["additionalProperties"], false);
        assert_eq!(required_names(nested), vec!["score", "title"]);
        assert_eq!(
            nested["properties"]["score"]["type"],
            json!(["number", "null"])
        );
    }

    #[test]
    fn structured_output_recurses_into_defs_and_definitions() {
        let schema = json!({
            "type": "object",
            "properties": {
                "item": { "$ref": "#/$defs/Item" },
                "legacy": { "$ref": "#/definitions/Legacy" }
            },
            "$defs": {
                "Item": {
                    "type": "object",
                    "properties": { "id": { "type": "string" } }
                }
            },
            "definitions": {
                "Legacy": {
                    "type": "object",
                    "properties": { "flag": { "type": "boolean" } }
                }
            }
        });
        let projected = project_structured_output(&schema).unwrap();
        assert_eq!(
            projected.schema["$defs"]["Item"]["additionalProperties"],
            false
        );
        assert_eq!(projected.schema["$defs"]["Item"]["required"], json!(["id"]));
        assert_eq!(
            projected.schema["definitions"]["Legacy"]["additionalProperties"],
            false
        );
        assert_eq!(
            projected.schema["definitions"]["Legacy"]["required"],
            json!(["flag"])
        );
    }

    #[test]
    fn structured_output_allows_nested_any_of_and_projects_branches() {
        let schema = json!({
            "type": "object",
            "properties": {
                "value": {
                    "anyOf": [
                        { "type": "string" },
                        {
                            "type": "object",
                            "properties": { "count": { "type": "integer" } }
                        }
                    ]
                }
            }
        });
        let projected = project_structured_output(&schema).unwrap();
        let object_branch = &projected.schema["properties"]["value"]["anyOf"][1];
        assert_eq!(object_branch["required"], json!(["count"]));
        assert_eq!(object_branch["additionalProperties"], false);
        assert_eq!(
            projected.schema["properties"]["value"]["anyOf"][2],
            json!({ "type": "null" })
        );
    }

    #[test]
    fn structured_output_rejects_root_scalar() {
        let err = project_structured_output(&json!({ "type": "string" })).unwrap_err();
        assert!(
            err.diagnostics
                .iter()
                .any(|diagnostic| diagnostic.contains("root schema must be an object"))
        );
    }

    #[test]
    fn structured_output_rejects_root_any_of() {
        let err = project_structured_output(&json!({
            "anyOf": [
                { "type": "object", "properties": {} },
                { "type": "object", "properties": {} }
            ]
        }))
        .unwrap_err();
        assert!(
            err.diagnostics
                .iter()
                .any(|diagnostic| diagnostic.contains("root anyOf"))
        );
    }

    #[test]
    fn projection_rejects_unsupported_lossy_keywords() {
        let err = project_structured_output(&json!({
            "type": "object",
            "properties": {},
            "allOf": [
                { "type": "object", "properties": {} },
                { "type": "object", "properties": {} }
            ],
            "patternProperties": {}
        }))
        .unwrap_err();
        assert!(
            err.diagnostics
                .iter()
                .any(|diagnostic| diagnostic.contains("allOf"))
        );
        assert!(
            err.diagnostics
                .iter()
                .any(|diagnostic| diagnostic.contains("patternProperties"))
        );
    }

    #[test]
    fn projection_rejects_non_object_properties() {
        let err = project_tool_parameters(&json!({
            "type": "object",
            "properties": []
        }))
        .unwrap_err();
        assert!(
            err.diagnostics
                .iter()
                .any(|diagnostic| diagnostic.contains("properties must be an object"))
        );
    }

    #[test]
    fn resolver_auto_prefers_explicit_override_for_matching_dialect() {
        let contract = SchemaContract::new(json!({
            "type": "object",
            "properties": { "raw": { "const": "x" } }
        }))
        .with_override(
            SchemaDialect::OPENAI_TOOL_PARAMETERS,
            json!({
                "type": "object",
                "properties": { "raw": { "type": "string", "enum": ["x"] } }
            }),
        );

        let resolved = resolve_schema(
            &contract,
            SchemaResolutionRequest {
                provider: "test",
                purpose: SchemaPurpose::ToolInput,
                dialects: &[SchemaDialect::openai_tool_parameters()],
            },
        )
        .unwrap();

        assert_eq!(
            resolved.schema["properties"]["raw"],
            json!({ "type": "string", "enum": ["x"] })
        );
    }

    #[test]
    fn resolver_explicit_only_fails_without_matching_override() {
        let mut contract = SchemaContract::new(json!({
            "type": "object",
            "properties": {}
        }));
        contract.projection.mode = ProjectionMode::ExplicitOnly;

        let err = resolve_schema(
            &contract,
            SchemaResolutionRequest {
                provider: "test",
                purpose: SchemaPurpose::StructuredOutput,
                dialects: &[SchemaDialect::openai_structured_output()],
            },
        )
        .unwrap_err();

        assert!(
            err.diagnostics
                .iter()
                .any(|diagnostic| diagnostic.contains("no explicit projection override"))
        );
    }

    #[test]
    fn bedrock_projection_strips_array_constraints_from_wire_schema_only() {
        let contract = SchemaContract::new(json!({
            "type": "object",
            "required": ["ranked"],
            "properties": {
                "ranked": {
                    "type": "array",
                    "minItems": 3,
                    "maxItems": 3,
                    "items": { "type": "string" }
                }
            }
        }));

        let resolved = resolve_schema(
            &contract,
            SchemaResolutionRequest {
                provider: "test",
                purpose: SchemaPurpose::StructuredOutput,
                dialects: &[SchemaDialect::bedrock_claude_output_config_json_schema()],
            },
        )
        .unwrap();

        let ranked = &resolved.schema["properties"]["ranked"];
        assert!(ranked.get("minItems").is_none());
        assert!(ranked.get("maxItems").is_none());
        assert_eq!(contract.canonical["properties"]["ranked"]["minItems"], 3);
        assert!(
            ranked["description"]
                .as_str()
                .is_some_and(|description| description.contains("minItems=3"))
        );
    }
}
