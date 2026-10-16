//! The catalogue listing the shipped presenters share: each tool's doc in
//! catalogue order, a module's heading opening each run of its tools, and
//! under a budget the pinned tools, then rounds over the namespaces.
//!
//! A presenter supplies each doc's text; this decides which docs are listed
//! and joins them. A tool appended to the catalogue appends its doc and leaves
//! every byte before it as it was (FIG-1435).

use lash_sansio::{ToolContract, ToolManifest};
use lash_tool_shape::{SchemaShape, ShapeRow};

use crate::{PresentedToolSection, ToolDisclosure, ToolPresentationEntry};

/// The heading of tools of no module listed after a module's tools.
const UNGROUPED_HEADING: &str = "#### Ungrouped tools";

/// A selected catalogue, with each document carrying only the definitions first
/// needed at its position. Headings and separators are outside catalogue cost.
///
/// ```rust,ignore
/// let plan = listing::plan(request, |_| pool.clone(), |index, pool| render(index, pool))?;
/// assert!(plan.catalogue_bytes() <= budget);
/// let section = plan.render(request.tools);
/// ```
#[derive(Clone, Debug)]
pub struct ListingPlan {
    docs: Vec<Option<String>>,
}

impl ListingPlan {
    /// Bytes charged to the catalogue: tool text and newly emitted closures.
    ///
    /// ```rust,ignore
    /// assert_eq!(plan.catalogue_bytes(), tool_bytes + new_definition_bytes);
    /// ```
    pub fn catalogue_bytes(&self) -> usize {
        self.docs.iter().flatten().map(String::len).sum()
    }

    /// A tool's exact charge, including its newly emitted closure. `None`
    /// means the tool was omitted and costs nothing.
    ///
    /// ```rust,ignore
    /// assert_eq!(plan.tool_bytes(omitted_index), None);
    /// ```
    pub fn tool_bytes(&self, index: usize) -> Option<usize> {
        self.docs.get(index)?.as_ref().map(String::len)
    }

