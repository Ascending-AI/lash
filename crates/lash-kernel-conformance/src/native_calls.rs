//! Direct native probes for libraries whose machine is not yet available.
//! These supplement the kernel-text corpus; they do not replace its cases.

use std::collections::BTreeSet;

use lash_kernel_doc::{Datum, ErrorDatum, FunctionId, FunctionRegistry};
use serde::{Deserialize, Serialize};

use crate::HarnessError;

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NativeShard {
    pub rule: String,
    pub cases: Vec<NativeCase>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NativeCase {
    pub name: String,
    pub function: FunctionId,
    pub args: Vec<Datum>,
    pub expected: NativeObservation,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NativeObservation {
    pub outcome: NativeOutcome,
    pub charged: u64,
    /// WorkCounter::spent(), including on guard failure.
    pub work: u64,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum NativeOutcome {
    Returned(Datum),
    Raised(ErrorDatum),
    Guard { limit: u64 },
}

/// Probes real registered natives with the library's own heap, decoder and
/// formula evaluator. No second implementation of those semantics lives here.
/// The probe must invoke `registry.get(case.function).native.call(...)`, copy
/// its result to a Datum, and return the formula charge and counter spent.
/// The factory creates fresh implementations and caches for each cold call.
pub fn check_native_calls(
    mut registry_factory: impl FnMut() -> Result<FunctionRegistry, HarnessError>,
    mut probe: impl FnMut(&FunctionRegistry, &NativeCase) -> Result<NativeObservation, HarnessError>,
    cases: &[NativeCase],
) -> Result<usize, HarnessError> {
    let warm = registry_factory()?;
    let ids: Vec<_> = warm.iter().map(|(id, _)| *id).collect();
    let natives: BTreeSet<_> = warm
        .iter()
        .filter(|(_, f)| f.native.is_some())
        .map(|(id, _)| *id)
        .collect();
    let mut covered = BTreeSet::new();
    let mut guarded = BTreeSet::new();
    let mut names = BTreeSet::new();
    for case in cases {
        if case.name.trim().is_empty()
            || !names.insert((&case.function, &case.name))
            || !natives.contains(&case.function)
        {
            return Err(HarnessError(format!("invalid native case {}", case.name)));
        }
        covered.insert(case.function);
        if matches!(case.expected.outcome, NativeOutcome::Guard { .. }) {
            guarded.insert(case.function);
        }
    }
    for (id, function) in warm.iter().filter(|(id, _)| natives.contains(id)) {
        if !covered.contains(id) || (function.definition.guard.is_some() && !guarded.contains(id)) {
            return Err(HarnessError(format!(
                "native {} lacks result or guard cases",
                function.definition.name
            )));
        }
    }
    for case in cases {
        for _ in 0..2 {
            let cold = registry_factory()?;
            if ids != cold.iter().map(|(id, _)| *id).collect::<Vec<_>>() {
                return Err(HarnessError(
                    "registry factory changed function identities".into(),
                ));
            }
            compare(case, probe(&cold, case)?)?;
        }
    }
    for _ in 0..2 {
        for case in cases {
            compare(case, probe(&warm, case)?)?;
        }
    }
    Ok(cases.len() * 4)
}

fn compare(case: &NativeCase, actual: NativeObservation) -> Result<(), HarnessError> {
    if actual != case.expected {
        return Err(HarnessError(format!(
            "{}: expected {:?}, observed {actual:?}",
            case.name, case.expected
        )));
    }
    Ok(())
}
