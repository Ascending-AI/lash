//! Selected executable reach. Imports are document-local slots, never operation names.
use lash_kernel_doc::{Document, FunctionCatalog, Node, StartConvention};

use lash_vocabulary::names::FunctionId;
use lash_vocabulary::names::FunctionName;
use lash_vocabulary::names::{ImportSlot, Name, StartSelector};
use std::collections::{BTreeMap, BTreeSet};

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Requirements {
    pub imports: BTreeSet<ImportSlot>,
    pub functions: BTreeMap<FunctionId, FunctionName>,
}
/// References retain callable code; invocations alone can plan unrecorded reads.
/// A dynamically selected callable remains on the recorded operation route.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReachMode {
    Referenced,
    Invoked,
}

impl ReachMode {
    pub fn selected(
        self,
        document: &Document,
        selector: &StartSelector,
        catalog: &dyn FunctionCatalog,
    ) -> Result<Requirements, crate::StartRefusal> {
        selected_with(document, selector, catalog, self)
    }

    pub fn follow(
        self,
        document: &Document,
        required: &mut Requirements,
        roots: BTreeSet<FunctionId>,
        catalog: &dyn FunctionCatalog,
    ) {
        follow_with(document, required, roots, catalog, self);
    }

    pub fn requirements(self, document: &Document, catalog: &dyn FunctionCatalog) -> Requirements {
        requirements_with(document, catalog, self)
    }
}

#[derive(Default)]
struct InvokedEdges {
    imports: BTreeSet<ImportSlot>,
    functions: BTreeSet<FunctionId>,
}

fn invoked_edges(definition: &lash_kernel_doc::FunctionDefinition) -> InvokedEdges {
    let mut edges = InvokedEdges::default();
    if let Some(body) = definition.body() {
        scan(
            &body.block,
            &mut edges.imports,
            &mut edges.functions,
            ReachMode::Invoked,
        );
    }
    edges
}

pub fn selected_reach(
    document: &Document,
    selector: &StartSelector,
    catalog: &dyn FunctionCatalog,
) -> Result<Requirements, crate::StartRefusal> {
    ReachMode::Referenced.selected(document, selector, catalog)
}

pub fn follow(
    document: &Document,
    required: &mut Requirements,
    roots: BTreeSet<FunctionId>,
    catalog: &dyn FunctionCatalog,
) {
    ReachMode::Referenced.follow(document, required, roots, catalog);
}

pub fn requirements(document: &Document, catalog: &dyn FunctionCatalog) -> Requirements {
    ReachMode::Referenced.requirements(document, catalog)
}