    /// Join the selected documents under module headings. Supply the same
    /// offered entries passed to [`plan`], in the same order.
    ///
    /// ```rust,ignore
    /// let section = plan.render(request.tools);
    /// ```
    pub fn render(self, tools: &[ToolPresentationEntry<'_>]) -> PresentedToolSection {
        let mut section = PresentedToolSection::default();
        let mut parts = Vec::new();
        let mut open: Option<Option<&str>> = None;
        for (tool, doc) in tools.iter().zip(self.docs) {
            let Some(doc) = doc else {
                section.omitted.push(tool.manifest.id.clone());
                continue;
            };
            section.listed.push(tool.manifest.id.clone());
            let module = tool
                .manifest
                .module
                .as_deref()
                .map(|module| module.name.as_str());
            match (module, open) {
                (Some(name), current) if current != Some(Some(name)) => {
                    parts.push(format!("#### {name}"));
                    open = Some(Some(name));
                }
                (None, Some(Some(_))) => {
                    parts.push(UNGROUPED_HEADING.to_string());
                    open = Some(None);
                }
                _ => {}
            }
            parts.push(doc);
        }
        section.text = parts.join("\n\n");
        section
    }
}

/// Plan namespace rounds using the exact rendered cost of each candidate's
/// selected closure. `initial` builds a fresh pool, reserving names from exactly the selected tools. Rendering always
/// visits selected tools in catalogue order, including when a pinned tool comes
/// last; rejected candidates never consume names, definitions or notes.
///
/// A trial replays the selected documents because inserting an earlier tool
/// can transfer definition ownership and collision suffixes. Only selected
/// closures allocate names; there is no global declaration prelude.
///
/// ```rust,ignore
/// let plan = listing::plan(request, |_| TypescriptDefinitions::new(request.reserved_names),
///     |index, pool| render_tool_doc(&request.tools[index], config, &inputs[index], &outputs[index], pool))?;
/// let section = plan.render(request.tools);
/// ```
///
/// # Errors
/// Propagates rendering refusals and refuses an oversized hard section without
/// truncating it, reporting the complete required byte count.
pub fn plan<S>(
    request: crate::ToolPresentationRequest<'_>,
    mut initial: impl FnMut(&[bool]) -> S,
    mut render: impl FnMut(usize, &mut S) -> Result<String, crate::PresentationRefusal>,
) -> Result<ListingPlan, crate::PresentationRefusal> {
    let mut selected = request
        .tools
        .iter()
        .map(|tool| tool.pinned || matches!(request.disclosure, ToolDisclosure::Complete))
        .collect::<Vec<_>>();
    let mut price = |selected: &[bool]| {
        let mut pool = initial(selected);
        let docs = selected
            .iter()
            .enumerate()
            .map(|(index, selected)| {
                if *selected {
                    render(index, &mut pool).map(Some)
                } else {
                    Ok(None)
                }
            })
            .collect::<Result<Vec<_>, crate::PresentationRefusal>>()?;
        Ok::<_, crate::PresentationRefusal>(ListingPlan { docs })
    };
    let mut planned = price(&selected)?;
    if let ToolDisclosure::Budget { catalogue_bytes } = request.disclosure {
        let mut namespaces = Vec::<(&str, std::collections::VecDeque<usize>)>::new();
        for (index, tool) in request.tools.iter().enumerate() {
            if selected[index] || !tool.manifest.inline {
                continue;
            }
            let namespace = tool
                .call_path
                .split_once('.')
                .map_or(tool.call_path, |(root, _)| root);
            if let Some((_, queue)) = namespaces.iter_mut().find(|(name, _)| *name == namespace) {
                queue.push_back(index);
            } else {
                namespaces.push((namespace, [index].into()));
            }
        }
        loop {
            let mut progressed = false;
            for (_, queue) in &mut namespaces {
                let Some(&index) = queue.front() else {
                    continue;
                };
                selected[index] = true;
                let trial = price(&selected)?;
                let within_section = request
                    .max_section_bytes
                    .is_none_or(|limit| trial.clone().render(request.tools).text.len() <= limit);
                if trial.catalogue_bytes() <= catalogue_bytes && within_section {
                    planned = trial;
                    queue.pop_front();
                    progressed = true;
                } else {
                    selected[index] = false;
                    queue.clear();
                }
            }
            if !progressed {
                break;
            }
        }
    }
    if let Some(limit) = request.max_section_bytes {
        let required = planned.clone().render(request.tools).text.len();
        if required > limit {
            return Err(crate::PresentationRefusal::SectionLimit { required, limit });
        }
    }
    Ok(planned)
}

/// Names and already emitted canonical closures for one listing. A host can
/// prepare a graph, spell the new definitions with its dialect's public helpers,
/// and charge that text with [`plan`]. Clone for speculative budget accounting.
///
/// ```rust,ignore
/// let mut pool = listing::DefinitionPool::new(["HistoryItem", "Filter"]);
/// let prepared = pool.prepare(&document)?;
/// for id in prepared.introduced { emit_definition(&document, id, &prepared.names)?; }
/// ```
#[derive(Clone, Debug)]
pub struct DefinitionPool {
    allocator: lash_tool_shape::ReadableNameAllocator,
    emitted: std::collections::BTreeSet<(lash_tool_shape::CanonicalIdentity, Option<String>)>,
    contexts:
        std::collections::BTreeMap<Option<String>, (lash_tool_shape::ReadableNameAllocator, usize)>,
    used_names: Vec<String>,
}

/// All reachable names and the definitions not yet emitted by a pool.
#[derive(Clone, Debug)]
pub struct PreparedDefinitions {
    pub names: std::collections::BTreeMap<lash_tool_shape::DefinitionId, String>,
    pub introduced: Vec<lash_tool_shape::DefinitionId>,
}

impl DefinitionPool {
    /// Start with the mode, host, inferred and dialect names already reserved.
    ///
    /// ```rust,ignore
    /// let pool = DefinitionPool::new(["HistoryItem", "Field"]);
    /// ```
    pub fn new(reserved: impl IntoIterator<Item = impl AsRef<str>>) -> Self {
        Self::with_allocator(lash_tool_shape::ReadableNameAllocator::new(reserved))
    }

    /// Use a dialect's allocator after reserving its keywords and host types.
    ///
    /// ```rust,ignore
    /// let pool = DefinitionPool::with_allocator(compact_name_allocator(&[&input], &["Host"]));
    /// ```
    pub fn with_allocator(allocator: lash_tool_shape::ReadableNameAllocator) -> Self {
        Self {
            allocator,
            emitted: Default::default(),
            contexts: Default::default(),
            used_names: Vec::new(),
        }
    }

    /// Reserve a host name before preparing any graph.
    ///
    /// ```rust,ignore
    /// pool.reserve("HostResult");
    /// ```
    pub fn reserve(&mut self, name: &str) {
        self.allocator.reserve(name);
        for (allocator, _) in self.contexts.values_mut() {
            allocator.reserve(name);
        }
    }

