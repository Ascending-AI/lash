//! K-EVAL-008: a selected child transfers its existing value at the exact join.
use super::*;
mod frame;
use crate::Machine;
use crate::admission::{Allowance, SourceResources};
use crate::compile::room::{self, ExitKind, Phase, Point, publication::*};
use lash_kernel_doc::{TaskId, Value};
use lash_kernel_state::kernel_internals::logical::{PublicationValue as PV, RootValue};
use lash_kernel_value::sites::{Site, Unit};
use std::sync::Arc;

pub(super) fn joins() {
    frame::entries();
    runtime();
    let run = Embedder::new("main { return if(true, 7, 9) }");
    let (program, _) = run.program(false);
    let make_source = || {
        Arc::new(room::Source::compile(
            lash_kernel_doc::constant_pool::PoolOwner::Document(program.document.identity()),
            Site::new(Unit::Main, vec![]),
            program.document.main.as_ref().unwrap(),
            &[],
            Default::default(),
            &std::collections::BTreeMap::new(),
            &Default::default(),
        ))
    };
    let source = make_source();
    let foreign_source = make_source();
    let at = Point {
        site: source.site.child(0).child(0),
        phase: Phase::Commit,
    };
    let from = at.site.child(1);
    let interval = source.compile_join(&from, &at.site).unwrap();
    assert!(Arc::ptr_eq(
        &interval,
        &source.compile_join(&from, &at.site).unwrap()
    ));
    assert_eq!(interval.exit(), ExitKind::Normal);
    let exit = ControlExit::compile(&source, Arc::clone(&interval)).unwrap();
    let heap = run.machine.source_law_heap();
    let store = heap.logical_ledger();
    let mut ledger = store.borrow_mut();
    let mut arena = lash_kernel_state::arena::Arena::new();
    let (invocation, enter) = ledger
        .prepare_enter(TaskId::MAIN, &mut arena, heap.pages(), &[], &[])
        .unwrap();
    ledger.commit(enter.admit(4096).ok().unwrap()).unwrap();
    let slots = source.owner_slots().unwrap();
    let root = |site| {
        slots
            .root(&room::Owner::Operand(site), TaskId::MAIN, invocation.id())
            .unwrap()
    };
    let input = root(from);
    let wrong = root(at.site.child(2));
    let output = root(at.site.clone());
    let holding = ledger
        .prepare_ownership(
            &mut arena,
            heap.pages(),
            &[],
            &[
                (input, RootValue::Value(&Value::Int(7))),
                (wrong, RootValue::Value(&Value::Int(9))),
            ],
        )
        .unwrap();
    ledger.commit(holding.admit(4096).ok().unwrap()).unwrap();
    let balance = ledger.balance().unwrap();
    drop(ledger);
    let ledger = store.borrow();
    let start = Position {
        at: at.clone(),
        unpaid: Some(at.clone()),
        recovery: [9; 32],
        occurrence: Occurrence {
            task: TaskId::MAIN,
            invocation: invocation.id(),
            loops: vec![],
            action: None,
        },
    };
    let mut frontier = source
        .frontier(
            &ledger,
            heap,
            &invocation,
            program.link,
            lash_kernel_doc::KernelVersion::One,
            start.clone(),
        )
        .unwrap();
    let next = Position {
        unpaid: Some(source.next_after_commit(&at.site).unwrap()),
        ..start.clone()
    };
    let allowance = Allowance {
        occurrences: vec![],
        before: 0,
        units: 0,
        close: None,
        strict: false,
        depth: 0,
        safe_point: 0,
    };
    let mut charged = 0;
    let mut counters = crate::machine::tasks::Occurrences::for_source_law(0);
    fn resource_owner<'a>(
        charged: &'a mut u64,
        counters: &'a mut crate::machine::tasks::Occurrences,
    ) -> SourceResources<'a> {
        SourceResources {
            charged,
            counters,
            bound: 10,
            slice: 10,
            depth: 0,
            limit: 10,
            safe_point: 0,
        }
    }
    let roots = |owner, value| ControlRoots {
        inputs: vec![owner],
        outputs: vec![(output, value)],
        ..Default::default()
    };
    struct Facts;
    impl room::Facts for Facts {
        fn dependencies(&self) -> Option<&[lash_kernel_doc::arena::ArenaHandle]> {
            Some(&[])
        }
        fn measure(&self, _: &room::Measure) -> Option<i128> {
            None
        }
        fn call(&self, _: &Site, _: room::CallSource) -> Option<&room::CheckedRoom> {
            None
        }
        fn raised(&self) -> bool {
            false
        }
        fn resolved(&self, _: &Site) -> Option<lash_kernel_value::FunctionId> {
            None
        }
    }
    struct Execution;
    impl room::Execution for Execution {
        fn event(&mut self, _: &Point) -> Option<Box<dyn room::Facts + '_>> {
            Some(Box::new(Facts))
        }
        fn choose(&mut self, _: &Site) -> Option<bool> {
            None
        }
    }
    let facts = || Execution;
    let mut rejected = Vec::new();
    for (selected_source, actual, next, allowance) in [
        (
            &source,
            roots(wrong, PV::Existing(wrong)),
            next.clone(),
            allowance.clone(),
        ),
        (
            &source,
            roots(input, PV::Native(Value::Int(7))),
            next.clone(),
            allowance.clone(),
        ),
        (
            &source,
            roots(input, PV::Existing(input)),
            start.clone(),
            allowance.clone(),
        ),
        (
            &source,
            roots(input, PV::Existing(input)),
            next.clone(),
            Allowance {
                units: 1,
                ..allowance.clone()
            },
        ),
        (
            &foreign_source,
            roots(input, PV::Existing(input)),
            next.clone(),
            allowance.clone(),
        ),
    ] {
        let resources = resource_owner(&mut charged, &mut counters);
        rejected.push(
            exit.prepare(
                selected_source,
                &frontier,
                &invocation,
                &ledger,
                arena.registration().unwrap(),
                heap,
                &mut facts(),
                actual,
                next,
                allowance.clone(),
                resources.facts(&allowance).unwrap(),
            )
            .unwrap()
            .is_none(),
        );
    }
    assert_eq!(
        rejected, [true; 5],
        "source roles, provenance, unpaid head, debt and source identity bind the join"
    );
    let resources = resource_owner(&mut charged, &mut counters);
    let prepared = exit
        .prepare(
            &source,
            &frontier,
            &invocation,
            &ledger,
            arena.registration().unwrap(),
            heap,
            &mut facts(),
            roots(input, PV::Existing(input)),
            next.clone(),
            allowance.clone(),
            resources.facts(&allowance).unwrap(),
        )
        .unwrap()
        .unwrap();
    assert!(
        prepared
            .admit(&frontier, &ledger, resources, balance.total().unwrap() - 1)
            .unwrap()
            .is_none()
    );
    assert_eq!(ledger.balance().unwrap(), balance);
    assert_eq!(charged, 0);
    let resources = resource_owner(&mut charged, &mut counters);
    let prepared = exit
        .prepare(
            &source,
            &frontier,
            &invocation,
            &ledger,
            arena.registration().unwrap(),
            heap,
            &mut facts(),
            roots(input, PV::Existing(input)),
            next.clone(),
            allowance.clone(),
            resources.facts(&allowance).unwrap(),
        )
        .unwrap()
        .unwrap();
    let accepted = prepared
        .admit(&frontier, &ledger, resources, balance.total().unwrap())
        .unwrap()
        .unwrap();
    // Dropping an acceptance publishes neither source roots nor charge state.
    drop(accepted);
    assert_eq!(frontier.position(), &start);
    assert_eq!(ledger.balance().unwrap(), balance);
    let resources = resource_owner(&mut charged, &mut counters);
    let prepared = exit
        .prepare(
            &source,
            &frontier,
            &invocation,
            &ledger,
            arena.registration().unwrap(),
            heap,
            &mut facts(),
            roots(input, PV::Existing(input)),
            next.clone(),
            allowance.clone(),
            resources.facts(&allowance).unwrap(),
        )
        .unwrap()
        .unwrap();
    let accepted = prepared
        .admit(&frontier, &ledger, resources, balance.total().unwrap())
        .unwrap()
        .unwrap();
    drop(ledger);
    let mut ledger = store.borrow_mut();
    accepted
        .publish_control(&mut frontier, &mut ledger, |_| {})
        .unwrap();
    assert_eq!(frontier.position(), &next);
    assert_eq!(ledger.balance().unwrap(), balance);
    assert!(ledger.holding(input, heap.pages()).unwrap().is_none());
    for owner in [output, wrong] {
        assert!(ledger.holding(owner, heap.pages()).unwrap().is_some());
    }
    assert_eq!(charged, 0);
}

