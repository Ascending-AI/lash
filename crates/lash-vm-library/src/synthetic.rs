//! The synthetic successor's helper change (FIG-5799).
//!
//! The synthetic successor moves the kernel version (FIG-5716) and, as a
//! release that fixes a helper does, ships one helper changed: `CHANGED`
//! gets a statement after its last `return`, which no call reaches. Its
//! identity changes, and so does the identity of every helper that calls
//! it, while every call answers and is charged as before. The successor
//! writes the changed helpers and retains release 1.0's, so the two-build
//! laws see a run of the build before it resume on exactly the functions it
//! pinned, a call parked inside the changed helper included. A worker and
//! its parent make the same change.

use lash_kernel_doc::{
    Expr, FunctionDefinition, FunctionId, FunctionRegistry, Implementation, Literal, Stmt,
};
use lash_kernel_migrate::{DocumentRefusal, Redeclared};

/// The helper the synthetic successor changes: one that calls a function
/// its caller passes, so a run can park inside it.
const CHANGED: &str = "ts.array.forEach";

/// `named`, the functions `registry` resolves names to, redeclared over the
/// change to `CHANGED`: the changed helper and every function that calls
/// it, each with the function it replaces.
pub(crate) fn redeclare(
    named: Vec<FunctionId>,
    registry: &FunctionRegistry,
) -> Result<Vec<Redeclared>, DocumentRefusal> {
    Ok(lash_kernel_migrate::redeclare(change, named, registry)?
        .into_iter()
        .filter(|function| function.from != function.to)
        .collect())
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
