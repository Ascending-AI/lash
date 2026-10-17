//! The standard protocol's tool presenter: the compact contract.
//!
//! A standard session offers its tools to the model as provider-native
//! specs, which carry the canonical schemas themselves. What this presenter
//! writes is the compact, language-neutral projection a host shows where it
//! documents those tools outside the specs (a discovery result, a catalog
//! view): a one-line signature (`search({ query: str }) -> list[str]`) and
//! named definitions and path-only annotation rows.

mod compact;

use lash_protocol_types::{JsonSchema, ToolContract, ToolManifest, ToolOutputContract};
use lash_tool_presenter_api::{
    PresentationRefusal, PresentedToolSection, ToolPresentationEntry, ToolPresentationRequest,
    ToolPresenter, import_schema, listing,
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

/// The standard presenter's configuration. Schemas are imported completely
/// under the shared node budget; there is no projection-depth setting.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StandardPresenterConfig {}

impl StandardPresenterConfig {
    /// Standard preset: complete schema presentation with no optional settings.
    pub const fn standard() -> Self {
        Self {}
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
        Self::config(request.config)?;
        let documents = request
            .tools
            .iter()
            .map(|tool| contract_documents(tool.contract))
            .collect::<Result<Vec<_>, _>>()?;
        let reserved = request
            .reserved_names
            .iter()
            .map(String::as_str)
            .chain(request.tools.iter().flat_map(|tool| {
                tool.call_path
                    .split('.')
                    .chain([tool.manifest.name.as_str()])
            }))
            .collect::<Vec<_>>();
        let initial = |selected: &[bool]| {
            let all_documents = documents
                .iter()
                .zip(selected)
                .filter(|(_, selected)| **selected)
                .flat_map(|(documents, _)| {
                    [&documents.0, &documents.1]
                        .into_iter()
                        .chain(documents.2.iter())
                })
                .collect::<Vec<_>>();
            listing::DefinitionPool::with_allocator(compact_name_allocator(
                &all_documents,
                &reserved,
            ))
        };
        listing::plan(request, initial, |index, pool| {
            let tool = &request.tools[index];
            let (input, output, default) = &documents[index];
            compact_contract_with_pool(
                tool.manifest,
                tool.contract,
                tool.call_path,
                (input.clone(), output.clone(), default.clone()),
                pool,
            )
            .map(|contract| contract.render_markdown())
        })
        .map(|plan| plan.render(request.tools))
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
        Self::config(config)?;
        let input = import_schema(input.as_value())?;
        let output = import_schema(output.as_value())?;
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
/// plus its description.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
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
    _config: &StandardPresenterConfig,
) -> Result<CompactToolContract, PresentationRefusal> {
    let documents = contract_documents(contract)?;
    let refs = [&documents.0, &documents.1]
        .into_iter()
        .chain(documents.2.iter())
        .collect::<Vec<_>>();
    let mut pool =
        listing::DefinitionPool::with_allocator(compact_name_allocator(&refs, &[signature_name]));
    compact_contract_with_pool(manifest, contract, signature_name, documents, &mut pool)
}

fn contract_documents(
    contract: &ToolContract,
) -> Result<(SchemaDocument, SchemaDocument, Option<SchemaDocument>), PresentationRefusal> {
    let input = import_schema(contract.input_schema.canonical.as_value())?;
    let output = import_schema(contract.output_schema.canonical.as_value())?;
    let default = match &contract.output_contract {
        ToolOutputContract::FromInputSchema {
            default_schema: Some(schema),
            ..
        } => Some(import_schema(schema.as_value())?),
        _ => None,
    };
    Ok((input, output, default))
}

/// Project a compact contract with a listing-local pool. Its newly needed
/// definitions and notes are part of the returned document's byte cost.
///
/// ```rust,ignore
/// let contract = compact_contract_with_pool(manifest, contract, "lookup",
///     (input, output, None), &mut pool)?;
/// ```
///
/// # Errors
/// Refuses missing references or shapes compact notation cannot spell.
pub fn compact_contract_with_pool(
    manifest: &ToolManifest,
    contract: &ToolContract,
    signature_name: &str,
    (mut input_document, output_document, default): (
        SchemaDocument,
        SchemaDocument,
        Option<SchemaDocument>,
    ),
    pool: &mut listing::DefinitionPool,
) -> Result<CompactToolContract, PresentationRefusal> {
    let output_contract = &contract.output_contract;
    // Root references keep their named body, including the witness field.
    let mut projected = None;
    if let ShapeKind::Ref(id) = input_document.roots[0].kind {
        let mut current = id;
        let mut visited = std::collections::BTreeSet::new();
        while visited.insert(current) {
            let shape = &mut input_document.definitions[current.0].shape;
            if let ShapeKind::Ref(next) = shape.kind {
                current = next;
            } else {
                let witnessed = witnessed_input(output_contract, shape);
                if witnessed != *shape {
                    projected = Some(current);
                }
                *shape = witnessed;
                break;
            }
        }
    } else {
        input_document.roots[0] = witnessed_input(output_contract, &input_document.roots[0]);
    }
    let input = match (projected, output_contract) {
        (Some(id), ToolOutputContract::FromInputSchema { input_field, .. }) => {
            let prepared = pool.prepare_projected(
                &input_document,
                id,
                &serde_json::json!({"output_witness":input_field}),
            )?;
            Document {
                document: input_document,
                names: prepared.names,
                introduced: prepared.introduced,
            }
        }
        _ => Document::prepare(input_document, pool)?,
    };
    let output = match output_contract {
        ToolOutputContract::Static => Document::prepare(output_document, pool)?,
        ToolOutputContract::FromInputSchema { .. } => Document {
            document: output_document,
            names: BTreeMap::new(),
            introduced: Vec::new(),
        },
    };
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
                let default = Document::prepare(document, pool)?;
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
    })
}

struct Document {
    document: SchemaDocument,
    names: BTreeMap<DefinitionId, String>,
    introduced: Vec<DefinitionId>,
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
            ShapeKind::Tuple(items) | ShapeKind::Union(items) | ShapeKind::Intersection(items) => {
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
        let introduced = document.reachable_closure(&document.roots);
        Self {
            document,
            names,
            introduced,
        }
    }

    fn prepare(
        document: SchemaDocument,
        pool: &mut listing::DefinitionPool,
    ) -> Result<Self, PresentationRefusal> {
        let prepared = pool.prepare(&document)?;
        Ok(Self {
            document,
            names: prepared.names,
            introduced: prepared.introduced,
        })
    }

    fn root(&self) -> SchemaShape {
        self.document.roots[0].clone()
    }

    fn definitions(&self) -> Result<Vec<String>, PresentationRefusal> {
        self.introduced
            .iter()
            .copied()
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
            title: field.shape.title.take(),
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
    /// description and its rows.
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
        sections.join("\n")
    }
}

#[cfg(test)]
mod tests;
