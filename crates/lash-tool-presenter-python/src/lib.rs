//! The Python tool presenter: how a Python code-mode session's tools read.
//! Each tool is an `async def` signature over dict types with the notes of
//! the fields that say more than their type, listed by the shared catalogue
//! listing. Authored examples are written in the call shape of dotted paths,
//! which no Python cell writes, so this presenter shows none.
//!
//! The mode validates every call path before it gets here; this crate only
//! spells.

use lash_sansio::{JsonSchema, ToolContract, ToolManifest};
use lash_tool_presenter_api::{
    PresentationRefusal, PresentedToolSection, ToolPresentationEntry, ToolPresentationRequest,
    ToolPresenter, listing,
};
use lash_tool_shape::{ExtraKeys, ObjectShape, SchemaShape, ShapeKind};
use serde_json::Value;

/// The Python presenter's id.
pub const PYTHON_TOOL_PRESENTER_ID: &str = "lash.tools.python.v1";

/// The Python presenter's config: its prompt-facing schema cut. Validation
/// always reads the full schema.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PythonPresenterConfig {
    pub schema_depth: usize,
}

impl Default for PythonPresenterConfig {
    fn default() -> Self {
        Self::standard()
    }
}

impl PythonPresenterConfig {
    /// Standard preset: eight schema container levels. A historical cut with
    /// no universal workload measurement.
    pub const fn standard() -> Self {
        Self { schema_depth: 8 }
    }
}

/// The Python presenter code-mode sessions ship with.
#[derive(Clone, Copy, Debug, Default)]
pub struct PythonToolPresenter;

impl PythonToolPresenter {
    fn config(config: &Value) -> Result<PythonPresenterConfig, PresentationRefusal> {
        lash_tool_presenter_api::read_config(PYTHON_TOOL_PRESENTER_ID, config)
    }

    fn doc(tool: &ToolPresentationEntry<'_>, config: PythonPresenterConfig) -> String {
        let input = &tool.contract.input_schema.canonical;
        let output = &tool.contract.output_schema.canonical;
        listing::code_mode_tool_doc(
            &signature(tool.call_path, input, output, config.schema_depth),
            tool.manifest,
            &SchemaShape::from_json_schema(input.as_value(), config.schema_depth),
            &SchemaShape::from_json_schema(output.as_value(), config.schema_depth),
            &render_shape,
            &[],
        )
    }
}

impl ToolPresenter for PythonToolPresenter {
    fn id(&self) -> &str {
        PYTHON_TOOL_PRESENTER_ID
    }

    fn admit_config(&self, raw: &Value) -> Result<Value, PresentationRefusal> {
        lash_tool_presenter_api::admit_over_standard(
            PYTHON_TOOL_PRESENTER_ID,
            &PythonPresenterConfig::standard(),
            raw,
        )
    }

    fn present(
        &self,
        request: ToolPresentationRequest<'_>,
    ) -> Result<PresentedToolSection, PresentationRefusal> {
        let config = Self::config(request.config)?;
        let docs = request
            .tools
            .iter()
            .map(|tool| Self::doc(tool, config))
            .collect();
        Ok(listing::list(request.tools, docs, request.disclosure))
    }

    fn tool_doc(
        &self,
        tool: &ToolPresentationEntry<'_>,
        config: &Value,
    ) -> Result<String, PresentationRefusal> {
        Ok(Self::doc(tool, Self::config(config)?))
    }

    fn tool_signature(
        &self,
        call_path: &str,
        input: &JsonSchema,
        output: &JsonSchema,
        config: &Value,
    ) -> Result<String, PresentationRefusal> {
        Ok(signature(
            call_path,
            input,
            output,
            Self::config(config)?.schema_depth,
        ))
    }

    fn discovery_text(
        &self,
        manifest: &ToolManifest,
        contract: &ToolContract,
        config: &Value,
    ) -> Result<String, PresentationRefusal> {
        Ok(listing::discovery_text(
            manifest,
            contract,
            Self::config(config)?.schema_depth,
        ))
    }
}

fn signature(call_path: &str, input: &JsonSchema, output: &JsonSchema, depth: usize) -> String {
    tool_signature(
        call_path,
        &SchemaShape::from_json_schema(input.as_value(), depth),
        input.accepts_empty_object(),
        &SchemaShape::from_json_schema(output.as_value(), depth),
    )
}

