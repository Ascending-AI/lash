//! Listing-local names and dependency ordering.

use std::collections::{BTreeMap, BTreeSet};

use lash_vocabulary::tool_shape::CanonicalIdentity;
use lash_vocabulary::tool_shape::DefinitionId;
use lash_vocabulary::tool_shape::ExtraKeys;
use lash_vocabulary::tool_shape::ReadableNameAllocator;
use lash_vocabulary::tool_shape::SchemaDocument;
use lash_vocabulary::tool_shape::SchemaShape;
use lash_vocabulary::tool_shape::ShapeImportRefusal;
use lash_vocabulary::tool_shape::ShapeKind;

use crate::{DefinitionNames, note_rows, render_type_expression, render_type_name, reserved_words};

/// Names and already emitted closures for one listing. Clone before considering
/// a budget candidate; retain the clone only when that candidate is selected.
///
/// ```rust,ignore
/// let mut pool = TypescriptDefinitions::new(["Row", "tools"]);
/// let prepared = pool.prepare(&document)?;
/// ```
#[derive(Clone, Debug)]
pub struct TypescriptDefinitions {
    names: ReadableNameAllocator,
    emitted: BTreeSet<CanonicalIdentity>,
}

impl Default for TypescriptDefinitions {
    fn default() -> Self {
        Self::new(std::iter::empty::<&str>())
    }
}

/// A document's reference names and newly needed declarations (including notes).
///
/// ```rust,ignore
/// let prepared = pool.prepare(&document)?;
/// let expression = render_type_expression(&document.roots[0], &prepared.names)?;
/// ```
#[derive(Clone, Debug)]
pub struct PreparedDefinitions {
    pub names: DefinitionNames,
    pub declarations: Vec<String>,
}

impl TypescriptDefinitions {
    /// Reserve dialect names plus every tool, host and inferred type name that
    /// the surrounding section exposes, before its first definition is emitted.
    ///
    /// ```rust,ignore
    /// let pool = TypescriptDefinitions::new(["Row", "tools", "HostResult"]);
    /// ```
    pub fn new(reserved: impl IntoIterator<Item = impl AsRef<str>>) -> Self {
        let mut names = ReadableNameAllocator::new(reserved_words().iter().copied());
        for name in ["Array", "Record", "Promise", "Process", "TypeSpec"] {
            names.reserve(name);
        }
        for name in reserved {
            names.reserve(&render_type_name(name.as_ref()));
        }
        Self {
            names,
            emitted: BTreeSet::new(),
        }
    }

    /// Reserve host references throughout a document, including reachable
    /// definitions. Call for every offered document before preparing any of them.
    ///
    /// ```rust,ignore
    /// pool.reserve_document(&input);
    /// pool.reserve_document(&output);
    /// ```
    pub fn reserve_document(&mut self, document: &SchemaDocument) {
        for root in &document.roots {
            reserve_shape(&mut self.names, root);
        }
        for id in document.reachable_closure(&document.roots) {
            reserve_shape(&mut self.names, &document.definitions[id.0].shape);
        }
    }

    /// Allocate all names before spelling any bodies, then emit each previously
    /// uncharged closure once. Dependencies precede their users; recursive groups
    /// are ordered by canonical closure identity. A root referenced as `#` is
    /// named too, so rendering that root should use its prepared name.
    ///
    /// ```rust,ignore
    /// let prepared = pool.prepare(&document)?;
    /// assert!(pool.prepare(&document)?.declarations.is_empty());
    /// ```
    ///
    /// # Errors
    /// Refuses invalid document edges or unsupported expression kinds.
    pub fn prepare(
        &mut self,
        document: &SchemaDocument,
    ) -> Result<PreparedDefinitions, ShapeImportRefusal> {
        let order = definition_order(document)?;
        let names = order
            .iter()
            .map(|id| (*id, self.names.allocate(document, *id)))
            .collect::<DefinitionNames>();
        let mut declarations = Vec::new();
        for id in order {
            if self.emitted.insert(document.canonical_identity(id)) {
                declarations.push(emit_definition(document, id, &names)?);
            }
        }
        Ok(PreparedDefinitions {
            names,
            declarations,
        })
    }
}

