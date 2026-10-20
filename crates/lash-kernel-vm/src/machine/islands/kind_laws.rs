//! PLAN-INDEP / K-CHG-002: a memo replays each call's formula and close,
//! in order, or leaves a crossing to the same general-path op.

use std::collections::BTreeMap;
use std::sync::Arc;

use lash_kernel_doc::arena::{ArenaHandle, ShortText};
use lash_kernel_doc::{Float, FunctionId, FunctionRegistry, TaskId, ValueKind};
use lash_kernel_exec::Weight;

use super::super::{ISLAND_DEPTH, Island, Scratch};
use super::*;
use crate::compile::{CompiledStep, Cx, Meter, Taken};
use crate::laws::embedder::{Embedder, Setup};
use crate::machine::registers::Arguments;
use crate::machine::sites::SiteCaches;
use crate::observe::ChargeTape;
use crate::{Charge, Layout, Machine, Step as MachineStep};

fn probes(
    registry: &mut FunctionRegistry,
    _: &BTreeMap<&'static str, FunctionId>,
) -> Vec<(&'static str, FunctionId)> {
    lash_kernel_lib::numbers()
        .into_iter()
        .filter_map(|(definition, native)| {
            let name = match definition.name.as_str() {
                "kind" => "kind",
                "same" => "same",
                _ => return None,
            };
            Some((name, registry.register(definition, Some(native)).unwrap()))
        })
        .collect()
}

fn machine() -> Embedder {
    Embedder::with(
        "fn tag(x) { return same(kind(x), \"record\") }\n\
         fn loop(x) { let r = false let i = 0 while num.lt(i, 2) { set r = same(kind(x), \"record\") let named = kind(x) set r = same(named, \"record\") set i = num.add(i, 1) } return r }\n\
         main { let r = call tag(null) let q = call loop(null) finish q }",
        Setup {
            probes: Some(probes),
            ..Setup::default()
        },
    )
}

/// The kind entry reads only the discriminant, including for arena-backed
/// representations. Deliberately nonresident handles catch accidental
/// measurements of the operand instead of the kind name.
fn values() -> Vec<Value> {
    let handle = ArenaHandle::new(999, 7);
    vec![
        Value::Null,
        Value::Absent,
        Value::Bool(false),
        Value::Bool(true),
        Value::Int(0),
        Value::Int(i64::MAX),
        Value::BigInt(handle),
        Value::Float(Float::new(f64::NAN)),
        Value::Text(ShortText::new("x").unwrap()),
        Value::LongText(handle),
        Value::Bytes(handle),
        Value::Timestamp(0),
        Value::BigTimestamp(handle),
        Value::Tuple(handle),
        Value::List(handle),
        Value::Map(handle),
        Value::Set(handle),
        Value::Record(handle),
        Value::Closure(handle),
        Value::Error(handle),
        Value::Task(TaskId::MAIN),
        Value::Function(ShortText::new("f").unwrap()),
        Value::LongFunction(handle),
        Value::Handle(handle),
        Value::TaskRef(TaskId::MAIN),
        Value::ObjectRef(handle),
    ]
}

type Ran = (Option<bool>, u64, Vec<Weight>, Option<u32>, String);

