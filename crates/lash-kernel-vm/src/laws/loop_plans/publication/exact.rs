//! Exact exits must bind values and entry versions, not merely matching prices.
use super::*;
use crate::Machine;
use crate::compile::room::{self, Phase, Point, publication::*};
use lash_kernel_doc::{TaskId, Value};
use lash_kernel_state::kernel_internals::logical;
use lash_kernel_value::sites::{Site, Unit};
use std::sync::Arc;

struct Facts {
    handles: Vec<lash_kernel_doc::arena::ArenaHandle>,
}
impl room::Facts for Facts {
    fn dependencies(&self) -> Option<&[lash_kernel_doc::arena::ArenaHandle]> {
        Some(&self.handles)
    }
    fn measure(&self, at: &room::Measure) -> Option<i128> {
        match at {
            room::Measure::Local(_) => Some(17),
            room::Measure::Release(_) => Some(0),
            _ => None,
        }
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
struct Execution(Vec<lash_kernel_doc::arena::ArenaHandle>);
impl room::Execution for Execution {
    fn event(&mut self, _: &Point) -> Option<Box<dyn room::Facts + '_>> {
        Some(Box::new(Facts {
            handles: self.0.clone(),
        }))
    }
    fn choose(&mut self, _: &Site) -> Option<bool> {
        None
    }
}
fn resources<'a>(
    charged: &'a mut u64,
    counters: &'a mut crate::machine::tasks::Occurrences,
) -> crate::admission::SourceResources<'a> {
    crate::admission::SourceResources {
        charged,
        counters,
        bound: 20,
        slice: 20,
        depth: 1,
        limit: 10,
        safe_point: 7,
    }
}
fn paid() -> crate::admission::Allowance {
    crate::admission::Allowance {
        occurrences: vec![],
        before: 0,
        units: 0,
        close: None,
        strict: false,
        depth: 0,
        safe_point: 7,
    }
}

