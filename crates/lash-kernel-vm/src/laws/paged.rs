//! Continuation and pressure laws over the real persistent heap and S7 census.
use super::embedder::{Embedder, Setup, int, result};
use crate::{Machine, Request, Step};
use lash_kernel_doc::graph_tokens::{decode_body, encode_body};
use lash_kernel_doc::heap::LogicalBytes;
use lash_kernel_state::paged::census_impl::Census;
use lash_kernel_state::paged::*;

fn reopen(embedder: &mut Embedder) -> CheckpointRoot {
    let sealed = embedder
        .machine
        .seal(SealBudget {
            pages: u64::MAX,
            bytes: u64::MAX,
        })
        .unwrap()
        .unwrap();
    let certificate = embedder.machine.accounting();
    let root: CheckpointRoot = decode_body(&encode_body(&sealed.root)).unwrap();
    let (program, _) = embedder.program(true);
    embedder.machine = crate::KernelMachine::open(
        program,
        Setup::default().bounds,
        &root,
        Vec::new(),
        Box::new(embedder.source.clone()),
        Box::new(
            Census::restore(certificate, LogicalBytes(Setup::default().bounds.memory)).unwrap(),
        ),
    )
    .unwrap();
    root
}

/// K-MACH-007/008: a suspended KIR action and shared-variable alias resume
/// from semantic coordinates, without any whole-state reader.
#[test]
fn a_parked_kir_frame_resumes_from_its_sealed_record() {
    let mut embedder = Embedder::new(
        "main { let xs = [1] let alias = xs let v = perform echo(7) as Any set alias[0] = v return xs[0] }",
    );
    assert!(matches!(embedder.run(), Step::Parked(_)));
    let root = reopen(&mut embedder);
    assert_eq!(root.owner, lash_kernel_doc::heap::PageId([9; 32]));
    assert_eq!(root.release, lash_kernel_doc::heap::PageId([8; 32]));
    embedder.deliver("echo");
    assert_eq!(result(embedder.run_to_end(&[])), int(7));
}

/// K-MACH-007, issued-phase amendment: a slice may freeze an issued intent
/// while another task is ready. Neither seal nor reopen reports that intent.
#[test]
fn an_issued_request_survives_a_slice_and_is_reported_once() {
    let mut embedder = Embedder::with(
        "fn work() { do perform echo(7) as Any } main { let h = spawn call work() do yield let v = join h return 9 }",
        Setup {
            slice: 1,
            ..Setup::default()
        },
    );
    loop {
        let step = embedder.run();
        if embedder.machine.wait_count() != 0 && matches!(step, Step::Slice) {
            break;
        }
        assert!(matches!(step, Step::Slice));
    }
    let root = reopen(&mut embedder);
    assert_eq!(root.waits.len, 1);
    assert!(embedder.requests.is_empty());
    loop {
        match embedder.run() {
            Step::Slice => (),
            Step::Parked(park) => {
                assert_eq!(park.requests.len(), 1);
                break;
            }
            other => panic!("{other:?}"),
        }
    }
    assert!(matches!(embedder.run(), Step::Parked(ref park) if park.requests.is_empty()));
    assert_eq!(embedder.requests.len(), 1);
    assert!(matches!(embedder.requests[0], Request::Effect(_)));
    embedder.deliver("echo");
    assert_eq!(result(embedder.run_to_end(&[])), int(9));
}

/// Committed-phase amendment: the frozen delivery remains in its wait
/// until the resumed statement installs it, including across a checkpoint.
#[test]
fn a_committed_outcome_survives_seal_before_semantic_resume() {
    let mut embedder =
        Embedder::new("main { let xs = [1, 2] let v = perform echo(xs) as Any return v }");
    assert!(matches!(embedder.run(), Step::Parked(_)));
    embedder.deliver("echo");
    let root = reopen(&mut embedder);
    assert_eq!(root.waits.len, 1);
    assert_eq!(
        result(embedder.run_to_end(&[])),
        lash_kernel_doc::Datum::List(vec![int(1), int(2)])
    );
    assert_eq!(embedder.machine.wait_count(), 0);
}
