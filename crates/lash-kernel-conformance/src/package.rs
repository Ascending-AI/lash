//! Package conformance: run the same authored witness with installed code
//! and with every `Both` native and generated plan disabled. Compare values,
//! errors, effects, parks and the ordered charge events, not just totals.

mod meter;
pub use meter::{LoopBounds, audit_metering};

use std::collections::BTreeSet;
use std::sync::Arc;

use lash_kernel_doc::{FunctionId, FunctionRegistry};
use lash_kernel_exec::charge::Weight;
use lash_kernel_package::PackageView;
use lash_kernel_vm::kernel_internals::{Layout, prepare_with_layout};
use lash_kernel_vm::{ChargeTape, KernelMachine, PreparedLibrary};

use crate::machine::{Run, run_case_in};
use crate::{Case, ExpectedEnd, HarnessError, Observations};

/// Which promise a witness pins. Shape witnesses must actually reach a
/// shape refusal in the reference body, so two successful runs cannot pass.
#[derive(Clone, Copy, Debug)]
pub enum Law {
    Native,
    Shape,
    Meter,
}

impl Law {
    fn name(self) -> &'static str {
        match self {
            Self::Native => "CONF-NATIVE",
            Self::Shape => "CONF-SHAPE",
            Self::Meter => "CONF-METER",
        }
    }
}

/// One call of a package definition, with its scripted host responses.
/// The case must name `function` in its manifest. Its expected observations
/// are for an independent `check_case` oracle; this comparison uses neither
/// side's expectations as the other side's observations.
pub struct Witness {
    pub function: FunctionId,
    pub law: Law,
    pub case: Case,
}

/// Both execution paths of a package set, prepared once and shared by its
/// witnesses. The view fixes the set whose members the kit is checking.
pub struct Kit<'a> {
    view: PackageView<'a>,
    installed: PreparedLibrary,
    bodies: PreparedLibrary,
}

impl<'a> Kit<'a> {
    pub fn new(view: PackageView<'a>, registry: Arc<FunctionRegistry>) -> Self {
        Self {
            view,
            installed: prepare_with_layout(Arc::clone(&registry), Layout(0)),
            bodies: prepare_with_layout(registry, Layout(3)),
        }
    }

    /// CONF-NATIVE coverage comes from the view's code, not an independently
    /// maintained function list. Native-only definitions have no body oracle.
    /// Missing optional natives are compared through their installed plans.
    pub fn check_all(&self, witnesses: &[Witness]) -> Result<usize, HarnessError> {
        let covered: BTreeSet<_> = witnesses
            .iter()
            .filter(|w| matches!(w.law, Law::Native))
            .map(|w| w.function)
            .collect();
        for package in self.view.packages() {
            for function in package.code().ids() {
                let Some(registered) = self.installed.registry().get(function) else {
                    return Err(HarnessError(format!(
                        "{}: definition @{function} is not installed",
                        package.path()
                    )));
                };
                if registered.has_body()
                    && registered.states_native()
                    && !covered.contains(function)
                {
                    return Err(HarnessError(format!(
                        "CONF-NATIVE {} {} lacks a witness",
                        package.path(),
                        registered.name()
                    )));
                }
            }
        }
        for witness in witnesses {
            self.check(witness)?;
        }
        Ok(witnesses.len())
    }

    /// Compare one witness, naming its package, function, law and the first
    /// differing observation on failure. Admission failures are not evidence.
    pub fn check(&self, witness: &Witness) -> Result<(), HarnessError> {
        let owner = self
            .view
            .packages()
            .find(|package| package.code().ids().contains(&witness.function))
            .ok_or_else(|| {
                HarnessError(format!(
                    "{}: witness names a function outside the package view",
                    witness.law.name()
                ))
            })?;
        let registered = self
            .installed
            .registry()
            .get(&witness.function)
            .ok_or_else(|| HarnessError("witness function is not installed".into()))?;
        if !registered.has_body() {
            return Err(HarnessError(format!(
                "{} {}: definition has no body oracle",
                witness.law.name(),
                registered.name()
            )));
        }
        if matches!(witness.law, Law::Native)
            && registered.native().is_none()
            && registered.plan().is_none()
        {
            return Err(HarnessError(format!(
                "CONF-NATIVE {} has neither a native nor a plan to compare",
                registered.name()
            )));
        }
        let label = format!(
            "{} {} {} / {}",
            witness.law.name(),
            owner.path(),
            registered.name(),
            witness.case.name
        );
        if matches!(witness.law, Law::Meter) && registered.plan().is_none() {
            return Err(HarnessError(format!(
                "{label}: no generated plan is installed"
            )));
        }
        let document = lash_kernel_doc::parse_document(&witness.case.document)
            .map_err(|error| HarnessError(format!("{label}: {error}")))?;
        if !document.manifest.functions.contains_key(&witness.function) {
            return Err(HarnessError(format!(
                "{label}: witness does not declare its function"
            )));
        }
        let (body, body_events) = observe(&self.bodies, &witness.case, Layout(3))?;
        if body.end == ExpectedEnd::Refused {
            return Err(HarnessError(format!(
                "{label}: body witness was refused before execution"
            )));
        }
        if matches!(witness.law, Law::Shape)
            && !matches!(&body.end, ExpectedEnd::Uncaught(lash_kernel_doc::Datum::Error(error)) if error.kind == "package.stale_value" || error.kind == "type_error")
        {
            return Err(HarnessError(format!(
                "{label}: body did not reach a shape refusal: {:?}",
                body.end
            )));
        }
        let (installed, installed_events) = observe(&self.installed, &witness.case, Layout(0))?;
        for (what, differs) in [
            ("value or error", installed.end != body.end),
            ("prints", installed.prints != body.prints),
            ("effects", installed.trace != body.trace),
            ("parks", installed.parks != body.parks),
            ("charge", installed.charged != body.charged),
            ("metered events", installed_events != body_events),
        ] {
            if differs {
                return Err(HarnessError(format!(
                    "{label}: {what} differs: installed={installed:?} body={body:?}; events installed={installed_events:?} body={body_events:?}"
                )));
            }
        }
        Ok(())
    }
}

fn observe(
    library: &PreparedLibrary,
    case: &Case,
    layout: Layout,
) -> Result<(Observations, Vec<Weight>), HarnessError> {
    let tape = ChargeTape::new();
    let mut start = |program, bounds, start, source: &crate::pages::Source| {
        let mut machine = lash_kernel_vm::kernel_internals::start_with_layout(
            program,
            bounds,
            start,
            layout,
            Box::new(source.clone()),
        )?;
        machine.record_charges(tape.clone());
        Ok(machine)
    };
    let observations = run_case_in::<KernelMachine>(
        library,
        case,
        Run {
            start: &mut start,
            printed: &mut || {},
            charge: None,
        },
        &mut |at| Ok(at.machine),
        None,
    )?;
    Ok((observations, Weight::expand(tape.steps())))
}

#[cfg(test)]
mod tests;
