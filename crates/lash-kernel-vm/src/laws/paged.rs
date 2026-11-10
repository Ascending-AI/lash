//! Continuation and pressure laws over the real persistent heap and S7 census.
use super::embedder::{Embedder, Setup, int, result};
use crate::{Machine, Request, Step};
use lash_kernel_doc::heap::PageValue;
use lash_kernel_state::kernel_internals::census_impl::Census;
use lash_kernel_state::kernel_internals::{PagedHeap, tree};
use lash_kernel_state::paged::*;
use lash_kernel_value::tokens::encode_body;

fn reopen(embedder: &mut Embedder) -> CheckpointRoot {
    embedder.reopen(true)
}

/// K-MACH-001/007: refusing publication must retain the page port's typed
/// cause, including when the failure happens before the budgeted tree seal.
#[test]
fn a_projection_refusal_preserves_the_typed_heap_cause() {
    use lash_kernel_doc::heap::{HeapFault, PageRef};
    use std::cell::Cell;
    use std::rc::Rc;

    struct LosingLease {
        source: super::embedder::Source,
        lost: Rc<Cell<bool>>,
        refused: Rc<Cell<usize>>,
    }
    impl PageSource for LosingLease {
        fn read(
            &mut self,
            pages: &[PageRef],
        ) -> Result<Vec<Result<PageAnswer, HeapFault>>, HeapFault> {
            self.source.read(pages)
        }
        fn seal(&mut self, body: PageBody) -> Result<PageRef, HeapFault> {
            if self.lost.get() {
                self.refused.set(self.refused.get() + 1);
                return Err(HeapFault::LeaseLost);
            }
            self.source.seal(body)
        }
    }

    // A long binding name must publish a metadata page while projecting
    // the frame, before seal_projection reaches the budgeted tree walk.
    let name = "binding".repeat(100);
    let mut run = Embedder::new(&format!("main {{ let {name} = 7 return {name} }}"));
    let (program, _) = run.program(true);
    let lost = Rc::new(Cell::new(false));
    let refused = Rc::new(Cell::new(0));
    run.machine = crate::KernelMachine::start(
        program,
        crate::Bounds {
            memory_meter: crate::MemoryMeter::Off,
            ..run.bounds
        },
        Setup::default().start,
        Box::new(LosingLease {
            source: run.source.clone(),
            lost: Rc::clone(&lost),
            refused: Rc::clone(&refused),
        }),
    )
    .unwrap();
    assert_eq!(run.machine.run(&mut run.world, 1).unwrap(), Step::Slice);
    lost.set(true);
    let error = run.machine.checkpoint().unwrap_err();
    assert_eq!(refused.get(), 1, "the publication reached the refused page");
    assert_eq!(error, crate::SealError::Heap(HeapFault::LeaseLost));
}

/// S4/K-SES-001: Main retains session function pins, Entry starts without
/// a session, and both retain their target kind through a cold continuation.
#[test]
fn main_and_entry_keep_their_distinct_checkpoint_roots() {
    use crate::{CellStart, EffectModes, Target};
    use lash_kernel_doc::Datum;
    use lash_kernel_value::Name;
    use tree::{SealWork, Tree};
    for entry in [false, true] {
        let mut embedder = Embedder::new(
            r#"definition "function f(x: Any) -> Any\nkernel 1\nmetering statements\nbody { let v = perform 0(x) as Any return v }"
main { let v = perform 0(7) as Any return v }"#,
        );
        let mut heap = PagedHeap::fresh(Box::new(embedder.source.clone()), EditEpoch(1));
        heap.set_census(Box::new(Census::restore(heap.certificate()).unwrap()))
            .unwrap();
        // The VM preserves host-validated immutable pins without interpreting
        // their payload. Use a real frozen page and the canonical function table.
        let pin = lash_kernel_state::paged::FunctionRecord {
            environment: heap.allocate_object(ObjectInit::Record(vec![])).unwrap(),
            callable: lash_kernel_value::CallableId {
                link: lash_kernel_value::LinkId::from_bytes([4; 32]),
                function: *embedder.document.functions.keys().next().unwrap(),
            },
            name: lash_kernel_state::paged::NameRef::new(&Name::new("host pin"), heap.pages())
                .unwrap(),
            meta: lash_kernel_doc::heap::PageValue::Null,
            closure: heap.freeze_datum(&Datum::Null).unwrap().root,
            captures: Default::default(),
            before: Default::default(),
            after: Default::default(),
        };
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
        embedder.machine = crate::KernelMachine::start_with_census(
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
                modes: EffectModes::new(),
                host: Vec::new(),
            },
            Box::new(embedder.source.clone()),
            Box::new(
                Census::restore(if entry {
                    AccountingCertificate::FRESH
                } else {
                    certificate
                })
                .unwrap(),
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
            assert_eq!(
                lash_kernel_state::kernel_internals::session_functions(&finished.carry.unwrap()),
                functions
            );
        }
    }
}