/// K-EVAL-008/K-CHG-001: controls retain padding and saved operands across cold stops.
fn runtime() {
    use crate::machine::islands::witness;
    use crate::{Bounds, Layout, Step};
    let source = "main { let leaf = fn(x, missing) { if true { do missing } return [x, missing] } let f = fn(x) { if true { let kept = x do kept } let selected = let(alias = if(true, x, 99), alias) return (selected, apply leaf(x)) } return apply f(7) }";
    let observe = |layout, cold, precise, bounds: Bounds| {
        let mut run = Embedder::with(
            source,
            Setup {
                layout,
                bounds,
                slice: if cold { 1 } else { u64::MAX },
                ..Default::default()
            },
        );
        let tape = crate::ChargeTape::new();
        run.machine.record_charges(tape.clone());
        witness::begin();
        let end = loop {
            if precise {
                run.machine.force_source_execution();
            } else {
                run.machine.force_generic_packets();
            }
            match run.run() {
                Step::Ended(end) => break end,
                Step::Slice => {
                    if cold {
                        run.reopen(layout, true);
                        run.machine.record_charges(tape.clone());
                    }
                }
                Step::Parked(_) => panic!("pure control fixture cannot park"),
            }
        };
        let precise_frames: u64 = witness::prepared_frame_entries().iter().sum();
        let packet_frames: u64 = witness::packet_frame_entries().iter().sum();
        if bounds.charge_limit == crate::laws::embedder::ROOMY.charge_limit
            && bounds.memory == crate::laws::embedder::ROOMY.memory
            && bounds.call_depth == crate::laws::embedder::ROOMY.call_depth
            && !cold
        {
            assert!(
                if precise {
                    precise_frames > 0
                } else {
                    packet_frames > 0
                },
                "certified frame entry actually executed: precise={precise_frames}, packet={packet_frames}"
            );
        }
        let counts = [
            witness::call_enters(),
            witness::precise_joins().iter().sum(),
            witness::precise_scopes(),
            witness::packet_call_enters(),
            witness::scope_publications(),
            witness::packet_call_resumes().iter().sum(),
        ];
        (
            (
                end,
                run.machine.meters().charged,
                lash_kernel_exec::Weight::expand(tape.steps()),
            ),
            counts,
        )
    };
    let roomy = crate::laws::embedder::ROOMY;
    let (reference, counts) = observe(Layout(0), false, true, roomy);
    assert!(
        counts[..3].iter().all(|count| *count > 0),
        "precise control publications executed: {counts:?}"
    );
    let (generic, counts) = observe(Layout(0), false, false, roomy);
    assert_eq!(generic, reference);
    assert!(
        counts[3..].iter().all(|count| *count > 0),
        "packet control publications executed: {counts:?}"
    );
    eprintln!(
        "certified packet Enter/Leave/Restore publications: {:?}",
        &counts[3..]
    );
    for layout in [Layout(0), Layout(2), Layout(0x5eed)] {
        for precise in [false, true] {
            assert_eq!(
                observe(layout, true, precise, roomy).0,
                reference,
                "cold source controls layout={layout:?} precise={precise}"
            );
        }
    }
    for bounds in [
        Bounds {
            charge_limit: 3,
            ..roomy
        },
        Bounds {
            memory: 512,
            ..roomy
        },
        Bounds {
            call_depth: 1,
            ..roomy
        },
    ] {
        let reference = observe(Layout(0), false, true, bounds).0;
        assert_eq!(
            observe(Layout(2), true, false, bounds).0,
            reference,
            "exact control refusal and paid prefix"
        );
    }
    // The generic observations above are an independent packet oracle. Also
    // require the specialized run to use an accepted whole-region publication.
    let region_observe = |layout, cold, precise, bounds: Bounds| {
        let mut run = Embedder::with(
            source,
            Setup {
                layout,
                bounds,
                slice: if cold { 1 } else { u64::MAX },
                ..Default::default()
            },
        );
        let tape = crate::ChargeTape::new();
        run.machine.record_charges(tape.clone());
        witness::begin();
        let mut holdings = Vec::new();
        let end = loop {
            if precise {
                run.machine.force_source_execution();
            }
            match run.run() {
                Step::Ended(end) => break end,
                Step::Slice => {
                    holdings.push((run.machine.meters(), run.machine.semantic_checkpoint()));
                    if cold {
                        run.reopen(layout, true);
                        run.machine.record_charges(tape.clone());
                    }
                }
                Step::Parked(_) => panic!("pure control fixture cannot park"),
            }
        };
        (
            end,
            run.machine.meters(),
            lash_kernel_exec::Weight::expand(tape.steps()),
            holdings,
        )
    };
    for layout in [Layout(0), Layout(2), Layout(0x5eed)] {
        let expected = region_observe(layout, false, true, roomy);
        crate::machine::islands::take_certified_statements();
        let actual = region_observe(layout, false, false, roomy);
        assert_eq!(
            actual, expected,
            "compiled region charge prefix and holdings"
        );
        assert!(
            crate::machine::islands::take_certified_statements() > 0,
            "the specialized observation must publish an accepted compiled region"
        );
        assert_eq!(
            region_observe(layout, true, false, roomy),
            region_observe(layout, true, true, roomy),
            "compiled region cold charge prefix and holdings"
        );
    }
    for bounds in [
        Bounds {
            charge_limit: 3,
            ..roomy
        },
        Bounds {
            memory: 512,
            ..roomy
        },
        Bounds {
            call_depth: 1,
            ..roomy
        },
    ] {
        assert_eq!(
            region_observe(Layout(2), true, false, bounds),
            region_observe(Layout(2), true, true, bounds),
            "compiled region refusal preserves its exact charge prefix and holdings"
        );
    }
}
