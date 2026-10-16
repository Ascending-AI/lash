//! The standard protocol's tool presenter: the compact contract.
//!
//! A standard session offers its tools to the model as provider-native
//! specs, which carry the canonical schemas themselves. What this presenter
//! writes is the compact, language-neutral projection a host shows where it
//! documents those tools outside the specs (a discovery result, a catalog
//! view): a one-line signature (`search({ query: str }) -> list[str]`) and
//! named definitions and path-only annotation rows.

mod compact;

use lash_sansio::{JsonSchema, ToolContract, ToolManifest, ToolOutputContract};
use lash_tool_presenter_api::{
    PresentationRefusal, PresentedToolSection, ToolPresentationEntry, ToolPresentationRequest,
    ToolPresenter, listing,
};
use lash_tool_shape::{
    DefinitionId, ReadableNameAllocator, SchemaDocument, SchemaShape, ShapeConstraints, ShapeKind,
};
use serde_json::Value;
use std::collections::BTreeMap;

pub use compact::{
    CompactNoteRow, compact_definition, compact_note_line, compact_note_rows, compact_signature,
    compact_type,
};

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
        let input = SchemaDocument::from_json_schema(input.as_value(), depth)?;
        let output = SchemaDocument::from_json_schema(output.as_value(), depth)?;
        let mut names = compact_name_allocator(&[&input, &output], &[call_path]);
        let input = Document::allocate(input, &mut names);
        let output = Document::allocate(output, &mut names);
        let mut parts = input.definitions()?;
        for definition in output.definitions()? {
            if !parts.contains(&definition) {
                parts.push(definition);
            }
        }
        // A signature-only response must carry the context binding its names.
        parts.push(format!(
            "{} -> {}",
            compact_signature(call_path, &input.root(), &input.names)?,
            compact_type(&output.root(), &output.names)?
        ));
        Ok(parts.join("\n"))
    }

    fn discovery_text(
        &self,
        manifest: &ToolManifest,
        contract: &ToolContract,
        config: &Value,
    ) -> Result<String, PresentationRefusal> {
        Ok(
            compact_contract(manifest, contract, &manifest.name, &Self::config(config)?)?
                .render_markdown(),
        )
    }
}

/// The compact projection of a tool: its one-line signature, its return
/// type, reachable definitions and path-only parameter and return notes,
/// plus its description and examples under `config`'s cuts.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct CompactToolContract {
    pub name: String,
    pub signature: String,
    pub returns: String,
    /// Reachable named bodies and their notes, emitted once before the tool.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub definitions: Vec<String>,
    /// Use-site notes qualified by `input`; types are in the signature.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub parameters: Vec<CompactNoteRow>,
    /// Use-site notes qualified by `output`, retaining distinct union branches.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub return_fields: Vec<CompactNoteRow>,
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
    let input = SchemaDocument::from_json_schema(
        contract.input_schema.canonical.as_value(),
        config.schema_depth,
    )?;
    let output = SchemaDocument::from_json_schema(
        contract.output_schema.canonical.as_value(),
        config.schema_depth,
    )?;
    let default = match &contract.output_contract {
        ToolOutputContract::FromInputSchema {
            default_schema: Some(schema),
            ..
        } => Some(SchemaDocument::from_json_schema(
            schema.as_value(),
            config.schema_depth,
        )?),
        _ => None,
    };
    let documents = [&input, &output]
        .into_iter()
        .chain(default.iter())
        .collect::<Vec<_>>();
    let mut names = compact_name_allocator(&documents, &[signature_name]);
    let mut input = Document::allocate(input, &mut names);
    let output = Document::allocate(output, &mut names);
    let output_contract = &contract.output_contract;
    // Root references keep their named body, including the witness field.
    if let ShapeKind::Ref(id) = input.document.roots[0].kind {
        let mut current = id;
        let mut visited = std::collections::BTreeSet::new();
        while visited.insert(current) {
            let shape = &mut input.document.definitions[current.0].shape;
            if let ShapeKind::Ref(next) = shape.kind {
                current = next;
            } else {
                *shape = witnessed_input(output_contract, shape);
                break;
            }
        }
    } else {
        input.document.roots[0] = witnessed_input(output_contract, &input.root());
    }
    let mut definitions = input.definitions()?;
    let mut output_definitions = match output_contract {
        ToolOutputContract::Static => output.definitions()?,
        ToolOutputContract::FromInputSchema { .. } => Vec::new(),
    };
    output_definitions.retain(|definition| !definitions.contains(definition));
    definitions.extend(output_definitions);
    let (type_parameters, returns) = match output_contract {
        ToolOutputContract::Static => (String::new(), compact_type(&output.root(), &output.names)?),
        ToolOutputContract::FromInputSchema { .. } => {
            let default = if let Some(document) = default {
                let default = Document::allocate(document, &mut names);
                for definition in default.definitions()? {
                    if !definitions.contains(&definition) {
                        definitions.push(definition);
                    }
                }
                compact_type(&default.root(), &default.names)?
            } else {
                "any".to_string()
            };
            (format!("<T = {default}>"), "T".to_string())
        }
    };
    Ok(CompactToolContract {
        name: signature_name.to_string(),
        signature: compact_signature(
            &format!("{signature_name}{type_parameters}"),
            &input.root(),
            &input.names,
        )?,
        returns,
        definitions,
        parameters: compact_note_rows(&input.root(), "input"),
        return_fields: match output_contract {
            ToolOutputContract::Static => compact_note_rows(&output.root(), "output"),
            ToolOutputContract::FromInputSchema { .. } => Vec::new(),
        },
        description: manifest.description.trim().to_string(),
        examples: listing::compact_examples(
            &contract.examples,
            config.example_limit,
            config.example_chars,
        ),
    })
}