    /// Allocate the reachable closure before spelling any body. Definition IDs
    /// are document-local; canonical identities deduplicate across documents.
    ///
    /// ```rust,ignore
    /// let prepared = pool.prepare(&document)?;
    /// assert!(pool.prepare(&document)?.introduced.is_empty());
    /// ```
    ///
    /// # Errors
    /// Refuses a dangling graph reference before allocating any names.
    pub fn prepare(
        &mut self,
        document: &lash_tool_shape::SchemaDocument,
    ) -> Result<PreparedDefinitions, crate::PresentationRefusal> {
        self.prepare_document(document, None)
    }

    /// Prepare a graph after a semantic projection changes `changed`'s body.
    /// The key describes the operation and its parameters, never rendered text.
    /// Only closures reaching `changed` acquire the key; unaffected definitions
    /// still share their canonical names. The calling dialect owns the projection.
    ///
    /// ```rust,ignore
    /// // The input's output-schema field now spells TypeSpec<T>.
    /// let prepared = pool.prepare_projected(&input, changed_id,
    ///     &serde_json::json!({"output_witness": "output"}))?;
    /// ```
    ///
    /// # Errors
    /// Refuses a dangling graph reference or an absent projection target.
    pub fn prepare_projected(
        &mut self,
        document: &lash_tool_shape::SchemaDocument,
        changed: lash_tool_shape::DefinitionId,
        projection: &serde_json::Value,
    ) -> Result<PreparedDefinitions, crate::PresentationRefusal> {
        self.prepare_document(document, Some((changed, projection.to_string())))
    }

