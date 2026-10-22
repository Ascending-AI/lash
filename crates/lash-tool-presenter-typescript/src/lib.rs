//! The TypeScript tool presenter: how a TypeScript code-mode session's tools
//! read. Each tool is a typed call signature with the notes of the fields
//! that say more than their type, listed by the shared catalogue listing.
//!
//! The mode validates every call path before it gets here; this crate only
//! spells.

mod definitions;
mod notes;
mod types;

pub use definitions::{PreparedDefinitions, TypescriptDefinitions, emit_definition};
pub use notes::note_rows;

use lash_tool_presenter_api::{
    PresentationRefusal, PresentedToolSection, ToolPresentationEntry, ToolPresentationRequest,
    ToolPresenter, import_schema, listing,
};
use lash_vocabulary::json_schema::JsonSchema;
use lash_vocabulary::tool_contract::ToolContract;
use lash_vocabulary::tool_contract::ToolManifest;
use lash_vocabulary::tool_shape::SchemaDocument;
use lash_vocabulary::tool_shape::SchemaShape;
use lash_vocabulary::tool_shape::ShapeImportRefusal;
use serde_json::Value;

pub use types::{
    DefinitionNames, is_identifier, is_reserved_word, render_schema_shape, render_type_expression,
    render_type_name, reserved_words,
};

/// The TypeScript presenter's id.
pub const TYPESCRIPT_TOOL_PRESENTER_ID: &str = "lash.tools.typescript.v1";

/// The TypeScript presenter's configuration. Schemas are imported completely
/// under the shared node budget; there is no projection-depth setting.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TypescriptPresenterConfig {}

impl TypescriptPresenterConfig {
    /// Standard preset: complete schema presentation with no optional settings.
    pub const fn standard() -> Self {
        Self {}
    }
}

/// The TypeScript presenter code-mode sessions ship with.
#[derive(Clone, Copy, Debug, Default)]
pub struct TypescriptToolPresenter;

impl TypescriptToolPresenter {
    fn config(config: &Value) -> Result<TypescriptPresenterConfig, PresentationRefusal> {
        lash_tool_presenter_api::read_config(TYPESCRIPT_TOOL_PRESENTER_ID, config)
    }

    fn doc(tool: &ToolPresentationEntry<'_>) -> Result<String, PresentationRefusal> {
        let input = import_schema(tool.contract.input_schema.canonical.as_value())?;
        let output = import_schema(tool.contract.output_schema.canonical.as_value())?;
        let mut pool = TypescriptDefinitions::new(tool.call_path.split('.'));
        pool.reserve_document(&input);
        pool.reserve_document(&output);
        render_tool_doc(tool, &input, &output, &mut pool)
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
        Self::config(request.config)?;
        let documents = request
            .tools
            .iter()
            .map(|tool| {
                Ok((
                    import_schema(tool.contract.input_schema.canonical.as_value())?,
                    import_schema(tool.contract.output_schema.canonical.as_value())?,
                ))
            })
            .collect::<Result<Vec<_>, ShapeImportRefusal>>()?;
        let initial = |selected: &[bool]| {
            let mut pool = TypescriptDefinitions::new(
                request.reserved_names.iter().map(String::as_str).chain(
                    request
                        .tools
                        .iter()
                        .flat_map(|tool| tool.call_path.split('.')),
                ),
            );
            for ((input, output), selected) in documents.iter().zip(selected) {
                if !selected {
                    continue;
                }
                pool.reserve_document(input);
                pool.reserve_document(output);
            }
            pool
        };
        listing::plan(request, initial, |index, pool| {
            let (input, output) = &documents[index];
            render_tool_doc(&request.tools[index], input, output, pool)
        })
        .map(|plan| plan.render(request.tools))
    }

    fn tool_doc(
        &self,
        tool: &ToolPresentationEntry<'_>,
        config: &Value,
    ) -> Result<String, PresentationRefusal> {
        Self::config(config)?;
        Self::doc(tool)
    }

    fn tool_signature(
        &self,
        call_path: &str,
        input: &JsonSchema,
        output: &JsonSchema,
        config: &Value,
    ) -> Result<String, PresentationRefusal> {
        Self::config(config)?;
        signature(call_path, input, output)
    }

    fn discovery_text(
        &self,
        manifest: &ToolManifest,
        contract: &ToolContract,
        config: &Value,
    ) -> Result<String, PresentationRefusal> {
        Self::config(config)?;
        let mut lines = vec![manifest.tool(), manifest.description.trim().to_string()];
        for schema in [
            &contract.input_schema.canonical,
            &contract.output_schema.canonical,
        ] {
            let document = import_schema(schema.as_value())?;
            for (shape, prefix) in document
                .roots
                .iter()
                .map(|root| (root, "".to_string()))
                .chain(
                    document
                        .reachable_closure(&document.roots)
                        .into_iter()
                        .map(|id| {
                            let definition = &document.definitions[id.0];
                            (&definition.shape, format!("{}.", definition.pointer))
                        }),
                )
            {
                if let Some(description) = &shape.description {
                    lines.push(format!("{prefix}: {description}"));
                }
                for row in shape.rows() {
                    let path = format!("{prefix}{}", row.path);
                    lines.push(match row.shape.description {
                        Some(description) => format!("{path}: {description}"),
                        None => path,
                    });
                }
            }
        }
        Ok(lines.join("\n"))
    }
}

