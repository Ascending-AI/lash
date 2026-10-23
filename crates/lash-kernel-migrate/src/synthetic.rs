//! The migration from kernel version 1 to its synthetic successor.
//!
//! The successor spells one form differently in the stored encoding and
//! prices one operation differently; neither moves a node. So the document
//! rewrite redeclares the document and the functions it lists for the
//! successor, every node of `main` and of a declared function keeps its
//! site, and a parked run is carried coordinate for coordinate.
//!
//! The redeclaration preserves library bodies, so their coordinates carry
//! with the document's coordinates through the same correspondence.

use std::collections::BTreeMap;

use lash_kernel_doc::{Document, FunctionCatalog, FunctionDefinition, KernelVersion};

use crate::migration::{DocumentRefusal, Migration, Rewritten};
use crate::rewrite::{redeclare, replace_functions};

const FROM: KernelVersion = KernelVersion::One;
const TO: KernelVersion = KernelVersion::SyntheticNext;

pub(crate) const MIGRATION: Migration = Migration {
    from: FROM,
    to: TO,
    definition,
    document,
};

fn definition(written: &FunctionDefinition) -> Result<FunctionDefinition, DocumentRefusal> {
    if written.kernel != FROM.number() {
        return Err(DocumentRefusal::Version {
            found: written.kernel,
            from: FROM.number(),
        });
    }
    Ok(FunctionDefinition {
        kernel: TO.number(),
        ..written.clone()
    })
}

fn document(
    written: &Document,
    functions: &dyn FunctionCatalog,
) -> Result<Rewritten, DocumentRefusal> {
    if written.kernel() != FROM.number() {
        return Err(DocumentRefusal::Version {
            found: written.kernel(),
            from: FROM.number(),
        });
    }
    let redeclared = redeclare(
        definition,
        written
            .functions
            .keys()
            .copied()
            .chain(written.library.iter().copied()),
        functions,
    )?;
    let functions: BTreeMap<_, _> = redeclared.iter().map(|f| (f.from, f.to)).collect();
    let mut next = written.clone();
    next.requires.kernels = [TO.number()].into();
    replace_functions(&mut next, &functions);
    next.functions = redeclared
        .into_iter()
        .filter(|function| written.functions.contains_key(&function.from))
        .map(|function| (function.to, function.definition))
        .collect();
    next.library.0 = written
        .library
        .iter()
        .map(|id| functions.get(id).copied().unwrap_or(*id))
        .collect();
    next.requires.natives = written
        .requires
        .natives
        .iter()
        .map(|id| functions.get(id).copied().unwrap_or(*id))
        .collect();
    let mut survivors = Vec::new();
    for (unit, body) in written.units() {
        let to = match &unit {
            lash_vocabulary::sites::Unit::Library(id) => {
                lash_vocabulary::sites::Unit::Library(functions.get(id).copied().unwrap_or(*id))
            }
            _ => unit.clone(),
        };
        crate::rewrite::same_sites(unit, to, body, &mut survivors);
    }
    let correspondence =
        lash_kernel_doc::Correspondence::of(written.identity(), next.identity(), survivors)
            .ok_or(DocumentRefusal::Correspondence)?;
    Ok(Rewritten {
        document: next,
        correspondence,
        functions,
    })
}
