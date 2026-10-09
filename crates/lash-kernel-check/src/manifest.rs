//! Manifest derivation (`K-DOC-002`, `K-ADM-007`): what a document's code
//! requires, read from the code, against what its manifest says.

use std::collections::{BTreeMap, BTreeSet};

use lash_kernel_doc::{
    Action, Block, Callee, Document, EffectName, Expr, FunctionCatalog, FunctionId, FunctionName,
    Node,
};

use crate::refusal::{RefusalReason, Refused, refused};

/// What a document's code requires of an environment, derived from the code
/// alone.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Requirements {
    /// Every effect some `perform` names.
    pub effects: BTreeSet<EffectName>,
    /// Every library function the code reaches, directly or through another
    /// function's body, with the name its definition carries. A function
    /// the catalog does not hold is listed under the name its caller gives
    /// it, and its own body is not followed.
    pub functions: BTreeMap<FunctionId, FunctionName>,
}

/// The library functions a block calls, and the effects it performs.
fn scan(block: &Block, effects: &mut BTreeSet<EffectName>, functions: &mut BTreeSet<FunctionId>) {
    let mut pending = vec![Node::Block(block)];
    while let Some(node) = pending.pop() {
        match node {
            Node::Expr(Expr::Call { function, .. }) => {
                functions.insert(*function);
            }
            Node::Action(Action::Perform { effect, .. }) => {
                effects.insert(effect.clone());
            }
            Node::Action(
                Action::Call {
                    callee: Callee::Library(function),
                    ..
                }
                | Action::Spawn {
                    callee: Callee::Library(function),
                    ..
                },
            ) => {
                functions.insert(*function);
            }
            _ => {}
        }
        pending.extend(node.children());
    }
}

/// A function reached, and the function whose body reached it; `None` when
/// the document's own code calls it.
type Reached = BTreeMap<FunctionId, Option<FunctionId>>;

fn reach(document: &Document, catalog: &dyn FunctionCatalog) -> (BTreeSet<EffectName>, Reached) {
    let mut effects = BTreeSet::new();
    let mut direct = BTreeSet::new();
    scan(&document.main, &mut effects, &mut direct);
    for function in document.functions.values() {
        scan(&function.body, &mut effects, &mut direct);
    }
    let mut reached: Reached = direct.iter().map(|function| (*function, None)).collect();
    let mut pending: Vec<FunctionId> = direct.into_iter().collect();
    while let Some(function) = pending.pop() {
        let Some(body) = catalog
            .definition(&function)
            .and_then(|definition| definition.body())
        else {
            continue;
        };
        let mut called = BTreeSet::new();
        scan(&body.block, &mut BTreeSet::new(), &mut called);
        for callee in called {
            if let std::collections::btree_map::Entry::Vacant(entry) = reached.entry(callee) {
                entry.insert(Some(function));
                pending.push(callee);
            }
        }
    }
    (effects, reached)
}

/// The name `function` goes by: its definition's, or the one the body that
/// reached it lists, or the manifest's.
fn name_of(
    function: &FunctionId,
    through: Option<&FunctionId>,
    document: &Document,
    catalog: &dyn FunctionCatalog,
) -> Option<FunctionName> {
    if let Some(definition) = catalog.definition(function) {
        return Some(definition.name.clone());
    }
    through
        .and_then(|through| catalog.definition(through))
        .and_then(|definition| definition.body())
        .and_then(|body| body.functions.get(function))
        .or_else(|| document.manifest.functions.get(function))
        .cloned()
}

/// Derives a document's requirements from its code and the bodies of the
/// library functions it reaches.
pub fn requirements(document: &Document, catalog: &dyn FunctionCatalog) -> Requirements {
    let (effects, reached) = reach(document, catalog);
    let functions = reached
        .iter()
        .filter_map(|(function, through)| {
            name_of(function, through.as_ref(), document, catalog).map(|name| (*function, name))
        })
        .collect();
    Requirements { effects, functions }
}

/// Every function the document needs that `catalog` does not hold: those
/// its manifest lists and those its code reaches.
pub(crate) fn missing_functions(
    document: &Document,
    catalog: &dyn FunctionCatalog,
) -> Vec<Refused> {
    let derived = requirements(document, catalog);
    let named: BTreeMap<&FunctionId, &FunctionName> = derived
        .functions
        .iter()
        .chain(&document.manifest.functions)
        .collect();
    named
        .into_iter()
        .filter(|(function, _)| catalog.definition(function).is_none())
        .map(|(function, name)| {
            refused(
                None,
                RefusalReason::MissingFunction {
                    function: *function,
                    name: name.clone(),
                },
            )
        })
        .collect()
}

/// Checks the manifest the document carries against the one its code
/// derives. A function the document's own code calls without listing is
/// structural validation's to refuse; this names the rest.
pub(crate) fn check(document: &Document, catalog: &dyn FunctionCatalog, errors: &mut Vec<Refused>) {
    let (effects, reached) = reach(document, catalog);
    for effect in document.manifest.effects.keys() {
        if !effects.contains(effect) {
            errors.push(refused(
                None,
                RefusalReason::EffectNotPerformed {
                    effect: effect.clone(),
                },
            ));
        }
    }
    for (function, listed) in &document.manifest.functions {
        if !reached.contains_key(function) {
            errors.push(refused(
                None,
                RefusalReason::FunctionNotReached {
                    function: *function,
                    name: listed.clone(),
                },
            ));
        }
        if let Some(definition) = catalog.definition(function)
            && &definition.name != listed
        {
            errors.push(refused(
                None,
                RefusalReason::FunctionName {
                    function: *function,
                    listed: listed.clone(),
                    defined: definition.name.clone(),
                },
            ));
        }
    }
    for (function, through) in &reached {
        let Some(through) = through else {
            continue;
        };
        if document.manifest.functions.contains_key(function) {
            continue;
        }
        if let Some(name) = name_of(function, Some(through), document, catalog) {
            errors.push(refused(
                None,
                RefusalReason::FunctionNotListed {
                    function: *function,
                    name,
                    through: *through,
                },
            ));
        }
    }
}