fn scan(
    block: &lash_kernel_doc::Block,
    imports: &mut BTreeSet<ImportSlot>,
    functions: &mut BTreeSet<FunctionId>,
    mode: ReachMode,
) {
    let mut pending = vec![Node::Block(block)];
    while let Some(node) = pending.pop() {
        match mode {
            ReachMode::Referenced => {
                imports.extend(node.imports());
                functions.extend(node.functions());
            }
            ReachMode::Invoked => {
                imports.extend(node.invoked_imports());
                functions.extend(node.invocations());
            }
        }
        node.children_into(&mut pending);
    }
}
fn convention(
    convention: &StartConvention,
    imports: &mut BTreeSet<ImportSlot>,
    functions: &mut BTreeSet<FunctionId>,
) {
    for target in convention.targets() {
        match target {
            lash_kernel_doc::FunctionTarget::Local(id) => {
                functions.insert(*id);
            }
            lash_kernel_doc::FunctionTarget::Import(slot) => {
                imports.insert(*slot);
            }
        }
    }
}
/// Follow only the chosen start. Retention separately holds every fill.
fn selected_with(
    document: &Document,
    selector: &StartSelector,
    catalog: &dyn FunctionCatalog,
    mode: ReachMode,
) -> Result<Requirements, crate::StartRefusal> {
    let mut required = Requirements::default();
    let mut roots = BTreeSet::new();
    match selector {
        StartSelector::Main => scan(
            document
                .main
                .as_ref()
                .ok_or(crate::StartRefusal::MissingMain)?,
            &mut required.imports,
            &mut roots,
            mode,
        ),
        StartSelector::Export(name) => {
            let export = crate::startable_export(document, name)?;
            roots.insert(export.function);
            convention(&export.convention, &mut required.imports, &mut roots);
        }
    }
    follow_with(document, &mut required, roots, catalog, mode);
    Ok(required)
}
fn follow_with(
    document: &Document,
    required: &mut Requirements,
    roots: BTreeSet<FunctionId>,
    catalog: &dyn FunctionCatalog,
    mode: ReachMode,
) {
    let mut allowed = document.library.0.clone();
    let mut pending: Vec<_> = roots.into_iter().collect();
    while let Some(id) = pending.pop() {
        if required.functions.contains_key(&id) {
            continue;
        }
        if let Some(function) = document.functions.get(&id) {
            required.functions.insert(id, function.name.clone());
            if let Some(body) = function.body() {
                let mut next = BTreeSet::new();
                scan(&body.block, &mut required.imports, &mut next, mode);
                pending.extend(next);
            }
        } else if allowed.contains(&id) {
            let Some(header) = catalog.header(&id) else {
                continue;
            };
            required.functions.insert(id, header.name.clone());
            if mode == ReachMode::Invoked {
                let edges = match catalog.registered(&id) {
                    Some(registered) => {
                        use lash_kernel_doc::core_support::RegisteredFunctionCoreSupport as _;
                        registered.derive(|| invoked_edges(registered.definition()))
                    }
                    None => {
                        let Some(definition) = catalog.definition(&id) else {
                            continue;
                        };
                        std::sync::Arc::new(invoked_edges(definition))
                    }
                };
                required.imports.extend(&edges.imports);
                allowed.extend(&edges.functions);
                pending.extend(&edges.functions);
            } else if let Some(registered) = catalog.registered(&id) {
                required.imports.extend(registered.imports().keys());
                allowed.extend(registered.callees());
                pending.extend(registered.callees());
            } else if let Some(body) = catalog
                .definition(&id)
                .and_then(lash_kernel_doc::FunctionDefinition::body)
            {
                required.imports.extend(body.imports.keys());
                allowed.extend(body.functions.keys());
                pending.extend(body.functions.keys());
            }
        }
    }
}
/// Reach of every offered Export and optional main, for publication and editor inspection.
fn requirements_with(
    document: &Document,
    catalog: &dyn FunctionCatalog,
    mode: ReachMode,
) -> Requirements {
    let mut required = Requirements::default();
    let mut roots = BTreeSet::new();
    if let Some(main) = &document.main {
        scan(main, &mut required.imports, &mut roots, mode);
    }
    for export in document.exports.values() {
        match export {
            lash_kernel_doc::Export::Function(f) => {
                roots.insert(f.function);
                convention(&f.convention, &mut required.imports, &mut roots);
            }
            lash_kernel_doc::Export::Type(ty) => {
                roots.insert(ty.checker);
                for member in ty.members.values() {
                    let f = member.function();
                    roots.insert(f.function);
                    convention(&f.convention, &mut required.imports, &mut roots);
                }
            }
            lash_kernel_doc::Export::Error { .. } => {}
        }
    }
    follow_with(document, &mut required, roots, catalog, mode);
    required
}
pub fn entry_reachability(
    document: &Document,
    name: &Name,
    catalog: &dyn FunctionCatalog,
) -> Result<Requirements, crate::StartRefusal> {
    selected_reach(document, &StartSelector::Export(name.clone()), catalog)
}
pub fn library_performs(catalog: &dyn FunctionCatalog, id: &FunctionId) -> BTreeSet<ImportSlot> {
    let mut imports = BTreeSet::new();
    let mut reached = BTreeSet::new();
    let mut pending = vec![*id];
    while let Some(id) = pending.pop() {
        if !reached.insert(id) {
            continue;
        }
        if let Some(body) = catalog
            .definition(&id)
            .and_then(lash_kernel_doc::FunctionDefinition::body)
        {
            let mut next = BTreeSet::new();
            scan(&body.block, &mut imports, &mut next, ReachMode::Referenced);
            pending.extend(next);
        }
    }
    imports
}
