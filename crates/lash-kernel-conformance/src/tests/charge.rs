//! The charge oracle on the corpus (`K-CHG-001`, `K-CHG-002`, PLAN-INDEP;
//! `docs/design/kernel-end-state.md` §B): the per-unit reference charger
//! equals the machine, under every current layout, at every bound around
//! every event of every case's charge program.

use lash_kernel_exec::charge::{Ending, ReferenceCharger, Weight};
use lash_kernel_vm::{ChargeTape, KernelMachine, Layout, PreparedLibrary};

use crate::machine::{Park, Run, run_case_in};
use crate::{Case, ExpectedEnd, HarnessError, Observations};

/// The natural layout; an odd one, through every native's general call;
/// one whose record hints all miss; one where every native beside a body
/// declines, so the body runs.
const LAYOUTS: [Layout; 4] = [Layout(0), Layout(1), Layout(2), Layout(3)];

/// One run of a case: what it observed, the charges it made, and how many
/// charges preceded each print and each park (with that park's requests).
struct Probe {
    observations: Observations,
    tape: Vec<Weight>,
    prints: Vec<usize>,
    parks: Vec<(usize, usize)>,
}

fn probe(
    library: &PreparedLibrary,
    case: &Case,
    layout: Layout,
    charge: Option<u64>,
) -> Result<Probe, HarnessError> {
    let tape = ChargeTape::new();
    let (mut prints, mut parks) = (Vec::new(), Vec::new());
    let resume = case.environment.resume;
    let mut start = |program, bounds, start, source: &crate::pages::Source| {
        let mut machine = crate::pages::with_source(program, bounds, start, layout, source)?;
        machine.record_charges(tape.clone());
        Ok(machine)
    };
    let run = Run {
        start: &mut start,
        printed: &mut || prints.push(tape.len()),
        charge,
    };
    let observations = run_case_in::<KernelMachine>(
        library,
        case,
        run,
        &mut |at: Park<'_, KernelMachine>| {
            parks.push((tape.len(), at.requests.len()));
            if !resume {
                return Ok(at.machine);
            }
            let mut machine = at.machine;
            let sealed = lash_kernel_vm::Machine::seal(&mut machine, crate::pages::BUDGET)
                .map_err(|e| HarnessError(e.to_string()))?
                .ok_or_else(|| HarnessError("unbounded seal incomplete".into()))?;
            let mut opened = crate::pages::open_in(
                at.program.clone(),
                at.bounds,
                &sealed.root,
                at.source,
                at.admissions,
                layout,
            )
            .map_err(|e| HarnessError(e.to_string()))?;
            opened.record_charges(tape.clone());
            Ok(opened)
        },
        None,
    )?;
    Ok(Probe {
        observations,
        tape: tape.steps(),
        prints,
        parks,
    })
}

/// Each raw tape position as a position in the expanded program.
fn expanded_positions(tape: &[Weight]) -> Vec<usize> {
    let mut positions = Vec::with_capacity(tape.len() + 1);
    let mut at = 0;
    positions.push(at);
    for weight in tape {
        at += Weight::expand([*weight]).len();
        positions.push(at);
    }
    positions
}

/// The bounds that tell a program's events apart, up to `limit`: for each
/// event, the least bound it passes and the greatest, and the whole charge.
fn sweep(program: &[Weight], limit: u64) -> Vec<u64> {
    let mut bounds = Vec::new();
    let mut charged = 0_u64;
    for weight in program {
        let units = weight.units();
        bounds.push(charged);
        bounds.push(charged.saturating_add(units).saturating_sub(1));
        charged = charged.saturating_add(units);
    }
    bounds.push(charged);
    bounds.retain(|bound| *bound <= limit);
    bounds.sort_unstable();
    bounds.dedup();
    bounds
}

