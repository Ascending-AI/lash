use super::*;

/// K-CHG-001: an existing iterable holding does not prove its read was paid.
pub(super) fn payment() {
    use room::publication::{
        Occurrence, Position,
        boundary::{LoopEntry, PaidLoopInput},
    };
    let definition = crate::laws::embedder::fragment(
        "function walk(xs: Any) -> Any\nkernel 1\nmetering statements\nbody { let total = 0 for x in xs { set total = flat.add(total,x) } return total }",
    );
    let run = Embedder::with(
        &format!("definition \"{definition}\"\nmain {{ return call walk([1]) }}"),
        Setup {
            probes: Some(crate::laws::layout::scalar_probes),
            ..Setup::default()
        },
    );
    let (program, _) = run.program(true);
    let admitted = program
        .library
        .admit(&program.document, Default::default(), &program.selection)
        .unwrap_or_else(|_| panic!("initial for admitted"));
    let exe = admitted
        .executable(
            &program.document,
            &program.library,
            &program.documents,
            &program.functions,
            &program.instances,
        )
        .unwrap_or_else(|_| panic!("initial for executable"));
    let plan = exe
        .islands
        .plans
        .iter()
        .find(|plan| {
            let source = &exe.code(plan.code).source_room;
            source.params.as_ref() == [lash_kernel_value::Name::new("xs")] && source.body.len() == 3
        })
        .unwrap();
    let source = &exe.code(plan.code).source_room;
    let statement = plan.body.statements[1].id;
    let placement = Arc::new(BindingPlacement::compile(&exe, plan.code).unwrap());
    let entry = LoopEntry::compile(&exe, &placement, statement).unwrap();
    let root_slot = |declared, name: &str| match source
        .owner_slots()
        .unwrap()
        .root(
            &room::Owner::Binding {
                declared,
                name: lash_kernel_value::Name::new(name),
            },
            TaskId::MAIN,
            0,
        )
        .unwrap()
    {
        logical::Owner::Value { slot, .. } => slot,
        _ => unreachable!(),
    };
    let input = root_slot(source.site.clone(), "xs");
    let total = root_slot(source.site.child(0), "total");
    let heap = run.machine.source_law_heap();
    let store = heap.logical_ledger();
    let mut arena = Arena::new();
    let list = arena.create_list(&[Value::Int(1)]);
    let zero = Value::Int(0);
    let mut ledger = store.borrow_mut();
    let (invocation, entered) = ledger
        .prepare_enter(
            TaskId::MAIN,
            &mut arena,
            heap.pages(),
            &[],
            &[
                (input, RootValue::Value(&list)),
                (total, RootValue::Value(&zero)),
            ],
        )
        .unwrap();
    ledger
        .commit(entered.admit(u64::MAX).ok().unwrap())
        .unwrap();
    let cursor = source
        .owner_slots()
        .unwrap()
        .root(
            &room::Owner::LoopCursor(exe.stmt(statement).site.clone()),
            TaskId::MAIN,
            invocation.id(),
        )
        .unwrap();
    let registration = arena.registration().unwrap();
    ledger
        .prepare_source_boundary(
            registration,
            heap,
            &[],
            &[(cursor, RootValue::Value(&list))],
        )
        .unwrap()
        .admit(u64::MAX)
        .ok()
        .unwrap()
        .publish(&mut ledger, |_, edits| assert!(edits.is_empty()))
        .unwrap();
    let mut physical = vec![crate::machine::SlotState::Empty; exe.code(plan.code).slots.len()];
    physical[placement.physical(input).unwrap() as usize] = crate::machine::SlotState::Value(list);
    physical[placement.physical(total).unwrap() as usize] = crate::machine::SlotState::Value(zero);
    let read = source
        .first_event(&exe.stmt(statement).site.child(0))
        .unwrap();
    let start = Position {
        at: read.clone(),
        unpaid: Some(read),
        recovery: [0; 32],
        occurrence: Occurrence {
            task: TaskId::MAIN,
            invocation: invocation.id(),
            loops: vec![],
            action: None,
        },
    };
    drop(ledger);
    let mut frontier = source
        .frontier(
            &store.borrow(),
            heap,
            &invocation,
            program.link,
            lash_kernel_doc::KernelVersion::One,
            start.clone(),
        )
        .unwrap();
    let mut wrong = start.clone();
    wrong.unpaid = Some(
        source
            .first_event(&exe.stmt(plan.body.statements[0].id).site)
            .unwrap(),
    );
    let mut wrong = source
        .frontier(
            &store.borrow(),
            heap,
            &invocation,
            program.link,
            lash_kernel_doc::KernelVersion::One,
            wrong,
        )
        .unwrap();
    let mut ledger = store.borrow_mut();
    assert!(
        !entry
            .adopt_paid_input(
                &mut frontier,
                &invocation,
                &mut ledger,
                &mut arena,
                heap,
                &physical,
                list,
                None,
                u64::MAX
            )
            .unwrap(),
        "an unpaid iterable read cannot advance its Frontier"
    );
    assert_eq!(frontier.position(), &start);
    assert!(
        entry
            .bind(
                &frontier,
                &invocation,
                &ledger,
                &mut arena,
                heap,
                &physical,
                0,
                Some((list, 0)),
                Some(Value::Int(1)),
                None,
                u64::MAX
            )
            .unwrap()
            .is_none(),
        "an unpaid iterable read cannot admit its callback scope"
    );
    let (stream, stmt, _) = exe.statement(statement);
    let tree = crate::compile::expr::statement(stream, stmt.ops, false).unwrap();
    let [close] = &*tree.closes else {
        panic!("one original iterable close");
    };
    let units = u64::from(close.total);
    assert!(units > 0);
    assert!(
        PaidLoopInput::record(&exe, plan.code, statement, *close, 0, units - 1).is_none(),
        "an unpaid close cannot mint a payment"
    );
    let paid = PaidLoopInput::record(&exe, plan.code, statement, *close, 0, units).unwrap();
    assert!(
        !entry
            .adopt_paid_input(
                &mut frontier,
                &invocation,
                &mut ledger,
                &mut arena,
                heap,
                &physical,
                list,
                Some(&paid),
                units - 1
            )
            .unwrap(),
        "a future payment cannot authorize the iterable read"
    );
    assert_eq!(frontier.position(), &start);
    let before_wrong = wrong.position().clone();
    assert!(
        !entry
            .adopt_paid_input(
                &mut wrong,
                &invocation,
                &mut ledger,
                &mut arena,
                heap,
                &physical,
                list,
                Some(&paid),
                units
            )
            .unwrap(),
        "the paid iterable cannot skip another unpaid position"
    );
    assert_eq!(wrong.position(), &before_wrong);
    assert!(
        entry
            .adopt_paid_input(
                &mut frontier,
                &invocation,
                &mut ledger,
                &mut arena,
                heap,
                &physical,
                list,
                Some(&paid),
                units
            )
            .unwrap()
    );
    assert_eq!(
        frontier.position().unpaid.as_ref(),
        Some(&room::Point {
            site: exe.stmt(statement).site.clone(),
            phase: room::Phase::Test
        })
    );
    assert!(
        !entry
            .adopt_paid_input(
                &mut frontier,
                &invocation,
                &mut ledger,
                &mut arena,
                heap,
                &physical,
                list,
                Some(&paid),
                units
            )
            .unwrap(),
        "a consumed iterable prefix cannot advance twice"
    );
}
