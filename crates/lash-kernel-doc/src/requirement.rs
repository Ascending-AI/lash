//! What code requires of an operation it performs (`K-DOC-002`,
//! `K-LIB-004`): the signature it calls the operation under and the
//! contract that pins what the operation means.

use std::collections::{BTreeMap, BTreeSet};

use crate::ast::Node;
use crate::function::FunctionDefinition;
use crate::native::FunctionCatalog;
use lash_vocabulary::legacy_names::EffectName;
use lash_vocabulary::legacy_names::FunctionId;
use lash_vocabulary::operations::OperationRequirement;

/// For each operation, what each body among `functions` that performs it
/// requires, by that body's function (`K-DOC-002`). `functions` is a
/// closed set, as a manifest lists one: every function a body in it calls
/// is in it too. A function `catalog` does not hold performs nothing here.
pub fn performed_by(
    catalog: &dyn FunctionCatalog,
    functions: impl IntoIterator<Item = FunctionId>,
) -> BTreeMap<EffectName, BTreeMap<FunctionId, &OperationRequirement>> {
    let mut performed: BTreeMap<_, BTreeMap<_, _>> = BTreeMap::new();
    for function in functions {
        let effects = match catalog.registered(&function) {
            Some(registered) => Some(registered.performs()),
            None => catalog
                .definition(&function)
                .and_then(FunctionDefinition::body)
                .map(|body| &body.effects),
        };
        for (effect, requirement) in effects.into_iter().flatten() {
            performed
                .entry(effect.clone())
                .or_default()
                .insert(function, requirement);
        }
    }
    performed
}

/// What the bodies of `roots`, and of every function they reach, perform:
/// [`performed_by`] over the functions `roots` reach.
pub fn performed_through(
    catalog: &dyn FunctionCatalog,
    roots: impl IntoIterator<Item = FunctionId>,
) -> BTreeMap<EffectName, BTreeMap<FunctionId, &OperationRequirement>> {
    let mut reached = BTreeSet::new();
    let mut pending: Vec<FunctionId> = roots.into_iter().collect();
    while let Some(function) = pending.pop() {
        if !reached.insert(function) {
            continue;
        }
        match catalog.registered(&function) {
            Some(registered) => pending.extend(registered.callees()),
            None => pending.extend(
                catalog
                    .definition(&function)
                    .and_then(FunctionDefinition::body)
                    .map(|body| Node::Block(&body.block).functions_within())
                    .unwrap_or_default(),
            ),
        }
    }
    performed_by(catalog, reached)
}