fn island_run(
    machine: &mut crate::KernelMachine,
    run: &dyn Fn(&mut Island<'_>, &mut Slots) -> Answer,
    x: Value,
    charging: bool,
    record: bool,
    bound: u64,
) -> Ran {
    let exe = Arc::clone(&machine.exe);
    let mut arena = machine.arena.borrow_mut();
    let mut sites = SiteCaches::default();
    let mut scratch = Scratch::default();
    let tape = ChargeTape::new();
    let mut island = Island {
        exe: &exe,
        stream: exe.document_stream(),
        arena: &mut arena,
        sites: &mut sites,
        session: &machine.session,
        tape: record.then_some(&tape),
        recorder: None,
        charging,
        charged: 0,
        bound,
        slice_end: u64::MAX,
        counters: &mut machine.tasks.get_mut(0).unwrap().unwrap().counters,
        observed: record,
        limits: Default::default(),
        memory: false,
        base: 0,
        call_depth: 200,
        depth: 1,
        frames: ISLAND_DEPTH,
        admitted: false,
        plain: false,
        swift: false,
        covered: false,
        scratch: &mut scratch,
    };
    let mut slots = [Some(x); MAX_SLOTS];
    let result = run(&mut island, &mut slots).ok().map(|value| {
        let Value::Bool(answer) = value else {
            panic!("kind test answers a boolean")
        };
        answer
    });
    (
        result,
        island.charged,
        Weight::expand(tape.steps()),
        island.scratch.exit.at,
        format!("{:?}", island.scratch.exit.regs),
    )
}

#[test]
fn kind_memo_replays_general_calls_for_every_kind_and_bound() {
    let mut embedder = machine();
    let exe = Arc::clone(&embedder.machine.exe);
    let [plan] = &exe.islands.plans[..] else {
        panic!("tag forms one island")
    };
    let StatementKind::Return { value, .. } = &plan.body.statements[0].kind else {
        panic!("tag returns its test")
    };
    let Kind::Operator { lib, args, .. } = &value.kind else {
        panic!("same operator")
    };
    let (inner, literal) = crate::compile::kind_test::test(&exe, *lib, args).unwrap();
    let kind = operator_site(inner, &exe).unwrap();
    let same = operator_site(value, &exe).unwrap();
    let compiled = expr(value, &mut Build::new(&exe, None)).unwrap();
    let general = |island: &mut Island<'_>, slots: &mut Slots| {
        let x = slots[0].unwrap();
        let name = island.operate(&kind, &[x])?;
        island.operate(&same, &[name, literal])
    };
    let memo = |island: &mut Island<'_>, slots: &mut Slots| compiled.eval(island, slots);
    let mut seen = std::collections::BTreeSet::new();
    for x in values() {
        seen.insert(x.kind());
        // First charged evaluation fills this kind; all subsequent
        // comparisons use that same node, including across charge modes.
        let expected = island_run(&mut embedder.machine, &general, x, true, true, u64::MAX);
        assert_eq!(expected.0, Some(x.kind() == ValueKind::Record));
        assert_eq!(
            island_run(&mut embedder.machine, &memo, x, true, true, u64::MAX),
            expected
        );
        for (charging, record) in [(true, false), (true, true), (false, false), (false, true)] {
            // Includes the cut after kind's formula, between the calls,
            // and every close/formula edge on either side of it.
            for bound in 0..=expected.1 + 1 {
                let general =
                    island_run(&mut embedder.machine, &general, x, charging, record, bound);
                let memo = island_run(&mut embedder.machine, &memo, x, charging, record, bound);
                assert_eq!(
                    memo, general,
                    "{x:?} charging={charging} tape={record} bound={bound}"
                );
            }
        }
    }
    assert_eq!(seen.len(), ValueKind::Ref as usize + 1);
}

