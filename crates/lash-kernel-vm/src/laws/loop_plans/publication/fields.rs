use super::*;

/// Equal-price field names cannot authorize each other's member journal.
pub(super) fn names() {
    use crate::compile::room::{
        self, ExitKind, Measure, Owner as Selector, Phase, Point, publication::*,
    };
    use lash_kernel_doc::{self as doc, TaskId, Value};
    use lash_kernel_state::kernel_internals::logical::{
        JournalEvent, Member, MemberEdit, Program, RootValue, Slot,
    };
    use lash_kernel_value::sites::{Site, Unit};
    use std::collections::BTreeMap;
    use std::sync::Arc;

    struct Facts(BTreeMap<Measure, i128>);
    impl room::Facts for Facts {
        fn dependencies(&self) -> Option<&[doc::arena::ArenaHandle]> {
            Some(&[])
        }
        fn measure(&self, measure: &Measure) -> Option<i128> {
            self.0.get(measure).copied()
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
    struct Execution<'a>(&'a Facts);
    impl room::Execution for Execution<'_> {
        fn event(&mut self, _: &Point) -> Option<Box<dyn room::Facts + '_>> {
            Some(Box::new(Facts(self.0.0.clone())))
        }
        fn choose(&mut self, _: &Site) -> Option<bool> {
            None
        }
    }
    let run =
        Embedder::new("main { let record = {left: 1, rght: 1} set record.left = 2 return record }");
    let (program, _) = run.program(false);
    let source = Arc::new(room::Source::compile(
        doc::constant_pool::PoolOwner::Document(program.document.identity()),
        Site::new(Unit::Main, vec![]),
        program.document.main.as_ref().unwrap(),
        &[],
        Default::default(),
        &BTreeMap::new(),
        &Default::default(),
    ));
    let at = Point {
        site: source.site.child(1),
        phase: Phase::Commit,
    };
    let interval = source.compile_event(&at, ExitKind::Normal).unwrap();
    let event = interval.events().next().unwrap();
    let heap = run.machine.source_law_heap();
    let store = heap.logical_ledger();
    let mut ledger = store.borrow_mut();
    let mut arena = lash_kernel_state::arena::Arena::new();
    let left = arena.text("left");
    let right = arena.text("rght");
    let mut nursery = doc::Nursery::unaccounted(arena.objects());
    let record = nursery.allocate(doc::NewObject::Record(vec![
        (left, Value::Int(1)),
        (right, Value::Int(1)),
    ]));
    let foreign = nursery.allocate(doc::NewObject::Record(vec![
        (left, Value::Int(1)),
        (right, Value::Int(1)),
    ]));
    arena.install(nursery.into_new()).unwrap();
    let Value::Record(handle) = record else {
        panic!("record body")
    };
    let Value::Record(foreign_handle) = foreign else {
        panic!("foreign record body")
    };
    let (invocation, entered) = ledger
        .prepare_enter(
            TaskId::MAIN,
            &mut arena,
            heap.pages(),
            &[],
            &[
                (Slot::Binding(0), RootValue::Value(&record)),
                (Slot::Binding(1), RootValue::Value(&foreign)),
            ],
        )
        .unwrap();
    ledger
        .commit(entered.admit(u64::MAX).ok().unwrap())
        .unwrap();
    let slots = source.owner_slots().unwrap();
    let values: Vec<_> = event
        .inputs
        .iter()
        .map(|selector| {
            let Selector::Operand(site) = selector else {
                panic!("evaluated member operands")
            };
            (
                slots.root(selector, TaskId::MAIN, invocation.id()).unwrap(),
                if site == &at.site.child(1) {
                    Value::Int(2)
                } else {
                    record
                },
            )
        })
        .collect();
    let outputs: Vec<_> = values
        .iter()
        .map(|(owner, value)| (*owner, RootValue::Value(value)))
        .collect();
    let held = ledger
        .prepare_ownership(&mut arena, heap.pages(), &[], &outputs)
        .unwrap();
    ledger.commit(held.admit(u64::MAX).ok().unwrap()).unwrap();
    let inputs: Vec<_> = values.iter().map(|(owner, _)| *owner).collect();
    let mut facts = BTreeMap::new();
    for (selector, (owner, _)) in event.inputs.iter().zip(&values) {
        facts.insert(
            Measure::Local(selector.clone()),
            i128::from(
                ledger
                    .holding(*owner, heap.pages())
                    .unwrap()
                    .unwrap()
                    .bytes(),
            ),
        );
    }
    facts.insert(Measure::MemberDelta(at.site.clone()), 0);
    facts.insert(
        Measure::Release(event.inputs.clone()),
        i128::from(ledger.release_cost(&inputs, heap.pages()).unwrap()),
    );
    let facts = Facts(facts);
    let position = Position {
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
    drop(ledger);
    let ledger = store.borrow();
    let frontier = source
        .frontier(
            &ledger,
            heap,
            &invocation,
            program.link,
            doc::KernelVersion::One,
            position.clone(),
        )
        .unwrap();
    let next = Position {
        unpaid: Some(source.next_after_commit(&at.site).unwrap()),
        ..position
    };
    let allowance = crate::admission::Allowance {
        occurrences: vec![],
        before: 0,
        units: 0,
        close: None,
        strict: false,
        depth: 0,
        safe_point: 7,
    };
    for (target, name, valid) in [
        (handle, "left", true),
        (handle, "rght", false),
        (foreign_handle, "left", false),
    ] {
        let mut charged = 0;
        let mut counters = crate::machine::tasks::Occurrences::for_source_law(0);
        let resources = crate::admission::SourceResources {
            charged: &mut charged,
            counters: &mut counters,
            bound: 10,
            slice: 10,
            depth: 0,
            limit: 10,
            safe_point: 7,
        };
        let actual = Program::new(vec![JournalEvent {
            inputs: inputs.clone(),
            member: Some(MemberEdit {
                handle: target,
                member: Member::Field(Arc::from(name)),
                next: Some(Value::Int(2)),
            }),
            ..Default::default()
        }]);
        let evaluated = frontier.evaluate_interval(
            Arc::clone(&interval),
            &mut Execution(&facts),
            &ledger,
            &arena,
            heap,
            actual,
            next.clone(),
            allowance.clone(),
            resources.facts(&allowance).unwrap(),
        );
        assert_eq!(
            evaluated.is_ok(),
            valid,
            "only the admitted code field and actual target operand may be edited: {name} {target:?}"
        );
        assert_eq!(charged, 0);
        assert!(matches!(
            arena.table_get(handle, &left).unwrap(),
            Some(Value::Int(1))
        ));
        assert!(matches!(
            arena.table_get(handle, &right).unwrap(),
            Some(Value::Int(1))
        ));
    }
}
