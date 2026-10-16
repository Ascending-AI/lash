//! The standard protocol's tool presenter: the compact contract.
//!
//! A standard session offers its tools to the model as provider-native
//! specs, which carry the canonical schemas themselves. What this presenter
//! writes is the compact, language-neutral projection a host shows where it
//! documents those tools outside the specs (a discovery result, a catalog
//! view): a one-line signature (`search({ query: str }) -> list[str]`) and
//! one row per field.

mod compact;

use lash_sansio::{JsonSchema, ToolContract, ToolManifest, ToolOutputContract};
use lash_tool_presenter_api::{
    PresentationRefusal, PresentedToolSection, ToolPresentationEntry, ToolPresentationRequest,
    ToolPresenter, listing,
};
use lash_tool_shape::{SchemaShape, ShapeConstraints, ShapeKind};
use serde_json::Value;

pub use compact::compact_type;
use compact::{compact_arguments, compact_doc_line, compact_row};

/// The standard presenter's id.
pub const STANDARD_TOOL_PRESENTER_ID: &str = "lash.tools.compact.v1";

/// The standard presenter's config: its prompt-facing schema and example
/// cuts. Validation always reads the full schema.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StandardPresenterConfig {
    pub example_limit: usize,
    pub example_chars: usize,
    pub schema_depth: usize,
}

impl Default for StandardPresenterConfig {
    fn default() -> Self {
        Self::standard()
    }
}

impl StandardPresenterConfig {
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

/// The compact presenter standard sessions ship with.
#[derive(Clone, Copy, Debug, Default)]
pub struct StandardToolPresenter;

impl StandardToolPresenter {
    fn config(config: &Value) -> Result<StandardPresenterConfig, PresentationRefusal> {
        lash_tool_presenter_api::read_config(STANDARD_TOOL_PRESENTER_ID, config)
    }
}

impl ToolPresenter for StandardToolPresenter {
    fn id(&self) -> &str {
        STANDARD_TOOL_PRESENTER_ID
    }

    fn admit_config(&self, raw: &Value) -> Result<Value, PresentationRefusal> {
        lash_tool_presenter_api::admit_over_standard(
            STANDARD_TOOL_PRESENTER_ID,
            &StandardPresenterConfig::standard(),
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
            .map(|tool| {
                compact_contract(tool.manifest, tool.contract, tool.call_path, &config)
                    .map(|contract| contract.render_markdown())
            })
            .collect::<Result<Vec<_>, _>>()?;
        Ok(listing::list(request.tools, docs, request.disclosure))
    }

    fn tool_doc(
        &self,
        tool: &ToolPresentationEntry<'_>,
        config: &Value,
    ) -> Result<String, PresentationRefusal> {
        Ok(compact_contract(
            tool.manifest,
            tool.contract,
            tool.call_path,
            &Self::config(config)?,
        )?
        .render_markdown())
    }

    fn tool_signature(
        &self,
        call_path: &str,
        input: &JsonSchema,
        output: &JsonSchema,
        config: &Value,
    ) -> Result<String, PresentationRefusal> {
        let depth = Self::config(config)?.schema_depth;
        Ok(format!(
            "{call_path}({}) -> {}",
            compact_arguments(&lash_tool_presenter_api::inline_schema(
                input.as_value(),
                depth
            )?),
            compact_type(&lash_tool_presenter_api::inline_schema(
                output.as_value(),
                depth
            )?)
        ))
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

/// The compact projection of a tool: its one-line signature, its return
/// type, a row per parameter and return field, its description and its
/// examples under `config`'s cuts.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
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

/// `manifest`'s tool under `contract`, compacted as `signature_name` under
/// `config`.
///
/// ```rust,ignore
/// let compact = compact_contract(&tool.manifest, &tool.contract, "lookup", &StandardPresenterConfig::standard())?;
/// assert_eq!(compact.render_markdown().lines().next(), Some("### lookup({ query: str }) -> str"));
/// ```
///
/// # Errors
/// Returns a typed schema import refusal when the contract cannot be presented.
pub fn compact_contract(
    manifest: &ToolManifest,
    contract: &ToolContract,
    signature_name: &str,
    config: &StandardPresenterConfig,
) -> Result<CompactToolContract, PresentationRefusal> {
    let input = lash_tool_presenter_api::inline_schema(
        contract.input_schema.canonical.as_value(),
        config.schema_depth,
    )?;
    let output = lash_tool_presenter_api::inline_schema(
        contract.output_schema.canonical.as_value(),
        config.schema_depth,
    )?;
    let output_contract = &contract.output_contract;
    let witnessed_input = witnessed_input(output_contract, &input);
    Ok(CompactToolContract {
        name: signature_name.to_string(),
        signature: format!(
            "{}{}({})",
            signature_name,
            type_parameter_suffix(output_contract, config.schema_depth)?.unwrap_or_default(),
            compact_arguments(&witnessed_input)
        ),
        returns: return_type_label(output_contract, &output),
        parameters: witnessed_input
            .rows()
            .iter()
            .map(|row| compact_row(row, "name"))
            .collect(),
        return_fields: return_fields(output_contract, &output),
        description: manifest.description.trim().to_string(),
        examples: listing::compact_examples(
            &contract.examples,
            config.example_limit,
            config.example_chars,
        ),
    })
}

fn return_type_label(output_contract: &ToolOutputContract, output: &SchemaShape) -> String {
    match output_contract {
        ToolOutputContract::Static => compact_type(output),
        ToolOutputContract::FromInputSchema { .. } => "T".to_string(),
    }
}

fn type_parameter_suffix(
    output_contract: &ToolOutputContract,
    depth: usize,
) -> Result<Option<String>, PresentationRefusal> {
    match output_contract {
        ToolOutputContract::Static => Ok(None),
        ToolOutputContract::FromInputSchema { default_schema, .. } => {
            let default = default_schema
                .as_ref()
                .map(|schema| {
                    lash_tool_presenter_api::inline_schema(schema.as_value(), depth)
                        .map(|shape| compact_type(&shape))
                })
                .transpose()?
                .unwrap_or_else(|| "any".to_string());
            Ok(Some(format!("<T = {default}>")))
        }
    }
}

/// The input shape as the compact contract shows it: the field that
/// carries the output's type is a `TypeSpec<T>` witness, not a record.
fn witnessed_input(output_contract: &ToolOutputContract, input: &SchemaShape) -> SchemaShape {
    let mut input = input.clone();
    if let ToolOutputContract::FromInputSchema { input_field, .. } = output_contract
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

fn return_fields(output_contract: &ToolOutputContract, output: &SchemaShape) -> Vec<Value> {
    match output_contract {
        ToolOutputContract::Static => output
            .rows()
            .iter()
            .map(|row| compact_row(row, "path"))
            .collect(),
        ToolOutputContract::FromInputSchema { .. } => Vec::new(),
    }
}

impl CompactToolContract {
    fn render_signature_head(&self) -> String {
        format!("{} -> {}", self.signature.trim(), self.returns.trim())
    }

    /// The signature line, then its parameter and return-field rows.
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

    /// The contract as one markdown block: its signature heading, its
    /// description, its rows and its examples.
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

#[cfg(test)]
mod tests;