#[test]
fn region_kind_memo_records_both_general_formulas_for_every_kind() {
    let embedder = machine();
    let exe = Arc::clone(&embedder.machine.exe);
    let [plan] = &exe.regions[..] else {
        panic!("loop forms one region")
    };
    let compiled = plan.compiled(&exe).unwrap();
    let CompiledStep::Write { value: node, .. } = &compiled.body.steps[0] else {
        panic!("first statement writes its kind test")
    };
    let CompiledStep::Bind {
        value: general_kind,
        at,
        ..
    } = &compiled.body.steps[1]
    else {
        panic!("general kind binding")
    };
    let CompiledStep::Write {
        value: general_same,
        ..
    } = &compiled.body.steps[2]
    else {
        panic!("general same write")
    };
    let mut arena = lash_kernel_state::arena::Arena::new();
    let mut sites = SiteCaches::default();
    let mut sessions = [];
    let mut cx = Cx {
        meter: Meter::new(true),
        charging: true,
        args: Arguments::new(),
        exe: &exe,
        arena: &mut arena,
        sites: &mut sites,
        sessions: &mut sessions,
        limits: Default::default(),
        memory: false,
        ceiling: u64::MAX,
        transient: 0,
        fault: None,
        lets: Vec::new(),
        declined: false,
        held: None,
        charged: 0,
        counters: None,
        guests: None,
        called: None,
        accumulator: None,
        keyed: None,
        absent: None,
    };
    for x in values() {
        let mut slots = vec![crate::machine::SlotState::Value(x); MAX_SLOTS];
        for (charging, record) in [(true, true), (true, false), (false, true), (false, false)] {
            cx.meter = Meter::new(record);
            cx.charging = charging;
            cx.meter.reset();
            slots.fill(crate::machine::SlotState::Value(x));
            let named = general_kind(&slots, &mut cx).unwrap();
            slots[*at as usize] = crate::machine::SlotState::Value(named);
            let expected = general_same(&slots, &mut cx).unwrap();
            slots.fill(crate::machine::SlotState::Value(x));
            let units = cx.meter.units;
            let formulas: Vec<_> = cx
                .meter
                .taken
                .iter()
                .map(|step| match step {
                    Taken::Formula(units) => *units,
                    Taken::Arm(_) => panic!("no arms"),
                })
                .collect();
            // Fill and replay the same node, with the two formulas kept
            // distinct for the statement's ordered charge program.
            for _ in 0..2 {
                cx.meter.reset();
                assert!(node(&slots, &mut cx).unwrap().same(&expected));
                assert_eq!(cx.meter.units, units, "{x:?} charging={charging}");
                let replay: Vec<_> = cx
                    .meter
                    .taken
                    .iter()
                    .map(|step| match step {
                        Taken::Formula(units) => *units,
                        Taken::Arm(_) => panic!("no arms"),
                    })
                    .collect();
                assert_eq!(replay, formulas, "{x:?} charging={charging}");
            }
        }
    }
}

fn observed(layout: Layout, charge: Charge, bound: u64) -> (String, Option<u64>, Vec<Weight>, u64) {
    let mut embedder = machine();
    let (mut program, _) = embedder.program(true);
    program.library = program.library.in_layout(layout);
    let setup = Setup::default();
    let mut run = crate::KernelMachine::start_with_census(
        program,
        crate::Bounds {
            charge,
            charge_limit: bound,
            ..setup.bounds
        },
        setup.start,
        layout,
        Box::new(embedder.source.clone()),
        crate::laws::embedder::census(),
    )
    .unwrap();
    let tape = ChargeTape::new();
    run.record_charges(tape.clone());
    let end = loop {
        match Machine::run(&mut run, &mut embedder.world, u64::MAX).unwrap() {
            MachineStep::Ended(end) => break end,
            MachineStep::Slice => (),
            MachineStep::Parked(_) => panic!("pure kind tests cannot park"),
        }
    };
    (
        format!("{end:?}"),
        run.meters().charged,
        Weight::expand(tape.steps()),
        tape.work().iterations,
    )
}

/// The meter's ordered formulas are committed only with the statement:
/// any bound inside the memo's two calls falls back to the reference,
/// including after earlier iterations have filled the same site's memo.
#[test]
fn region_kind_memo_stops_at_the_reference_op() {
    let natural = observed(Layout(0), Charge::Exact, u64::MAX);
    assert!(natural.3 > 0, "the kind test runs in loop regions");
    for charge in [Charge::Exact, Charge::Off] {
        for bound in 0..=natural.1.unwrap() + 1 {
            let on = observed(Layout(0), charge, bound);
            let off = observed(Layout(0).without_regions(), charge, bound);
            assert_eq!(
                (on.0, on.1, on.2),
                (off.0, off.1, off.2),
                "{charge:?} bound={bound}"
            );
        }
    }
}