/// Emit one definition and its definition-qualified path-only annotations.
/// Names must have been allocated for the whole reachable closure first.
///
/// ```rust,ignore
/// let text = emit_definition(&document, id, &prepared.names)?;
/// assert!(text.starts_with("type Filter = "));
/// ```
///
/// # Errors
/// Refuses an invalid ID, a missing name or an unsupported expression.
pub fn emit_definition(
    document: &SchemaDocument,
    id: DefinitionId,
    names: &DefinitionNames,
) -> Result<String, ShapeImportRefusal> {
    let missing = || ShapeImportRefusal::InvalidSchema {
        keyword: format!("definition {}", id.0),
    };
    let definition = document.definition(id).ok_or_else(missing)?;
    let name = names.get(&id).ok_or_else(missing)?;
    let mut lines = vec![format!(
        "type {name} = {};",
        render_type_expression(&definition.shape, names)?
    )];
    lines.extend(note_rows(&definition.shape, name));
    Ok(lines.join("\n"))
}

pub(crate) fn root_expression(
    document: &SchemaDocument,
    names: &DefinitionNames,
) -> Result<String, ShapeImportRefusal> {
    // The importer reserves # as definition 0. If # is reachable, spell the
    // root through its name too, avoiding a second copy of its recursive body.
    match names.get(&DefinitionId(0)) {
        Some(name) => Ok(name.clone()),
        None => render_type_expression(&document.roots[0], names),
    }
}

fn reserve_shape(names: &mut ReadableNameAllocator, shape: &SchemaShape) {
    match &shape.kind {
        ShapeKind::Named(name) => {
            names.reserve(&render_type_name(name));
        }
        ShapeKind::List(item) => reserve_shape(names, item),
        ShapeKind::Tuple(items) | ShapeKind::Union(items) | ShapeKind::Intersection(items) => {
            for item in items {
                reserve_shape(names, item);
            }
        }
        ShapeKind::Object(object) => {
            for field in &object.fields {
                reserve_shape(names, &field.shape);
            }
            if let ExtraKeys::Open(extra) = &object.extra_keys {
                reserve_shape(names, extra);
            }
        }
        ShapeKind::Process(Some(process)) | ShapeKind::Function(Some(process)) => {
            for param in &process.params {
                reserve_shape(names, &param.shape);
            }
            reserve_shape(names, &process.output);
        }
        _ => {}
    }
}

// Kosaraju with explicit stacks: a long reference chain consumes heap, not
// call stack. Then choose dependency-ready components in first-use order.
fn definition_order(document: &SchemaDocument) -> Result<Vec<DefinitionId>, ShapeImportRefusal> {
    let mut edges = BTreeMap::new();
    let mut pending = document
        .roots
        .iter()
        .flat_map(SchemaDocument::references)
        .collect::<Vec<_>>();
    pending.reverse();
    let mut first_use = Vec::new();
    while let Some(id) = pending.pop() {
        if edges.contains_key(&id) {
            continue;
        }
        let definition =
            document
                .definition(id)
                .ok_or_else(|| ShapeImportRefusal::InvalidSchema {
                    keyword: format!("definition {}", id.0),
                })?;
        let references = SchemaDocument::references(&definition.shape);
        pending.extend(references.iter().rev());
        edges.insert(id, references);
        first_use.push(id);
    }
    let mut visited = BTreeSet::new();
    let mut finished = Vec::new();
    for &id in &first_use {
        let mut stack = vec![(id, false)];
        while let Some((id, exiting)) = stack.pop() {
            if exiting {
                finished.push(id);
                continue;
            }
            if !visited.insert(id) {
                continue;
            }
            stack.push((id, true));
            stack.extend(edges[&id].iter().rev().map(|id| (*id, false)));
        }
    }
    let mut reverse = BTreeMap::<DefinitionId, Vec<DefinitionId>>::new();
    for (&id, references) in &edges {
        for reference in references {
            reverse.entry(*reference).or_default().push(id);
        }
    }
    let mut components = Vec::<Vec<DefinitionId>>::new();
    let mut membership = BTreeMap::new();
    for &id in finished.iter().rev() {
        if membership.contains_key(&id) {
            continue;
        }
        let mut component = Vec::new();
        let mut stack = vec![id];
        while let Some(id) = stack.pop() {
            if membership.contains_key(&id) {
                continue;
            }
            membership.insert(id, components.len());
            component.push(id);
            stack.extend(reverse.get(&id).into_iter().flatten());
        }
        component.sort_by_cached_key(|id| document.canonical_identity(*id));
        components.push(component);
    }
    let mut emitted = BTreeSet::new();
    let mut result = Vec::new();
    while emitted.len() < components.len() {
        for id in &first_use {
            let index = membership[id];
            if emitted.contains(&index) {
                continue;
            }
            if components[index].iter().all(|id| {
                edges[id]
                    .iter()
                    .all(|dep| membership[dep] == index || emitted.contains(&membership[dep]))
            }) {
                result.extend(&components[index]);
                emitted.insert(index);
            }
        }
    }
    Ok(result)
}
