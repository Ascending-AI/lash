//! The TypeScript tool presenter: how a TypeScript code-mode session's tools
//! read. Each tool is a typed call signature with the notes of the fields
//! that say more than their type, listed by the shared catalogue listing.
//!
//! The mode validates every call path before it gets here; this crate only
//! spells.

mod types;

use lash_sansio::{JsonSchema, ToolContract, ToolManifest};
use lash_tool_presenter_api::{
    PresentationRefusal, PresentedToolSection, ToolPresentationEntry, ToolPresentationRequest,
    ToolPresenter, listing,
};
use lash_tool_shape::SchemaShape;
use serde_json::Value;

pub use types::{
    is_identifier, is_reserved_word, render_schema_shape, render_type_name, reserved_words,
};

/// The TypeScript presenter's id.
pub const TYPESCRIPT_TOOL_PRESENTER_ID: &str = "lash.tools.typescript.v1";

/// The TypeScript presenter's config: its prompt-facing schema and example
/// cuts. Validation always reads the full schema.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TypescriptPresenterConfig {
    pub example_limit: usize,
    pub example_chars: usize,
    pub schema_depth: usize,
}

impl Default for TypescriptPresenterConfig {
    fn default() -> Self {
        Self::standard()
    }
}

impl TypescriptPresenterConfig {
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

/// The TypeScript presenter code-mode sessions ship with.
#[derive(Clone, Copy, Debug, Default)]
pub struct TypescriptToolPresenter;

impl TypescriptToolPresenter {
    fn config(config: &Value) -> Result<TypescriptPresenterConfig, PresentationRefusal> {
        lash_tool_presenter_api::read_config(TYPESCRIPT_TOOL_PRESENTER_ID, config)
    }

    fn doc(
        tool: &ToolPresentationEntry<'_>,
        config: TypescriptPresenterConfig,
    ) -> Result<String, PresentationRefusal> {
        let input = &tool.contract.input_schema.canonical;
        let output = &tool.contract.output_schema.canonical;
        let signature = signature(tool.call_path, input, output, config.schema_depth)?;
        // Authored examples are Lash VM source; this presenter spells them.
        let examples = listing::compact_examples(
            &tool.contract.examples,
            config.example_limit,
            config.example_chars,
        )
        .iter()
        .map(|example| render_tool_example(example))
        .collect::<Vec<_>>();
        Ok(listing::code_mode_tool_doc(
            &signature,
            tool.manifest,
            &lash_tool_presenter_api::inline_schema(input.as_value(), config.schema_depth)?,
            &lash_tool_presenter_api::inline_schema(output.as_value(), config.schema_depth)?,
            &render_schema_shape,
            &examples,
        ))
    }
}

impl ToolPresenter for TypescriptToolPresenter {
    fn id(&self) -> &str {
        TYPESCRIPT_TOOL_PRESENTER_ID
    }

