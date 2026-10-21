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
use crate::rewrite::{redeclare, replace_functions, unchanged};

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
    if written.manifest.kernel != FROM.number() {
        return Err(DocumentRefusal::Version {
            found: written.manifest.kernel,
            from: FROM.number(),
        });
    }
    let catalog = functions;
    let functions: BTreeMap<_, _> = redeclare(
        definition,
        written.manifest.functions.keys().copied(),
        functions,
    )?
    .into_iter()
    .map(|redeclared| (redeclared.from, redeclared.to))
    .collect();
    let mut next = written.clone();
    next.manifest.kernel = TO.number();
    replace_functions(&mut next, &functions);
    let correspondence =
        crate::rewrite::library_sites(unchanged(written, &next)?, &functions, catalog)?;
    Ok(Rewritten {
        document: next,
        correspondence,
        functions,
    })
}