    fn prepare_document(
        &mut self,
        document: &lash_tool_shape::SchemaDocument,
        projection: Option<(lash_tool_shape::DefinitionId, String)>,
    ) -> Result<PreparedDefinitions, crate::PresentationRefusal> {
        // Check every edge before asking the graph's canonical identity and
        // allocator helpers to traverse it. Imported graphs already satisfy
        // this; custom presenters may construct or project graphs themselves.
        let mut closure = Vec::new();
        let mut pending = document
            .roots
            .iter()
            .flat_map(lash_tool_shape::SchemaDocument::references)
            .collect::<std::collections::VecDeque<_>>();
        while let Some(id) = pending.pop_front() {
            if closure.contains(&id) {
                continue;
            }
            let definition = document.definition(id).ok_or_else(|| {
                lash_tool_shape::ShapeImportRefusal::MissingReference {
                    reference: format!("definition {}", id.0),
                }
            })?;
            closure.push(id);
            pending.extend(lash_tool_shape::SchemaDocument::references(
                &definition.shape,
            ));
        }
        if let Some((id, _)) = &projection
            && document.definition(*id).is_none()
        {
            return Err(lash_tool_shape::ShapeImportRefusal::MissingReference {
                reference: format!("definition {}", id.0),
            }
            .into());
        }
        let mut names = std::collections::BTreeMap::new();
        let mut introduced = Vec::new();
        for id in closure {
            let context = projection
                .as_ref()
                .filter(|(changed, _)| {
                    document
                        .reachable_closure(&[lash_tool_shape::ShapeKind::Ref(id).into()])
                        .contains(changed)
                })
                .map(|(_, key)| key.clone());
            let key = (document.canonical_identity(id), context.clone());
            let (allocator, reserved) = self
                .contexts
                .entry(context)
                .or_insert_with(|| (self.allocator.clone(), 0));
            for name in &self.used_names[*reserved..] {
                allocator.reserve(name);
            }
            *reserved = self.used_names.len();
            let name = allocator.allocate(document, id);
            if self.emitted.insert(key) {
                introduced.push(id);
                self.used_names.push(name.clone());
                *reserved = self.used_names.len();
            }
            names.insert(id, name);
        }
        Ok(PreparedDefinitions { names, introduced })
    }
}

/// A tool's doc as a code-mode presenter lists it: its signature, its
/// description, the notes of the fields that say more than their type
/// (spelled with `schema_type`), and its examples.
///
/// ```rust,ignore
/// let doc = listing::code_mode_tool_doc(
///     "tools.lookup({ query: string }): Promise<string>",
///     tool.manifest,
///     &input_shape,
///     &output_shape,
///     &render_schema_shape,
///     &[],
/// );
/// assert!(doc.starts_with("`tools.lookup("));
/// ```
pub fn code_mode_tool_doc(
    signature: &str,
    manifest: &ToolManifest,
    input: &SchemaShape,
    output: &SchemaShape,
    schema_type: &dyn Fn(&SchemaShape) -> String,
    examples: &[String],
) -> String {
    // The signature carries every field's name and type. The rows under
    // it add what a type cannot say, for the fields that say it.
    let mut sections = vec![format!("`{signature}`")];
    let description = manifest.description.trim();
    if !description.is_empty() {
        sections.push(description.to_string());
    }
    for (title, shape) in [("Parameters", input), ("Return fields", output)] {
        let rows = noted_field_rows(shape, schema_type);
        if !rows.is_empty() {
            sections.push(format!("{title}:\n{}", rows.join("\n")));
        }
    }
    if !examples.is_empty() {
        sections.push(format!("Examples: {}", examples.join("; ")));
    }
    sections.join("\n")
}

/// The fields of `shape` that say more than their type — a description,
/// a constraint or a default — one prompt row each. Every field's name
/// and type is already in the signature the rows sit under.
///
/// ```rust,ignore
/// let rows = listing::noted_field_rows(&input_shape, &render_schema_shape);
/// // e.g. "- `limit?: number` (>= 1) — Page size."
/// ```
pub fn noted_field_rows(
    shape: &SchemaShape,
    schema_type: &dyn Fn(&SchemaShape) -> String,
) -> Vec<String> {
    shape
        .rows()
        .iter()
        .filter(|row| row.shape.has_notes())
        .map(|row| field_row(row, schema_type))
        .collect()
}

fn field_row(row: &ShapeRow, schema_type: &dyn Fn(&SchemaShape) -> String) -> String {
    let mut line = format!(
        "- `{}{}: {}`",
        row.path,
        if row.required { "" } else { "?" },
        schema_type(&row.shape)
    );
    let mut notes = row.shape.constraints.notes();
    if let Some(default) = &row.shape.default {
        notes.push(format!("default {default}"));
    }
    if !notes.is_empty() {
        line.push_str(&format!(" ({})", notes.join(", ")));
    }
    if let Some(description) = &row.shape.description {
        line.push_str(&format!(" — {description}"));
    }
    line
}

/// The text a host's discovery search matches a tool against: its name, its
/// description, and every input and output field's path with its
/// description, imported `schema_depth` deep, one per line, so a query that
/// names what a field holds finds the tool (FIG-1435).
///
/// ```rust,ignore
/// let text = listing::discovery_text(&tool.manifest, &tool.contract, 8)?;
/// assert!(text.starts_with(&tool.manifest.name));
/// ```
///
/// # Errors
/// Returns a typed schema import refusal when the contract cannot be presented.
pub fn discovery_text(
    manifest: &ToolManifest,
    contract: &ToolContract,
    schema_depth: usize,
) -> Result<String, crate::PresentationRefusal> {
    let mut lines = vec![manifest.name.clone()];
    let description = manifest.description.trim();
    if !description.is_empty() {
        lines.push(description.to_string());
    }
    for schema in [
        &contract.input_schema.canonical,
        &contract.output_schema.canonical,
    ] {
        let document =
            lash_tool_shape::SchemaDocument::from_json_schema(schema.as_value(), schema_depth)?;
        for (shape, prefix) in document
            .roots
            .iter()
            .map(|root| (root, String::new()))
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
                lines.push(match &row.shape.description {
                    Some(description) => format!("{path}: {description}"),
                    None => path,
                });
            }
        }
    }
    Ok(lines.join("\n"))
}

/// Authored examples under a presenter's cuts: the first `limit` nonempty
/// ones, each at most `max_chars` characters.
///
/// ```rust,ignore
/// let cut = listing::compact_examples(&["a".into(), " ".into(), "bc".into()], 2, 240);
/// assert_eq!(cut, ["a", "bc"]);
/// ```
pub fn compact_examples(examples: &[String], limit: usize, max_chars: usize) -> Vec<String> {
    examples
        .iter()
        .map(|example| example.trim())
        .filter(|example| !example.is_empty())
        .take(limit)
        .map(|example| {
            if example.chars().count() <= max_chars {
                return example.to_string();
            }
            let mut out = example
                .chars()
                .take(max_chars.saturating_sub(3))
                .collect::<String>();
            out.extend("...".chars().take(max_chars.min(3)));
            out
        })
        .collect()
}