/// One named definition: `Name = <type>`.
///
/// ```rust,ignore
/// let shape = SchemaShape::from_json_schema(&json!({"type": "string"}), 8);
/// assert_eq!(schema_definition("Query", &shape), "Query = str");
/// ```
pub fn schema_definition(name: &str, shape: &SchemaShape) -> String {
    format!("{name} = {}", render_shape(shape))
}

/// The note rows of `shape`'s fields that say more than their type, with
/// their types in Python: `- `limit?: int` (>= 1) — Page size.`
///
/// ```rust,ignore
/// let rows = note_rows(&SchemaShape::from_json_schema(&input_schema, 8));
/// ```
pub fn note_rows(shape: &SchemaShape) -> Vec<String> {
    listing::noted_field_rows(shape, &render_shape)
}

/// A tool's callable signature in Python. When `input_omittable`, the tool's
/// input schema accepts the empty object, and a call may leave the input out
/// to pass exactly `{}`.
///
/// ```rust,ignore
/// let signature = tool_signature("tools.lookup", &input, false, &output);
/// assert_eq!(signature, r#"async def tools.lookup(input: {"query": str}) -> str"#);
/// ```
pub fn tool_signature(
    call_path: &str,
    input: &SchemaShape,
    input_omittable: bool,
    output: &SchemaShape,
) -> String {
    // An omitted input is exactly `{}`: the default the call shows.
    format!(
        "async def {call_path}(input: {}{}) -> {}",
        render_shape(input),
        if input_omittable { " = {}" } else { "" },
        render_shape(output)
    )
}

/// Spells a schema shape as a Python type.
///
/// ```rust,ignore
/// let shape = SchemaShape::from_json_schema(&json!({"type": "array", "items": {"type": "integer"}}), 8);
/// assert_eq!(render_shape(&shape), "list[int]");
/// ```
pub fn render_shape(shape: &SchemaShape) -> String {
    match &shape.kind {
        ShapeKind::Unknown => "Any".to_string(),
        ShapeKind::Null => "None".to_string(),
        ShapeKind::Bool => "bool".to_string(),
        ShapeKind::Int => "int".to_string(),
        ShapeKind::Float => "float".to_string(),
        ShapeKind::Str => "str".to_string(),
        ShapeKind::Literals(values) => format!(
            "Literal[{}]",
            values
                .iter()
                .map(serde_json::Value::to_string)
                .collect::<Vec<_>>()
                .join(", ")
        ),
        ShapeKind::List(item) => format!("list[{}]", render_shape(item)),
        ShapeKind::Tuple(items) => format!(
            "tuple[{}]",
            items
                .iter()
                .map(render_shape)
                .collect::<Vec<_>>()
                .join(", ")
        ),
        ShapeKind::Object(object) => render_object(object),
        ShapeKind::Union(members) => {
            let mut spelled = Vec::<String>::new();
            for member in members.iter().map(render_shape) {
                if !spelled.contains(&member) {
                    spelled.push(member);
                }
            }
            spelled.join(" | ")
        }
        ShapeKind::Named(name) => name.replace('.', "_"),
        // `ShapeKind` is non-exhaustive: a kind this dialect has no spelling
        // for is shown as the widest type rather than guessed at.
        _ => "Any".to_string(),
    }
}

/// A record as the dict it is in a cell: its keys with their types, a key
/// that may be absent marked `?`.
fn render_object(object: &ObjectShape) -> String {
    let extra = match &object.extra_keys {
        ExtraKeys::Closed => None,
        ExtraKeys::Unstated if object.fields.is_empty() => Some("Any".to_string()),
        ExtraKeys::Unstated => None,
        ExtraKeys::Open(extra) => Some(render_shape(extra)),
        _ => Some("Any".to_string()),
    };
    if object.fields.is_empty() {
        return format!(
            "dict[str, {}]",
            extra.unwrap_or_else(|| "Never".to_string())
        );
    }
    let mut members = object
        .fields
        .iter()
        .map(|field| {
            format!(
                "{}{}: {}",
                serde_json::Value::String(field.name.clone()),
                if field.required { "" } else { "?" },
                render_shape(&field.shape)
            )
        })
        .collect::<Vec<_>>();
    if let Some(extra) = extra {
        members.push(format!("str: {extra}"));
    }
    format!("{{{}}}", members.join(", "))
}
