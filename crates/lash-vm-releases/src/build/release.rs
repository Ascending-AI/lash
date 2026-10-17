//! Defines the current build's helper release in dependency order.
use crate::releases::{HelperRelease, ReleasedFunction};
use lash_kernel_doc::{FunctionDefinition, FunctionId, FunctionRegistry};
use std::collections::BTreeSet;

pub(crate) fn current(registry: &FunctionRegistry) -> HelperRelease {
    let mut functions = Vec::new();
    let mut listed = BTreeSet::new();
    for (function, _) in registry.iter() {
        list(*function, registry, &mut listed, &mut functions);
    }
    HelperRelease {
        release: "1.0".to_owned(),
        ordinal: 1,
        writes: listed,
        functions,
    }
}

/// Lists each function after its callees.
fn list(
    function: FunctionId,
    registry: &FunctionRegistry,
    listed: &mut BTreeSet<FunctionId>,
    functions: &mut Vec<ReleasedFunction>,
) {
    if listed.contains(&function) {
        return;
    }
    let Some(registered) = registry.get(&function) else {
        return;
    };
    if let Some(body) = registered.definition().body() {
        for called in body.functions.keys() {
            list(*called, registry, listed, functions);
        }
    }
    listed.insert(function);
    functions.push(ReleasedFunction {
        function,
        definition: FunctionDefinition::clone(registered.definition()),
    });
}
