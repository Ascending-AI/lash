//! What every document rewrite shares: restating the library functions a
//! document lists, replacing their identities, and the correspondence of a
//! rewrite that moves no node.

use lash_kernel_doc::{Correspondence, Survivor};

use std::collections::BTreeMap;
use std::convert::Infallible;

use lash_kernel_doc::{
    Block, Document, FunctionBody, FunctionCatalog, FunctionDefinition, Implementation, Node,
    NodeMut,
};

use lash_kernel_value::names::FunctionId;
use lash_kernel_value::names::FunctionName;
use lash_kernel_value::sites::Site;
use lash_kernel_value::sites::Unit;

use crate::migration::DocumentRefusal;

/// A library function redeclared for the next kernel version.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Redeclared {
    pub from: FunctionId,
    pub to: FunctionId,
    pub definition: FunctionDefinition,
}

/// Redeclares `functions` and every function their bodies call, each after
/// the functions it calls. `definition` redeclares one function for the next
/// version; the identities its body calls are replaced here.
pub fn redeclare(
    definition: fn(&FunctionDefinition) -> Result<FunctionDefinition, DocumentRefusal>,
    functions: impl IntoIterator<Item = FunctionId>,
    catalog: &dyn FunctionCatalog,
) -> Result<Vec<Redeclared>, DocumentRefusal> {
    let mut done = BTreeMap::new();
    let mut ordered = Vec::new();
    for function in functions {
        redeclare_one(definition, function, catalog, &mut done, &mut ordered)?;
    }
    Ok(ordered)
}

/// A body names what it calls by identity, a hash of the callee, so the
/// calls of a library form no cycle and the recursion ends.
fn redeclare_one(
    definition: fn(&FunctionDefinition) -> Result<FunctionDefinition, DocumentRefusal>,
    function: FunctionId,
    catalog: &dyn FunctionCatalog,
    done: &mut BTreeMap<FunctionId, FunctionId>,
    ordered: &mut Vec<Redeclared>,
) -> Result<FunctionId, DocumentRefusal> {
    if let Some(to) = done.get(&function) {
        return Ok(*to);
    }
    let Some(written) = catalog.definition(&function) else {
        return Err(DocumentRefusal::FunctionNotHeld { function });
    };
    let called: Vec<FunctionId> = written
        .body()
        .map(|body| body.functions.keys().copied().collect())
        .unwrap_or_default();
    for called in called {
        redeclare_one(definition, called, catalog, done, ordered)?;
    }
    let mut redeclared = definition(written)?;
    if let Implementation::Guest { body, .. } | Implementation::Both { body, .. } =
        &mut redeclared.implementation
    {
        replace_in_body(function, body, done);
    }
    let to = redeclared.identity();
    done.insert(function, to);
    ordered.push(Redeclared {
        from: function,
        to,
        definition: redeclared,
    });
    Ok(to)
}

/// Replaces each library function `document` lists and holds, in any slot,
/// with the one `functions` maps it to. A function the map does not name is
/// kept.
pub fn replace_functions(document: &mut Document, functions: &BTreeMap<FunctionId, FunctionId>) {
    let Ok(()) = document.try_each_function_mut(|_, function| replace(function, functions));
    fn export(
        f: &mut lash_kernel_doc::FunctionExport,
        functions: &BTreeMap<FunctionId, FunctionId>,
    ) {
        if let Some(id) = functions.get(&f.function) {
            f.function = *id;
        }
        for target in f.convention.targets_mut() {
            if let lash_kernel_doc::FunctionTarget::Local(id) = target
                && let Some(new) = functions.get(id)
            {
                *id = *new;
            }
        }
    }
    for value in document.exports.values_mut() {
        match value {
            lash_kernel_doc::Export::Function(f) => export(f, functions),
            lash_kernel_doc::Export::Type(ty) => {
                if let Some(id) = functions.get(&ty.checker) {
                    ty.checker = *id;
                }
                for member in ty.members.values_mut() {
                    match member {
                        lash_kernel_doc::TypeMember::Static(f)
                        | lash_kernel_doc::TypeMember::Method(f)
                        | lash_kernel_doc::TypeMember::Property(f) => export(f, functions),
                    }
                }
            }
            lash_kernel_doc::Export::Error { .. } => {}
        }
    }
}

/// Replaces each library function the body of `function` lists and holds,
/// as [`replace_functions`] does a document's.
pub(crate) fn replace_in_body(
    function: FunctionId,
    body: &mut FunctionBody,
    functions: &BTreeMap<FunctionId, FunctionId>,
) {
    body.functions = replace_listed(&mut body.functions, functions);
    let Ok(()) = NodeMut::Block(&mut body.block)
        .try_each_function_mut(Site::new(Unit::Library(function), []), &mut |_, held| {
            replace(held, functions)
        });
}

fn replace_listed(
    listed: &mut BTreeMap<FunctionId, FunctionName>,
    functions: &BTreeMap<FunctionId, FunctionId>,
) -> BTreeMap<FunctionId, FunctionName> {
    std::mem::take(listed)
        .into_iter()
        .map(|(id, name)| (functions.get(&id).copied().unwrap_or(id), name))
        .collect()
}

fn replace(
    function: &mut FunctionId,
    functions: &BTreeMap<FunctionId, FunctionId>,
) -> Result<(), Infallible> {
    if let Some(to) = functions.get(function) {
        *function = *to;
    }
    Ok(())
}

/// The correspondence of a rewrite that moved no node of `main` or of a
/// declared function: every node of `base` to the same site of `result`.
/// Blocks survive changes beneath them; their children carry separately.
pub fn unchanged(base: &Document, result: &Document) -> Result<Correspondence, DocumentRefusal> {
    Correspondence::of(
        base.identity(),
        result.identity(),
        unmoved(base)
            .into_iter()
            .filter(|survivor| {
                base.node(&survivor.from)
                    .zip(result.node(&survivor.to))
                    .is_some_and(|(old, new)| {
                        matches!((old, new), (Node::Block(_), Node::Block(_))) || old == new
                    })
            })
            .collect(),
    )
    .ok_or(DocumentRefusal::Correspondence)
}

/// Every node of `base`'s `main` and declared functions, at the same site.
pub(crate) fn unmoved(base: &Document) -> Vec<Survivor> {
    let mut entries = Vec::new();
    for (unit, body) in base.units() {
        same_sites(unit.clone(), unit, body, &mut entries);
    }
    entries
}

/// Lists every node of `body`, the code of `from`, as at the same path of
/// `to`.
pub(crate) fn same_sites(from: Unit, to: Unit, body: &Block, entries: &mut Vec<Survivor>) {
    let mut pending = vec![(Vec::new(), Node::Block(body))];
    while let Some((path, node)) = pending.pop() {
        if let Node::Block(block) = node {
            let mut end = path.clone();
            end.push(block.len() as u32);
            entries.push(Survivor::new(
                Site::new(from.clone(), end.clone()),
                Site::new(to.clone(), end),
                false,
            ));
        }
        for (index, child) in (0u32..).zip(node.children()) {
            let mut child_path = path.clone();
            child_path.push(index);
            pending.push((child_path, child));
        }
        entries.push(Survivor {
            from: Site::new(from.clone(), path.clone()),
            to: Site::new(to.clone(), path),
            edited: false,
        });
    }
}
