//! Native determinism and cache independence (`K-LIB-006`, `K-LIB-008`).

use std::collections::BTreeSet;
use std::sync::Arc;

use lash_kernel_doc::{FunctionRegistry, parse_document};
use lash_kernel_vm::Machine;

use crate::{ExpectedEnd, HarnessError, MachineRunner, Shard, check_case};

/// The factory must create fresh native implementations, including their
/// caches. Cold runs each get a new registry; warm passes share one registry.
/// Every native has cases, charges are pinned, and every guard has a failing
/// case. Returns the number of executions, four per selected case.
pub fn check_native<M: Machine>(
    mut registry_factory: impl FnMut() -> Result<FunctionRegistry, HarnessError>,
    shards: &[Shard],
) -> Result<usize, HarnessError> {
    let registry = Arc::new(registry_factory()?);
    let natives: BTreeSet<_> = registry
        .iter()
        .filter(|(_, function)| function.native.is_some())
        .map(|(id, _)| *id)
        .collect();
    let mut covered = BTreeSet::new();
    let mut guarded = BTreeSet::new();
    let mut cases = Vec::new();
    for case in shards.iter().flat_map(|shard| &shard.cases) {
        let Ok(document) = parse_document(&case.document) else {
            continue;
        };
        let used: BTreeSet<_> = document
            .manifest
            .functions
            .keys()
            .copied()
            .filter(|id| natives.contains(id))
            .collect();
        if used.is_empty() {
            continue;
        }
        if case.expected.charged.is_none() {
            return Err(HarnessError(format!(
                "native case {} must pin its charge",
                case.name
            )));
        }
        covered.extend(used);
        if let ExpectedEnd::Bound { name, .. } = &case.expected.end {
            for id in &natives {
                if name == &format!("guard:{id}") {
                    guarded.insert(*id);
                }
            }
        }
        cases.push(case);
    }
    for (id, function) in registry.iter().filter(|(id, _)| natives.contains(id)) {
        if !covered.contains(id) || (function.definition.guard.is_some() && !guarded.contains(id)) {
            return Err(HarnessError(format!(
                "native {} lacks result or guard cases",
                function.definition.name
            )));
        }
    }
    let identities: Vec<_> = registry.iter().map(|(id, _)| *id).collect();
    let mut baseline = Vec::new();
    for case in &cases {
        let mut first = None;
        for _ in 0..2 {
            let cold = registry_factory()?;
            if identities != cold.iter().map(|(id, _)| *id).collect::<Vec<_>>() {
                return Err(HarnessError(
                    "registry factory changed function identities".into(),
                ));
            }
            let actual = check_case(&mut MachineRunner::<M>::new(Arc::new(cold)), case)?;
            if first.as_ref().is_some_and(|first| first != &actual) {
                return Err(HarnessError(format!(
                    "{} changed between cold runs",
                    case.name
                )));
            }
            first = Some(actual);
        }
        baseline.push(first.ok_or_else(|| HarnessError("no cold observation".into()))?);
    }
    let mut warm = MachineRunner::<M>::new(registry);
    for _ in 0..2 {
        for (case, expected) in cases.iter().zip(&baseline) {
            let actual = check_case(&mut warm, case)?;
            if actual != *expected {
                return Err(HarnessError(format!(
                    "{} changed with warm caches",
                    case.name
                )));
            }
        }
    }
    Ok(cases.len() * 4)
}
