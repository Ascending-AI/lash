//! The migration from kernel version 1 to its synthetic successor.
//!
//! The successor spells one form differently in the stored encoding and
//! prices one operation differently; neither moves a node. So the document
//! rewrite redeclares the document and the functions it lists for the
//! successor, every node of `main` and of a declared function keeps its
//! site, and a parked run is carried coordinate for coordinate.
//!
//! What it declares it cannot carry: a run that stands in a library
//! function's kernel body, or holds a coordinate there (a loop under way, a
//! variable, a closure, the spawn of a live task, an action it has run). A
//! redeclared body is its package's to write, and this migration vouches for
//! no correspondence into one. Such a run is refused, naming the site.

use std::collections::BTreeMap;

use lash_kernel_doc::{Document, FunctionCatalog, FunctionDefinition, KernelVersion};
use lash_kernel_state::ParkedRun;

use crate::carry::carry;
use crate::migration::{DocumentRefusal, Migration, ParkedRefusal, Rewritten};
use crate::rewrite::{redeclare, replace_functions, unchanged};

const FROM: KernelVersion = KernelVersion::One;
const TO: KernelVersion = KernelVersion::SyntheticNext;

pub(crate) const MIGRATION: Migration = Migration {
    from: FROM,
    to: TO,
    definition,
    document,
    parked,
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
    let correspondence = unchanged(written, &next)?;
    Ok(Rewritten {
        document: next,
        correspondence,
        functions,
    })
}

fn parked(
    parked: &ParkedRun,
    base: &Document,
    rewritten: &Rewritten,
) -> Result<ParkedRun, ParkedRefusal> {
    if parked.run.kernel != FROM.number() {
        return Err(ParkedRefusal::Version {
            found: parked.run.kernel,
            from: FROM.number(),
        });
    }
    carry(parked, base, rewritten, TO)
}