pub(super) fn events() {
    let run = Embedder::new("main { let n = 1 return n }");
    let (program, _) = run.program(false);
    let source = Arc::new(room::Source::compile(
        lash_kernel_doc::constant_pool::PoolOwner::Document(program.document.identity()),
        Site::new(Unit::Main, vec![]),
        program.document.main.as_ref().unwrap(),
        &[],
        Default::default(),
        &std::collections::BTreeMap::new(),
        &std::collections::BTreeSet::new(),
    ));
    let heap = run.machine.source_law_heap();
    let store = heap.logical_ledger();
    let mut arena = lash_kernel_state::arena::Arena::new();
    let dependency = arena.new_cell(Value::Int(1));
    let mut ledger = store.borrow_mut();
    let (invocation, enter) = ledger
        .prepare_enter(TaskId::MAIN, &mut arena, heap.pages(), &[], &[])
        .unwrap();
    ledger.commit(enter.admit(u64::MAX).ok().unwrap()).unwrap();
    drop(ledger);
    let ledger = store.borrow();
    let at = Point {
        site: source.site.child(0).child(0),
        phase: Phase::Commit,
    };
    let interval = source.compile_event(&at, room::ExitKind::Normal).unwrap();
    let output = source
        .owner_slots()
        .unwrap()
        .root(
            &room::Owner::Operand(at.site.clone()),
            TaskId::MAIN,
            invocation.id(),
        )
        .unwrap();
    let start = Position {
        at: at.clone(),
        unpaid: Some(at.clone()),
        recovery: [1; 32],
        occurrence: Occurrence {
            task: TaskId::MAIN,
            invocation: invocation.id(),
            loops: vec![],
            action: None,
        },
    };
    let next = Position {
        unpaid: Some(source.next_after_commit(&at.site).unwrap()),
        recovery: [2; 32],
        ..start.clone()
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
    let event = ExactEvent::compile(&source, interval, Successor::Normal).unwrap();
    let roots = |owner, value| ExactRoots {
        inputs: vec![],
        outputs: vec![(owner, logical::PublicationValue::Native(value))],
        ..Default::default()
    };
    let mut charged = 7;
    let mut counters = crate::machine::tasks::Occurrences::for_source_law(0);
    let before = ledger.balance().unwrap().total().unwrap();
    // 1 and 2 have the same canonical holding. The source literal still decides.
    for (owner, value, position, allowance) in [
        (output, Value::Int(2), next.clone(), paid()),
        (
            invocation.owner(logical::Slot::Argument(0)),
            Value::Int(1),
            next.clone(),
            paid(),
        ),
        (output, Value::Int(1), start.clone(), paid()),
        (
            output,
            Value::Int(1),
            next.clone(),
            crate::admission::Allowance { units: 1, ..paid() },
        ),
    ] {
        let facts = resources(&mut charged, &mut counters)
            .facts(&allowance)
            .unwrap();
        assert!(
            event
                .bind(
                    &frontier,
                    &ledger,
                    &arena,
                    heap,
                    &mut Execution(vec![]),
                    roots(owner, value),
                    position,
                    allowance,
                    facts
                )
                .is_err(),
            "wrong exact value, owner, successor or already-paid close must be rejected"
        );
        assert_eq!(charged, 7);
        assert_eq!(ledger.balance().unwrap().total().unwrap(), before);
        assert_eq!(frontier.position(), &start);
    }
    let facts = resources(&mut charged, &mut counters)
        .facts(&paid())
        .unwrap();
    let bound = event
        .bind(
            &frontier,
            &ledger,
            &arena,
            heap,
            &mut Execution(vec![dependency]),
            roots(output, Value::Int(1)),
            next.clone(),
            paid(),
            facts,
        )
        .unwrap();
    arena.cell_set(dependency, Value::Int(2)).unwrap();
    assert!(
        bound
            .prepare(&frontier, &ledger, arena.registration().unwrap(), heap)
            .is_err(),
        "stale discarded-input dependency must reject before any root is installed"
    );
    assert!(ledger.holding(output, heap.pages()).unwrap().is_none());
    // Every insufficient exact bound leaves the paid prefix and roots intact.
    for limit in [0, before, before + 16] {
        let facts = resources(&mut charged, &mut counters)
            .facts(&paid())
            .unwrap();
        let bound = event
            .bind(
                &frontier,
                &ledger,
                &arena,
                heap,
                &mut Execution(vec![]),
                roots(output, Value::Int(1)),
                next.clone(),
                paid(),
                facts,
            )
            .unwrap();
        let prepared = bound
            .prepare(&frontier, &ledger, arena.registration().unwrap(), heap)
            .unwrap();
        assert_eq!(prepared.peak(), before + 17);
        assert!(
            prepared
                .admit(
                    &frontier,
                    &ledger,
                    resources(&mut charged, &mut counters),
                    limit
                )
                .unwrap()
                .is_none()
        );
        assert_eq!(charged, 7);
        assert_eq!(ledger.balance().unwrap().total().unwrap(), before);
    }
    let facts = resources(&mut charged, &mut counters)
        .facts(&paid())
        .unwrap();
    let bound = event
        .bind(
            &frontier,
            &ledger,
            &arena,
            heap,
            &mut Execution(vec![]),
            roots(output, Value::Int(1)),
            next.clone(),
            paid(),
            facts,
        )
        .unwrap();
    let prepared = bound
        .prepare(&frontier, &ledger, arena.registration().unwrap(), heap)
        .unwrap();
    let accepted = prepared
        .admit(
            &frontier,
            &ledger,
            resources(&mut charged, &mut counters),
            before + 17,
        )
        .unwrap()
        .unwrap();
    drop(ledger);
    accepted
        .publish_event(&mut frontier, &mut store.borrow_mut(), |_, edits| {
            assert!(edits.is_empty())
        })
        .unwrap();
    let ledger = store.borrow();
    assert_eq!(frontier.position(), &next);
    assert_eq!(
        charged, 7,
        "an exact paid event must not repay its canonical close"
    );
    assert_eq!(ledger.balance().unwrap().total().unwrap(), before + 17);
    assert_eq!(
        ledger
            .holding(output, heap.pages())
            .unwrap()
            .unwrap()
            .bytes(),
        17
    );
    // The binding must move the literal's actual provenance, not substitute a
    // same-price scalar under the correct source owner.
    let bind_at = Point {
        site: source.site.child(0),
        phase: Phase::Commit,
    };
    let binding = source
        .owner_slots()
        .unwrap()
        .root(
            source
                .compile_event(&bind_at, room::ExitKind::Normal)
                .unwrap()
                .events()
                .next()
                .unwrap()
                .output
                .as_ref()
                .unwrap(),
            TaskId::MAIN,
            invocation.id(),
        )
        .unwrap();
    drop(ledger);
    frontier
        .handoff(
            &mut store.borrow_mut(),
            Position {
                at: bind_at.clone(),
                unpaid: Some(bind_at.clone()),
                ..next.clone()
            },
        )
        .unwrap();
    let ledger = store.borrow();
    let next_binding = Position {
        at: bind_at.clone(),
        unpaid: Some(source.next_after_commit(&bind_at.site).unwrap()),
        ..next
    };
    let binding_event = ExactEvent::compile(
        &source,
        source
            .compile_event(&bind_at, room::ExitKind::Normal)
            .unwrap(),
        Successor::Normal,
    )
    .unwrap();
    let facts = resources(&mut charged, &mut counters)
        .facts(&paid())
        .unwrap();
    assert!(
        binding_event
            .bind(
                &frontier,
                &ledger,
                &arena,
                heap,
                &mut Execution(vec![]),
                ExactRoots {
                    inputs: vec![output],
                    outputs: vec![(binding, logical::PublicationValue::Native(Value::Int(2)))],
                    ..Default::default()
                },
                next_binding.clone(),
                paid(),
                facts
            )
            .is_err(),
        "binding must preserve the source RHS value"
    );
    let facts = resources(&mut charged, &mut counters)
        .facts(&paid())
        .unwrap();
    let bound = binding_event
        .bind(
            &frontier,
            &ledger,
            &arena,
            heap,
            &mut Execution(vec![]),
            ExactRoots {
                inputs: vec![output],
                outputs: vec![(binding, logical::PublicationValue::Existing(output))],
                ..Default::default()
            },
            next_binding.clone(),
            paid(),
            facts,
        )
        .unwrap();
    let accepted = bound
        .prepare(&frontier, &ledger, arena.registration().unwrap(), heap)
        .unwrap()
        .admit(
            &frontier,
            &ledger,
            resources(&mut charged, &mut counters),
            before + 17,
        )
        .unwrap()
        .unwrap();
    drop(ledger);
    accepted
        .publish_event(&mut frontier, &mut store.borrow_mut(), |_, _| {})
        .unwrap();
    let ledger = store.borrow();
    assert_eq!(ledger.balance().unwrap().total().unwrap(), before + 17);
    assert!(ledger.holding(output, heap.pages()).unwrap().is_none());
    assert!(ledger.holding(binding, heap.pages()).unwrap().is_some());
    drop(ledger);
    runtime_exits();
}

fn runtime_exits() {
    use crate::{Bounds, Layout, Step};
    let source = r#"main {
        let f = fn(x) { return x }
        let n = "a long resident string that cannot fit inside a short inline text value"
        let xs = [n]
        if true { let v = xs[0] set n = v }
        let i = 0
        while num.lt(i, 2) { set n = xs[0] set i = num.add(i, 1) }
        for v in xs { if true { set n = v } }
        return apply f(n)
    }"#;
    let observe = |layout, cold, source_mode, bounds: Bounds| {
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
        run.machine.take_exact_events();
        let end = loop {
            if source_mode {
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
                Step::Parked(_) => panic!("pure exact-source fixture cannot park"),
            }
        };
        let counts = run.machine.take_exact_events();
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
        counts.iter().all(|count| *count > 0),
        "every migrated exact family actually publishes: {counts:?}"
    );
    eprintln!("exact source families published: {counts:?}");
    for layout in [Layout(0), Layout(1), Layout(2), Layout(0x5eed)] {
        for cold in [false, true] {
            assert_eq!(
                observe(layout, cold, true, roomy).0,
                reference,
                "exact source cold={cold} layout={layout:?}"
            );
            assert_eq!(
                observe(layout, cold, false, roomy).0,
                reference,
                "generic packets cold={cold} layout={layout:?}"
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
    ] {
        let reference = observe(Layout(0), false, true, bounds).0;
        assert_eq!(
            observe(Layout(2), true, false, bounds).0,
            reference,
            "exact refusal and cold paid prefix"
        );
    }
}
