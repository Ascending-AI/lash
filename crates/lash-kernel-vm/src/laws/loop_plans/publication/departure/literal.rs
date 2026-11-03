//! K-MACH-008: elided constant roots retain their real peak and canonical debt.
use super::*;
use crate::admission::SourceResources;
use lash_kernel_state::kernel_internals::logical::Holding;

fn resources<'a>(
    charged: &'a mut u64,
    counters: &'a mut crate::machine::tasks::Occurrences,
    bound: u64,
) -> SourceResources<'a> {
    SourceResources {
        charged,
        counters,
        bound,
        slice: u64::MAX,
        depth: 0,
        limit: 10,
        safe_point: 7,
    }
}

pub(super) fn returns() {
    let run = Embedder::new("main { return 7 }");
    let (program, _) = run.program(false);
    let admitted = program
        .library
        .admit(&program.document, Default::default(), &program.selection)
        .unwrap_or_else(|_| panic!("literal source admitted"));
    let exe = admitted
        .executable(
            &program.document,
            &program.library,
            &program.documents,
            &program.functions,
            &program.instances,
        )
        .unwrap_or_else(|_| panic!("literal source compiled"));
    let code = exe.main.unwrap();
    let source = &exe.code(code).source_room;
    let exit = source.literal_return_exit(&exe, code).unwrap();
    assert!(Arc::ptr_eq(
        &exit,
        &source.literal_return_exit(&exe, code).unwrap()
    ));
    let heap = run.machine.source_law_heap();
    let store = heap.logical_ledger();
    let mut arena = lash_kernel_state::arena::Arena::new();
    let mut ledger = store.borrow_mut();
    let (invocation, enter) = ledger
        .prepare_enter(TaskId::MAIN, &mut arena, heap.pages(), &[], &[])
        .unwrap();
    ledger.commit(enter.admit(u64::MAX).ok().unwrap()).unwrap();
    drop(ledger);
    let ledger = store.borrow();
    let before = ledger.balance().unwrap().total().unwrap();
    let bytes = Holding::from_native(&mut arena, heap.pages(), &Value::Int(7))
        .unwrap()
        .bytes();
    let start = exit.literal_start(Occurrence {
        task: TaskId::MAIN,
        invocation: invocation.id(),
        loops: vec![],
        action: None,
    });
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
    let mut charged = 7;
    let mut counters = crate::machine::tasks::Occurrences::for_source_law(0);
    let mut rejected = Vec::new();
    let foreign = Arc::new(room::Source::compile(
        lash_kernel_doc::constant_pool::PoolOwner::Document(program.document.identity()),
        source.site.clone(),
        &source.body,
        &[],
        Default::default(),
        &std::collections::BTreeMap::new(),
        &std::collections::BTreeSet::new(),
    ));
    for (actual, entry) in [(Value::Int(8), source), (Value::Int(7), &foreign)] {
        rejected.push(
            exit.prepare_literal(
                entry,
                &frontier,
                &invocation,
                &ledger,
                arena.registration().unwrap(),
                heap,
                actual,
                resources(&mut charged, &mut counters, u64::MAX),
                u64::MAX,
            )
            .unwrap()
            .is_none(),
        );
        assert_eq!(charged, 7);
        assert_eq!(ledger.balance().unwrap().total().unwrap(), before);
    }
    let mut wrong = start.clone();
    wrong.recovery = [0; 32];
    let wrong = source
        .frontier(
            &ledger,
            heap,
            &invocation,
            program.link,
            lash_kernel_doc::KernelVersion::One,
            wrong,
        )
        .unwrap();
    rejected.push(
        exit.prepare_literal(
            source,
            &wrong,
            &invocation,
            &ledger,
            arena.registration().unwrap(),
            heap,
            Value::Int(7),
            resources(&mut charged, &mut counters, u64::MAX),
            u64::MAX,
        )
        .unwrap()
        .is_none(),
    );
    for (charge_bound, memory_bound) in [(7, u64::MAX), (u64::MAX, before + bytes - 1)] {
        rejected.push(
            exit.prepare_literal(
                source,
                &frontier,
                &invocation,
                &ledger,
                arena.registration().unwrap(),
                heap,
                Value::Int(7),
                resources(&mut charged, &mut counters, charge_bound),
                memory_bound,
            )
            .unwrap()
            .is_none(),
        );
        assert_eq!(charged, 7);
        assert_eq!(frontier.position(), &start);
        assert_eq!(ledger.balance().unwrap().total().unwrap(), before);
    }
    assert_eq!(
        rejected, [true; 5],
        "literal source, value, recovery, charge and interior peak must authenticate"
    );
    {
        let accepted = exit
            .prepare_literal(
                source,
                &frontier,
                &invocation,
                &ledger,
                arena.registration().unwrap(),
                heap,
                Value::Int(7),
                resources(&mut charged, &mut counters, u64::MAX),
                before + bytes,
            )
            .unwrap()
            .unwrap();
        drop(accepted);
    }
    assert_eq!(charged, 7);
    assert_eq!(ledger.balance().unwrap().total().unwrap(), before);
    assert_eq!(frontier.position(), &start);
    let accepted = exit
        .prepare_literal(
            source,
            &frontier,
            &invocation,
            &ledger,
            arena.registration().unwrap(),
            heap,
            Value::Int(7),
            resources(&mut charged, &mut counters, u64::MAX),
            before + bytes,
        )
        .unwrap()
        .unwrap();
    drop(ledger);
    let mut ledger = store.borrow_mut();
    let stale = ledger
        .prepare_ownership(
            &mut lash_kernel_state::arena::Arena::new(),
            heap.pages(),
            &[],
            &[(
                Owner::Incoming(TaskId(99)),
                RootValue::Value(&Value::Int(1)),
            )],
        )
        .unwrap();
    ledger.commit(stale.admit(u64::MAX).ok().unwrap()).unwrap();
    assert!(
        accepted
            .publish_literal(&mut frontier, &mut ledger)
            .is_err()
    );
    assert_eq!(frontier.position(), &start);
    let stale = ledger
        .prepare_ownership(
            &mut arena,
            heap.pages(),
            &[Owner::Incoming(TaskId(99))],
            &[],
        )
        .unwrap();
    ledger.commit(stale.admit(u64::MAX).ok().unwrap()).unwrap();
    let id = *exe.block(exe.code(code).body).stmts.first().unwrap();
    let (_, stmt, _) = exe.statement(id);
    let charges = room::charges::ChargeProgram::compile(&exe, code, id, stmt.ops).unwrap();
    let debt = charges.bind(&[]).unwrap();
    let expected = crate::observe::ChargeTape::new();
    debt.record(&expected);
    let accepted = exit
        .prepare_literal(
            source,
            &frontier,
            &invocation,
            &ledger,
            arena.registration().unwrap(),
            heap,
            Value::Int(7),
            resources(&mut charged, &mut counters, u64::MAX),
            before + bytes,
        )
        .unwrap()
        .unwrap();
    let balance = accepted
        .publish_literal(&mut frontier, &mut ledger)
        .unwrap();
    assert_eq!(charged, 7 + debt.units());
    assert!(frontier.position().unpaid.is_none());
    assert_eq!(frontier.position().at.phase, Phase::Leave);
    assert!(
        ledger
            .invocation(TaskId::MAIN, invocation.id(), heap.pages())
            .is_err()
    );
    assert!(
        ledger
            .frame_owners(TaskId::MAIN, invocation.id(), heap.pages())
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        ledger
            .holding(Owner::Incoming(TaskId::MAIN), heap.pages())
            .unwrap()
            .unwrap()
            .bytes(),
        bytes
    );
    assert_eq!(
        balance.total().unwrap(),
        ledger.balance().unwrap().total().unwrap()
    );
    let actual = crate::observe::ChargeTape::new();
    exit.record_literal(&actual);
    assert_eq!(
        lash_kernel_exec::Weight::expand(actual.steps()),
        lash_kernel_exec::Weight::expand(expected.steps())
    );
}