/// S4/D-SEAL: exhaustion returns no candidate, preserves the prior root,
/// and leaves the working continuation sealable and resumable.
#[test]
fn an_exhausted_seal_budget_preserves_both_continuations() {
    let mut embedder = Embedder::new(
        "main { let xs = [1] let v = perform 0(7) as Any set xs[0] = v return xs[0] }",
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
    let mut old = crate::KernelMachine::open_in(
        program,
        Setup::default().bounds,
        &prior,
        Box::new(embedder.source.clone()),
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
        "definition \"function work() -> Any\\nkernel 1\\nmetering statements\\nbody {  do perform 0(7) as Any  }\"\n main { let h = spawn call work() do yield let v = join h return 9 }",
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
    reopen(&mut embedder);
    assert_eq!(embedder.run_record().waits.len(), 1);
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
        Embedder::new("main { let xs = [1, 2] let v = perform 0(xs) as Any return v }");
    assert!(matches!(embedder.run(), Step::Parked(_)));
    embedder.deliver("echo");
    reopen(&mut embedder);
    assert_eq!(embedder.run_record().waits.len(), 1);
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
            "definition \"function work(tag: Any) -> Any\\nkernel 1\\nmetering statements\\nbody {  let v = perform 0(tag) as Any return v  }\"\n main { let a = spawn call work(\"a\") let b = spawn call work(\"b\") let handles = [a, b] let values = join all handles return values }",
        );
        assert!(matches!(embedder.run(), Step::Parked(ref park) if park.requests.len()==2));
        embedder.deliver("a");
        embedder.deliver("b");
        assert!(matches!(
            embedder.machine.run(&mut embedder.world, 1).unwrap(),
            Step::Slice
        ));
        let waits = embedder.run_record().waits;
        assert_eq!(waits.len(), 1);
        assert!(matches!(
            waits[0].phase,
            lash_kernel_state::paged::run::continuation::WaitPhase::Completed(_)
        ));
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
                "import 4 operation host.typed() -> Int main { try { let v = perform 4() as Int return v } catch e { return 1 } }",
            );
            let Step::Parked(park) = embedder.run() else {
                panic!("must park");
            };
            let Request::Effect(request) = &park.requests[0];
            let before = embedder.machine.meters().charged.expect("exact charge");
            let outcome = if rejected {
                Outcome::Completed(Datum::Text("not an integer".into()))
            } else {
                Outcome::Failed(ErrorDatum::new("host".into(), "bad".into(), Datum::Null))
            };
            embedder.machine.deliver(request.wait, outcome).unwrap();
            assert_eq!(
                embedder.machine.meters().charged.expect("exact charge"),
                before,
                "delivery is not a semantic step"
            );
            if cold {
                reopen(&mut embedder);
            }
            assert_eq!(result(embedder.run_to_end(&[])), int(1));
            // Return 1: statement 1 + literal 1 + outbound integer size 2.
            // Failed error: header 1 + kind bytes 4 + message bytes 3;
            // object record 4; properties record 3 + code text 5 + message text 4;
            // empty nonEnumerable record 1; raw null 1. Deep size = 26.
            let expected = 4 + if rejected { 0 } else { 26 };
            assert_eq!(
                embedder.machine.meters().charged.expect("exact charge") - before,
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
    use crate::{CellStart, EffectModes, StartError, Target};
    use lash_kernel_doc::Datum;
    use lash_kernel_doc::heap::{HeapFault, PageId, PageRef};
    struct Lost;
    impl PageSource for Lost {
        fn read(&mut self, _: &[PageRef]) -> Result<Vec<Result<PageAnswer, HeapFault>>, HeapFault> {
            Err(HeapFault::LeaseLost)
        }
        fn seal(&mut self, _: PageBody) -> Result<PageRef, HeapFault> {
            Err(HeapFault::LeaseLost)
        }
    }
    let (program, _) = Embedder::new(
        r#"definition "function f(x: Any) -> Any\nkernel 1\nmetering statements\nbody { return x }"
main {}"#,
    )
    .program(true);
    let start = CellStart {
        owner: PageId([9; 32]),
        target: Target::Entry("f".into()),
        args: vec![Datum::Text("x".repeat(8192))],
        session: None,
        modes: EffectModes::new(),
        host: Vec::new(),
    };
    assert!(matches!(
        crate::KernelMachine::start_with_census(program, ROOMY, start, Box::new(Lost), census()),
        Err(StartError::Heap(HeapFault::LeaseLost))
    ));
}

/// D-IMM/S4: a native error remains a bounded descriptor while finally
/// parks, then resumes with its exact kind, message and data.
#[test]
fn a_megabyte_native_error_survives_a_parked_finally() {
    let mut embedder = Embedder::new(
        "main { try { let unused = work.error() } finally { do perform 0(7) as Any } }",
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
    let crate::End::Error(crate::RunError::Uncaught {
        error: lash_kernel_doc::Datum::Error(error),
        ..
    }) = embedder.run_to_end(&[])
    else {
        panic!("native error")
    };
    assert_eq!(error.kind, "k".repeat(1 << 20));
    assert_eq!(error.message, "m".repeat(1 << 20));
    assert_eq!(error.raw().clone(), lash_kernel_doc::Datum::Null);
}

/// `K-SES-003` across execution closures (FIG-6193): the session a main
/// cell carries, classified by `classify_carry` over the real heap and
/// census, is adopted by the next cell whatever closure that cell's
/// document derives, with every binding it carried and their aliases, a
/// closure dropped with its typed diagnostic, and that cell's root pins
/// its own closure.
///
/// The run's one held total (`K-MEM`) holds a carried alias once, so
/// carrying `ys = xs` holds less than carrying a copy. A cold session body
/// counts once read (contract §2), so no cold-start hold is pinned here.
#[test]
fn a_session_carried_under_one_closure_is_adopted_under_another() {
    let carried = |ys: &str| {
        let mut first = Embedder::new(&format!(
            "main {{ let xs = [1, 2] let ys = {ys} let n = num.add(40, 2) let f = fn(k) {{ return num.add(k, 1) }} return null }}"
        ));
        let crate::End::Finished(finished) = first.run_to_end(&[]) else {
            panic!("the first cell finishes")
        };
        (first.machine.meters().memory, finished, first.source)
    };
    let (aliased, finished, first_source) = carried("xs");
    let (copied, _, _) = carried("[1, 2]");
    assert!(aliased < copied, "carried aliases share one object");
    let dropped: Vec<&str> = finished
        .not_carried
        .iter()
        .map(|(name, _)| name.as_str())
        .collect();
    assert_eq!(dropped, ["f"], "a closure ends with its cell");
    let start = |session: SessionRoot, source: &super::embedder::Source, closure| {
        let mut next = Embedder::new(
            "main { set ys[0] = 5 let got = num.add(num.add(n, xs[1]), xs[0]) let r = perform 0(1) as Any return got }",
        );
        let (mut program, _) = next.program(true);
        program.closure = closure;
        next.source = source.clone();
        next.machine = crate::KernelMachine::start(
            program,
            Setup::default().bounds,
            crate::CellStart {
                owner: lash_kernel_doc::heap::PageId([9; 32]),
                target: crate::Target::Main,
                args: Vec::new(),
                session: Some(session),
                modes: crate::EffectModes::new(),
                host: Vec::new(),
            },
            Box::new(next.source.clone()),
        )
        .unwrap();
        next
    };
    let session = finished.carry.expect("a main cell carries its session");
    for closure in [
        lash_kernel_doc::heap::PageId([8; 32]),
        lash_kernel_doc::heap::PageId([7; 32]),
    ] {
        let mut next = start(session, &first_source, closure);
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
            sealed.root.closure, closure,
            "the next cell pins its own closure"
        );
        next.deliver("echo");
        let crate::End::Finished(ended) = next.run_to_end(&[]) else {
            panic!("the next cell finishes under {closure:?}")
        };
        assert_eq!(ended.result, int(49), "under {closure:?}");
    }
}

/// FIG-7455: a dead cell-local closure must not make carrying a resident
/// data graph reconstruct every object from its stored directory.
#[test]
fn carrying_resident_records_does_not_read_them_back_from_pages() {
    let source = r#"
definition "function build(callback: Any) -> Any\nkernel 1\nmetering statements\nbody { let rows = [] let i = 0 while num.lt(i, 500) { set rows[list.len(rows)] = {key: i} set i = num.add(i, 1) } let temporary = callback do perform 0(0) as Any return rows }"
main { let host_callback = fn() { return 1 } let records = call build(fn() { return 1 }) do perform 0(0) as Any return list.len(records) }
"#;
    for ending in ["return list.len(records)", "finish list.len(records)"] {
        let mut run = Embedder::with(
            &source.replace("return list.len(records)", ending),
            Setup {
                start: crate::CellStart {
                    host: vec![(
                        lash_kernel_value::names::Name::new("host_callback"),
                        lash_kernel_doc::Datum::Null,
                    )],
                    ..Setup::default().start
                },
                ..Setup::default()
            },
        );
        assert!(matches!(run.run(), Step::Parked(_)));
        run.deliver_next();
        assert!(matches!(run.run(), Step::Parked(_)));
        run.deliver_next();
        crate::perf_witness::enable_work();
        let ended = run.run_to_end(&[]);
        let work = crate::perf_witness::take_work().unwrap();
        crate::perf_witness::disable_work();
        assert!(matches!(ended, crate::End::Finished(ref end) if end.result == int(500)));
        assert!(
            work.heap.object_head_lookups < 100,
            "carry rebuilt the resident graph: {} stored object lookups",
            work.heap.object_head_lookups
        );
    }
}

/// `K-EFF-003` (FIG-6193): a failed effect's error is decoded under the
/// document's number policy before the heap freezes it, as a completed
/// result is, so a number token in its data reaches the catching cell as
/// a number.
#[test]
fn a_failed_outcomes_number_tokens_are_decoded_before_it_is_frozen() {
    let mut embedder = Embedder::new(
        "main { let got = null try { do perform 0(1) as Any } catch e { set got = e.data.n } return got }",
    );
    assert!(matches!(embedder.run(), Step::Parked(_)));
    let request = embedder.pending.remove(0);
    let Request::Effect(effect) = &request;
    embedder
        .machine
        .deliver(
            effect.wait,
            crate::Outcome::Failed(lash_kernel_doc::ErrorDatum::new(
                "refused".into(),
                "a host refusal with numeric detail".into(),
                lash_kernel_doc::Datum::Record(vec![(
                    "n".into(),
                    lash_kernel_doc::Datum::Number(lash_kernel_doc::NumberToken::new("2").unwrap()),
                )]),
            )),
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
    use crate::{CellStart, EffectModes, StartError, Target};
    use lash_kernel_doc::Datum;
    use lash_kernel_value::Name;
    let embedder = Embedder::new("main { }");
    let limit = CheckpointRoot::HOST_METADATA_LIMIT;
    // A long name takes five framing bytes; an empty-name list takes
    // three plus two per name. The root's fixed fields own the budget.
    let empty_names = (limit - 3) / 2;
    for (names, refused_size) in [
        (vec![Name::new("h".repeat(limit - 5))], None),
        (vec![Name::new("h".repeat(limit - 4))], Some(limit + 1)),
        (vec![Name::new("é".repeat(2048))], Some(4101)),
        (vec![Name::new(""); empty_names], None),
        (
            vec![Name::new(""); empty_names + 1],
            Some(3 + 2 * (empty_names + 1)),
        ),
    ] {
        let (program, _) = embedder.program(false);
        let result = crate::KernelMachine::start_with_census(
            program,
            ROOMY,
            CellStart {
                owner: lash_kernel_doc::heap::PageId([9; 32]),
                target: Target::Main,
                args: Vec::new(),
                session: None,
                modes: EffectModes::new(),
                host: names.into_iter().map(|name| (name, Datum::Null)).collect(),
            },
            Box::new(embedder.source.clone()),
            census(),
        );
        if let Some(actual) = refused_size {
            assert!(
                matches!(
                    result,
                    Err(StartError::HostMetadataTooLarge { limit: bound, actual: size })
                        if bound == limit && size == actual
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

/// S4/K-TASK: a saved list join keeps its identity when another join is
/// allocated after a cold resume; neither live join overwrites the other.
#[test]
fn a_cold_resume_allocates_after_the_existing_list_join() {
    use super::embedder::{Embedder, result, text};
    use crate::Step;
    use lash_kernel_doc::Datum::List;
    let mut embedder = Embedder::new(
        r#"
definition "function work(tag: Any) -> Any\nkernel 1\nmetering statements\nbody {  let x = perform 0(tag) as Any return x  }"

definition "function gather(h: Any) -> Any\nkernel 1\nmetering statements\nbody {  let hs = [h] let xs = join all hs return xs  }"

main {
    let a = spawn call work("a")
    let first = spawn call gather(a)
    do perform 0("gate") as Any
    let b = spawn call work("b")
    let hs = [b]
    let second = join all hs
    let done = join first
    return [done, second]
}"#,
    );
    assert!(matches!(embedder.run(), Step::Parked(_)));
    let record = embedder.run_record();
    assert_eq!(record.next_join, 1);
    assert_eq!(record.joins.len(), 1);
    embedder.reopen(true);
    embedder.deliver("gate");
    assert!(matches!(embedder.run(), Step::Parked(_)));
    let record = embedder.run_record();
    assert_eq!(record.next_join, 2);
    assert_eq!(record.joins.len(), 2, "both live join identities survive");
    assert_eq!(
        result(embedder.run_to_end(&["b", "a"])),
        List(vec![List(vec![text("a")]), List(vec![text("b")])])
    );
}

/// Equality domain: allocator/census history is irrelevant; the pc, values,
/// identities and sharing reachable from a continuation remain observable.
#[test]
fn checkpoint_equality_keeps_continuations_and_ignores_allocator_history() {
    // Record edits are required to vary allocator history and pc independently of execution.
    use super::embedder::Embedder;
    use lash_kernel_state::kernel_internals::tree::Tree;
    use lash_kernel_state::paged::{ObjectInit, SemanticHeap};
    let mut run = Embedder::new("main { let a = [1] let b = a do perform 0(1) as Any return b }");
    assert!(matches!(run.run(), crate::Step::Parked(_)));
    let root = run.checkpoint();
    let snapshot = |root: &CheckpointRoot| {
        lash_kernel_state::semantic::checkpoint(root, Box::new(run.source.clone())).unwrap()
    };
    let original = snapshot(&root);
    let mut heap =
        PagedHeap::open_header(root.heap, Box::new(run.source.clone()), EditEpoch(20)).unwrap();
    heap.restore_execution_roots(&root);
    heap.set_census(Box::new(Census::restore(heap.certificate()).unwrap()))
        .unwrap();
    heap.allocate_object(ObjectInit::List(vec![PageValue::Bool(false)]))
        .unwrap();
    heap.seal(SealBudget {
        pages: u64::MAX,
        bytes: u64::MAX,
    })
    .unwrap()
    .unwrap();
    let mut garbage = root.clone();
    garbage.heap = heap.sealed_root().unwrap();
    assert_eq!(
        snapshot(&garbage),
        original,
        "allocator and unreachable garbage are outside equality"
    );
    let mut tasks = Tree::<TaskRecord>::of_keyed(&root.tasks);
    let key = lash_kernel_state::kernel_internals::directory::ordinal_key(0);
    let mut task = tasks.get(&key, heap.pages()).unwrap().unwrap();
    let frame_key = lash_kernel_state::kernel_internals::directory::ordinal_key(0);
    let mut frame = task.frames.get(&frame_key, heap.pages()).unwrap().unwrap();
    frame
        .pending
        .as_mut()
        .expect("the canonical region continuation")
        .site = vec![0, 0];
    task.frames.write(frame_key, frame, heap.pages()).unwrap();
    tasks.write(key, task, heap.pages()).unwrap();
    tasks
        .seal(
            heap.pages(),
            &mut lash_kernel_state::kernel_internals::tree::SealWork::new(SealBudget {
                pages: u64::MAX,
                bytes: u64::MAX,
            }),
        )
        .unwrap();
    let mut moved = root;
    moved.tasks = tasks.keyed_root().unwrap();
    assert_ne!(snapshot(&moved), original, "a frame pc is exact");
    let mut bindings = Tree::<BindingRecord>::of_keyed(&garbage.session);
    let key = lash_kernel_state::kernel_internals::metadata::metadata_key(
        &lash_kernel_value::names::Name::new("b"),
    );
    let mut binding = bindings.get(&key, heap.pages()).unwrap().unwrap();
    let independent = heap
        .allocate_object(ObjectInit::List(vec![PageValue::Int(
            lash_kernel_doc::heap::IntRef::Inline(1.into()),
        )]))
        .unwrap();
    binding.value = BoundRecord::Value(PageValue::List(independent));
    bindings.write(key, binding, heap.pages()).unwrap();
    bindings
        .seal(
            heap.pages(),
            &mut lash_kernel_state::kernel_internals::tree::SealWork::new(SealBudget {
                pages: u64::MAX,
                bytes: u64::MAX,
            }),
        )
        .unwrap();
    heap.seal(SealBudget {
        pages: u64::MAX,
        bytes: u64::MAX,
    })
    .unwrap()
    .unwrap();
    garbage.heap = heap.sealed_root().unwrap();
    garbage.session = bindings.keyed_root().unwrap();
    assert_ne!(
        snapshot(&garbage),
        original,
        "equal lists with broken sharing are different graphs"
    );
}

/// FIG-6737: a refinement under pressure in the middle of a statement,
/// whose later lookups of the running task go through its cursor, is
/// reconciled before the task table's rows are written. `g` takes an id at
/// the park, so dropping it leaves its bytes held, and the argument of the
/// spawn of task 64 refines; that spawn then evicts task 32 from main's
/// bucket, and the row the eviction writes is the one task 32 resumes from.
#[test]
fn a_mid_statement_refinement_is_reconciled_before_a_spawn_evicts_a_task() {
    let mut embedder = Embedder::with(
        r#"
definition "function w(log: Any, tag: Any, pad: Any) -> Any\nkernel 1\nmetering statements\nbody { \n  set log[list.len(log)] = tag\n  do yield\n  set log[list.len(log)] = tag\n  return tag\n }"

main {
  let log = []
  let hs = []
  let g = [work.repeat(16777216)]
  let v = perform 0("park") as Any
  set g = null
  let i = 1
  while num.lt(i, 64) {
    let h = spawn call w(log, i, null)
    set hs[list.len(hs)] = h
    set i = num.add(i, 1)
  }
  let last = spawn call w(log, 64, work.repeat(16777216))
  set hs[list.len(hs)] = last
  do join all hs
  return log
}"#,
        Setup {
            bounds: crate::Bounds {
                memory: 40 << 20,
                ..super::embedder::ROOMY
            },
            ..Setup::default()
        },
    );
    let end = embedder.run_to_end(&[]);
    let order: Vec<_> = (1..=64).chain(1..=64).map(int).collect();
    assert_eq!(result(end), lash_kernel_doc::Datum::List(order));
}
