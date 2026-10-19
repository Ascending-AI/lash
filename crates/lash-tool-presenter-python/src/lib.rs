//! The Python tool presenter: how a Python code-mode session's tools read.
//! Tools use `TypedDict` records and named, quoted reference edges. These
//! declarations describe data; they do not install classes in the cell runtime.
//!
//! The mode validates every call path before it gets here; this crate only
//! spells.

use lash_protocol_types::{JsonSchema, ToolContract, ToolManifest};
use lash_tool_presenter_api::{
    PresentationRefusal, PresentedToolSection, ToolPresentationEntry, ToolPresentationRequest,
    ToolPresenter, import_schema, listing,
};
use std::collections::{BTreeMap, BTreeSet};

use lash_tool_shape::{DefinitionId, ReadableNameAllocator, SchemaDocument, SchemaShape};

mod types;
use serde_json::Value;
pub use types::{emit_definition, emit_tool_signature, render_type, reserve_host_names};

/// The Python presenter's id.
pub const PYTHON_TOOL_PRESENTER_ID: &str = "lash.tools.python.v1";

/// The Python presenter's configuration. Schemas are imported completely
/// under the shared node budget; there is no projection-depth setting.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PythonPresenterConfig {}

impl PythonPresenterConfig {
    /// Standard preset: complete schema presentation with no optional settings.
    pub const fn standard() -> Self {
        Self {}
    }
}

/// The Python presenter code-mode sessions ship with.
#[derive(Clone, Copy, Debug, Default)]
pub struct PythonToolPresenter;

impl PythonToolPresenter {
    fn config(config: &Value) -> Result<PythonPresenterConfig, PresentationRefusal> {
        lash_tool_presenter_api::read_config(PYTHON_TOOL_PRESENTER_ID, config)
    }