    fn admit_config(&self, raw: &Value) -> Result<Value, PresentationRefusal> {
        lash_tool_presenter_api::admit_over_standard(
            TYPESCRIPT_TOOL_PRESENTER_ID,
            &TypescriptPresenterConfig::standard(),
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
            .collect::<Result<Vec<_>, _>>()?;
        Ok(listing::list(request.tools, docs, request.disclosure))
    }

    fn tool_doc(
        &self,
        tool: &ToolPresentationEntry<'_>,
        config: &Value,
    ) -> Result<String, PresentationRefusal> {
        Self::doc(tool, Self::config(config)?)
    }

    fn tool_signature(
        &self,
        call_path: &str,
        input: &JsonSchema,
        output: &JsonSchema,
        config: &Value,
    ) -> Result<String, PresentationRefusal> {
        signature(call_path, input, output, Self::config(config)?.schema_depth)
    }

    fn discovery_text(
        &self,
        manifest: &ToolManifest,
        contract: &ToolContract,
        config: &Value,
    ) -> Result<String, PresentationRefusal> {
        listing::discovery_text(manifest, contract, Self::config(config)?.schema_depth)
    }
}

fn signature(
    call_path: &str,
    input: &JsonSchema,
    output: &JsonSchema,
    depth: usize,
) -> Result<String, PresentationRefusal> {
    Ok(tool_signature(
        call_path,
        &lash_tool_presenter_api::inline_schema(input.as_value(), depth)?,
        input.accepts_empty_object(),
        &lash_tool_presenter_api::inline_schema(output.as_value(), depth)?,
    ))
}

/// One named definition: `type Name = <type>`.
///
/// ```rust,ignore
/// let shape = lash_tool_presenter_api::inline_schema(&json!({"type": "string"}), 8)?;
/// assert_eq!(schema_definition("Query", &shape), "type Query = string");
/// ```
pub fn schema_definition(name: &str, shape: &SchemaShape) -> String {
    format!("type {name} = {}", render_schema_shape(shape))
}

/// The note rows of `shape`'s fields that say more than their type, with
/// their types in TypeScript: `- `limit?: number` (>= 1) — Page size.`
///
/// ```rust,ignore
/// let rows = note_rows(&lash_tool_presenter_api::inline_schema(&input_schema, 8)?);
/// ```
pub fn note_rows(shape: &SchemaShape) -> Vec<String> {
    listing::noted_field_rows(shape, &render_schema_shape)
}

/// A tool's callable signature in TypeScript. When `input_omittable`, the
/// tool's input schema accepts the empty object, and a call may leave the
/// input out to pass exactly `{}`.
///
/// ```rust,ignore
/// let signature = tool_signature("tools.lookup", &input, false, &output);
/// assert_eq!(signature, "tools.lookup({ query: string }): Promise<string>");
/// ```
pub fn tool_signature(
    call_path: &str,
    input: &SchemaShape,
    input_omittable: bool,
    output: &SchemaShape,
) -> String {
    let input = render_schema_shape(input);
    // An omitted input is exactly `{}`: the default the call shows. A
    // tool whose only input is `{}` is called with nothing.
    let input = match (input.as_str(), input_omittable) {
        ("Record<string, never>", true) => String::new(),
        ("Record<string, never>", false) => "{}".to_owned(),
        (_, true) => format!("{input} = {{}}"),
        (_, false) => input,
    };
    let output = render_schema_shape(output);
    format!("{call_path}({input}): Promise<{output}>")
}

fn is_plain_identifier(text: &str) -> bool {
    !text.is_empty()
        && text
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || character == '_')
        && !text.starts_with(|character: char| character.is_ascii_digit())
}

/// Rewrites an authored Lash VM example into this dialect.
///
/// Deliberately a small, total rewriter over the shapes the authored corpus
/// actually uses rather than a translator: every example is a sequence of
/// statement lines that are either an awaited call or an assignment.
/// Anything it does not recognize still loses the try-operator
/// and gains a terminator, which is the difference between "reads like
/// TypeScript" and "is a syntax error".
///
/// It rewrites line by line, so an example whose *string literal* spans a
/// real newline would have a terminator inserted inside the literal. No
/// authored example does that (they escape it as `\n`), and the walker
/// parses every rendered example, so the day one does the check fails
/// rather than the model reading a syntax error.
///
/// ```rust,ignore
/// assert_eq!(
///     render_tool_example("page = await web.fetch({ url: \"u\" })?"),
///     "const page = await web.fetch({ url: \"u\" });"
/// );
/// ```
pub fn render_tool_example(example: &str) -> String {
    example
        .lines()
        .map(|line| {
            let trimmed = line.trim_end();
            if trimmed.is_empty() {
                return String::new();
            }
            let indent_len = trimmed.len() - trimmed.trim_start().len();
            let (indent, body) = trimmed.split_at(indent_len);
            // `expr?` — the Lash VM try-operator. TypeScript propagates a
            // rejection from `await` itself, so the operator has no twin.
            let body = body.strip_suffix('?').unwrap_or(body);
            let body = match body.split_once(" = ") {
                Some((name, value)) if is_plain_identifier(name) => {
                    format!("const {name} = {value}")
                }
                _ => body.to_string(),
            };
            let body = if body.ends_with(';') || body.ends_with('{') || body.ends_with(',') {
                body
            } else {
                format!("{body};")
            };
            format!("{indent}{body}")
        })
        .collect::<Vec<_>>()
        .join("\n")
}
