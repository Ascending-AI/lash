//! The conformance case a kernel version's migration ships with
//! (`K-VER-005`).
//!
//! A scripted case is run under the migration's source version. Then, for
//! each park of that run in turn, it is run again: parked there, its
//! document rewritten and admitted, its state carried across, and resumed
//! under the target version to its end. What it prints, how it ends and
//! every request it issues, with its identity, must equal the run that
//! never left. Charges are not compared: a version may price an operation
//! differently.

use std::sync::Arc;

use lash_kernel_doc::FunctionRegistry;
use lash_kernel_migrate::Migration;
use lash_kernel_state::ParkedRun;
use lash_kernel_vm::{Machine, Program};

use crate::machine::{Park, run_case};
use crate::{Case, HarnessError, Observations, Trace};

/// What [`check_migration`] found.
#[derive(Clone, Debug, PartialEq)]
pub struct MigrationCheck {
    /// The run under the source version, never migrated.
    pub stayed: Observations,
    /// The run migrated at each park of `stayed`, in park order.
    pub migrated: Vec<Observations>,
}

/// Runs `case` under `migration.from`, then once per park of that run,
/// migrating there, and requires every migrated run to observe what the
/// unmigrated one did.
///
/// `registry` holds the library for both versions. A case that never parks
/// proves nothing and is refused, as is a refusal by the migration.
pub fn check_migration<M: Machine<Parked = ParkedRun>>(
    migration: &Migration,
    registry: &Arc<FunctionRegistry>,
    case: &Case,
) -> Result<MigrationCheck, HarnessError> {
    let stayed = run_case::<M>(registry, case, &mut |at| Ok(at.machine))?;
    case.expected
        .check(&stayed)
        .map_err(|error| HarnessError(format!("{}: {error}", case.name)))?;
    if stayed.parks == 0 {
        return Err(HarnessError(format!("{}: the run never parks", case.name)));
    }
    let mut migrated = Vec::new();
    for park in 1..=stayed.parks {
        let mut expected = stayed.trace.clone();
        let observed = run_case::<M>(registry, case, &mut |at| {
            if at.number != park {
                return Ok(at.machine);
            }
            migrate(migration, registry, case, at, &mut expected)
        })?;
        let same = observed.prints == stayed.prints
            && observed.end == stayed.end
            && observed.trace == expected
            && observed.parks == stayed.parks;
        if !same {
            return Err(HarnessError(format!(
                "{}: migrated at park {park}, the run observed {observed:?}; the run that \
                 stayed observed {stayed:?}",
                case.name
            )));
        }
        migrated.push(observed);
    }
    Ok(MigrationCheck { stayed, migrated })
}

/// Carries the run at `at` across `migration` and answers the machine that
/// resumes it. `expected` is the unmigrated run's trace: every request
/// issued after this park is identified in the rewritten document.
fn migrate<M: Machine<Parked = ParkedRun>>(
    migration: &Migration,
    registry: &Arc<FunctionRegistry>,
    case: &Case,
    at: Park<'_, M>,
    expected: &mut [Trace],
) -> Result<M, HarnessError> {
    let refused = |error: &dyn std::fmt::Display| HarnessError(format!("{}: {error}", case.name));
    let mut machine = at.machine;
    let parked = machine.export().map_err(|error| refused(&error))?;
    let base = Arc::clone(&at.program.document);
    let rewritten =
        (migration.document)(&base, registry.as_ref()).map_err(|error| refused(&error))?;
    let mut admission = lash_kernel_check::Environment::new(registry.as_ref());
    admission.effects = case
        .environment
        .effects
        .clone()
        .unwrap_or_else(|| rewritten.document.manifest.effects.clone());
    lash_kernel_check::admit(&rewritten.document, &admission).map_err(|error| {
        HarnessError(format!(
            "{}: the rewritten document is not admitted: {error:?}",
            case.name
        ))
    })?;
    let carried =
        (migration.parked)(&parked, &base, &rewritten).map_err(|error| refused(&error))?;
    for request in expected.iter_mut().skip(at.issued) {
        let (Trace::Effect { identity, .. } | Trace::Sleep { identity, .. }) = request;
        if let Some(identity) = identity {
            *identity = rewritten
                .effect_identity(&base, identity)
                .map_err(|error| refused(&error))?;
        }
    }
    *at.program = Program {
        document: Arc::new(rewritten.document),
        registry: Arc::clone(registry),
    };
    M::import(at.program.clone(), at.bounds, carried).map_err(|error| refused(&error))
}
