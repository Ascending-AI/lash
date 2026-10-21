//! RELINK-CLOSURE (K-LIB-009): a carried saved function relinked by name
//! to the package set its next cell runs on.
//!
//! A saved function's document names each library function by identity.
//! At a cell start, each one takes the identity the cell's set defines
//! under its name, and what the function performs takes what that set
//! requires, so everything one cell runs comes from that one set. A name the set does not define, or defines or
//! offers under another signature, refuses the function, naming it and the
//! callee ([`Unusable::Relink`]).

use std::collections::BTreeMap;

use lash_kernel_doc::FunctionCatalog;
use lash_kernel_doc::performed_through;
use lash_kernel_doc::snapshot::CodeReads;
use lash_vocabulary::legacy_names::EffectName;
use lash_vocabulary::legacy_names::FunctionId;
use lash_vocabulary::legacy_names::FunctionName;
use lash_vocabulary::operations::OperationRequirement;

use super::{SavedFunction, Unusable};
use crate::Export;
use crate::ExportView;
use crate::Library;
use lash_vocabulary::names::QualifiedOperation;

/// What the cell's package set lacks of a saved function's callee.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum RelinkGap {
    /// The set defines no function, or offers no operation, under its
    /// name.
    #[error("the cell's package set does not define")]
    Removed,
    /// The set defines or offers it under another signature.
    #[error("the cell's package set declares under another signature")]
    Signature,
    /// The set defines another function under its name: the saved
    /// function was not relinked to this set.
    #[error("the cell's package set defines as another function")]
    Unlinked,
}

/// `function` relinked to the package set whose names `library` resolves
/// and whose exports `exports` shows: each library function its code
/// holds takes the set's identity of its name, its document lists that
/// set's closure of them, and each operation a package of the set owns
/// takes the set's requirement. A changed contract under an unchanged
/// signature is taken. A callee whose identity changes keeps the signature
/// the function was saved or last relinked against
/// ([`SavedFunction::against`]) or refuses it, whether or not the node
/// still holds the old callee. The record's closure is the caller's to
/// derive.
///
/// # Errors
///
/// [`Unusable::Relink`] naming the function and the first callee the set
/// does not serve; [`Unusable::EffectRequirement`] when the relinked code
/// requires one operation two ways. A function saved under a generated
/// name is named by the path a binding held it at
/// ([`SavedFunction::label`]).
pub fn relink_saved(
    function: &SavedFunction,
    library: &dyn Library,
    exports: &dyn ExportView,
) -> Result<SavedFunction, Unusable> {
    let catalog: &dyn FunctionCatalog = library;
    let refuse = |callee: &str, gap: RelinkGap| Unusable::Relink {
        function: function.label(),
        callee: callee.to_owned(),
        gap,
    };
    let listed = &function.document.manifest.functions;
    let mut resolved: BTreeMap<FunctionId, FunctionId> = BTreeMap::new();
    let mut relinked = function.clone();
    let mut against = BTreeMap::new();
    relinked.document.try_each_function_mut(|_, identity| {
        if let Some(replacement) = resolved.get(identity) {
            *identity = *replacement;
            return Ok(());
        }
        let name = listed
            .get(identity)
            .ok_or_else(|| refuse(&identity.to_string(), RelinkGap::Removed))?;
        let replacement = library
            .resolve(name.as_str())
            .ok_or_else(|| refuse(name.as_str(), RelinkGap::Removed))?;
        // A signature that cannot be read is no signature that serves.
        let now = catalog
            .header(&replacement)
            .map(|header| header.signature.clone())
            .ok_or_else(|| refuse(name.as_str(), RelinkGap::Signature))?;
        if function.against.get(identity) != Some(&now) {
            return Err(refuse(name.as_str(), RelinkGap::Signature));
        }
        against.insert(replacement, now);
        resolved.insert(*identity, replacement);
        *identity = replacement;
        Ok(())
    })?;
    relinked.against = against;
    let direct: Vec<FunctionId> = resolved.values().copied().collect();
    relinked.document.manifest.functions = reachable(&direct, catalog);
    // What its own code performs: an operation a package of the set owns
    // takes the set's requirement; any other keeps the one it was saved
    // under, which the cell's offer is checked against at install.
    let own = function
        .definition()
        .map(|code| CodeReads::of(&code.params, &code.body).effects)
        .unwrap_or_default();
    let mut effects: BTreeMap<EffectName, OperationRequirement> = BTreeMap::new();
    for effect in &own {
        let saved = function
            .document
            .manifest
            .effects
            .get(effect)
            .ok_or_else(|| refuse(effect.as_str(), RelinkGap::Removed))?;
        let requirement = match owned(exports, effect) {
            Owned::Not => saved.clone(),
            Owned::Missing => return Err(refuse(effect.as_str(), RelinkGap::Removed)),
            Owned::By(requirement) if requirement.signature != saved.signature => {
                return Err(refuse(effect.as_str(), RelinkGap::Signature));
            }
            Owned::By(requirement) => requirement.clone(),
        };
        effects.insert(effect.clone(), requirement);
    }
    // What the bodies it reaches perform, as the set's bodies require it
    // (`K-DOC-002`).
    for (effect, bodies) in performed_through(catalog, direct.iter().copied()) {
        for requirement in bodies.into_values() {
            let held = effects
                .entry(effect.clone())
                .or_insert_with(|| requirement.clone());
            if held != requirement {
                return Err(Unusable::EffectRequirement {
                    function: function.label(),
                    effect,
                    held: Box::new(held.clone()),
                    saved: Box::new(requirement.clone()),
                });
            }
        }
    }
    relinked.document.manifest.effects = effects;
    Ok(relinked)
}

/// Whether a package of the view owns `effect`'s path.
enum Owned<'a> {
    /// No package owns the path: the operation is a host's tool.
    Not,
    /// A package owns the path and exports no such operation.
    Missing,
    By(&'a OperationRequirement),
}

fn owned<'a>(exports: &'a dyn ExportView, effect: &EffectName) -> Owned<'a> {
    let Ok(operation) = QualifiedOperation::from_effect(effect) else {
        return Owned::Not;
    };
    let Some(package) = exports.exports(&operation.package) else {
        return Owned::Not;
    };
    match package.get(&operation.name) {
        Some(Export::Operation(export)) => Owned::By(&export.requirement),
        _ => Owned::Missing,
    }
}

/// The functions `roots` hold and every function their bodies reach,
/// each by the name its definition carries (`K-DOC-002`).
pub(super) fn reachable(
    roots: &[FunctionId],
    catalog: &dyn FunctionCatalog,
) -> BTreeMap<FunctionId, FunctionName> {
    let mut listed = BTreeMap::new();
    let mut pending: Vec<FunctionId> = roots.to_vec();
    while let Some(function) = pending.pop() {
        if listed.contains_key(&function) {
            continue;
        }
        let Some(header) = catalog.header(&function) else {
            continue;
        };
        listed.insert(function, header.name.clone());
        match catalog.registered(&function) {
            Some(registered) => pending.extend(registered.callees()),
            None => pending.extend(
                catalog
                    .definition(&function)
                    .and_then(lash_kernel_doc::FunctionDefinition::body)
                    .map(|body| body.functions.keys().copied().collect::<Vec<_>>())
                    .unwrap_or_default(),
            ),
        }
    }
    listed
}
