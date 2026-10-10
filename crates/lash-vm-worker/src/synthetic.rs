//! The synthetic successor's helper change (FIG-5799).
//!
//! The synthetic successor moves the kernel version (FIG-5716) and, as a
//! release that fixes a helper does, ships one helper changed: `CHANGED`
//! gets a statement after its last `return`, which no call reaches. Its
//! identity changes, and so does the identity of every helper that calls
//! it, while every call answers and is charged as before. The successor
//! writes the changed helpers and retains release 1.0's, so the two-build
//! laws see a run of the build before it resume on exactly the functions it
//! pinned, a call parked inside the changed helper included.

use lash_kernel_doc::{Expr, FunctionDefinition, FunctionId, Implementation, Literal, Stmt};
use lash_kernel_migrate::DocumentRefusal;

use crate::embedding::{EmbedError, Embedder};

/// The helper the synthetic successor changes: one that calls a function
/// its caller passes, so a run can park inside it.
pub const CHANGED: &str = "ts.array.forEach";

/// Registers, in `embedder`, `CHANGED` changed and every function that
/// calls it redeclared over the change, under their names; the functions
/// they replace stay registered under none.
///
/// # Errors
///
/// [`EmbedError::Registry`]: a defect of the build.
pub(crate) fn change_helpers(embedder: &mut Embedder) -> Result<(), EmbedError> {
    let named: Vec<FunctionId> = embedder
        .registry()
        .iter()
        .map(|(function, _)| *function)
        .collect::<Vec<_>>()
        .into_iter()
        .filter(|function| !embedder.retained.contains(function))
        .collect();
    let redeclared = lash_kernel_migrate::redeclare(change, named, &*embedder.registry())
        .map_err(|error| EmbedError::Registry(error.to_string()))?;
    for function in redeclared {
        if function.from == function.to {
            continue;
        }
        embedder
            .registry()
            .register(function.definition, None)
            .map_err(|error| EmbedError::Registry(error.to_string()))?;
        embedder.retained.insert(function.from);
    }
    Ok(())
}

/// `definition`, with a `return absent` after its body's last statement
/// when it is `CHANGED`.
fn change(definition: &FunctionDefinition) -> Result<FunctionDefinition, DocumentRefusal> {
    let mut changed = definition.clone();
    if changed.name.to_string() == CHANGED
        && let Implementation::Body(body) = &mut changed.implementation
    {
        body.block.push(Stmt::Return {
            value: Expr::Literal(Literal::Absent),
        });
    }
    Ok(changed)
}
