//! A document adopted onto other library functions of its own kernel
//! version (FIG-5799).
//!
//! A build that stops retaining a helper release holds a run written
//! against that release only once the run is adopted: its document's
//! library functions replaced by the build's own, and the run carried onto
//! the result. A run stands in the document's own code, which a replacement
//! does not move, and in the bodies of the library functions it called. The
//! body of a function that is kept stays where it was. The body of a
//! replaced function is carried only when the replacement is that body with
//! replaced callees alone, as a helper redeclared over a changed callee is,
//! or when a correspondence declared for the pair says where each of its
//! nodes went; a run that stands inside any other replaced body is refused
//! at the site it stands on.

use std::collections::{BTreeMap, BTreeSet};

use lash_kernel_doc::{Document, FunctionBody, FunctionCatalog, FunctionId, Unit};
use lash_kernel_edit::{Correspondence, Survivor};

use crate::migration::{DocumentRefusal, Rewritten};
use crate::rewrite::{document_identity, replace_functions, replace_in_block, same_sites, unmoved};

/// `base` with each library function `replacements` names replaced, and
/// the correspondence a run of it is carried over: every node of its own
/// code, every node of a kept library body, and every node of a replaced
/// body the replacement keeps in place or `declared` places. `declared`
/// maps a replaced function to where each node of its body is in its
/// replacement's. `functions` holds every function `base` reaches and every
/// replacement.
///
/// # Errors
///
/// [`DocumentRefusal::FunctionNotHeld`] for a function `functions` lacks,
/// and [`DocumentRefusal::Correspondence`] when what is declared places two
/// nodes at one site.
pub fn adopt(
    base: &Document,
    replacements: &BTreeMap<FunctionId, FunctionId>,
    functions: &dyn FunctionCatalog,
    declared: &BTreeMap<FunctionId, Vec<Survivor>>,
) -> Result<Rewritten, DocumentRefusal> {
    let mut document = base.clone();
    replace_functions(&mut document, replacements);
    let mut entries = unmoved(base);
    let mut carried = BTreeMap::new();
    for function in reached(base, functions)? {
        let body = |function: &FunctionId| {
            functions
                .definition(function)
                .map(|definition| definition.body().cloned())
                .ok_or(DocumentRefusal::FunctionNotHeld {
                    function: *function,
                })
        };
        let Some(replacement) = replacements.get(&function).copied() else {
            carried.insert(function, function);
            if let Some(kept) = body(&function)? {
                let unit = Unit::Library(function);
                same_sites(unit.clone(), unit, &kept.block, &mut entries);
            }
            continue;
        };
        carried.insert(function, replacement);
        if let Some(survivors) = declared.get(&function) {
            entries.extend(survivors.iter().cloned());
            continue;
        }
        if let (Some(old), Some(new)) = (body(&function)?, body(&replacement)?)
            && replaced(&old, replacements) == new
        {
            same_sites(
                Unit::Library(function),
                Unit::Library(replacement),
                &old.block,
                &mut entries,
            );
        }
    }
    let correspondence = Correspondence::of(
        document_identity(base)?,
        document_identity(&document)?,
        entries,
    )
    .ok_or(DocumentRefusal::Correspondence)?;
    Ok(Rewritten {
        document,
        correspondence,
        functions: carried,
    })
}

/// Every library function `document` lists and every function their bodies
/// call.
fn reached(
    document: &Document,
    functions: &dyn FunctionCatalog,
) -> Result<BTreeSet<FunctionId>, DocumentRefusal> {
    let mut reached = BTreeSet::new();
    let mut pending: Vec<FunctionId> = document.manifest.functions.keys().copied().collect();
    while let Some(function) = pending.pop() {
        if !reached.insert(function) {
            continue;
        }
        let definition = functions
            .definition(&function)
            .ok_or(DocumentRefusal::FunctionNotHeld { function })?;
        if let Some(body) = definition.body() {
            pending.extend(body.functions.keys().copied());
        }
    }
    Ok(reached)
}

/// `body` with the callees `replacements` names replaced.
fn replaced(body: &FunctionBody, replacements: &BTreeMap<FunctionId, FunctionId>) -> FunctionBody {
    let mut body = body.clone();
    replace_in_block(&mut body.block, replacements);
    body.functions = std::mem::take(&mut body.functions)
        .into_iter()
        .map(|(id, name)| (replacements.get(&id).copied().unwrap_or(id), name))
        .collect();
    body
}
