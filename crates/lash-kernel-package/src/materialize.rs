//! Turn retained callable state into ordinary authored code, without executing it.
use crate::{LinkRefusal, LinkedContent};
use lash_kernel_doc::*;
use std::collections::{BTreeMap, BTreeSet};
use {
    lash_kernel_value::names::*, lash_kernel_value::package::LinkId,
    lash_kernel_value::sites::Site, lash_kernel_value::sites::Unit,
};

mod lexical;
mod source;
mod values;
use values::Values;

/// Why frozen runtime data cannot become ordinary document content.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum MaterializeRefusal {
    #[error("unrepresentable value at node {node} {path}: {reason:?}")]
    UnrepresentableValue {
        node: u32,
        path: String,
        reason: UnrepresentableValue,
    },
    #[error("node {node} refers to missing environment {index}")]
    InvalidReference { node: u32, index: u32 },
    #[error("callable {callable:?} has no authored body or installed helper")]
    OpaqueCallable { callable: CallableId },
    #[error("materialized {resource} takes {actual}, above {limit}")]
    LimitExceeded {
        resource: String,
        limit: u64,
        actual: u64,
    },
    #[error("missing construction helper {name}")]
    MissingHelper { name: String },
    #[error(transparent)]
    Package(#[from] LinkRefusal),
    #[error(transparent)]
    Decode(#[from] DecodeRefusal),
    #[error(transparent)]
    Callable(#[from] FunctionValueRefusal),
    #[error(transparent)]
    Check(#[from] lash_kernel_check::Refusal),
    #[error(transparent)]
    Captions(#[from] CaptionsRefusal),
    #[error("invalid materialized document: {message}")]
    Document { message: String },
}
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UnrepresentableValue {
    LiveHandle,
    OpaqueValue,
}

/// An owned document; no saved-definition or capture descriptor is retained.
pub struct MaterializedWorkflow {
    pub document: Document,
    pub selector: StartSelector,
    pub captions: Captions,
    pub dropped: Vec<CaptionTarget>,
}

/// Open retained content without granting execution authority or writing artifacts.
pub fn materialize_workflow(
    content: &LinkedContent,
    reference: &WorkflowReference,
    function: Option<&FunctionValue>,
    captions: Option<&Captions>,
    helpers: &BuildHelpers,
) -> Result<MaterializedWorkflow, MaterializeRefusal> {
    let original = content.document(content.link(reference.link())?.document)?;
    if let WorkflowReference::Linked(start) = reference {
        let captions = captions
            .cloned()
            .unwrap_or_else(|| Captions::empty(original.identity()));
        captions.validate(original)?;
        return Ok(MaterializedWorkflow {
            document: original.clone(),
            selector: start.selector.clone(),
            captions,
            dropped: vec![],
        });
    }
    let function = function.ok_or_else(|| MaterializeRefusal::Document {
        message: "callable reference needs its frozen value".into(),
    })?;
    if *reference != WorkflowReference::Callable(function.callable) {
        return Err(MaterializeRefusal::Callable(FunctionValueRefusal::Identity));
    }
    let nodes = bounded_nodes(function)?;
    let mut document = Document::new(original.numbers, vec![]);
    document.main = None;
    let private = original
        .functions
        .get(&function.callable.function)
        .map(|f| f.signature.clone())
        .or_else(|| {
            helpers
                .header(&function.callable.function)
                .map(|f| f.signature.clone())
        })
        .ok_or(MaterializeRefusal::OpaqueCallable {
            callable: function.callable,
        })?;
    let mut used: BTreeSet<Name> = private.params.iter().map(|p| p.name.clone()).collect();
    for doc in content.documents.values() {
        for definition in doc.functions.values() {
            used.extend(definition.signature.params.iter().map(|p| p.name.clone()));
            if let Some(body) = definition.body() {
                collect_names(Node::Block(&body.block), &mut used);
            }
        }
    }
    // Captures may occur only as action callees or surplus values, so the
    // executable-name walk alone cannot reserve all lexical cell names.
    used.extend(nodes.iter().flat_map(|node| node.captures.keys().cloned()));
    let mut fresh = |preferred: String| {
        let mut name = Name::new(&preferred);
        let mut suffix = 0;
        while !used.insert(name.clone()) {
            suffix += 1;
            name = Name::new(format!("{preferred}_{suffix}"));
        }
        name
    };
    let callable_names: Vec<_> = (0..nodes.len())
        .map(|n| fresh(format!("saved_callable_{n}")))
        .collect();
    let environment_names: Vec<_> = (0..nodes.len())
        .map(|n| fresh(format!("saved_environment_{n}")))
        .collect();
    let setter_names: Vec<_> = (0..nodes.len())
        .map(|n| fresh(format!("saved_fill_{n}")))
        .collect();
    let mut values = Values::new(&nodes, original.numbers, helpers);
    values.names = callable_names.clone();
    let mut initializers = Vec::new();
    for (n, node) in nodes.iter().enumerate() {
        for (name, value) in &node.captures {
            initializers.push((
                n,
                name.clone(),
                values.capture(n as u32, &format!("captures.{name}"), value, 0)?,
            ));
        }
    }
    function.check(
        helpers,
        FunctionValueLimits::standard(),
        lash_kernel_value::data_limits::KernelDataLimits::standard(),
    )?;
    let mut block = Vec::new();
    let mut factories = BTreeSet::new();
    let mut sites = BTreeMap::<(usize, LinkId, Site), Site>::new();
    let mut root_convention = StartConvention::Direct;
    let mut slots = BTreeMap::new();
    let mut code_budget = Budget {
        bytes: 0,
        limit: 4 * 1024 * 1024,
    };
    for (n, node) in nodes.iter().enumerate() {
        let source = content.document(content.link(node.callable.link)?.document)?;
        let mut needed: BTreeSet<_> = [node.callable.function].into();
        needed.extend(node.convention.functions().into_iter().copied());
        let mut pending: Vec<_> = needed.iter().copied().collect();
        while let Some(id) = pending.pop() {
            if let Some(body) = source.functions.get(&id).and_then(|d| d.body()) {
                for child in Node::Block(&body.block).functions_within() {
                    if needed.insert(child) {
                        pending.push(child);
                    }
                }
            }
        }
        let mut copied = Document::new(source.numbers, vec![]);
        for id in &needed {
            if let Some(definition) = source.functions.get(id) {
                if definition.body().is_none() {
                    if !helpers.contains(id) {
                        return Err(MaterializeRefusal::OpaqueCallable {
                            callable: CallableId {
                                link: node.callable.link,
                                function: *id,
                            },
                        });
                    }
                } else {
                    code_budget.add(definition)?;
                    copied.functions.insert(*id, definition.clone());
                }
            }
        }
        let catalog = DocumentCatalog::new(source, helpers);
        let definition = source.functions.get(&node.callable.function);
        if let Some(definition) = definition {
            lash_kernel_check::derive_captured_definition(definition, &catalog, &node.captures)?;
        } else if !helpers.contains(&node.callable.function) || !node.captures.is_empty() {
            return Err(MaterializeRefusal::OpaqueCallable {
                callable: node.callable,
            });
        }
        let mut ids = BTreeMap::new();
        for id in &needed {
            if let Some(definition) = copied.functions.get_mut(id) {
                definition.name = FunctionName::new(format!("materialized.node{n}.f{}", ids.len()))
                    .map_err(|e| MaterializeRefusal::Document {
                        message: e.to_string(),
                    })?;
                ids.insert(*id, definition.identity());
            } else {
                document.library.insert(*id);
            }
        }
        // Import slots are scoped by defining Link, never by their old integer alone.
        for id in &needed {
            if let Some(body) = source.functions.get(id).and_then(|d| d.body()) {
                for (slot, import) in &body.imports {
                    let key = (node.callable.link, *slot);
                    if let Some(new) = slots.get(&key) {
                        if document.imports[new] != *import {
                            return Err(MaterializeRefusal::Document {
                                message: "inconsistent import signatures within a defining Link"
                                    .into(),
                            });
                        }
                    } else {
                        let new = ImportSlot(document.imports.len() as u32);
                        slots.insert(key, new);
                        document.imports.insert(new, import.clone());
                    }
                }
            }
        }
        for target in node.convention.targets() {
            if let FunctionTarget::Import(slot) = target {
                let key = (node.callable.link, *slot);
                if let std::collections::btree_map::Entry::Vacant(entry) = slots.entry(key) {
                    let import =
                        source
                            .imports
                            .get(slot)
                            .ok_or_else(|| MaterializeRefusal::Document {
                                message: "missing convention import".into(),
                            })?;
                    let new = ImportSlot(document.imports.len() as u32);
                    entry.insert(new);
                    document.imports.insert(new, import.clone());
                }
            }
        }
        let slot = |slot| slots[&(node.callable.link, slot)];
        let mut convention = node.convention.clone();
        for target in convention.targets_mut() {
            remap_target(target, &ids, &slot);
        }
        if n == 0 {
            root_convention = convention.clone();
        }
        for (old, new) in &ids {
            let mut definition = copied.functions[old].clone();
            let mut normalized_paths = BTreeMap::new();
            let parameter_names = definition.signature.params.clone();
            if let Some(body) = definition.body_mut() {
                if matches!(
                    node.convention,
                    StartConvention::TsArgsList { .. } | StartConvention::TsDirect { .. }
                ) {
                    normalized_paths =
                        source::declarations(&mut body.block, &parameter_names, source);
                }
                remap(NodeMut::Block(&mut body.block), &ids, &slot);
                body.imports = std::mem::take(&mut body.imports)
                    .into_iter()
                    .map(|(slot, import)| (slots[&(node.callable.link, slot)], import))
                    .collect();
                body.functions = Node::Block(&body.block)
                    .functions_within()
                    .into_iter()
                    .filter_map(|id| {
                        ids.iter()
                            .find(|(_, new)| **new == id)
                            .map(|(old, _)| (id, copied.functions[old].name.clone()))
                            .or_else(|| helpers.header(&id).map(|h| (id, h.name.clone())))
                    })
                    .collect();
            }
            for (site, _) in source
                .site_ordinals()
                .iter()
                .filter(|(s, _)| s.unit == Unit::Library(*old))
            {
                sites.insert(
                    (n, node.callable.link, site.clone()),
                    Site::new(
                        Unit::Library(*new),
                        normalized_paths
                            .get(&site.path)
                            .cloned()
                            .unwrap_or_else(|| site.path.clone()),
                    ),
                );
            }
            // Include non-execution nodes and function-body captions too.
            sites.insert(
                (n, node.callable.link, Site::new(Unit::Library(*old), [])),
                Site::new(Unit::Library(*new), []),
            );
            document.functions.insert(*new, definition);
            document
                .source_evidence
                .insert(*new, node.implicit_text_conversions.clone());
        }
        let raw = ids.get(&node.callable.function).map_or_else(
            || Expr::Literal(Literal::Function(node.callable.function)),
            |id| {
                Expr::Closure(Box::new(Closure {
                    function: *id,
                    presentation: Some(CallablePresentation {
                        signature: node.signature.clone(),
                        representation: node.representation.clone(),
                        convention: convention.clone(),
                    }),
                }))
            },
        );
        // One lexical factory per environment keeps copied code unchanged, even
        // when one nested body is instantiated beneath different local shadows.
        let ts = matches!(node.convention, StartConvention::TsArgsList { .. });
        let factory_name = fresh(format!("saved_factory_{n}"));
        factories.insert(factory_name.clone());
        let factory = lexical::environment(&mut document, n, node, raw, helpers, &mut fresh)?;
        block.push(binding(factory_name.clone(), factory));
        block.push(binding(
            environment_names[n].clone(),
            source::call(
                &factory_name,
                vec![],
                ts,
                matches!(node.convention, StartConvention::PyArgsList { .. }),
                true,
                helpers,
                &mut fresh,
            )?,
        ));
        block.push(binding(
            callable_names[n].clone(),
            source::member(&environment_names[n], 0, ts),
        ));
        block.push(binding(
            setter_names[n].clone(),
            source::member(&environment_names[n], 1, ts),
        ));
    }
    // Every environment's cells, callable and setter now exist. Fill those
    // same cells only after all callable/token objects have been constructed.
    let mut fills = vec![vec![]; nodes.len()];
    for (n, name, value) in initializers {
        let initializer = fresh(name.to_string());
        block.push(binding(initializer.clone(), value));
        fills[n].push(Expr::Variable(initializer));
    }
    for (n, args) in fills.into_iter().enumerate() {
        block.push(Stmt::Do {
            value: source::call(
                &setter_names[n],
                args,
                matches!(nodes[n].convention, StartConvention::TsArgsList { .. }),
                matches!(nodes[n].convention, StartConvention::PyArgsList { .. }),
                false,
                helpers,
                &mut fresh,
            )?,
        });
    }
    document.library.0.extend(values.library);
    let ts = matches!(root_convention, StartConvention::TsArgsList { .. });
    let py = matches!(root_convention, StartConvention::PyArgsList { .. });
    let params = if ts || py {
        function.signature.params.clone()
    } else {
        private.params
    };
    let invocation = if let StartConvention::PyArgsList { positional } = root_convention {
        let positional = positional as usize;
        Expr::Act(Box::new(ActExpr {
            kind: ActKind::Call(Callee::Local(source::helper(helpers, "py.call")?)),
            args: vec![
                Expr::Variable(callable_names[0].clone()),
                Expr::List(
                    params
                        .iter()
                        .take(positional)
                        .map(|p| Expr::Variable(p.name.clone()))
                        .collect(),
                ),
                Expr::Map(
                    params
                        .iter()
                        .skip(positional)
                        .map(|p| MapEntry {
                            key: Expr::Literal(Literal::Text(p.name.to_string())),
                            value: Expr::Variable(p.name.clone()),
                        })
                        .collect(),
                ),
                Expr::Literal(Literal::Absent),
            ],
        }))
    } else {
        source::call(
            &callable_names[0],
            params
                .iter()
                .map(|p| Expr::Variable(p.name.clone()))
                .collect(),
            ts,
            py,
            false,
            helpers,
            &mut fresh,
        )?
    };
    block.push(Stmt::Return { value: invocation });
    if py {
        root_convention = StartConvention::Direct;
    }
    if let StartConvention::TsArgsList { await_fn, .. } = root_convention {
        root_convention = StartConvention::TsDirect { await_fn };
    }
    if py {
        source::python_authored(
            &mut document,
            &mut block,
            &params,
            &factories,
            helpers,
            &mut fresh,
        )?;
    }
    let wrapper = lexical::definition(
        "materialized.entry".into(),
        params
            .into_iter()
            .map(|mut param| {
                param.ty = lash_kernel_value::Type::Any;
                param
            })
            .collect(),
        lash_kernel_value::Type::Any,
        block,
        &document,
        helpers,
    )?;
    let wrapper_id = wrapper.identity();
    document.functions.insert(wrapper_id, wrapper);
    document.exports.insert(
        Name::new("run"),
        Export::Function(FunctionExport {
            function: wrapper_id,
            signature: function.signature.clone(),
            convention: root_convention,
            asynchronous: function
                .written_signature
                .as_ref()
                .is_some_and(|written| written.language == SourceLanguage::Python),
            rest: false,
        }),
    );
    document.main = Some(vec![binding(
        Name::new("run"),
        Expr::Literal(Literal::Function(wrapper_id)),
    )]);
    let used_helpers: BTreeSet<_> = document
        .units()
        .flat_map(|(_, block)| Node::Block(block).functions_within())
        .filter(|id| helpers.contains(id))
        .collect();
    document.library.0.extend(used_helpers);
    // Helper bodies retain their fixed import slots. Assign those first, then
    // relocate the copied imports and every reference to them as one document.
    let authored_imports = std::mem::take(&mut document.imports);
    lash_kernel_doc::library_requirements(&mut document, helpers).map_err(
        |refusal| match refusal {
            lash_kernel_doc::LibraryRequirementRefusal::MissingHelper { function } => {
                MaterializeRefusal::MissingHelper {
                    name: function.to_string(),
                }
            }
            refusal => MaterializeRefusal::Document {
                message: refusal.to_string(),
            },
        },
    )?;
    let mut relocated = BTreeMap::new();
    let mut next = ImportSlot(0);
    for (old, import) in authored_imports {
        while document.imports.contains_key(&next) {
            next.0 += 1;
        }
        document.imports.insert(next, import);
        relocated.insert(old, next);
    }
    let slot = |slot| relocated[&slot];
    let ids = BTreeMap::new();
    for definition in document.functions.values_mut() {
        if let Some(body) = definition.body_mut() {
            remap(NodeMut::Block(&mut body.block), &ids, &slot);
            body.imports = std::mem::take(&mut body.imports)
                .into_iter()
                .map(|(old, import)| (slot(old), import))
                .collect();
        }
    }
    if let Some(main) = &mut document.main {
        remap(NodeMut::Block(main), &ids, &slot);
    }
    for export in document.exports.values_mut() {
        if let Export::Function(function) = export {
            for target in function.convention.targets_mut() {
                remap_target(target, &ids, &slot);
            }
        }
    }
    let reidentified = document
        .reidentify_functions()
        .map_err(|message| MaterializeRefusal::Document { message })?;
    Budget {
        bytes: 0,
        limit: 4 * 1024 * 1024,
    }
    .add(&document)?;
    validate_document(&document, helpers).map_err(|e| MaterializeRefusal::Document {
        message: e.to_string(),
    })?;
    let mut entries = vec![];
    let mut dropped = vec![];
    for (n, node) in nodes.iter().enumerate() {
        let source = content.document(content.link(node.callable.link)?.document)?;
        let captions = if n == 0 {
            captions.unwrap_or(&node.captions)
        } else {
            &node.captions
        };
        captions.validate(source)?;
        for entry in captions.entries() {
            let mut next = entry.clone();
            let target = match &entry.target {
                CaptionTarget::Site(old)
                    if n == 0
                        && old.unit == Unit::Library(node.callable.function)
                        && old.path.is_empty() =>
                {
                    Some(CaptionTarget::Site(Site::new(
                        Unit::Library(reidentified.get(&wrapper_id).copied().unwrap_or(wrapper_id)),
                        [],
                    )))
                }
                CaptionTarget::Site(old) => {
                    sites
                        .get(&(n, node.callable.link, old.clone()))
                        .map(|site| {
                            let mut site = site.clone();
                            if let Unit::Library(id) = &mut site.unit {
                                *id = reidentified.get(id).copied().unwrap_or(*id);
                            }
                            CaptionTarget::Site(site)
                        })
                }
                CaptionTarget::Param(param)
                    if n == 0
                        && selected_caption(source, &param.function, node.callable.function) =>
                {
                    Some(CaptionTarget::Param(ParamTarget {
                        function: Name::new("run"),
                        param: param.param.clone(),
                    }))
                }
                CaptionTarget::Field(field)
                    if n == 0
                        && selected_caption(source, &field.function, node.callable.function) =>
                {
                    Some(CaptionTarget::Field(FieldTarget {
                        function: Name::new("run"),
                        path: field.path.clone(),
                    }))
                }
                _ => None,
            };
            if let Some(target) = target.filter(|t| t.kind(&document).is_some()) {
                next.target = target;
                entries.push(next);
            } else if n == 0 {
                dropped.push(entry.target.clone());
            }
        }
    }
    let captions = Captions::new(&document, entries)?;
    Ok(MaterializedWorkflow {
        document,
        selector: StartSelector::Export(Name::new("run")),
        captions,
        dropped,
    })
}

fn binding(name: Name, value: Expr) -> Stmt {
    let value = match value {
        Expr::Act(ref act) => act
            .as_action()
            .map_or_else(|| Rhs::Expr(value.clone()), Rhs::Action),
        _ => Rhs::Expr(value),
    };
    Stmt::Let { name, value }
}
fn collect_names(node: Node<'_>, names: &mut BTreeSet<Name>) {
    match node {
        Node::Stmt(Stmt::Let { name, .. } | Stmt::For { binding: name, .. })
        | Node::Expr(Expr::Let { name, .. } | Expr::Variable(name)) => {
            names.insert(name.clone());
        }
        Node::Stmt(Stmt::Try(scope)) => {
            if let Some(catch) = &scope.catch {
                names.insert(catch.binding.clone());
            }
        }
        _ => {}
    }
    for child in node.children() {
        collect_names(child, names);
    }
}
fn remap_target(
    target: &mut FunctionTarget,
    ids: &BTreeMap<FunctionId, FunctionId>,
    slot: &impl Fn(ImportSlot) -> ImportSlot,
) {
    match target {
        FunctionTarget::Local(id) => *id = ids.get(id).copied().unwrap_or(*id),
        FunctionTarget::Import(s) => *s = slot(*s),
    }
}
fn remap(
    mut node: NodeMut<'_>,
    ids: &BTreeMap<FunctionId, FunctionId>,
    slots: &impl Fn(ImportSlot) -> ImportSlot,
) {
    for id in node.functions_mut() {
        *id = ids.get(id).copied().unwrap_or(*id);
    }
    let slot = |s: &mut ImportSlot| *s = slots(*s);
    let target = |t: &mut FunctionTarget| {
        if let FunctionTarget::Import(s) = t {
            slot(s);
        }
    };
    match &mut node {
        NodeMut::Stmt(Stmt::ReturnVia { policy, .. }) => target(policy),
        NodeMut::Stmt(Stmt::For { via: Some(via), .. }) => {
            target(&mut via.open);
            for t in via
                .next
                .iter_mut()
                .chain(&mut via.item)
                .chain(&mut via.check)
            {
                target(t);
            }
        }
        NodeMut::Action(Action::Perform { slot: s, .. }) => slot(s),
        NodeMut::Action(
            Action::Call {
                callee: Callee::Import(s),
                ..
            }
            | Action::Spawn {
                callee: Callee::Import(s),
                ..
            },
        ) => slot(s),
        NodeMut::Action(Action::Await { policy, .. }) => target(policy),
        NodeMut::Expr(Expr::Act(act)) => match &mut act.kind {
            ActKind::Perform { slot: s, .. }
            | ActKind::Call(Callee::Import(s))
            | ActKind::Spawn(Callee::Import(s)) => slot(s),
            ActKind::Await(t) => target(t),
            _ => {}
        },
        NodeMut::Expr(Expr::Closure(c)) => {
            if let Some(p) = &mut c.presentation {
                for t in p.convention.targets_mut() {
                    target(t);
                }
            }
        }
        _ => {}
    }
    for child in node.children() {
        remap(child, ids, slots);
    }
}

// Count encoded expansion without allocating an encoded copy of a possibly large body.
struct Budget {
    bytes: u64,
    limit: u64,
}
impl std::io::Write for Budget {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.bytes = self.bytes.saturating_add(bytes.len() as u64);
        if self.bytes > self.limit {
            return Err(std::io::Error::other("materialization size bound"));
        }
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}
impl Budget {
    fn add(&mut self, value: &impl serde::Serialize) -> Result<(), MaterializeRefusal> {
        serde_json::to_writer(&mut *self, value).map_err(|_| MaterializeRefusal::LimitExceeded {
            resource: "document bytes".into(),
            limit: self.limit,
            actual: self.bytes,
        })
    }
}
fn bounded_nodes(root: &FunctionValue) -> Result<Vec<&FunctionValue>, MaterializeRefusal> {
    let mut nodes = vec![root];
    let mut pending: Vec<_> = root.captures.values().rev().map(|v| (v, 1)).collect();
    while let Some((value, depth)) = pending.pop() {
        if depth > 48 {
            return Err(MaterializeRefusal::LimitExceeded {
                resource: "environment depth".into(),
                limit: 48,
                actual: depth,
            });
        }
        if nodes.len() > FunctionValue::MAX_ENVIRONMENT_NODES as usize {
            return Err(MaterializeRefusal::LimitExceeded {
                resource: "environment nodes".into(),
                limit: u64::from(FunctionValue::MAX_ENVIRONMENT_NODES),
                actual: nodes.len() as u64,
            });
        }
        let children: Vec<_> = match value {
            CaptureValue::Function(f) => {
                nodes.push(f);
                f.captures.values().collect()
            }
            CaptureValue::Tuple(v) | CaptureValue::List(v) | CaptureValue::Set(v) => {
                v.iter().collect()
            }
            CaptureValue::Record(v) => v.iter().map(|(_, v)| v).collect(),
            CaptureValue::Map(v) => v.iter().flat_map(|(k, v)| [k, v]).collect(),
            CaptureValue::Error { data, .. } => vec![data.as_ref()],
            _ => vec![],
        };
        pending.extend(children.into_iter().rev().map(|v| (v, depth + 1)));
        if pending.len() > FunctionValueLimits::CEILING as usize {
            return Err(MaterializeRefusal::LimitExceeded {
                resource: "value nodes".into(),
                limit: FunctionValueLimits::CEILING,
                actual: pending.len() as u64,
            });
        }
    }
    Ok(nodes)
}

#[cfg(test)]
mod laws;

fn selected_caption(document: &Document, name: &Name, function: FunctionId) -> bool {
    matches!(document.exports.get(name), Some(Export::Function(export)) if export.function == function)
}
