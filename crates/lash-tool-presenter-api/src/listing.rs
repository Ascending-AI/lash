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

/// `tools` listed under `disclosure`, each with its doc from `docs` (one per
/// tool, in order).
///
/// ```rust,ignore
/// // A presenter that writes its own docs keeps the shared listing.
/// let docs = request.tools.iter().map(|tool| my_doc(tool)).collect();
/// Ok(listing::list(request.tools, docs, request.disclosure))
/// ```
pub fn list(
    tools: &[ToolPresentationEntry<'_>],
    docs: Vec<String>,
    disclosure: ToolDisclosure,
) -> PresentedToolSection {
    let docs = tools
        .iter()
        .zip(docs)
        .map(|(entry, text)| ToolDoc { entry, text })
        .collect::<Vec<_>>();
    let listed = match disclosure {
        ToolDisclosure::Complete => vec![true; docs.len()],
        ToolDisclosure::Budget { catalogue_bytes } => select_within_budget(&docs, catalogue_bytes),
    };
    let ids = |want: bool| {
        docs.iter()
            .zip(&listed)
            .filter(|(_, listed)| **listed == want)
            .map(|(doc, _)| doc.entry.manifest.id.clone())
            .collect()
    };
    PresentedToolSection {
        text: render_docs(
            docs.iter()
                .zip(&listed)
                .filter(|(_, listed)| **listed)
                .map(|(doc, _)| doc),
        ),
        listed: ids(true),
        omitted: ids(false),
    }
}

/// One tool's doc.
struct ToolDoc<'a> {
    entry: &'a ToolPresentationEntry<'a>,
    text: String,
}

impl ToolDoc<'_> {
    /// The root of its call path: the namespace selection is fair across.
    fn namespace(&self) -> &str {
        self.entry
            .call_path
            .split_once('.')
            .map_or(self.entry.call_path, |(root, _)| root)
    }

    fn module(&self) -> Option<&str> {
        self.entry
            .manifest
            .module
            .as_deref()
            .map(|module| module.name.as_str())
    }
}

/// Which of `docs` a discovery session lists: the pinned ones (the discovery
/// tool and the control tools), then rounds over the namespaces in the order
/// the catalogue first names them, each round taking every namespace's next
/// inline doc that still fits the budget. A namespace whose next doc does not
/// fit is done, so a namespace's listed tools are always its first.
fn select_within_budget(docs: &[ToolDoc<'_>], catalogue_bytes: usize) -> Vec<bool> {
    let mut listed = docs.iter().map(|doc| doc.entry.pinned).collect::<Vec<_>>();
    let mut used = docs
        .iter()
        .filter(|doc| doc.entry.pinned)
        .map(|doc| doc.text.len())
        .sum::<usize>();
    let mut namespaces = Vec::<(&str, std::collections::VecDeque<usize>)>::new();
    for (index, doc) in docs.iter().enumerate() {
        if listed[index] || !doc.entry.manifest.inline {
            continue;
        }
        match namespaces
            .iter_mut()
            .find(|(namespace, _)| *namespace == doc.namespace())
        {
            Some((_, queue)) => queue.push_back(index),
            None => namespaces.push((doc.namespace(), [index].into())),
        }
    }
    let mut progressed = true;
    while progressed {
        progressed = false;
        for (_, queue) in &mut namespaces {
            let Some(&index) = queue.front() else {
                continue;
            };
            let bytes = docs[index].text.len();
            if used.saturating_add(bytes) <= catalogue_bytes {
                used += bytes;
                listed[index] = true;
                queue.pop_front();
                progressed = true;
            } else {
                queue.clear();
            }
        }
    }
    listed
}

/// `docs` in their order, a module's heading opening each run of its tools.
fn render_docs<'a, 'b: 'a>(docs: impl Iterator<Item = &'a ToolDoc<'b>>) -> String {
    let mut parts = Vec::new();
    // The heading the last doc sat under: `None` before any, `Some(None)`
    // under the ungrouped heading.
    let mut open: Option<Option<&str>> = None;
    for doc in docs {
        let module = doc.module();
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
        parts.push(doc.text.clone());
    }
    parts.join("\n\n")
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
/// let text = listing::discovery_text(&tool.manifest, &tool.contract, 8);
/// assert!(text.starts_with(&tool.manifest.name));
/// ```
pub fn discovery_text(
    manifest: &ToolManifest,
    contract: &ToolContract,
    schema_depth: usize,
) -> String {
    let mut lines = vec![manifest.name.clone()];
    let description = manifest.description.trim();
    if !description.is_empty() {
        lines.push(description.to_string());
    }
    for schema in [
        &contract.input_schema.canonical,
        &contract.output_schema.canonical,
    ] {
        for row in SchemaShape::from_json_schema(schema.as_value(), schema_depth).rows() {
            lines.push(match &row.shape.description {
                Some(description) => format!("{}: {description}", row.path),
                None => row.path,
            });
        }
    }
    lines.join("\n")
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