    fn doc(tool: &ToolPresentationEntry<'_>) -> Result<String, PresentationRefusal> {
        let input = import_schema(tool.contract.input_schema.canonical.as_value())?;
        let output = import_schema(tool.contract.output_schema.canonical.as_value())?;
        let reserved = tool
            .call_path
            .split('.')
            .chain([tool.manifest.name.as_str()])
            .collect::<Vec<_>>();
        let allocator = python_name_allocator(&[&input, &output], &reserved);
        render_tool_doc(
            tool,
            &input,
            &output,
            &mut listing::DefinitionPool::with_allocator(allocator),
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
            .collect::<Result<Vec<_>, PresentationRefusal>>()?;
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
            let documents = documents
                .iter()
                .zip(selected)
                .filter(|(_, selected)| **selected)
                .flat_map(|((input, output), _)| [input, output])
                .collect::<Vec<_>>();
            listing::DefinitionPool::with_allocator(python_name_allocator(&documents, &reserved))
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
        signature_bundle(call_path, input, output, false)
    }

    fn discovery_text(
        &self,
        manifest: &ToolManifest,
        contract: &ToolContract,
        config: &Value,
    ) -> Result<String, PresentationRefusal> {
        Self::config(config)?;
        listing::discovery_text(manifest, contract)
    }
}

/// Reserve Python keywords, typing vocabulary and the selected documents' host
/// references before allocating any definitions. Include the surrounding mode,
/// tool and inferred names in `reserved`.
///
/// ```rust,ignore
/// let allocator = python_name_allocator(&[&input, &output], &["HistoryItem", "Field"]);
/// let mut pool = listing::DefinitionPool::with_allocator(allocator);
/// ```
pub fn python_name_allocator(
    documents: &[&SchemaDocument],
    reserved: &[&str],
) -> ReadableNameAllocator {
    let mut allocator = ReadableNameAllocator::new(
        types::KEYWORDS
            .iter()
            .chain(types::RESERVED_NAMES)
            .copied()
            .chain(reserved.iter().copied()),
    );
    for document in documents {
        for shape in document.roots.iter().chain(
            document
                .reachable_closure(&document.roots)
                .into_iter()
                .map(|id| &document.definitions[id.0].shape),
        ) {
            reserve_host_names(shape, &mut allocator);
        }
    }
    allocator
}

/// Allocate every reachable definition against a listing's reserved names.
/// Host names in this document and Python's typing vocabulary are reserved too.
///
/// ```rust,ignore
/// let names = definition_names(&document, &mut ReadableNameAllocator::default());
/// let expression = render_type(&document.roots[0], &names)?;
/// ```
pub fn definition_names(
    document: &SchemaDocument,
    allocator: &mut ReadableNameAllocator,
) -> BTreeMap<DefinitionId, String> {
    for name in types::KEYWORDS.iter().chain(types::RESERVED_NAMES) {
        allocator.reserve(name);
    }
    for shape in document
        .roots
        .iter()
        .chain(document.definitions.iter().map(|d| &d.shape))
    {
        reserve_host_names(shape, allocator);
    }
    document
        .reachable_closure(&document.roots)
        .into_iter()
        .map(|id| (id, allocator.allocate(document, id)))
        .collect()
}

/// Path-only notes, stopping at reference edges. Definition paths use their
/// allocated names; use-site notes use `root_label` (for example `input`).
///
/// ```rust,ignore
/// let rows = note_rows(&document, "input", &names);
/// // - `input.limit` (>= 1) — Page size.
/// ```
pub fn note_rows(
    document: &SchemaDocument,
    root_label: &str,
    names: &BTreeMap<DefinitionId, String>,
) -> Vec<String> {
    selected_note_rows(
        document,
        root_label,
        names,
        &document.reachable_closure(&document.roots),
    )
}

fn selected_note_rows(
    document: &SchemaDocument,
    root_label: &str,
    names: &BTreeMap<DefinitionId, String>,
    introduced: &[DefinitionId],
) -> Vec<String> {
    let rows = document
        .note_rows()
        .into_iter()
        .filter(|row| {
            if row.path.starts_with("$0") {
                return !names.contains_key(&DefinitionId(0));
            }
            // A nested definition pointer may extend another pointer. Match
            // the longest owner before deciding whether its notes are new.
            document
                .definitions
                .iter()
                .filter(|definition| {
                    row.path
                        .strip_prefix(&definition.pointer)
                        .is_some_and(|suffix| {
                            suffix.is_empty() || suffix.starts_with(['.', '[', '|', '('])
                        })
                })
                .max_by_key(|definition| definition.pointer.len())
                .is_some_and(|definition| introduced.contains(&definition.id))
        })
        .map(|row| {
            let mut path = row.path;
            if let Some(suffix) = path.strip_prefix("$0") {
                path = format!("{root_label}{suffix}");
            } else {
                // Longest pointer first: a referenced nested property can also
                // be a definition in its own right.
                let mut definitions = document.definitions.iter().collect::<Vec<_>>();
                definitions.sort_by_key(|definition| std::cmp::Reverse(definition.pointer.len()));
                for definition in definitions {
                    if let Some(name) = names.get(&definition.id)
                        && let Some(suffix) = path.strip_prefix(&definition.pointer)
                        && (suffix.is_empty() || suffix.starts_with(['.', '[', '|', '(']))
                    {
                        path = format!("{name}{suffix}");
                        break;
                    }
                }
            }
            let mut line = format!("- `{}`", types::note_text(&path));
            let mut notes = row.shape.constraints.notes();
            if let Some(default) = row.shape.default {
                notes.push(format!("default {default}"));
            }
            if !notes.is_empty() {
                line.push_str(&format!(" ({})", types::note_text(&notes.join(", "))));
            }
            if let Some(description) = row.shape.description {
                line.push_str(&format!(" — {}", types::note_text(&description)));
            }
            line
        })
        .collect();
    scope_union_notes(document, root_label, names, introduced, rows)
}

/// Render a tool using a listing-local pool. Reserve Python words, host and
/// inferred names before the first call. A fresh pool produces standalone docs.
///
/// ```rust,ignore
/// let text = render_tool_doc(&tool, &input, &output, &mut pool)?;
/// ```
///
/// # Errors
/// Refuses unbound references or unsupported shapes.
pub fn render_tool_doc(
    tool: &ToolPresentationEntry<'_>,
    input: &SchemaDocument,
    output: &SchemaDocument,
    pool: &mut listing::DefinitionPool,
) -> Result<String, PresentationRefusal> {
    let mut parts = Vec::new();
    let mut contexts = Vec::new();
    for (document, label) in [(input, "input"), (output, "output")] {
        let prepared = pool.prepare(document)?;
        for id in &prepared.introduced {
            parts.push(emit_definition(
                &prepared.names[id],
                &document.definitions[id.0].shape,
                &prepared.names,
            )?);
        }
        let rows = selected_note_rows(document, label, &prepared.names, &prepared.introduced);
        contexts.push((prepared.names, rows));
    }
    let root = |document: &SchemaDocument, names: &BTreeMap<DefinitionId, String>| {
        if names.contains_key(&DefinitionId(0)) {
            lash_tool_shape::ShapeKind::Ref(DefinitionId(0)).into()
        } else {
            document.roots[0].clone()
        }
    };
    parts.push(format!(
        "`{}`",
        emit_tool_signature(
            tool.call_path,
            &root(input, &contexts[0].0),
            tool.contract.input_schema.canonical.accepts_empty_object(),
            &contexts[0].0,
            &root(output, &contexts[1].0),
            &contexts[1].0
        )?
    ));
    if !tool.manifest.description.trim().is_empty() {
        parts.push(tool.manifest.description.trim().to_owned());
    }
    for (_, rows) in contexts {
        parts.extend(rows);
    }
    Ok(parts.join("\n"))
}

fn signature_bundle(
    call_path: &str,
    input: &JsonSchema,
    output: &JsonSchema,
    with_notes: bool,
) -> Result<String, PresentationRefusal> {
    let input_doc = import_schema(input.as_value())?;
    let output_doc = import_schema(output.as_value())?;
    let mut allocator = ReadableNameAllocator::new([call_path]);
    // Reserve both documents' host names before allocating either closure.
    for document in [&input_doc, &output_doc] {
        for shape in document
            .roots
            .iter()
            .chain(document.definitions.iter().map(|d| &d.shape))
        {
            reserve_host_names(shape, &mut allocator);
        }
    }
    let input_names = definition_names(&input_doc, &mut allocator);
    let output_names = definition_names(&output_doc, &mut allocator);
    let mut emitted = BTreeSet::new();
    let mut parts = Vec::new();
    for (document, names) in [(&input_doc, &input_names), (&output_doc, &output_names)] {
        for id in document.reachable_closure(&document.roots) {
            let name = &names[&id];
            if emitted.insert(name.clone()) {
                parts.push(emit_definition(
                    name,
                    &document.definitions[id.0].shape,
                    names,
                )?);
            }
        }
    }
    parts.push(emit_tool_signature(
        call_path,
        &input_doc.roots[0],
        input.accepts_empty_object(),
        &input_names,
        &output_doc.roots[0],
        &output_names,
    )?);
    if with_notes {
        let mut emitted_notes = BTreeSet::new();
        for (document, label, names) in [
            (&input_doc, "input", &input_names),
            (&output_doc, "output", &output_names),
        ] {
            parts.extend(
                note_rows(document, label, names)
                    .into_iter()
                    .filter(|row| emitted_notes.insert(row.clone())),
            );
        }
    }
    Ok(parts.join("\n"))
}

/// Spell a reference-free shape for history or inferred host types.
/// Use [`render_type`] with definition names for graph nodes.
///
/// ```rust,ignore
/// assert_eq!(render_shape(&ShapeKind::Int.into())?, "int");
/// ```
///
/// # Errors
/// Refuses an unbound reference or a shape Python cannot express.
pub fn render_shape(shape: &SchemaShape) -> Result<String, PresentationRefusal> {
    render_type(shape, &BTreeMap::new())
}

/// Emit one reference-free host or inferred definition.
///
/// ```rust,ignore
/// assert_eq!(schema_definition("Query", &ShapeKind::Str.into())?, "Query = str");
/// ```
///
/// # Errors
/// Refuses invalid names, unbound references and unsupported shapes.
pub fn schema_definition(name: &str, shape: &SchemaShape) -> Result<String, PresentationRefusal> {
    emit_definition(name, shape, &BTreeMap::new())
}

fn scope_union_notes(
    document: &SchemaDocument,
    root_label: &str,
    names: &BTreeMap<DefinitionId, String>,
    introduced: &[DefinitionId],
    rows: Vec<String>,
) -> Vec<String> {
    fn collect(shape: &SchemaShape, path: &str, scopes: &mut Vec<String>) {
        match &shape.kind {
            lash_tool_shape::ShapeKind::Union(members) => {
                for (member, arm) in members.iter().zip(shape.union_arm_paths(path)) {
                    scopes.push(types::note_text(&arm));
                    collect(member, &arm, scopes);
                }
            }
            lash_tool_shape::ShapeKind::Object(object) => {
                for field in &object.fields {
                    collect(&field.shape, &format!("{path}.{}", field.name), scopes);
                }
                if let lash_tool_shape::ExtraKeys::Open(extra) = &object.extra_keys {
                    collect(extra, &format!("{path}.*"), scopes);
                }
            }
            lash_tool_shape::ShapeKind::List(item) => collect(item, &format!("{path}[]"), scopes),
            lash_tool_shape::ShapeKind::Tuple(items) => {
                for (index, item) in items.iter().enumerate() {
                    collect(item, &format!("{path}[{index}]"), scopes);
                }
            }
            lash_tool_shape::ShapeKind::Intersection(items) => {
                for item in items {
                    collect(item, path, scopes);
                }
            }
            lash_tool_shape::ShapeKind::Process(Some(process))
            | lash_tool_shape::ShapeKind::Function(Some(process)) => {
                for param in &process.params {
                    collect(&param.shape, &format!("{path}({})", param.name), scopes);
                }
                collect(&process.output, &format!("{path}.return"), scopes);
            }
            _ => {}
        }
    }
    fn scope<'a>(row: &str, scopes: &'a [String]) -> Option<&'a str> {
        let tail = row.strip_prefix("- `")?;
        scopes
            .iter()
            .filter(|scope| {
                tail.strip_prefix(scope.as_str())
                    .is_some_and(|suffix| suffix.starts_with('.'))
            })
            .max_by_key(|scope| scope.len())
            .map(String::as_str)
    }
    let mut scopes = Vec::new();
    for root in &document.roots {
        collect(root, root_label, &mut scopes);
    }
    for id in introduced {
        if let Some(definition) = document.definition(*id) {
            collect(
                &definition.shape,
                names.get(id).unwrap_or(&definition.pointer),
                &mut scopes,
            );
        }
    }
    scopes.sort();
    scopes.dedup();
    let mut counts = BTreeMap::<String, usize>::new();
    for row in &rows {
        if let Some(scope) = scope(row, &scopes) {
            *counts.entry(scope.into()).or_default() += 1;
        }
    }
    let mut rendered = Vec::new();
    let mut active = None::<String>;
    for row in rows {
        if let Some(scope) = scope(&row, &scopes)
            && counts.get(scope).copied().unwrap_or(0) >= 3
        {
            if active.as_deref() != Some(scope) {
                rendered.push(format!("- `{scope}`:"));
                active = Some(scope.into());
            }
            rendered.push(format!("  - `{tail}", tail = &row[scope.len() + 4..]));
        } else {
            active = None;
            rendered.push(row);
        }
    }
    rendered
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn recursive_list_keeps_one_definition_and_quoted_forward_reference() {
        let input = JsonSchema::admit(json!({
            "$ref": "#/$defs/Node",
            "$defs": {"Node": {"type": "array", "items": {"$ref": "#/$defs/Node"}}}
        }))
        .expect("valid recursive schema");
        let output = JsonSchema::admit(json!({"type": "null"})).expect("valid schema");
        let text = PythonToolPresenter
            .tool_signature("walk", &input, &output, &json!({}))
            .expect("presentable recursive list");
        assert!(text.contains("Node = list[\"Node\"]"), "{text}");
        assert_eq!(text.matches("Node = ").count(), 1, "{text}");
        assert!(!text.contains("Any"), "{text}");
        assert!(
            text.contains("async def walk(input: \"Node\") -> None"),
            "{text}"
        );
    }
    fn signature_for(input: Value) -> String {
        let input = JsonSchema::admit(input).expect("valid schema");
        let output = JsonSchema::admit(json!({"type": "null"})).expect("valid schema");
        PythonToolPresenter
            .tool_signature("inspect", &input, &output, &json!({}))
            .expect("presentable schema")
    }

    #[test]
    fn mutual_aliases_emit_each_reachable_definition_once() {
        let text = signature_for(json!({
            "$ref": "#/$defs/Left",
            "$defs": {
                "Left": {"anyOf": [{"const": "leaf"}, {"type": "array", "items": {"$ref": "#/$defs/Right"}}]},
                "Right": {"anyOf": [{"type": "null"}, {"type": "array", "items": {"$ref": "#/$defs/Left"}}]}
            }
        }));
        assert!(
            text.contains("Left = Union[Literal[\"leaf\"], list[\"Right\"]]"),
            "{text}"
        );
        assert!(
            text.contains("Right = Union[list[\"Left\"], None]"),
            "{text}"
        );
        for name in ["Left", "Right"] {
            assert_eq!(text.matches(&format!("{name} = ")).count(), 1, "{text}");
        }
        assert!(!text.contains("Any"), "{text}");
    }

    #[test]
    fn reserved_record_keys_use_functional_typeddict_and_python_literals() {
        let text = signature_for(json!({
            "$ref": "#/$defs/Filter",
            "$defs": {"Filter": {"type": "object", "properties": {
                "and": {"type": "array", "items": {"$ref": "#/$defs/Filter"}},
                "two words": {"enum": [true, false, "line\n\"quoted\""]}
            }, "required": ["two words"], "additionalProperties": false}}
        }));
        assert!(text.contains("Filter = TypedDict(\"Filter\", {"), "{text}");
        assert!(
            text.contains("\"and\": NotRequired[list[\"Filter\"]]"),
            "{text}"
        );
        assert!(
            text.contains(r#""two words": Literal[True, False, "line\n\"quoted\""]"#),
            "{text}"
        );
        assert!(!text.contains("Any"), "{text}");
        let signature = emit_tool_signature(
            "outil_été",
            &lash_tool_shape::ShapeKind::Str.into(),
            false,
            &BTreeMap::new(),
            &lash_tool_shape::ShapeKind::Null.into(),
            &BTreeMap::new(),
        )
        .expect("the mode accepts Unicode callable names");
        assert_eq!(signature, "async def outil_été(input: str) -> None");
    }

    #[test]
    fn optional_keys_and_nullable_values_are_distinct() {
        let text = signature_for(json!({"type": "object", "properties": {
            "optional": {"type": "string"},
            "nullable": {"type": ["string", "null"]},
            "both": {"type": ["integer", "null"]}
        }, "required": ["nullable"], "additionalProperties": false}));
        assert!(text.contains("\"optional\": NotRequired[str]"), "{text}");
        assert!(text.contains("\"nullable\": Union[str, None]"), "{text}");
        assert!(
            text.contains("\"both\": NotRequired[Union[int, None]]"),
            "{text}"
        );
        assert!(!text.contains("NotRequired[Union[str, None]]"), "{text}");
    }

    #[test]
    fn shared_closures_emit_once_and_readable_collisions_reserve_host_names() {
        let schema = JsonSchema::admit(json!({"type": "object", "properties": {
            "first": {"$ref": "#/$defs/Kind"}, "second": {"$ref": "#/$defs/Kind"},
            "keyword": {"$ref": "#/$defs/list"},
            "one": {"$ref": "#/$defs/A-B"}, "two": {"$ref": "#/$defs/A_B"}
        }, "$defs": {"Kind": {"enum": ["a", "b"]}, "list": {"type": "integer"},
            "A-B": {"type": "integer"}, "A_B": {"type": "string"}}}))
        .expect("valid schema");
        let text = PythonToolPresenter
            .tool_signature("Kind", &schema, &schema, &json!({}))
            .expect("presentable");
        assert_eq!(text.matches("Kind_2 = Literal").count(), 1, "{text}");
        assert!(text.contains("list_2 = int"), "{text}");
        assert!(text.contains("A_B = int"), "{text}");
        assert!(text.contains("A_B_2 = str"), "{text}");
        assert_eq!(text.matches(" = Literal").count(), 1, "{text}");
        let document = SchemaDocument::from_json_schema(
            &json!({"type": "object", "properties": {
            "host": {"$ref": "Kind"}, "local": {"$ref": "#/$defs/Kind"}
        }, "$defs": {"Kind": {"type": "string"}}}),
            8,
        )
        .expect("graph supports host type names");
        let names = definition_names(&document, &mut ReadableNameAllocator::default());
        assert_eq!(names.values().collect::<Vec<_>>(), vec!["Kind_2"]);
    }

    #[test]
    fn notes_are_path_only_with_definition_and_use_site_ownership() {
        let doc = SchemaDocument::from_json_schema(
            &json!({"type": "object", "properties": {
                "value": {"$ref": "#/$defs/Choice", "description": "Use here"}
            }, "$defs": {"Choice": {"description": "Shared choice", "anyOf": [
                {"type": "string", "description": "Text", "minLength": 2},
                {"type": "integer", "description": "Count", "default": 3}
            ]}}}),
            8,
        )
        .expect("valid graph");
        let names = definition_names(&doc, &mut ReadableNameAllocator::default());
        let notes = note_rows(&doc, "input", &names).join("\n");
        assert!(notes.contains("`input.value` — Use here"), "{notes}");
        assert!(notes.contains("`Choice` — Shared choice"), "{notes}");
        assert!(
            notes.contains("`Choice|string` (min length 2) — Text"),
            "{notes}"
        );
        assert!(
            notes.contains("`Choice|integer` (default 3) — Count"),
            "{notes}"
        );
        for type_text in ["Union[", "Literal[", ": str", ": int"] {
            assert!(!notes.contains(type_text), "{notes}");
        }
    }

    fn shape_text(schema: &Value) -> String {
        let doc = SchemaDocument::from_json_schema(schema, 32).expect("valid schema");
        document_text(&doc)
    }
    fn document_text(doc: &SchemaDocument) -> String {
        let names = definition_names(doc, &mut ReadableNameAllocator::default());
        let mut text = doc
            .reachable_closure(&doc.roots)
            .into_iter()
            .map(|id| emit_definition(&names[&id], &doc.definitions[id.0].shape, &names).unwrap())
            .collect::<Vec<_>>();
        text.push(render_type(&doc.roots[0], &names).expect("presentable type"));
        text.extend(note_rows(doc, "input", &names));
        text.join("\n")
    }

    #[test]
    fn catalog_annotations_do_not_refuse_python_tools() {
        for keyword in [
            "example",
            "human_parameter_name",
            "human_parameter_description",
            "file_uploadable",
            "file_downloadable",
            "discriminator",
            "inventedKeyword",
        ] {
            let text = shape_text(&json!({"type":"string", keyword:"documentation"}));
            assert_eq!(text, "str", "{keyword}: {text}");
        }
    }

    #[test]
    fn factored_composition_and_shared_refinements_present_in_python() {
        for composition in ["oneOf", "anyOf", "allOf"] {
            let schema = json!({"type":"object", "properties":{
                "query":{"type":"string"}, "cursor":{"type":"string"}},
                composition:[{"required":["query"]},{"required":["cursor"]}], "minProperties":1});
            let text = shape_text(&schema);
            assert_eq!(text.matches("\"query\": str").count(), 1, "{text}");
            assert!(
                text.contains("requires query") && text.contains("requires cursor"),
                "{text}"
            );
            assert!(text.contains("minProperties 1"), "{text}");
        }
    }

    fn conditional_fixture() -> Value {
        json!({"type":"object", "properties":{"kind":{"type":"string"}, "url":{"type":"string"}},
        "if":{"properties":{"kind":{"const":"web"}}, "required":["kind"]},
        "then":{"required":["url"]}, "else":{"not":{"required":["url"]}},
        "dependentRequired":{"url":["kind"]},
        "dependentSchemas":{"url":{"required":["kind"]}}})
    }

    #[test]
    fn conditional_and_dependent_rules_are_presented_in_words() {
        let text = shape_text(&conditional_fixture());
        assert!(
            text.contains("url: required when kind is \"web\""),
            "{text}"
        );
        assert!(
            text.contains("kind: required when url is present"),
            "{text}"
        );
    }

    #[test]
    fn inexpressible_schema_rules_keep_an_honest_type_and_a_note() {
        for schema in [
            json!({"$ref":"https://example.com/value.json", "description":"Host description"}),
            json!({"$ref":"#/missing"}),
            json!({"$id":"https://example.com/root", "$defs":{
            "Nested":{"$id":"nested", "$defs":{"Value":{"type":"integer"}},
                "type":"object", "properties":{"v":{"$ref":"#/$defs/Value"}}},
            "Value":{"type":"string"}}, "$ref":"#/$defs/Nested"}),
            json!({"$dynamicRef":"#node"}),
            json!({"$recursiveRef":"#"}),
            json!({"type":"object", "unevaluatedProperties":false}),
            json!({"type":"array", "prefixItems":[{"type":"string"}], "items":{"type":"number"}}),
            json!({"type":"object", "patternProperties":{"^a":{"type":"string"}}}),
            json!({"type":"array", "contains":{"type":"integer"}}),
            json!({"enum":[{"key":1}, 1.5]}),
        ] {
            let text = shape_text(&schema);
            assert!(!text.is_empty(), "{schema}");
            assert!(
                text.contains("validated")
                    || text.contains("patternProperties")
                    || text.contains("contains")
                    || text.contains("one of"),
                "{schema}: {text}"
            );
        }
    }

    #[test]
    fn presentation_limits_keep_a_note_instead_of_refusing_a_valid_tool() {
        let mut schema = json!({"type":"string"});
        for _ in 0..10 {
            schema = json!({"type":"array", "items":schema});
        }
        let doc = lash_tool_shape::SchemaDocument::import(
            &schema,
            lash_tool_shape::ImportLimits {
                max_depth: 2,
                max_nodes: 2,
            },
        )
        .expect("valid tool");
        let text = document_text(&doc);
        assert!(text.contains("validated"), "{text}");
    }

    #[test]
    fn original_figments_schemas_print_no_larger_than_source() {
        for (name, source) in [
            (
                "people.lookup",
                include_str!("fixtures/figments/people.lookup.json"),
            ),
            (
                "information.search",
                include_str!("fixtures/figments/information.search.json"),
            ),
            (
                "information.read",
                include_str!("fixtures/figments/information.read.json"),
            ),
            (
                "information.list",
                include_str!("fixtures/figments/information.list.json"),
            ),
        ] {
            let schemas: Value = serde_json::from_str(source).unwrap();
            for direction in ["input", "output"] {
                let schema = &schemas[direction];
                let text = shape_text(schema);
                assert!(
                    text.len() <= schema.to_string().len(),
                    "{name} {direction}: {} > {}: {text}",
                    text.len(),
                    schema.to_string().len()
                );
                println!(
                    "{name} {direction}: {} printed <= {} source bytes",
                    text.len(),
                    schema.to_string().len()
                );
            }
        }
    }

    #[test]
    fn union_notes_name_the_title_and_discriminant_in_python() {
        let text = shape_text(&json!({"oneOf":[
            {"title":"Calendar.v1", "properties":{"date":{"type":"string", "description":"Day"},
                "from":{"type":"string", "description":"Start"}, "to":{"type":"string", "description":"End"}}},
            {"properties":{"kind":{"const":"mail"}, "date":{"type":"string", "description":"Sent"}}}
        ]}));
        assert!(
            text.contains("- `input|Calendar.v1`:")
                && text.contains("  - `date` — Day")
                && text.contains("input|kind=\"mail\".date"),
            "{text}"
        );
    }

    /// FIG-6807: an optional value's notes sit on its field path; only a
    /// real union names its arms.
    #[test]
    fn a_nullable_field_is_noted_on_its_own_path_and_a_real_union_names_its_arms() {
        let text = shape_text(&json!({"type": "object", "properties": {
            "endCursor": {"anyOf": [{"type": "string", "title": "End Cursor", "description": "Cursor of the last item."}, {"type": "null"}]},
            "assignee": {"anyOf": [{"type": "object", "title": "Assignee", "description": "Who owns the issue.",
                "properties": {"name": {"type": "string", "description": "Display name."}}, "required": ["name"]}, {"type": "null"}]},
            "target": {"anyOf": [{"type": "string", "title": "Url", "description": "A link."}, {"type": "integer", "title": "Id", "description": "An identifier."}]}
        }, "required": ["endCursor", "assignee", "target"], "additionalProperties": false}));
        for row in [
            "- `input.endCursor` — Cursor of the last item.",
            "- `input.assignee` — Who owns the issue.",
            "- `input.assignee.name` — Display name.",
            "- `input.target|Url` — A link.",
            "- `input.target|Id` — An identifier.",
        ] {
            assert!(text.contains(row), "{row}\n{text}");
        }
        assert!(
            !text.contains("|End Cursor") && !text.contains("|Assignee"),
            "{text}"
        );
    }
}