/// `K-CHG-002`, `K-CHG-001`, PLAN-INDEP: on every corpus case, each
/// current layout makes the same charge program, and a run bounded
/// anywhere around any of its events ends where the per-unit reference
/// charger ends it: charged with the event that passes, after the same
/// charges, prints, parks and requests, or, past its whole charge, exactly
/// as the unbounded run.
#[test]
fn every_corpus_charge_bound_ends_where_the_per_unit_reference_charger_ends() {
    let registry = std::sync::Arc::new(super::corpus_registry());
    let libraries: Vec<PreparedLibrary> = LAYOUTS
        .iter()
        .map(|layout| PreparedLibrary::with_layout(std::sync::Arc::clone(&registry), *layout))
        .collect();
    let (mut cases, mut bounded, mut crossed) = (0, 0, 0);
    let mut failures = Vec::new();
    for shard in super::corpus_shards() {
        for case in &shard.cases {
            let name = format!("{}: {}", shard.rule, case.name);
            let Ok(natural) = probe(&libraries[0], case, LAYOUTS[0], None) else {
                // A case the harness cannot run is the corpus tests' to report.
                continue;
            };
            if natural.observations.end == ExpectedEnd::Refused {
                continue;
            }
            cases += 1;
            let program = Weight::expand(natural.tape.iter().copied());
            for (library, layout) in libraries.iter().zip(LAYOUTS) {
                let whole = match probe(library, case, layout, None) {
                    Ok(whole) => whole,
                    Err(error) => {
                        failures.push(format!("{name} {layout:?}: {error}"));
                        continue;
                    }
                };
                if whole.observations != natural.observations
                    || Weight::expand(whole.tape.iter().copied()) != program
                {
                    failures.push(format!(
                        "{name} {layout:?}: the run or its charge program differs from layout 0"
                    ));
                    continue;
                }
                let at = expanded_positions(&whole.tape);
                for bound in sweep(&program, case.environment.bounds.charge) {
                    bounded += 1;
                    let run = match probe(library, case, layout, Some(bound)) {
                        Ok(run) => run,
                        Err(error) => {
                            failures.push(format!("{name} {layout:?} at {bound}: {error}"));
                            continue;
                        }
                    };
                    let expected = match ReferenceCharger::run(0, bound, &program) {
                        Ending::Within { .. } => Expected {
                            observations: whole.observations.clone(),
                            program: program.clone(),
                        },
                        Ending::Crossed { charged, event, .. } => {
                            crossed += 1;
                            let before = |position: usize| at[position] <= event;
                            let parks: Vec<usize> = whole
                                .parks
                                .iter()
                                .filter(|(position, _)| before(*position))
                                .map(|(_, requests)| *requests)
                                .collect();
                            Expected {
                                observations: Observations {
                                    prints: whole
                                        .observations
                                        .prints
                                        .iter()
                                        .zip(&whole.prints)
                                        .filter(|(_, position)| before(**position))
                                        .map(|(print, _)| print.clone())
                                        .collect(),
                                    end: ExpectedEnd::Bound {
                                        name: "charge".into(),
                                        limit: bound,
                                    },
                                    trace: whole.observations.trace[..parks.iter().sum::<usize>()]
                                        .to_vec(),
                                    charged,
                                    parks: parks.len(),
                                },
                                program: program[..=event].to_vec(),
                            }
                        }
                    };
                    let charges = Weight::expand(run.tape.iter().copied());
                    if run.observations != expected.observations || charges != expected.program {
                        failures.push(format!(
                            "{name} {layout:?} at {bound}: observed {:?} after {} charges, \
                             the reference ends {:?} after {}",
                            run.observations,
                            charges.len(),
                            expected.observations,
                            expected.program.len(),
                        ));
                    }
                }
            }
        }
    }
    assert!(
        failures.is_empty(),
        "{} of {bounded} bounded runs disagree with the reference:\n{}",
        failures.len(),
        failures.join("\n")
    );
    assert!(cases > 0 && crossed > 0, "cases {cases}, crossed {crossed}");
    println!("charge oracle: cases={cases} bounded={bounded} crossed={crossed}");
}

/// What the reference says a bounded run observes and charges.
struct Expected {
    observations: Observations,
    program: Vec<Weight>,
}