struct Document {
    document: SchemaDocument,
    names: BTreeMap<DefinitionId, String>,
}

/// Reserve compact notation words, tool names and the documents' host types
/// before allocating readable reference names with the shared graph allocator.
/// Pass every input, output and dynamic-default document in the same bundle.
///
/// ```rust,ignore
/// let mut names = compact_name_allocator(&[&input, &output], &["lookup"]);
/// let filter_name = names.allocate(&input, filter_id);
/// ```
pub fn compact_name_allocator(
    documents: &[&SchemaDocument],
    reserved: &[&str],
) -> ReadableNameAllocator {
    let mut allocator = ReadableNameAllocator::new(
        [
            "T", "TypeSpec", "process", "record", "list", "tuple", "enum", "any", "never", "str",
            "int", "float", "bool", "null", "type", "true", "false", "input", "output",
        ]
        .into_iter()
        .chain(reserved.iter().copied()),
    );
    for document in documents {
        reserve_host_names(document, &mut allocator);
    }
    allocator
}

fn reserve_host_names(document: &SchemaDocument, allocator: &mut ReadableNameAllocator) {
    fn reserve(shape: &SchemaShape, allocator: &mut ReadableNameAllocator) {
        if let ShapeKind::Named(name) = &shape.kind {
            allocator.reserve(name);
        }
        match &shape.kind {
            ShapeKind::Object(object) => {
                for field in &object.fields {
                    reserve(&field.shape, allocator);
                }
                if let lash_tool_shape::ExtraKeys::Open(extra) = &object.extra_keys {
                    reserve(extra, allocator);
                }
            }
            ShapeKind::List(item) => reserve(item, allocator),
            ShapeKind::Tuple(items) | ShapeKind::Union(items) => {
                for item in items {
                    reserve(item, allocator);
                }
            }
            ShapeKind::Process(Some(process)) => {
                for param in &process.params {
                    reserve(&param.shape, allocator);
                }
                reserve(&process.output, allocator);
            }
            _ => {}
        }
    }
    for shape in document.roots.iter().chain(
        document
            .definitions
            .iter()
            .map(|definition| &definition.shape),
    ) {
        reserve(shape, allocator);
    }
}

impl Document {
    fn allocate(document: SchemaDocument, allocator: &mut ReadableNameAllocator) -> Self {
        let names = document
            .reachable_closure(&document.roots)
            .into_iter()
            .map(|id| (id, allocator.allocate(&document, id)))
            .collect();
        Self { document, names }
    }

    fn root(&self) -> SchemaShape {
        self.document.roots[0].clone()
    }

    fn definitions(&self) -> Result<Vec<String>, PresentationRefusal> {
        self.document
            .reachable_closure(&self.document.roots)
            .into_iter()
            .map(|id| compact_definition(&self.document, id, &self.names))
            .collect()
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

impl CompactToolContract {
    fn render_signature_head(&self) -> String {
        format!("{} -> {}", self.signature.trim(), self.returns.trim())
    }

    /// The reachable definitions, signature line and path-only note rows.
    pub fn render_signature(&self) -> String {
        let mut sections = self.definitions.clone();
        sections.push(self.render_signature_head());
        let parameter_lines = self
            .parameters
            .iter()
            .map(compact_note_line)
            .collect::<Vec<_>>();
        if !parameter_lines.is_empty() {
            sections.push(format!("Parameters:\n{}", parameter_lines.join("\n")));
        }
        let return_field_lines = self
            .return_fields
            .iter()
            .map(compact_note_line)
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
            .map(compact_note_line)
            .collect::<Vec<_>>();
        if !return_field_lines.is_empty() {
            sections.push(format!("Return fields:\n{}", return_field_lines.join("\n")));
        }
        sections.join("\n")
    }

    /// The contract as one markdown block: its signature heading, its
    /// description, its rows and its examples.
    pub fn render_markdown(&self) -> String {
        let mut sections = self.definitions.clone();
        sections.push(format!("### {}", self.render_signature_head()));
        if !self.description.trim().is_empty() {
            sections.push(self.description.trim().to_string());
        }
        if !self.parameters.is_empty() {
            sections.push(format!(
                "Parameters:\n{}",
                self.parameters
                    .iter()
                    .map(compact_note_line)
                    .collect::<Vec<_>>()
                    .join("\n")
            ));
        }
        if !self.return_fields.is_empty() {
            sections.push(format!(
                "Return fields:\n{}",
                self.return_fields
                    .iter()
                    .map(compact_note_line)
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
