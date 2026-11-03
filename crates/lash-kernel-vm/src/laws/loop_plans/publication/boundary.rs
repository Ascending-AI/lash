//! The common affine exit consumes the source proof and actual staged storage.
use super::*;
use crate::compile::room::{
    self, charges::ChargeProgram, publication::*, region::ScalarRegionValues,
};
use lash_kernel_doc::{TaskId, Value};
use lash_kernel_state::{
    arena::Arena,
    kernel_internals::logical::{self, RootValue, Slot},
};
use std::sync::Arc;

fn resources<'a>(
    charged: &'a mut u64,
    counts: &'a mut crate::machine::tasks::Occurrences,
    bound: u64,
) -> crate::admission::SourceResources<'a> {
    crate::admission::SourceResources {
        charged,
        counters: counts,
        bound,
        slice: u64::MAX,
        depth: 1,
        limit: u64::MAX,
        safe_point: 0,
    }
}

pub(super) fn exits() {
    let text = crate::laws::layout::scalar_document("let x = 1 let y = 2 do 3 return y");
    let run = Embedder::new(&text);
    let (program, _) = run.program(true);
    let admitted = program
        .library
        .admit(&program.document, Default::default(), &program.selection)
        .unwrap_or_else(|_| panic!("affine source admitted"));
    let exe = admitted
        .executable(
            &program.document,
            &program.library,
            &program.documents,
            &program.functions,
            &program.instances,
        )
        .unwrap_or_else(|_| panic!("affine source executable"));
    let body = exe
        .islands
        .plans
        .iter()
        .find(|plan| plan.params.is_empty() && plan.body.statements.len() == 4)
        .unwrap();
    let source = &exe.code(body.code).source_room;
    let charge = |index: usize| {
        let statement = body.body.statements[index].id;
        ChargeProgram::compile(&exe, body.code, statement, exe.stmt(statement).ops).unwrap()
    };
    let charges = [Arc::new(charge(1)), Arc::new(charge(2))];
    let first = charges[0].first().clone();
    let through = room::Point {
        site: exe.stmt(body.body.statements[2].id).site.clone(),
        phase: room::Phase::Commit,
    };
    let route = [
        room::Point {
            site: source.site.clone(),
            phase: room::Phase::Enter,
        },
        source
            .first_event(&exe.stmt(body.body.statements[0].id).site)
            .unwrap(),
        room::Point {
            site: exe.stmt(body.body.statements[0].id).site.clone(),
            phase: room::Phase::Commit,
        },
        first.clone(),
        room::Point {
            site: exe.stmt(body.body.statements[1].id).site.clone(),
            phase: room::Phase::Commit,
        },
        charges[1].first().clone(),
        through.clone(),
    ]
    .map(|at| room::Selection::Event {
        at,
        exit: room::ExitKind::Normal,
    });
    let interval = source
        .compile_between(&first, &through, &route, 100)
        .unwrap();
    let region = source.compile_scalar_region(interval).unwrap();
    assert!(
        region
            .publication_plan(&exe, body.code, vec![Arc::clone(&charges[1])])
            .is_err(),
        "a suffix cannot omit an earlier original close"
    );
    assert!(
        region
            .publication_plan(&exe, body.code, charges.iter().rev().cloned().collect())
            .is_err(),
        "equal sums cannot reorder the original closes"
    );
    let plan = region
        .publication_plan(&exe, body.code, charges.to_vec())
        .unwrap();
    assert!(
        !plan.accepts_debt_for_law(charge(1).bind(&[]).unwrap()),
        "equal canonical totals from another program cannot replace the pinned close"
    );
    let heap = run.machine.source_law_heap();
    let store = heap.logical_ledger();
    let mut arena = Arena::new();
    let cell = arena.new_cell(Value::Int(11));
    let input_root = source
        .owner_slots()
        .unwrap()
        .root(
            &room::Owner::Binding {
                declared: exe.stmt(body.body.statements[0].id).site.clone(),
                name: lash_kernel_value::Name::new("x"),
            },
            TaskId::MAIN,
            0,
        )
        .unwrap();
    let logical::Owner::Value { slot, .. } = input_root else {
        panic!("source binding");
    };
    let invocation = {
        let mut ledger = store.borrow_mut();
        let (invocation, entered) = ledger
            .prepare_enter(
                TaskId::MAIN,
                &mut arena,
                heap.pages(),
                &[],
                &[
                    (slot, RootValue::Value(&Value::Int(1))),
                    (Slot::Capture(99), RootValue::Capture(cell)),
                ],
            )
            .unwrap();
        ledger
            .commit(entered.admit(u64::MAX).ok().unwrap())
            .unwrap();
        invocation
    };
    let logical::Owner::Value {
        slot: output_slot, ..
    } = source
        .owner_slots()
        .unwrap()
        .root(
            &room::Owner::Binding {
                declared: exe.stmt(body.body.statements[1].id).site.clone(),
                name: lash_kernel_value::Name::new("y"),
            },
            TaskId::MAIN,
            0,
        )
        .unwrap()
    else {
        panic!("source output binding");
    };
    let root = invocation.owner(output_slot);
    let occurrence = Occurrence {
        task: TaskId::MAIN,
        invocation: invocation.id(),
        loops: vec![],
        action: None,
    };
    let start = plan.start(occurrence.clone());
    let mut frontier = source
        .frontier(
            &store.borrow(),
            heap,
            &invocation,
            lash_kernel_value::content_ids::LinkId::from_bytes([71; 32]),
            lash_kernel_doc::KernelVersion::of(program.document.kernel()).unwrap(),
            start.clone(),
        )
        .unwrap();
    let foreign = source
        .frontier(
            &store.borrow(),
            heap,
            &invocation,
            lash_kernel_value::content_ids::LinkId::from_bytes([71; 32]),
            lash_kernel_doc::KernelVersion::of(program.document.kernel()).unwrap(),
            start.clone(),
        )
        .unwrap();
    let mut values = vec![Value::Null; region.fact_count()];
    for (_, fact) in region.entries() {
        values[fact.index()] = Value::Int(1);
    }
    for (at, fact) in region.results() {
        values[fact.index()] = Value::Int(
            if at
                .site
                .path
                .starts_with(&exe.stmt(body.body.statements[1].id).site.path)
            {
                2
            } else {
                3
            },
        );
    }
    let retained = arena.logical_local(&Value::Int(2)).unwrap();
    let bind = |arena: &mut Arena, next: Position| {
        let ledger = store.borrow();
        region
            .bind(
                &ledger,
                arena,
                heap,
                &invocation,
                ScalarRegionValues {
                    values: &values,
                    calls: &[],
                },
            )
            .unwrap()
            .unwrap()
            .bind_publication(
                &plan,
                &frontier,
                &ledger,
                arena,
                heap,
                &[vec![], vec![]],
                next,
            )
            .unwrap()
    };
    let next = plan.exit(occurrence);
    let mut wrong = next.clone();
    wrong.unpaid = Some(first.clone());
    assert!(
        bind(&mut arena, wrong).is_none(),
        "another unpaid source head is not this exit"
    );
    let before = store.borrow().balance().unwrap();
    let total = before.total().unwrap();
    let mut charged = 0;
    let mut counts = crate::machine::tasks::Occurrences::for_source_law(0);
    for (foreign_frontier, output, memory, charge_bound, extra_member) in [
        (true, 2, u64::MAX, u64::MAX, false),
        (false, 4, u64::MAX, u64::MAX, false),
        (false, 2, total + retained, u64::MAX, false),
        (false, 2, u64::MAX, 0, false),
        (false, 2, u64::MAX, u64::MAX, true),
    ] {
        let proof = bind(&mut arena, next.clone()).unwrap();
        let registration = arena.registration().unwrap();
        let ledger = store.borrow();
        let output = Value::Int(output);
        let mut transaction = ledger
            .prepare_source_boundary(
                registration,
                heap,
                &[],
                &[(root, RootValue::Value(&output))],
            )
            .unwrap();
        if extra_member {
            transaction = transaction
                .then_member(
                    &ledger,
                    heap.pages(),
                    cell,
                    logical::Member::Variable,
                    Some(Value::Int(12)),
                    &[],
                    &[],
                )
                .unwrap();
        }
        let selected = if foreign_frontier {
            &foreign
        } else {
            &frontier
        };
        assert!(
            proof
                .prepare_external(
                    selected,
                    &ledger,
                    heap,
                    transaction,
                    resources(&mut charged, &mut counts, charge_bound),
                    memory
                )
                .unwrap()
                .is_none(),
            "a foreign frontier, substituted value, missing interior room, refused debt or extra edit cannot publish: {foreign_frontier}/{output:?}/{memory}/{charge_bound}/{extra_member}"
        );
        assert_eq!(charged, 0);
        assert_eq!(ledger.balance().unwrap(), before);
        drop(ledger);
        assert!(
            arena.cell(cell).unwrap().same(&Value::Int(11)),
            "a rejected foreign edit cannot mutate its storage"
        );
    }
    let proof = bind(&mut arena, next.clone()).unwrap();
    drop(proof);
    assert_eq!(
        store.borrow().balance().unwrap(),
        before,
        "abandonment changes no owner"
    );
    let proof = bind(&mut arena, next.clone()).unwrap();
    let registration = arena.registration().unwrap();
    let ledger = store.borrow();
    let value = Value::Int(2);
    let transaction = ledger
        .prepare_source_boundary(registration, heap, &[], &[(root, RootValue::Value(&value))])
        .unwrap();
    let accepted = proof
        .prepare_external(
            &frontier,
            &ledger,
            heap,
            transaction,
            resources(&mut charged, &mut counts, u64::MAX),
            u64::MAX,
        )
        .unwrap()
        .unwrap();
    drop(ledger);
    let tape = crate::ChargeTape::new();
    let applied = std::cell::Cell::new(false);
    let balance = accepted
        .publish_boundary(
            &mut frontier,
            &mut store.borrow_mut(),
            |_, edits| {
                assert!(edits.is_empty());
                applied.set(true);
            },
            Some(&tape),
        )
        .unwrap();
    assert!(
        applied.get(),
        "actual semantic publication runs once under acceptance"
    );
    assert_eq!(frontier.position(), &next);
    let expected = crate::ChargeTape::new();
    for charge in &charges {
        charge.bind(&[]).unwrap().record(&expected);
    }
    assert_eq!(
        tape.steps(),
        expected.steps(),
        "every original close records exactly once"
    );
    assert_eq!(
        charged,
        charges
            .iter()
            .map(|charge| charge.bind(&[]).unwrap().units())
            .sum::<u64>()
    );
    assert_eq!(store.borrow().balance().unwrap(), balance);
    assert_eq!(balance.total().unwrap(), total + retained);
}
