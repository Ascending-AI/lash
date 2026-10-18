//! Ephemeral runs (FIG-6733): a run the host never saves keeps its values
//! and continuations in memory, does no projection or publication, parks
//! and resumes in the same machine, and refuses every save and open.

use super::embedder::{Embedder, Setup, int, result};
use lash_kernel_doc::heap::LogicalBytes;
use lash_kernel_state::paged::AllocationCensus;

use crate::{Bounds, Durability, End, OpenError, SealError, Step};

/// Session writes, calls, more spawned tasks than the task cache holds,
/// a list join, and an effect park the run resumes from.
const WORKERS: &str = r#"
fn worker(n) {
  let r = perform echo(n) as Any
  return num.add(r, 1)
}
fn total(hs) {
  let rs = join all hs
  let sum = 0
  let i = 0
  while num.lt(i, list.len(rs)) {
    set sum = num.add(sum, rs[i])
    set i = num.add(i, 1)
  }
  return sum
}
main {
  let hs = []
  let i = 0
  while num.lt(i, 80) {
    set hs[list.len(hs)] = spawn call worker(i)
    set i = num.add(i, 1)
  }
  let sum = call total(hs)
  return sum
}
"#;

/// 0 + 1 + ... + 79, each plus one.
const TOTAL: i64 = 80 * 79 / 2 + 80;

fn ephemeral() -> Setup {
    Setup {
        bounds: Bounds {
            durability: Durability::Ephemeral,
            ..Setup::default().bounds
        },
        ..Setup::default()
    }
}

/// An ephemeral run publishes no execution root and projects no value,
/// from its first statement to its end, and carries no session.
#[test]
fn an_ephemeral_run_publishes_and_projects_nothing() {
    let mut embedder = Embedder::with(WORKERS, ephemeral());
    crate::perf_witness::enable_work();
    let end = embedder.run_to_end(&[]);
    let work = crate::perf_witness::take_work().unwrap();
    crate::perf_witness::disable_work();
    assert_eq!(work.root_publishes, 0, "the run published its roots");
    assert_eq!(work.heap.projection_visits, 0, "the run projected values");
    assert_eq!(work.heap.arena_marks, 0, "the run marked the arena");
    let End::Finished(finished) = &end else {
        panic!("the run finishes: {end:?}")
    };
    assert!(
        finished.carry.is_none(),
        "an ephemeral cell carries nothing"
    );
    assert_eq!(result(end), int(TOTAL));
}

/// An ephemeral run parks and resumes in the machine that ran it, still
/// ephemeral after every resume: it refuses to seal before and after, and
/// a checkpoint root of a durable run refuses to open as ephemeral.
#[test]
fn an_ephemeral_run_resumes_in_place_and_refuses_to_be_saved() {
    let mut embedder = Embedder::with(WORKERS, ephemeral());
    crate::perf_witness::enable_work();
    assert!(matches!(embedder.run(), Step::Parked(_)));
    assert_eq!(embedder.pending.len(), 80);
    assert!(matches!(
        embedder.try_checkpoint(),
        Err(SealError::Ephemeral)
    ));
    let end = loop {
        embedder.deliver_next();
        match embedder.run() {
            Step::Parked(_) => assert!(matches!(
                embedder.try_checkpoint(),
                Err(SealError::Ephemeral)
            )),
            Step::Ended(end) => break end,
            Step::Slice => panic!("the run has no slice"),
        }
    };
    assert_eq!(result(end), int(TOTAL));
    let work = crate::perf_witness::take_work().unwrap();
    crate::perf_witness::disable_work();
    assert_eq!(work.root_publishes, 0, "a resume published its roots");
    assert_eq!(work.heap.projection_visits, 0, "a resume projected values");

    let mut durable = Embedder::new(WORKERS);
    assert!(matches!(durable.run(), Step::Parked(_)));
    let (root, certificate) = durable.checkpoint();
    let (program, _) = durable.program(true);
    let opened = crate::KernelMachine::open_with_census(
        program,
        ephemeral().bounds,
        &root,
        Vec::new(),
        Box::new(durable.source.clone()),
        Box::new(
            lash_kernel_state::kernel_internals::census_impl::Census::restore(
                certificate,
                LogicalBytes(Setup::default().bounds.memory),
            )
            .unwrap(),
        ),
        crate::Layout::default(),
    );
    assert!(matches!(opened, Err(OpenError::Ephemeral)));
}

/// A loop that replaces what it keeps and calls a function each time: its
/// slots grow and its frames come and go, and what it can reach stays
/// small however long it runs.
const CHURN: &str = r#"
fn make(n) {
  return [n, n, n, n]
}
main {
  let keep = [0]
  let i = 0
  while num.lt(i, 2000) {
    set keep[0] = call make(i)
    set i = num.add(i, 1)
  }
  return list.len(keep)
}
"#;

/// `K-BND-003` for an ephemeral run, which never refines over published
/// roots: under a memory bound twice the least one a durable run of the
/// same program fits, the ephemeral run fits too. What its frames and
/// slots held and released does not accumulate against the bound.
#[test]
fn an_ephemeral_run_fits_where_a_durable_run_fits() {
    let run = |memory, durability| {
        let setup = Setup {
            bounds: Bounds {
                memory,
                durability,
                ..Setup::default().bounds
            },
            ..Setup::default()
        };
        Embedder::with(CHURN, setup).run_to_end(&[])
    };
    let fits = (10..24)
        .map(|power| 1_u64 << power)
        .find(|memory| matches!(run(*memory, Durability::Durable), End::Finished(_)))
        .expect("a durable run fits some bound");
    assert_eq!(
        result(run(2 * fits, Durability::Ephemeral)),
        int(1),
        "under {} bytes",
        2 * fits
    );
}
