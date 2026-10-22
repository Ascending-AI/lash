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
fn scan(
    block: &lash_kernel_doc::Block,
    imports: &mut BTreeSet<ImportSlot>,
    functions: &mut BTreeSet<FunctionId>,
) {
    let mut pending = vec![Node::Block(block)];
    while let Some(node) = pending.pop() {
        imports.extend(node.imports());
        functions.extend(node.functions());
        if let Node::Action(action) = node {
            let atoms: &[lash_kernel_doc::Atom] = match action {
                lash_kernel_doc::Action::Call { args, .. }
                | lash_kernel_doc::Action::Spawn { args, .. }
                | lash_kernel_doc::Action::Perform { args, .. } => args,
                lash_kernel_doc::Action::Await { value, .. } => std::slice::from_ref(value),
                lash_kernel_doc::Action::Join { task }
                | lash_kernel_doc::Action::Cancel { task } => std::slice::from_ref(task),
                lash_kernel_doc::Action::JoinMany { tasks, .. } => std::slice::from_ref(tasks),
                lash_kernel_doc::Action::Yield => &[],
            };
            for atom in atoms {
                if let lash_kernel_doc::Atom::Literal(lash_kernel_doc::Literal::Function(id)) = atom
                {
                    functions.insert(*id);
                }
            }
        }
        pending.extend(node.children());
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
pub fn selected_reach(
    document: &Document,
    selector: &StartSelector,
    catalog: &dyn FunctionCatalog,
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
        ),
        StartSelector::Export(name) => {
            let export = crate::startable_export(document, name)?;
            roots.insert(export.function);
            convention(&export.convention, &mut required.imports, &mut roots);
        }
    }
    follow(document, &mut required, roots, catalog);
    Ok(required)
}
pub fn follow(
    document: &Document,
    required: &mut Requirements,
    roots: BTreeSet<FunctionId>,
    catalog: &dyn FunctionCatalog,
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
                scan(&body.block, &mut required.imports, &mut next);
                pending.extend(next);
            }
        } else if allowed.contains(&id) {
            let Some(header) = catalog.header(&id) else {
                continue;
            };
            required.functions.insert(id, header.name.clone());
            if let Some(registered) = catalog.registered(&id) {
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
pub fn requirements(document: &Document, catalog: &dyn FunctionCatalog) -> Requirements {
    let mut required = Requirements::default();
    let mut roots = BTreeSet::new();
    if let Some(main) = &document.main {
        scan(main, &mut required.imports, &mut roots);
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
    follow(document, &mut required, roots, catalog);
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
            scan(&body.block, &mut imports, &mut next);
            pending.extend(next);
        }
    }
    imports
}
