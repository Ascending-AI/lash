//! Ephemeral runs on the corpus (FIG-6733): every case run ephemerally
//! observes what the same case observes durably, each in one machine from
//! start to end: the same prints, end, effect trace, charge and parks.
//! A memory refusal is the one difference allowed: an ephemeral run holds
//! its session in the arena, never also as pages, so it may first observe
//! the bound later, after more charge, but it never holds past it.

use lash_kernel_vm::kernel_internals::Layout;
use lash_kernel_vm::{Bounds, Durability, KernelMachine, PreparedLibrary};

use crate::machine::{Park, Run, run_case_in};
use crate::{Case, ExpectedEnd, HarnessError, Observations};

fn observe(
    library: &PreparedLibrary,
    case: &Case,
    durability: Durability,
) -> Result<Observations, HarnessError> {
    let mut start = |program, bounds: Bounds, start, source: &_| {
        let bounds = Bounds {
            durability,
            ..bounds
        };
        crate::pages::with_source(program, bounds, start, Layout(0), source)
    };
    let run = Run {
        start: &mut start,
        printed: &mut || {},
        charge: None,
    };
    run_case_in::<KernelMachine>(
        library,
        case,
        run,
        &mut |at: Park<'_, KernelMachine>| Ok(at.machine),
        None,
    )
}

fn alike(durable: &Observations, ephemeral: &Observations) -> bool {
    match durable.end {
        ExpectedEnd::Bound { ref name, .. } if name == "memory" => {
            Observations {
                charged: durable.charged,
                ..ephemeral.clone()
            } == *durable
        }
        _ => ephemeral == durable,
    }
}

#[test]
fn corpus_runs_alike_ephemerally() {
    let library = PreparedLibrary::new(std::sync::Arc::new(super::corpus_registry()));
    let (mut cases, mut failures) = (0, Vec::new());
    for shard in super::corpus_shards() {
        for case in &shard.cases {
            // A case the harness cannot run is the corpus tests' to report.
            let Ok(durable) = observe(&library, case, Durability::Durable) else {
                continue;
            };
            if durable.end == ExpectedEnd::Refused {
                continue;
            }
            cases += 1;
            match observe(&library, case, Durability::Ephemeral) {
                Ok(ephemeral) if alike(&durable, &ephemeral) => {}
                Ok(ephemeral) => failures.push(format!(
                    "{} {}: durably {durable:?}, ephemerally {ephemeral:?}",
                    shard.rule, case.name
                )),
                Err(error) => failures.push(format!("{} {}: {error}", shard.rule, case.name)),
            }
        }
    }
    assert!(cases > 0, "the corpus must contain cases");
    eprintln!("{cases} corpus cases ran alike ephemerally");
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}
