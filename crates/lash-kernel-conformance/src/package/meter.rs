//! A loop-bound audit is keyed by the definition's content identity. There
//! is no sound inference of arbitrary program termination from a signature:
//! an author supplies the size-bound argument for each unmetered body. A
//! changed body invalidates that evidence; a new loop cannot inherit it by
//! name. Metered bodies need no such evidence.

use std::collections::{BTreeMap, BTreeSet};

use lash_kernel_doc::{Expr, FunctionId, Node, Stmt};
use lash_kernel_package::PackageView;

use crate::HarnessError;

/// Reviewed size bounds for a body's unmetered loops, keyed by the entire
/// definition, including its signature, formula and body. The explanation
/// names the arguments/result (or fixed algorithm bound) covering all loops.
pub type LoopBounds = BTreeMap<FunctionId, String>;

/// CONF-METER: every loop either declares its continuation charge or has
/// an explicit, content-pinned size-bound argument. Closure bodies are
/// ordinary charged code, outside the library's synchronous extent.
/// Returns the number of audited loops. Unused evidence is refused too.
pub fn audit_metering(view: &PackageView<'_>, bounds: &LoopBounds) -> Result<usize, HarnessError> {
    let mut used = BTreeSet::new();
    let mut loops = 0;
    let mut missing = Vec::new();
    for package in view.packages() {
        for function in package.ids() {
            let definition = view
                .catalog()
                .get(&function)
                .ok_or_else(|| HarnessError(format!("@{function} is not in the catalog")))?
                .definition();
            let Some(body) = definition.body() else {
                continue;
            };
            let mut pending = vec![(vec![], Node::Block(&body.block))];
            while let Some((path, node)) = pending.pop() {
                if matches!(node, Node::Expr(Expr::Closure(_))) {
                    continue;
                }
                if let Node::Stmt(Stmt::For { metered, .. } | Stmt::While { metered, .. }) = node {
                    loops += 1;
                    if *metered == 0 {
                        match bounds.get(&function) {
                            Some(reason) if !reason.trim().is_empty() => { used.insert(function); }
                            _ => missing.push(format!("CONF-METER {} {} @{function} at {path:?}: unmetered loop lacks reviewed argument/result-size bound evidence; declare metered N or supply its size-bound proof", package.path(), definition.name)),
                        }
                    }
                }
                pending.extend(
                    node.children()
                        .into_iter()
                        .enumerate()
                        .map(|(index, node)| {
                            let mut child = path.clone();
                            child.push(u32::try_from(index).unwrap_or_else(|_| {
                                unreachable!("a validated body has fewer than u32::MAX children")
                            }));
                            (child, node)
                        }),
                );
            }
        }
    }
    for function in bounds.keys().filter(|function| !used.contains(function)) {
        missing.push(format!("CONF-METER unused size-bound proof @{function}"));
    }
    if missing.is_empty() {
        Ok(loops)
    } else {
        Err(HarnessError(missing.join("\n")))
    }
}