/// Render a tool's document using a listing-local definition pool. Input and
/// output graphs must have been imported under the shared node budget; reserve
/// all surrounding host names before the first call. A fresh pool makes a
/// discovery response self-contained.
///
/// ```rust,ignore
/// let mut pool = TypescriptDefinitions::new(["HistoryItem"]);
/// pool.reserve_document(&input);
/// pool.reserve_document(&output);
/// let text = render_tool_doc(&tool, &input, &output, &mut pool)?;
/// ```
///
/// # Errors
/// Returns a typed refusal for missing references or unsupported shapes.
pub fn render_tool_doc(
    tool: &ToolPresentationEntry<'_>,
    input: &SchemaDocument,
    output: &SchemaDocument,
    pool: &mut TypescriptDefinitions,
) -> Result<String, PresentationRefusal> {
    let input_definitions = pool.prepare(input)?;
    let output_definitions = pool.prepare(output)?;
    let signature = tool_signature(
        tool.call_path,
        input,
        &input_definitions.names,
        tool.contract.input_schema.canonical.accepts_empty_object(),
        output,
        &output_definitions.names,
    )?;
    let mut sections = input_definitions.declarations;
    sections.extend(output_definitions.declarations);
    sections.push(format!("`{signature}`"));
    let description = tool.manifest.description.trim();
    if !description.is_empty() {
        sections.push(description.to_string());
    }
    for (title, document, names, path) in [
        ("Parameters", input, &input_definitions.names, "input"),
        ("Return fields", output, &output_definitions.names, "output"),
    ] {
        // A referenced # already owns its root notes in its declaration.
        if names.contains_key(&lash_vocabulary::tool_shape::DefinitionId(0)) {
            continue;
        }
        let rows = note_rows(&document.roots[0], path);
        if !rows.is_empty() {
            sections.push(format!("{title}:\n{}", rows.join("\n")));
        }
    }
    Ok(sections.join("\n"))
}

fn signature(
    call_path: &str,
    input: &JsonSchema,
    output: &JsonSchema,
) -> Result<String, PresentationRefusal> {
    let input_document = import_schema(input.as_value())?;
    let output_document = import_schema(output.as_value())?;
    let mut pool = TypescriptDefinitions::new(call_path.split('.'));
    pool.reserve_document(&input_document);
    pool.reserve_document(&output_document);
    let input_definitions = pool.prepare(&input_document)?;
    let output_definitions = pool.prepare(&output_document)?;
    let mut sections = input_definitions.declarations;
    sections.extend(output_definitions.declarations);
    sections.push(tool_signature(
        call_path,
        &input_document,
        &input_definitions.names,
        input.accepts_empty_object(),
        &output_document,
        &output_definitions.names,
    )?);
    Ok(sections.join("\n"))
}

/// One reference-free named definition (for inferred runtime shapes).
/// The declaration and every host reference apply the same identifier escaping.
///
/// ```rust,ignore
/// assert_eq!(schema_definition("Query", &ShapeKind::Str.into())?, "type Query = string");
/// ```
///
/// # Errors
/// Refuses local references without a document and unsupported kinds.
pub fn schema_definition(name: &str, shape: &SchemaShape) -> Result<String, ShapeImportRefusal> {
    Ok(format!(
        "type {} = {}",
        render_type_name(name),
        render_schema_shape(shape)?
    ))
}

/// Emit a tool signature using names allocated for both graphs. The input can
/// be omitted exactly when canonical validation accepts the empty object.
/// Include the prepared definitions beside the signature when showing it alone.
///
/// ```rust,ignore
/// let text = tool_signature("tools.lookup", &input, &input_names, false, &output, &output_names)?;
/// ```
///
/// # Errors
/// Refuses missing reference names and unsupported kinds.
pub fn tool_signature(
    call_path: &str,
    input: &SchemaDocument,
    input_names: &DefinitionNames,
    input_omittable: bool,
    output: &SchemaDocument,
    output_names: &DefinitionNames,
) -> Result<String, ShapeImportRefusal> {
    let input = definitions::root_expression(input, input_names)?;
    let input = match (input.as_str(), input_omittable) {
        ("Record<string, never>", true) => String::new(),
        ("Record<string, never>", false) => "{}".to_owned(),
        (_, true) => format!("{input} = {{}}"),
        (_, false) => input,
    };
    let output = definitions::root_expression(output, output_names)?;
    Ok(format!("{call_path}({input}): Promise<{output}>"))
}

#[cfg(test)]
mod tests;
