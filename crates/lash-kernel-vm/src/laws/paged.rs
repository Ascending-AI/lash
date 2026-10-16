//! Continuation and pressure laws over the real persistent heap and S7 census.
use super::embedder::{Embedder, Setup, int, result};
use crate::{Machine, Request, Step};
use lash_kernel_doc::graph_tokens::encode_body;
use lash_kernel_doc::heap::LogicalBytes;
use lash_kernel_state::paged::census_impl::Census;
use lash_kernel_state::paged::*;

fn reopen(embedder: &mut Embedder) -> CheckpointRoot {
    embedder.reopen(crate::Layout::default(), true)
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

/// S4/K-SES-001: Main retains session function pins, Entry starts without
/// a session, and both retain their target kind through a cold continuation.
#[test]
fn main_and_entry_keep_their_distinct_checkpoint_roots() {
    use crate::{CellStart, EffectModes, KernelMachine, Target};
    use lash_kernel_doc::{Datum, Name};
    use tree::{SealWork, Tree};
    for entry in [false, true] {
        let mut embedder = Embedder::new(
            "entry f(x: Int) -> Any fn f(x) { let v = perform echo(x) as Any return v } main { let v = perform echo(7) as Any return v }",
        );
        let mut heap = PagedHeap::fresh(Box::new(embedder.source.clone()), EditEpoch(1));
        heap.set_census(Box::new(
            Census::restore(heap.certificate(), LogicalBytes(1 << 30)).unwrap(),
        ))
        .unwrap();
        // The VM preserves host-validated immutable pins without interpreting
        // their payload. Use a real frozen page and the canonical function table.
        let pin = heap.freeze_datum(&Datum::Null).unwrap().root;
        let mut functions = Tree::new(false);
        functions
            .write(b"host pin".to_vec(), pin, heap.pages())
            .unwrap();
        functions
            .seal(
                heap.pages(),
                &mut SealWork::new(SealBudget {
                    pages: u64::MAX,
                    bytes: u64::MAX,
                }),
            )
            .unwrap();
        let functions = functions.keyed_root().unwrap();
        let session = heap
            .carry(CarryRoots {
                bindings: KeyedRoot::Empty,
                functions,
            })
            .unwrap();
        let certificate = heap.certificate();
        let (program, _) = embedder.program(true);
        embedder.machine = KernelMachine::start(
            program,
            Setup::default().bounds,
            CellStart {
                owner: lash_kernel_doc::heap::PageId([9; 32]),
                target: if entry {
                    Target::Entry(Name::new("f"))
                } else {
                    Target::Main
                },
                args: if entry {
                    vec![Datum::Int(7.into())]
                } else {
                    Vec::new()
                },
                session: if entry { None } else { Some(session) },
                modes: EffectModes::recorded(),
                host: Vec::new(),
            },
            Box::new(embedder.source.clone()),
            Box::new(
                Census::restore(certificate, LogicalBytes(Setup::default().bounds.memory)).unwrap(),
            ),
        )
        .unwrap();
        assert!(matches!(embedder.run(), Step::Parked(_)));
        let root = reopen(&mut embedder);
        assert_eq!(
            root.functions,
            if entry { KeyedRoot::Empty } else { functions }
        );
        assert_eq!(
            root.target,
            if entry {
                CellTargetKind::Entry
            } else {
                CellTargetKind::Main
            }
        );
        embedder.deliver("echo");
        let crate::End::Finished(finished) = embedder.run_to_end(&[]) else {
            panic!("finish")
        };
        assert_eq!(finished.result, int(7));
        if entry {
            assert!(finished.carry.is_none());
        } else {
            assert_eq!(finished.carry.unwrap().functions, functions);
        }
    }
}

/// S4/D-SEAL: exhaustion returns no candidate, preserves the prior root,
/// and leaves the working continuation sealable and resumable.
#[test]
fn an_exhausted_seal_budget_preserves_both_continuations() {
    let mut embedder = Embedder::new(
        "main { let xs = [1] let v = perform echo(7) as Any set xs[0] = v return xs[0] }",
    );
    assert!(matches!(embedder.run(), Step::Parked(_)));
    let prior = reopen(&mut embedder);
    let bytes = encode_body(&prior);
    embedder.deliver("echo");
    assert!(
        embedder
            .machine
            .seal(SealBudget { pages: 0, bytes: 0 })
            .unwrap()
            .is_none()
    );
    assert_eq!(encode_body(&prior), bytes);
    let (program, _) = embedder.program(true);
    let old_heap =
        PagedHeap::open_header(prior.heap, Box::new(embedder.source.clone()), EditEpoch(2))
            .unwrap();
    let mut old = crate::KernelMachine::open(
        program,
        Setup::default().bounds,
        &prior,
        Vec::new(),
        Box::new(embedder.source.clone()),
        Box::new(
            Census::restore(
                old_heap.certificate(),
                LogicalBytes(Setup::default().bounds.memory),
            )
            .unwrap(),
        ),
    )
    .unwrap();
    assert!(
        matches!(old.run(&mut embedder.world,u64::MAX).unwrap(),Step::Parked(ref park) if park.requests.is_empty())
    );
    reopen(&mut embedder);
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
    assert_eq!(root.waits.len(), 1);
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
    assert_eq!(root.waits.len(), 1);
    assert_eq!(
        result(embedder.run_to_end(&[])),
        lash_kernel_doc::Datum::List(vec![int(1), int(2)])
    );
    assert_eq!(embedder.machine.wait_count(), 0);
}

/// S4 committed phase: a delivery queued behind other runnable work stays
/// frozen until its own statement resumes, in both warm and cold branches.
#[test]
fn a_committed_delivery_waits_behind_other_ready_work() {
    for cold in [false, true] {
        let mut embedder = Embedder::new(
            "fn work(tag) { let v = perform echo(tag) as Any return v } main { let a = spawn call work(\"a\") let b = spawn call work(\"b\") let handles = [a, b] let values = join all handles return values }",
        );
        assert!(matches!(embedder.run(), Step::Parked(ref park) if park.requests.len()==2));
        embedder.deliver("a");
        embedder.deliver("b");
        assert!(matches!(
            embedder.machine.run(&mut embedder.world, 1).unwrap(),
            Step::Slice
        ));
        let sealed = embedder
            .machine
            .seal(SealBudget {
                pages: u64::MAX,
                bytes: u64::MAX,
            })
            .unwrap()
            .unwrap();
        assert_eq!(sealed.root.waits.len(), 1);
        let heap = PagedHeap::open_header(
            sealed.root.heap,
            Box::new(embedder.source.clone()),
            EditEpoch(2),
        )
        .unwrap();
        let waits = tree::Tree::<WaitRecord>::of_keyed(&sealed.root.waits);
        waits
            .visit(heap.pages(), &mut |_, wait| {
                assert!(matches!(
                    wait.phase,
                    WaitPhase::Committed(CommittedOutcomeRecord::Completed(_))
                ));
                true
            })
            .unwrap();
        if cold {
            reopen(&mut embedder);
        }
        assert_eq!(
            result(embedder.run_to_end(&[])),
            lash_kernel_doc::Datum::List(vec![
                super::embedder::text("a"),
                super::embedder::text("b")
            ])
        );
        assert_eq!(embedder.machine.wait_count(), 0);
    }
}

/// K-EFF-005/009: rejected Completed data has no inbound charge. A host
/// failure is charged once by its error's deep size, even after reopen.
#[test]
fn rejected_and_failed_outcomes_keep_their_distinct_charges_after_open() {
    use crate::Outcome;
    use lash_kernel_doc::{Datum, ErrorDatum};
    for cold in [false, true] {
        for rejected in [false, true] {
            let mut embedder = Embedder::new(
                "effect typed() -> Int main { try { let v = perform typed() as Int return v } catch e { return 1 } }",
            );
            let Step::Parked(park) = embedder.run() else {
                panic!("must park");
            };
            let Request::Effect(request) = &park.requests[0] else {
                panic!("must perform");
            };
            let before = embedder.machine.meters().charged;
            let outcome = if rejected {
                Outcome::Completed(Datum::Text("not an integer".into()))
            } else {
                Outcome::Failed(ErrorDatum {
                    kind: "host".into(),
                    message: "bad".into(),
                    data: Datum::Null,
                })
            };
            embedder.machine.deliver(request.wait, outcome).unwrap();
            assert_eq!(
                embedder.machine.meters().charged,
                before,
                "delivery is not a semantic step"
            );
            if cold {
                reopen(&mut embedder);
            }
            assert_eq!(result(embedder.run_to_end(&[])), int(1));
            // Return 1: statement 1 + literal 1 + outbound integer size 2.
            // Failed error: size 1 + kind bytes 4 + message bytes 3 + null 1.
            let expected = 4 + if rejected { 0 } else { 9 };
            assert_eq!(
                embedder.machine.meters().charged - before,
                expected,
                "cold={cold}, rejected={rejected}"
            );
        }
    }
}

/// K-MACH infrastructure causes stay typed when inbound argument freezing
/// fails; a page-port loss is never an argument/type refusal.
#[test]
fn argument_freezing_preserves_a_page_ports_typed_failure() {
    use super::embedder::{ROOMY, census};
    use crate::{
        CellStart, EffectModes, KernelMachine, PreparedLibrary, Program, StartError, Target,
    };
    use lash_kernel_doc::heap::{HeapFault, PageId, PageRef};
    use lash_kernel_doc::{Datum, FunctionRegistry, parse_document};
    use std::sync::Arc;
    struct Lost;
    impl PageSource for Lost {
        fn read(&mut self, _: &[PageRef]) -> Result<Vec<PageAnswer>, HeapFault> {
            Err(HeapFault::LeaseLost)
        }
        fn seal(&mut self, _: PageBody) -> Result<PageRef, HeapFault> {
            Err(HeapFault::LeaseLost)
        }
    }
    let program = Program {
        release: PageId([8; 32]),
        document: Arc::new(parse_document("kernel 1\nnumbers by_spelling\nentry f(x: Any) -> Any\nfn f(x) { return x }\nmain { }").unwrap()),
        library: PreparedLibrary::new(Arc::new(FunctionRegistry::new())),
    };
    let start = CellStart {
        owner: PageId([9; 32]),
        target: Target::Entry("f".into()),
        args: vec![Datum::Int(1.into())],
        session: None,
        modes: EffectModes::recorded(),
        host: Vec::new(),
    };
    assert!(matches!(
        KernelMachine::start(program, ROOMY, start, Box::new(Lost), census(ROOMY.memory)),
        Err(StartError::Heap(HeapFault::LeaseLost))
    ));
}

/// S5 bounded diagnostics / K-TASK-018: counts are immediate and a query
/// bounds examined rows even when its first row does not match its filter.
#[test]
fn task_diagnostics_count_and_enumerate_only_the_requested_prefix() {
    use super::embedder::main_site;
    use crate::TaskFilter;
    use lash_kernel_doc::{TaskId, TaskIdentity};
    let mut embedder = Embedder::new(
        "fn waits() { do perform echo(1) as Any } fn fails() { throw 2 } main { do spawn call waits() do spawn call fails() do perform echo(3) as Any }",
    );
    let Step::Parked(park) = embedder.run() else {
        panic!("tasks wait");
    };
    assert_eq!((park.live, park.outstanding), (1, 2));
    let first = embedder
        .machine
        .tasks(park.tasks, TaskFilter::Outstanding, None, 1)
        .unwrap();
    assert!(first.tasks.is_empty(), "main is examined but excluded");
    assert_eq!(first.after, Some(TaskId::MAIN));
    assert!(!first.finished);
    let worker = embedder
        .machine
        .tasks(park.tasks, TaskFilter::Live, first.after, 1)
        .unwrap();
    assert_eq!(worker.tasks.len(), 1);
    assert_eq!(worker.tasks[0].0, TaskId(1));
    let projected = embedder.machine.task_identity(worker.tasks[0].1).unwrap();
    assert_eq!(
        projected,
        TaskIdentity::Spawned(lash_kernel_doc::SpawnIdentity {
            parent: std::sync::Arc::new(TaskIdentity::Main),
            site: main_site([0, 0]),
            occurrence: 0,
        })
    );
    let failed = embedder
        .machine
        .tasks(park.tasks, TaskFilter::Outstanding, worker.after, 1)
        .unwrap();
    assert_eq!(failed.tasks[0].0, TaskId(2));
    assert!(failed.finished);
    let none = embedder
        .machine
        .tasks(park.tasks, TaskFilter::Live, worker.after, 1)
        .unwrap();
    assert!(none.tasks.is_empty());
    assert!(none.finished);
}

/// D-IMM/S4: a native error remains a bounded descriptor while finally
/// parks, then resumes with its exact kind, message and data.
#[test]
fn a_megabyte_native_error_survives_a_parked_finally() {
    let mut embedder = Embedder::new(
        "main { try { let unused = work.error() } finally { do perform echo(7) as Any } }",
    );
    assert!(matches!(embedder.run(), Step::Parked(_)));
    reopen(&mut embedder);
    assert!(
        embedder
            .source
            .0
            .borrow()
            .values()
            .all(|page| page.tokens.len() <= 65_536)
    );
    embedder.deliver("echo");
    let crate::End::Error(crate::RunError::Uncaught(lash_kernel_doc::Datum::Error(error))) =
        embedder.run_to_end(&[])
    else {
        panic!("native error")
    };
    assert_eq!(error.kind, "k".repeat(1 << 20));
    assert_eq!(error.message, "m".repeat(1 << 20));
    assert_eq!(error.data, lash_kernel_doc::Datum::Null);
}

/// `K-SES-003` across helper releases (FIG-6193): the session a main
/// cell under one release carries, classified by `classify_carry` over the
/// real heap and census, is adopted by the next cell under the same
/// release or another one with every binding it carried and their aliases,
/// a closure dropped with its typed diagnostic, and that cell's root pins
/// its own release.
#[test]
fn a_session_carried_under_one_release_is_adopted_under_another() {
    let mut first = Embedder::new(
        "main { let xs = [1, 2] let ys = xs let n = num.add(40, 2) let f = fn(k) { return num.add(k, 1) } return null }",
    );
    let crate::End::Finished(finished) = first.run_to_end(&[]) else {
        panic!("the first cell finishes")
    };
    let dropped: Vec<&str> = finished
        .not_carried
        .iter()
        .map(|(name, _)| name.as_str())
        .collect();
    assert_eq!(dropped, ["f"], "a closure ends with its cell");
    let session = finished.carry.expect("a main cell carries its session");
    let mut source = first.source.clone();
    let certificate = heap_accounting(&mut source, session.heap).unwrap();
    for release in [
        lash_kernel_doc::heap::PageId([8; 32]),
        lash_kernel_doc::heap::PageId([7; 32]),
    ] {
        let mut next = Embedder::new(
            "main { set ys[0] = 5 let got = num.add(num.add(n, xs[1]), xs[0]) let r = perform echo(1) as Any return got }",
        );
        let (mut program, _) = next.program(true);
        program.release = release;
        next.source = first.source.clone();
        next.machine = crate::KernelMachine::start(
            program,
            Setup::default().bounds,
            crate::CellStart {
                owner: lash_kernel_doc::heap::PageId([9; 32]),
                target: crate::Target::Main,
                args: Vec::new(),
                session: Some(session),
                modes: crate::EffectModes::recorded(),
                host: Vec::new(),
            },
            Box::new(next.source.clone()),
            Box::new(
                Census::restore(certificate, LogicalBytes(Setup::default().bounds.memory)).unwrap(),
            ),
        )
        .unwrap();
        assert!(matches!(next.run(), Step::Parked(_)));
        let sealed = next
            .machine
            .seal(SealBudget {
                pages: u64::MAX,
                bytes: u64::MAX,
            })
            .unwrap()
            .unwrap();
        assert_eq!(
            sealed.root.release, release,
            "the next cell pins its own release"
        );
        next.deliver("echo");
        let crate::End::Finished(ended) = next.run_to_end(&[]) else {
            panic!("the next cell finishes under {release:?}")
        };
        assert_eq!(ended.result, int(49), "under {release:?}");
    }
}

/// `K-EFF-003` (FIG-6193): a failed effect's error is decoded under the
/// document's number policy before the heap freezes it, as a completed
/// result is, so a number token in its data reaches the catching cell as
/// a number.
#[test]
fn a_failed_outcomes_number_tokens_are_decoded_before_it_is_frozen() {
    let mut embedder = Embedder::new(
        "main { let got = null try { do perform echo(1) as Any } catch e { set got = e.data.n } return got }",
    );
    assert!(matches!(embedder.run(), Step::Parked(_)));
    let request = embedder.pending.remove(0);
    let Request::Effect(effect) = &request else {
        panic!("the cell waits on its effect")
    };
    embedder
        .machine
        .deliver(
            effect.wait,
            crate::Outcome::Failed(lash_kernel_doc::ErrorDatum {
                kind: "refused".into(),
                message: "a host refusal with numeric detail".into(),
                data: lash_kernel_doc::Datum::Record(vec![(
                    "n".into(),
                    lash_kernel_doc::Datum::Number(lash_kernel_doc::NumberToken::new("2").unwrap()),
                )]),
            }),
        )
        .unwrap();
    let crate::End::Finished(finished) = embedder.run_to_end(&[]) else {
        panic!("the cell catches the failure and finishes")
    };
    assert_eq!(finished.result, int(2));
}

/// FIG-6499 / S1: host metadata is refused at admission, before a cell can
/// publish an oversized checkpoint. Count token framing and UTF-8 bytes.
#[test]
fn long_host_binding_names_are_bounded_before_a_checkpoint_is_published() {
    use super::embedder::{ROOMY, census};
    use crate::{CellStart, EffectModes, KernelMachine, StartError, Target};
    use lash_kernel_doc::{Datum, Name};
    let embedder = Embedder::new("main { }");
    for (names, refused_size) in [
        (vec![Name::new("h".repeat(3062))], None),
        (vec![Name::new("h".repeat(3063))], Some(3068)),
        (vec![Name::new("é".repeat(2048))], Some(4101)),
        (vec![Name::new(""); 1532], None),
        (vec![Name::new(""); 1533], Some(3069)),
    ] {
        let (program, _) = embedder.program(false);
        let result = KernelMachine::start(
            program,
            ROOMY,
            CellStart {
                owner: lash_kernel_doc::heap::PageId([9; 32]),
                target: Target::Main,
                args: Vec::new(),
                session: None,
                modes: EffectModes::recorded(),
                host: names.into_iter().map(|name| (name, Datum::Null)).collect(),
            },
            Box::new(embedder.source.clone()),
            census(ROOMY.memory),
        );
        if let Some(actual) = refused_size {
            assert!(
                matches!(
                    result,
                    Err(StartError::HostMetadataTooLarge { limit: 3067, actual: size })
                        if size == actual
                ),
                "oversized host metadata must retain its typed limit and size"
            );
        } else {
            let mut machine = result.expect("maximal metadata is admitted");
            let sealed = machine
                .seal(SealBudget {
                    pages: u64::MAX,
                    bytes: u64::MAX,
                })
                .unwrap()
                .expect("admitted metadata seals");
            assert!(encode_body(&sealed.root).len() + 256 <= 4096);
        }
    }
}
