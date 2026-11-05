use super::code::{
    Call, Cost, Fee, Instruction, Op, ReadDependency, ReadSelector, Recovery, Register,
};
use super::fixtures::State;
use super::*;
use crate::compile::room::Phase;
use lash_kernel_doc::scalar::ScalarOp;
use lash_kernel_state::kernel_internals::logical::Slot;
use lash_kernel_value::sites::{Site, Unit};
use std::sync::atomic::{AtomicU64, Ordering};

// Canonical-shaped references belong only to the separate fixture graph.
fn object(index: u32) -> Value {
    Value::List(lash_kernel_doc::arena::ArenaHandle::new(index, 1))
}

fn point(index: u32) -> Point {
    Point {
        site: Site::new(Unit::Main, vec![index]),
        phase: Phase::Commit,
    }
}
fn map(index: u32, roles: &[(Slot, Register)]) -> Recovery {
    Recovery {
        roles: roles.into(),
        operands: Box::new([]),
        pending: None,
        incoming: false,
        continuation: point(index),
        counters: super::source_counters::CounterPlan::empty(),
    }
}
fn limits() -> Limits {
    Limits {
        charge: u64::MAX,
        memory: u64::MAX,
        depth: u32::MAX,
        slice: u64::MAX,
        entry_balance: 10,
        entry_charge: 0,
        entry_progress: 0,
        charge_enabled: true,
        entry_depth: 0,
    }
}
fn cost(charge: u64, delta: i128, peak: u64, depth: i32) -> Cost {
    Cost {
        fee: Fee::Fixed(charge),
        pace: u64::from(charge != 0),
        delta,
        peak,
        depth,
    }
}
fn instruction(op: Op, next: u32, prefix: u32, cost: Cost) -> Instruction {
    Instruction {
        op,
        next,
        prefix,
        cost,
    }
}
fn code(
    instructions: Vec<Instruction>,
    initial: Vec<(Register, Value)>,
    roles: &[(Slot, Register)],
    prefixes: u32,
) -> Code {
    Code {
        id: CodeId(0),
        entry: 0,
        entry_prefix: 0,
        instructions: instructions.into_boxed_slice(),
        recovery: (0..prefixes).map(|i| map(i, roles)).collect(),
        calls: Box::new([]),
        heap: Box::new([]),
        dependencies: Box::new([]),
        fields: Box::new([]),
        registers: 5,
        source_initial: Box::new([]),
        initial: initial.into_boxed_slice(),
        invocation: 1,
        safe_integers: false,
        semantic_operations: Arc::new(AtomicU64::new(0)),
    }
}
fn arithmetic() -> Arc<Code> {
    Arc::new(code(
        vec![
            instruction(
                Op::Int {
                    kernel: ScalarOp::Add,
                    lhs: 0,
                    rhs: 1,
                    dst: Register::Int(2),
                },
                1,
                1,
                cost(2, 2, 4, 0),
            ),
            instruction(
                Op::Int {
                    kernel: ScalarOp::Mul,
                    lhs: 2,
                    rhs: 1,
                    dst: Register::Int(0),
                },
                2,
                2,
                cost(3, -1, 6, 0),
            ),
            instruction(
                Op::Pack {
                    src: Register::Int(0),
                    dst: Register::Value(0),
                },
                3,
                3,
                cost(1, -1, 1, 0),
            ),
            instruction(Op::Exit, 3, 4, Cost::ZERO),
        ],
        vec![
            (Register::Int(0), Value::Int(3)),
            (Register::Int(1), Value::Int(5)),
            (Register::Int(2), Value::Int(0)),
            (Register::Value(0), Value::Null),
        ],
        &[
            (Slot::Binding(0), Register::Int(0)),
            (Slot::Binding(1), Register::Int(2)),
            (Slot::Binding(2), Register::Value(0)),
        ],
        5,
    ))
}
fn role(image: &Reconstruction, frame: usize, slot: u32) -> Value {
    image.frames[frame]
        .roles
        .iter()
        .find_map(|(role, value)| (*role == Slot::Binding(slot)).then_some(*value))
        .expect("source binding")
}

#[test]
fn exact_charge_and_memory_prefixes_recover_without_reexecuting() {
    // Independent source trace: (3 + 5), then 8 * 5, then bind 40. The
    // four legal post-prefix states include the zero-charge final exit.
    let oracle = [
        (0, 10, 10, 3, 0, Value::Null),
        (2, 12, 14, 3, 8, Value::Null),
        (5, 11, 18, 40, 8, Value::Null),
        (6, 10, 18, 40, 8, Value::Int(40)),
        (6, 10, 18, 40, 8, Value::Int(40)),
    ];
    for charge in 1..=7 {
        for memory in 10..=19 {
            let expected = oracle
                .iter()
                .take_while(|(fee, _, peak, ..)| *fee <= charge && *peak <= memory)
                .count()
                - 1;
            let code = arithmetic();
            let operations = Arc::clone(&code.semantic_operations);
            let mut state = State::new();
            let view = state.view();
            let mut lease = RegionLease::acquire_fixture(
                &mut state,
                code,
                Limits {
                    charge,
                    memory,
                    ..limits()
                },
            )
            .expect("entry");
            let stopped = lease.run(StopAt::NextBoundary).expect("execution");
            assert_eq!(
                stopped,
                if expected == 4 {
                    RegionStop::Complete
                } else {
                    RegionStop::Refusal
                },
                "limits {charge}/{memory}"
            );
            view.inspect(|image, revision, roots| {
                assert!(image.is_none());
                assert_eq!((revision, roots), (0, 0));
            });
            let evaluated = operations.load(Ordering::Relaxed);
            let materialized = lease.materialize().expect("recover reached prefix");
            let image = materialized.reconstruction();
            let (fee, balance, peak, first, sum, bound) = oracle[expected];
            assert_eq!(image.prefix.ordinal, expected as u64);
            assert_eq!(
                (
                    image.counters.charge,
                    image.counters.balance,
                    image.counters.peak
                ),
                (fee, balance, peak)
            );
            assert!(role(image, 0, 0).same(&Value::Int(first)));
            assert!(role(image, 0, 1).same(&Value::Int(sum)));
            assert!(role(image, 0, 2).same(&bound));
            if expected < 3 {
                assert!(image.pending.is_none());
                assert_eq!(image.attempted, Some(point(expected as u32 + 1)));
            }
            assert_eq!(
                operations.load(Ordering::Relaxed),
                evaluated,
                "recovery repeated a semantic operation"
            );
            let prepared = materialized.prepare_exit().expect("prepare");
            view.inspect(|image, revision, roots| {
                assert!(image.is_none());
                assert_eq!((revision, roots), (0, 1));
            });
            assert_eq!(prepared.commit().expect("commit"), stopped);
            view.inspect(|image, revision, roots| {
                let image = image.expect("installed canonical frame");
                assert_eq!((revision, roots, image.counters.charge), (1, 1, fee));
                assert!(role(image, 0, 0).same(&Value::Int(first)));
                assert_eq!(image.stop, stopped);
            });
            assert_eq!(
                operations.load(Ordering::Relaxed),
                evaluated,
                "publication repeated a semantic operation"
            );
        }
    }
}

#[test]
fn an_unaccepted_phase_retains_only_the_reached_prefix_pending_value() {
    let mut state = State::new();
    let mut lease = RegionLease::acquire_fixture(
        &mut state,
        arithmetic(),
        Limits {
            charge: 1,
            ..limits()
        },
    )
    .expect("entry");
    assert_eq!(
        lease.run(StopAt::NextBoundary).expect("source fee refusal"),
        RegionStop::Refusal
    );
    let materialized = lease.materialize().expect("entry prefix");
    let image = materialized.reconstruction();
    assert_eq!((image.prefix.ordinal, image.counters.charge), (0, 0));
    assert!(role(image, 0, 0).same(&Value::Int(3)));
    assert!(role(image, 0, 1).same(&Value::Int(0)));
    assert!(
        image.pending.is_none(),
        "a rejected candidate is not source-retained pending work"
    );
    drop(materialized);
    let mut code = Arc::try_unwrap(arithmetic()).ok().expect("unique fixture");
    code.instructions[0].cost.fee = Fee::Fixed(0);
    code.recovery[1].pending = Some(Register::Int(2));
    let census = Arc::clone(&code.semantic_operations);
    let mut lease = RegionLease::acquire_fixture(
        &mut state,
        Arc::new(code),
        Limits {
            charge: 0,
            ..limits()
        },
    )
    .expect("entry");
    assert_eq!(
        lease.run(StopAt::NextBoundary).expect("later fee refusal"),
        RegionStop::Refusal
    );
    let materialized = lease.materialize().expect("earlier accepted result");
    let image = materialized.reconstruction();
    assert_eq!((image.prefix.ordinal, image.counters.charge), (1, 0));
    assert!(
        image
            .pending
            .expect("source-retained result")
            .same(&Value::Int(8))
    );
    assert_eq!(image.attempted, Some(point(2)));
    assert_eq!(
        census.load(Ordering::Relaxed),
        1,
        "a refused later phase executed its kernel"
    );
}

#[test]
fn value_dependent_fees_use_each_actual_prefix_value() {
    for (initial, expected) in [(0, 4), (9, 7)] {
        let variable = Cost {
            fee: Fee::WordSize {
                value: Register::Int(0),
                zero: 2,
                nonzero: 5,
            },
            ..Cost::ZERO
        };
        let code = Arc::new(code(
            vec![
                instruction(Op::Meter, 1, 1, variable),
                instruction(
                    Op::Int {
                        kernel: ScalarOp::Sub,
                        lhs: 0,
                        rhs: 0,
                        dst: Register::Int(0),
                    },
                    2,
                    2,
                    Cost::ZERO,
                ),
                instruction(Op::Meter, 3, 3, variable),
                instruction(Op::Exit, 3, 4, Cost::ZERO),
            ],
            vec![(Register::Int(0), Value::Int(initial))],
            &[(Slot::Binding(0), Register::Int(0))],
            5,
        ));
        let mut state = State::new();
        let mut lease = RegionLease::acquire_fixture(&mut state, code, limits()).expect("entry");
        assert_eq!(
            lease.run(StopAt::NextBoundary).expect("source phases"),
            RegionStop::Complete
        );
        let materialized = lease.materialize().expect("recover");
        assert_eq!(materialized.reconstruction().counters.charge, expected);
        assert!(role(materialized.reconstruction(), 0, 0).same(&Value::Int(0)));
    }
}

fn read_loop(iterations: i64) -> Arc<Code> {
    let mut code = code(
        vec![
            instruction(
                Op::Read {
                    base: 0,
                    member: ReadSelector::List(0),
                    dst: Register::Int(4),
                },
                1,
                1,
                cost(1, 0, 0, 0),
            ),
            instruction(
                Op::Int {
                    kernel: ScalarOp::Add,
                    lhs: 3,
                    rhs: 4,
                    dst: Register::Int(3),
                },
                2,
                2,
                cost(2, 0, 0, 0),
            ),
            instruction(
                Op::Int {
                    kernel: ScalarOp::Add,
                    lhs: 0,
                    rhs: 2,
                    dst: Register::Int(0),
                },
                3,
                3,
                cost(1, 0, 0, 0),
            ),
            instruction(
                Op::Int {
                    kernel: ScalarOp::Lt,
                    lhs: 0,
                    rhs: 1,
                    dst: Register::Bool(0),
                },
                4,
                4,
                cost(1, 0, 0, 0),
            ),
            instruction(
                Op::Branch {
                    test: 0,
                    yes: 0,
                    no: 5,
                },
                0,
                5,
                cost(1, 0, 0, 0),
            ),
            instruction(
                Op::Pack {
                    src: Register::Int(3),
                    dst: Register::Value(1),
                },
                6,
                6,
                Cost::ZERO,
            ),
            instruction(
                Op::Return {
                    value: Register::Value(1),
                },
                6,
                7,
                Cost::ZERO,
            ),
        ],
        vec![
            (Register::Int(0), Value::Int(0)),
            (Register::Int(1), Value::Int(iterations)),
            (Register::Int(2), Value::Int(1)),
            (Register::Int(3), Value::Int(0)),
            (Register::Value(0), object(99)),
        ],
        &[
            (Slot::Binding(0), Register::Int(0)),
            (Slot::Binding(1), Register::Int(3)),
            (Slot::Binding(2), Register::Value(0)),
            (Slot::Binding(3), Register::Int(1)),
            (Slot::Binding(4), Register::Int(2)),
        ],
        8,
    );
    use super::source_counters::{CounterEdit, CounterPlan, CounterRead};
    code.source_initial = Box::new([17, 91]);
    for (prefix, counter) in [(1, 1), (5, 0)] {
        code.recovery[prefix].counters = CounterPlan::bind(
            Box::new([CounterEdit {
                index: counter,
                advance: 1,
            }]),
            Some(CounterRead::Issued(counter)),
            2,
        )
        .expect("verified source counter");
    }
    for prefix in [6, 7] {
        code.recovery[prefix].counters =
            CounterPlan::bind(Box::new([]), Some(CounterRead::Issued(0)), 2)
                .expect("same reached source occurrence");
    }
    code.recovery[1].pending = Some(Register::Int(4));
    code.recovery[4].operands = Box::new([Register::Bool(0)]);
    code.dependencies = Box::new([ReadDependency {
        identity: object(99),
        members: Box::new([Value::Int(7)]),
        fields: Box::new([]),
    }]);
    Arc::new(code)
}

fn field_loop(n: i64) -> Arc<Code> {
    let mut code = Arc::try_unwrap(read_loop(n))
        .ok()
        .expect("unique field fixture");
    let identity = Value::Record(lash_kernel_doc::arena::ArenaHandle::new(99, 1));
    code.initial.last_mut().expect("read base").1 = identity;
    code.dependencies[0].identity = identity;
    code.dependencies[0].fields = Box::new([Arc::from("field")]);
    code.fields = Box::new([Arc::from("field")]);
    for instruction in &mut code.instructions {
        if let Op::Read { member, .. } = &mut instruction.op {
            *member = ReadSelector::Field(0);
        }
    }
    Arc::new(code)
}

#[test]
fn stable_scalar_and_member_loops_publish_once_with_bounded_recovery() {
    entry::resumed_entries();
    for (n, field) in [1, 17, 4096]
        .into_iter()
        .flat_map(|n| [(n, false), (n, true)])
    {
        let code = if field { field_loop(n) } else { read_loop(n) };
        let census = Arc::clone(&code.semantic_operations);
        let mut state = State::new();
        let view = state.view();
        let mut lease = RegionLease::acquire_fixture(&mut state, code, limits()).expect("entry");
        let frame_capacity = lease.execution.fixture().frames.capacity();
        assert_eq!(
            lease.run(StopAt::NextBoundary).expect("loop"),
            RegionStop::Complete
        );
        assert_eq!(lease.execution.fixture().frames.capacity(), frame_capacity);
        assert_eq!(lease.execution.fixture().frames.len(), 1);
        view.inspect(|image, revision, roots| {
            assert!(image.is_none());
            assert_eq!((revision, roots), (0, 0));
        });
        let materialized = lease.materialize().expect("bounded recovery");
        let image = materialized.reconstruction();
        assert!(image.payload.expect("return").same(&Value::Int(7 * n)));
        assert!(role(image, 0, 0).same(&Value::Int(n)));
        assert_eq!(
            (
                image.counters.charge,
                image.counters.debt,
                image.counters.balance,
                image.counters.peak
            ),
            (6 * n as u64, 5 * n as u64 + 2, 10, 10)
        );
        assert_eq!(&*image.source_counts, &[17 + n as u64, 91 + n as u64]);
        assert_eq!(image.prefix.occurrence, Some(17 + n as u64 - 1));
        assert_eq!(image.prefix.ordinal, 5 * n as u64 + 2);
        assert_eq!(image.frames[0].roles.len(), 5);
        assert_eq!(census.load(Ordering::Relaxed), 4 * n as u64);
        materialized
            .prepare_exit()
            .expect("one exit")
            .commit()
            .expect("publish");
        view.inspect(|image, revision, roots| {
            assert_eq!((revision, roots), (1, 1));
            assert!(
                image
                    .expect("result")
                    .payload
                    .expect("return")
                    .same(&Value::Int(7 * n))
            );
        });
        assert_eq!(census.load(Ordering::Relaxed), 4 * n as u64);
    }
}

fn recursive(n: i64) -> Arc<Code> {
    let mut code = code(
        vec![
            instruction(Op::Call { site: 0 }, 1, 1, cost(2, 8, 8, 1)),
            instruction(
                Op::Return {
                    value: Register::Int(0),
                },
                1,
                2,
                Cost::ZERO,
            ),
            instruction(
                Op::Int {
                    kernel: ScalarOp::Le,
                    lhs: 0,
                    rhs: 2,
                    dst: Register::Bool(0),
                },
                3,
                3,
                cost(1, 0, 0, 0),
            ),
            instruction(
                Op::Branch {
                    test: 0,
                    yes: 7,
                    no: 4,
                },
                0,
                4,
                cost(1, 0, 0, 0),
            ),
            instruction(
                Op::Int {
                    kernel: ScalarOp::Sub,
                    lhs: 0,
                    rhs: 1,
                    dst: Register::Int(0),
                },
                5,
                5,
                cost(1, 0, 0, 0),
            ),
            instruction(Op::Call { site: 1 }, 6, 1, cost(2, 8, 8, 1)),
            instruction(
                Op::Int {
                    kernel: ScalarOp::Add,
                    lhs: 0,
                    rhs: 1,
                    dst: Register::Int(0),
                },
                7,
                6,
                cost(1, 0, 0, 0),
            ),
            instruction(
                Op::Return {
                    value: Register::Int(0),
                },
                7,
                7,
                Cost::ZERO,
            ),
        ],
        vec![
            (Register::Int(0), Value::Int(n)),
            (Register::Int(1), Value::Int(1)),
            (Register::Int(2), Value::Int(0)),
        ],
        &[(Slot::Binding(0), Register::Int(0))],
        16,
    );
    for index in [10, 11, 12, 13] {
        code.recovery[index].incoming = true;
    }
    let plan = |restore_prefix, leave_prefix, completion_prefix, before_prefix| Call {
        target: CodeId(0),
        before: Cost::ZERO,
        before_prefix,
        entry: 2,
        prefix: 8,
        arguments: Box::new([
            (Register::Int(0), Register::Int(0)),
            (Register::Int(1), Register::Int(1)),
            (Register::Int(2), Register::Int(2)),
        ]),
        result: Register::Int(0),
        destination: Register::Int(0),
        leave: cost(0, -8, 0, -1),
        leave_prefix,
        completion: cost(3, 0, 0, 0),
        completion_prefix,
        restore: Cost::ZERO,
        restore_prefix,
    };
    code.calls = Box::new([plan(7, 11, 10, 14), plan(9, 13, 12, 15)]);
    Arc::new(code)
}

#[test]
fn affine_calls_use_the_executor_stack_and_preserve_exact_depth_prefixes() {
    calls::reserved_invocations();
    for (n, depth, entry_depth, memory) in [
        (8, 8, 0, u64::MAX),
        (8, 9, 0, u64::MAX),
        (8, 10, 0, u64::MAX),
        (1024, 1025, 0, u64::MAX),
        (2, 4, 3, u64::MAX),
        (8, u32::MAX, 0, 81),
        (8, u32::MAX, 0, 82),
        (8, u32::MAX, 0, 83),
        (0, u32::MAX, 0, 17),
        (0, u32::MAX, 0, 18),
        (0, u32::MAX, 0, 19),
    ] {
        let mut state = State::new();
        let mut lease = RegionLease::acquire_fixture(
            &mut state,
            recursive(n),
            Limits {
                depth,
                entry_depth,
                memory,
                ..limits()
            },
        )
        .expect("entry");
        let stopped = lease
            .run(StopAt::NextBoundary)
            .expect("recursive execution");
        let materialized = lease.materialize().expect("active-frame recovery");
        let image = materialized.reconstruction();
        let allowed = (u64::from(depth - entry_depth).min((memory - 10) / 8)) as u32;
        if allowed <= n as u32 {
            assert_eq!(stopped, RegionStop::Refusal);
            assert_eq!(
                (
                    image.counters.charge,
                    image.counters.depth,
                    image.counters.balance
                ),
                (
                    5 * u64::from(allowed),
                    entry_depth + allowed,
                    10 + 8 * u64::from(allowed)
                )
            );
            assert_eq!(image.frames.len(), allowed as usize + 1);
            assert!(role(image, allowed as usize, 0).same(&Value::Int(n - i64::from(allowed))));
            let invocations: Vec<_> = image.frames.iter().map(|frame| frame.invocation).collect();
            assert_eq!(
                invocations,
                (1..=u64::from(allowed) + 1).collect::<Vec<_>>()
            );
        } else {
            assert_eq!(stopped, RegionStop::Complete);
            assert!(image.payload.expect("return").same(&Value::Int(n)));
            assert_eq!(
                (
                    image.counters.charge,
                    image.counters.depth,
                    image.counters.balance,
                    image.counters.peak
                ),
                (9 * n as u64 + 7, entry_depth, 10, 10 + 8 * (n as u64 + 1))
            );
            assert_eq!(image.frames.len(), 1);
        }
        materialized
            .prepare_exit()
            .expect("prepare")
            .commit()
            .expect("commit");
    }
}

#[test]
fn foreign_and_early_call_leaves_publish_nothing() {
    let mut a = State::new();
    let mut b = State::new();
    let av = a.view();
    let bv = b.view();
    let mut first = RegionLease::acquire_fixture(&mut a, recursive(0), limits()).expect("first");
    let mut second = RegionLease::acquire_fixture(&mut b, recursive(0), limits()).expect("second");
    let foreign = first.enter_call(CallId::fixture(0)).expect("enter first");
    let early = second.enter_call(CallId::fixture(0)).expect("enter second");
    for lease in [&mut first, &mut second] {
        assert_eq!(
            lease.run(StopAt::NextLegalPrefix).expect("compare"),
            RegionStop::Handoff
        );
        assert_eq!(
            lease
                .run(StopAt::NextLegalPrefix)
                .expect("branch to return"),
            RegionStop::Handoff
        );
    }
    assert!(second.leave_call(foreign).is_err());
    assert!(first.leave_call(early).is_err());
    assert!(first.materialize().is_err());
    assert!(second.materialize().is_err());
    for view in [av, bv] {
        view.inspect(|image, revision, roots| {
            assert!(image.is_none());
            assert_eq!((revision, roots), (0, 0));
        });
    }
    let mut state = State::new();
    let view = state.view();
    let mut lease =
        RegionLease::acquire_fixture(&mut state, recursive(0), limits()).expect("entry");
    let early = lease.enter_call(CallId::fixture(0)).expect("enter");
    assert!(
        lease.leave_call(early).is_err(),
        "an argument is not a verified returned result"
    );
    assert!(lease.materialize().is_err());
    view.inspect(|image, revision, roots| {
        assert!(image.is_none());
        assert_eq!((revision, roots), (0, 0));
    });
    let mut state = State::new();
    let view = state.view();
    let mut lease = RegionLease::acquire_fixture_programs(
        &mut state,
        multicode_recursive(0),
        CodeId(0),
        limits(),
    )
    .expect("multi-code entry");
    let mut activation = lease.enter_call(CallId::fixture(0)).expect("actual caller");
    for _ in 0..2 {
        assert_eq!(
            lease
                .run(StopAt::NextLegalPrefix)
                .expect("callee return path"),
            RegionStop::Handoff
        );
    }
    activation.caller_program = CodeId(1);
    assert!(
        lease.leave_call(activation).is_err(),
        "another valid code table is not this invocation's caller"
    );
    assert!(lease.materialize().is_err());
    view.inspect(|image, version, roots| {
        assert!(image.is_none());
        assert_eq!((version, roots), (0, 0));
    });

    let mut state = State::new();
    let view = state.view();
    let mut lease =
        RegionLease::acquire_fixture(&mut state, recursive(0), limits()).expect("entry");
    let mut activation = lease
        .enter_call(CallId::fixture(0))
        .expect("first source edge");
    for _ in 0..2 {
        assert_eq!(
            lease
                .run(StopAt::NextLegalPrefix)
                .expect("callee return path"),
            RegionStop::Handoff
        );
    }
    activation.plan = 1;
    assert!(
        lease.leave_call(activation).is_err(),
        "another valid call site cannot replace the entered source edge"
    );
    assert!(lease.materialize().is_err());
    view.inspect(|image, version, roots| {
        assert!(image.is_none());
        assert_eq!((version, roots), (0, 0));
    });
}

#[test]
fn failed_or_abandoned_exits_leave_canonical_state_unchanged() {
    for code in [arithmetic(), graph::local_edit(), graph::fresh_list()] {
        for failure in 0..5 {
            let mut state = State::new();
            if failure == 0 {
                state.fail_prepare();
            }
            if failure == 1 {
                state.fail_commit();
            }
            let view = state.view();
            let mut lease = RegionLease::acquire_fixture(&mut state, Arc::clone(&code), limits())
                .expect("entry");
            assert!(
                view.publish_alias().is_err(),
                "lease exclusion must cover aliases"
            );
            lease.run(StopAt::NextBoundary).expect("execute");
            let materialized = lease.materialize().expect("materialize");
            if failure == 2 {
                view.corrupt_dependency();
            }
            let prepared = materialized.prepare_exit();
            if failure == 0 || failure == 2 {
                assert!(prepared.is_err());
            } else {
                let prepared = prepared.expect("prepare");
                if failure == 3 {
                    view.corrupt_dependency();
                }
                if failure == 4 {
                    drop(prepared);
                } else {
                    assert!(prepared.commit().is_err());
                }
            }
            view.inspect(|image, revision, _| {
                assert!(image.is_none());
                assert_eq!(revision, 0);
            });
            view.inspect_graph(|graph| assert!(graph.is_none()));
            view.publish_alias()
                .expect("dropping authority releases exclusion");
        }
    }

    for reserve_failure in [true, false] {
        let mut state = State::new();
        let view = state.view();
        let mut lease =
            RegionLease::acquire_fixture(&mut state, graph::fresh_list(), limits()).expect("entry");
        if reserve_failure {
            lease.execution.fixture_mut().view.fail_reserve = true;
        } else {
            lease.execution.fixture_mut().view.fail_freeze = true;
        }
        let run = lease.run(StopAt::NextBoundary);
        if reserve_failure {
            assert!(run.is_err());
        } else {
            assert_eq!(run.expect("private allocation"), RegionStop::Complete);
        }
        assert!(lease.materialize().is_err());
        view.inspect(|image, revision, roots| {
            assert!(image.is_none());
            assert_eq!((revision, roots), (0, 0));
        });
        view.inspect_graph(|graph| assert!(graph.is_none()));
        view.publish_alias()
            .expect("failed private work released exclusion");
    }

    let mut first = State::new();
    let mut second = State::new();
    let views = [first.view(), second.view()];
    let mut a = RegionLease::acquire_fixture(&mut first, graph::fresh_list(), limits())
        .expect("first lease");
    let mut b = RegionLease::acquire_fixture(&mut second, graph::fresh_list(), limits())
        .expect("second lease");
    a.run(StopAt::NextBoundary).expect("first graph");
    b.run(StopAt::NextBoundary).expect("second graph");
    let mut a = a.materialize().expect("first packet");
    let mut b = b.materialize().expect("second packet");
    std::mem::swap(&mut a.graph, &mut b.graph);
    assert!(
        a.prepare_exit().is_err(),
        "equal-shaped foreign graph packet cannot publish"
    );
    assert!(b.prepare_exit().is_err());
    for view in views {
        view.inspect(|image, revision, roots| {
            assert!(image.is_none());
            assert_eq!((revision, roots), (0, 0));
        });
        view.inspect_graph(|graph| assert!(graph.is_none()));
    }
}

#[test]
fn slices_and_explicit_boundaries_keep_the_actual_operands_and_unpaid_result() {
    let mut state = State::new();
    let mut lease = RegionLease::acquire_fixture(
        &mut state,
        arithmetic(),
        Limits {
            slice: 2,
            ..limits()
        },
    )
    .expect("entry");
    assert_eq!(
        lease.run(StopAt::NextBoundary).expect("slice"),
        RegionStop::Slice
    );
    let materialized = lease.materialize().expect("recover");
    assert_eq!(
        (
            materialized.reconstruction().counters.charge,
            materialized.reconstruction().prefix.prefix
        ),
        (2, 1)
    );
    assert!(role(materialized.reconstruction(), 0, 1).same(&Value::Int(8)));
    materialized
        .prepare_exit()
        .expect("prepare")
        .commit()
        .expect("commit");
    let mut code = Arc::try_unwrap(arithmetic()).ok().expect("unique fixture");
    code.instructions[1].op = Op::Boundary;
    let mut state = State::new();
    let mut lease =
        RegionLease::acquire_fixture(&mut state, Arc::new(code), limits()).expect("entry");
    assert_eq!(
        lease.run(StopAt::NextLegalPrefix).expect("prefix"),
        RegionStop::Handoff
    );
    assert_eq!(
        lease.run(StopAt::NextBoundary).expect("boundary"),
        RegionStop::Boundary
    );
    let image = lease.materialize().expect("recover");
    assert_eq!(image.reconstruction().counters.charge, 2);
    assert!(role(image.reconstruction(), 0, 0).same(&Value::Int(3)));
}

#[test]
fn overflow_number_policy_and_read_identity_are_checked_without_publication() {
    for (kernel, lhs, rhs, expected) in [
        (
            ScalarOp::Add,
            0.5,
            0.25,
            Value::Float(lash_kernel_doc::Float::new(0.75)),
        ),
        (ScalarOp::Lt, 0.5, 0.75, Value::Bool(true)),
        (
            ScalarOp::Mul,
            f64::MAX,
            2.0,
            Value::Float(lash_kernel_doc::Float::new(f64::INFINITY)),
        ),
    ] {
        for cap in [99, 100, 101] {
            let destination = if kernel.compares() {
                Register::Bool(0)
            } else {
                Register::Float(2)
            };
            let mut code = code(
                vec![
                    instruction(
                        Op::Float {
                            kernel,
                            lhs: 0,
                            rhs: 1,
                            dst: destination,
                        },
                        1,
                        1,
                        cost(100, 0, 0, 0),
                    ),
                    instruction(Op::Return { value: destination }, 1, 2, Cost::ZERO),
                ],
                vec![
                    (
                        Register::Float(0),
                        Value::Float(lash_kernel_doc::Float::new(lhs)),
                    ),
                    (
                        Register::Float(1),
                        Value::Float(lash_kernel_doc::Float::new(rhs)),
                    ),
                ],
                &[(Slot::Binding(0), Register::Float(0))],
                3,
            );
            for prefix in [1, 2] {
                code.recovery[prefix].roles = Box::new([(Slot::Binding(0), destination)]);
            }
            let operations = Arc::clone(&code.semantic_operations);
            let mut state = State::new();
            let mut lease = RegionLease::acquire_fixture(
                &mut state,
                Arc::new(code),
                Limits {
                    charge: cap,
                    ..limits()
                },
            )
            .expect("typed float entry");
            let stop = lease
                .run(StopAt::NextBoundary)
                .expect("one canonical float execution");
            let prefix = lease
                .materialize()
                .expect("pack only the current float bank");
            let image = prefix.reconstruction();
            if cap < 100 {
                assert_eq!(stop, RegionStop::Refusal);
                assert_eq!(image.counters.charge, 0);
                assert!(role(image, 0, 0).same(&Value::Float(lash_kernel_doc::Float::new(lhs))));
                assert_eq!(operations.load(Ordering::Relaxed), 0);
            } else {
                assert_eq!(stop, RegionStop::Complete);
                assert_eq!(image.counters.charge, 100);
                assert!(role(image, 0, 0).same(&expected));
                assert!(image.payload.expect("typed answer").same(&expected));
                assert_eq!(operations.load(Ordering::Relaxed), 1);
            }
        }
    }

    for (left, right, safe) in [(i64::MAX, 1, false), (9_007_199_254_740_991, 1, true)] {
        let mut code = Arc::try_unwrap(arithmetic()).ok().expect("unique fixture");
        code.initial[0].1 = Value::Int(left);
        code.initial[1].1 = Value::Int(right);
        code.safe_integers = safe;
        let mut state = State::new();
        let view = state.view();
        let mut lease =
            RegionLease::acquire_fixture(&mut state, Arc::new(code), limits()).expect("entry");
        assert_eq!(
            lease
                .run(StopAt::NextBoundary)
                .expect("checked overflow boundary"),
            RegionStop::Boundary
        );
        let materialized = lease.materialize().expect("recover inputs");
        assert_eq!(materialized.reconstruction().counters.charge, 0);
        assert!(role(materialized.reconstruction(), 0, 0).same(&Value::Int(left)));
        view.inspect(|image, revision, roots| {
            assert!(image.is_none());
            assert_eq!((revision, roots), (0, 0));
        });
    }
    {
        let mut state = State::new();
        let view = state.view();
        let mut lease =
            RegionLease::acquire_fixture(&mut state, field_loop(1), limits()).expect("entry");
        let Executing::Fixture(execution) = &mut lease.execution else {
            unreachable!()
        };
        execution.view.boundary_reads = true;
        assert_eq!(
            lease.run(StopAt::NextBoundary).expect("authority miss"),
            RegionStop::Boundary
        );
        let prefix = lease
            .materialize()
            .expect("preceding prefix remains recoverable");
        let image = prefix.reconstruction();
        assert_eq!((image.prefix.ordinal, image.counters.charge), (0, 0));
        assert_eq!(image.source_counts.as_ref(), &[17, 91]);
        assert!(image.pending.is_none());
        view.inspect(|image, revision, roots| {
            assert!(image.is_none());
            assert_eq!((revision, roots), (0, 0));
        });
    }
    for initial in [u64::MAX - 1, u64::MAX] {
        let mut code = Arc::try_unwrap(read_loop(2)).ok().expect("unique fixture");
        code.source_initial[0] = initial;
        let mut state = State::new();
        let view = state.view();
        let mut lease =
            RegionLease::acquire_fixture(&mut state, Arc::new(code), limits()).expect("entry");
        assert!(
            lease.run(StopAt::NextBoundary).is_err(),
            "source occurrence overflow must not wrap"
        );
        assert!(lease.materialize().is_err());
        view.inspect(|image, version, roots| {
            assert!(image.is_none());
            assert_eq!((version, roots), (0, 0));
        });
    }
    let mut code = Arc::try_unwrap(read_loop(1)).ok().expect("unique fixture");
    code.initial.last_mut().expect("actual read base").1 = object(100);
    let mut state = State::new();
    let view = state.view();
    let mut lease =
        RegionLease::acquire_fixture(&mut state, Arc::new(code), limits()).expect("entry");
    assert!(
        lease.run(StopAt::NextBoundary).is_err(),
        "equal price cannot substitute a read identity"
    );
    assert!(lease.materialize().is_err());
    view.inspect(|image, revision, roots| {
        assert!(image.is_none());
        assert_eq!((revision, roots), (0, 0));
    });
}

#[test]
fn accepted_frame_leave_survives_a_later_completion_fee_refusal() {
    for bound in [3, 4, 6, 7, 8] {
        let mut state = State::new();
        let view = state.view();
        let mut lease = RegionLease::acquire_fixture(
            &mut state,
            recursive(0),
            Limits {
                charge: bound,
                ..limits()
            },
        )
        .expect("entry");
        let stop = lease
            .run(StopAt::NextBoundary)
            .expect("ordered call phases");
        let materialized = lease.materialize().expect("current call phase");
        let image = materialized.reconstruction();
        if bound == 3 {
            assert_eq!(stop, RegionStop::Refusal);
            assert_eq!(
                (
                    image.counters.charge,
                    image.counters.balance,
                    image.counters.depth
                ),
                (3, 18, 1)
            );
            assert_eq!(image.frames.len(), 2);
            assert!(image.pending.is_none());
        } else if bound < 7 {
            assert_eq!(stop, RegionStop::Refusal);
            assert_eq!(
                (
                    image.counters.charge,
                    image.counters.balance,
                    image.counters.depth
                ),
                (4, 10, 0)
            );
            assert_eq!(image.frames.len(), 1);
            assert!(
                image
                    .pending
                    .expect("returned value survives Leave")
                    .same(&Value::Int(0))
            );
            assert_eq!(image.attempted, Some(point(10)));
        } else {
            assert_eq!(stop, RegionStop::Complete);
            assert_eq!(
                (
                    image.counters.charge,
                    image.counters.balance,
                    image.counters.depth
                ),
                (7, 10, 0)
            );
            assert!(image.pending.is_none());
            assert!(image.payload.expect("caller return").same(&Value::Int(0)));
        }
        view.inspect(|image, version, roots| {
            assert!(image.is_none());
            assert_eq!((version, roots), (0, 0));
        });
        materialized
            .prepare_exit()
            .expect("prepare exact prefix")
            .commit()
            .expect("commit exact prefix");
        view.inspect(|image, version, roots| {
            assert_eq!((version, roots), (1, 1));
            let image = image.expect("published prefix");
            assert_eq!(image.counters.depth, if bound == 3 { 1 } else { 0 });
        });
    }
}

#[test]
fn callee_body_return_charge_is_paid_before_frame_leave() {
    for bound in [4, 5, 6, 8, 9, 10] {
        let mut code = Arc::try_unwrap(recursive(0)).ok().expect("unique fixture");
        code.instructions[7].cost = cost(2, 0, 0, 0);
        let mut state = State::new();
        let mut lease = RegionLease::acquire_fixture(
            &mut state,
            Arc::new(code),
            Limits {
                charge: bound,
                ..limits()
            },
        )
        .expect("entry");
        let stop = lease.run(StopAt::NextBoundary).expect("body return close");
        let materialized = lease.materialize().expect("current prefix");
        let image = materialized.reconstruction();
        if bound < 6 {
            assert_eq!(stop, RegionStop::Refusal);
            assert_eq!(
                (
                    image.counters.charge,
                    image.counters.balance,
                    image.counters.depth
                ),
                (4, 18, 1)
            );
            assert_eq!(image.attempted, Some(point(7)));
        } else if bound < 9 {
            assert_eq!(stop, RegionStop::Refusal);
            assert_eq!(
                (
                    image.counters.charge,
                    image.counters.balance,
                    image.counters.depth
                ),
                (6, 10, 0)
            );
            assert!(image.pending.expect("returned result").same(&Value::Int(0)));
        } else {
            assert_eq!(stop, RegionStop::Complete);
            assert_eq!(
                (
                    image.counters.charge,
                    image.counters.balance,
                    image.counters.depth
                ),
                (9, 10, 0)
            );
        }
    }
}

#[test]
fn caller_argument_debt_precedes_a_refused_frame_entry() {
    for (charge, depth) in [(4, 0), (5, 0), (6, 0), (5, 1), (7, 1)] {
        let mut code = Arc::try_unwrap(recursive(0)).ok().expect("unique fixture");
        code.calls[0].before = cost(5, 0, 0, 0);
        let mut state = State::new();
        let mut lease = RegionLease::acquire_fixture(
            &mut state,
            Arc::new(code),
            Limits {
                charge,
                depth,
                ..limits()
            },
        )
        .expect("entry");
        assert_eq!(
            lease
                .run(StopAt::NextBoundary)
                .expect("ordered entry phases"),
            RegionStop::Refusal
        );
        let prefix = lease.materialize().expect("exact caller debt prefix");
        let image = prefix.reconstruction();
        if charge < 5 {
            assert_eq!(
                (
                    image.counters.charge,
                    image.counters.depth,
                    image.counters.balance
                ),
                (0, 0, 10)
            );
            assert_eq!(image.attempted, Some(point(14)));
            assert_eq!(image.entering, None);
        } else if depth == 0 || charge < 7 {
            assert_eq!(
                (
                    image.counters.charge,
                    image.counters.depth,
                    image.counters.balance
                ),
                (5, 0, 10)
            );
            assert_eq!(image.attempted, Some(point(1)));
            assert_eq!(image.entering, Some(0));
        } else {
            assert_eq!(
                (
                    image.counters.charge,
                    image.counters.depth,
                    image.counters.balance
                ),
                (7, 1, 18)
            );
            assert_eq!(image.attempted, Some(point(3)));
            assert_eq!(image.entering, None);
        }
    }
}

#[test]
fn charge_off_slices_use_source_pace_and_atomic_fees_keep_the_slice() {
    for charging in [false, true] {
        let code = Arc::new(code(
            vec![
                instruction(Op::Meter, 1, 1, cost(1000, 0, 0, 0)),
                instruction(Op::Meter, 2, 2, cost(1000, 0, 0, 0)),
                instruction(Op::Meter, 3, 3, cost(1000, 0, 0, 0)),
                instruction(Op::Exit, 3, 4, Cost::ZERO),
            ],
            vec![],
            &[],
            5,
        ));
        let mut state = State::new();
        let mut lease = RegionLease::acquire_fixture(
            &mut state,
            code,
            Limits {
                slice: 2,
                charge_enabled: charging,
                ..limits()
            },
        )
        .expect("entry");
        assert_eq!(
            lease.run(StopAt::NextBoundary).expect("mandatory slice"),
            RegionStop::Slice
        );
        let prefix = lease.materialize().expect("actual reached prefix");
        let image = prefix.reconstruction();
        assert_eq!(
            (
                image.prefix.ordinal,
                image.counters.charge,
                image.counters.progress
            ),
            if charging { (1, 1000, 1000) } else { (2, 0, 2) }
        );
        assert_eq!(image.frames[0].pc, if charging { 1 } else { 2 });
    }
}

fn multicode_recursive(n: i64) -> Arc<programs::Programs> {
    let mut root = Arc::try_unwrap(recursive(n)).ok().expect("unique fixture");
    let mut body = code(vec![], vec![], &[(Slot::Binding(9), Register::Int(0))], 16);
    body.id = CodeId(1);
    let mut instructions = std::mem::take(&mut root.instructions).into_vec();
    let mut body_instructions = instructions.split_off(2);
    for instruction in &mut body_instructions {
        match &mut instruction.op {
            Op::Branch { yes, no, .. } => {
                instruction.next = 0;
                *yes -= 2;
                *no -= 2;
            }
            Op::Call { site } => {
                instruction.next -= 2;
                *site = 0;
            }
            _ => instruction.next -= 2,
        }
    }
    root.instructions = instructions.into_boxed_slice();
    body.instructions = body_instructions.into_boxed_slice();
    let mut calls = std::mem::take(&mut root.calls).into_vec();
    let mut recursive = calls.pop().expect("recursive edge");
    let mut outer = calls.pop().expect("outer edge");
    for plan in [&mut outer, &mut recursive] {
        plan.target = CodeId(1);
        plan.entry = 0;
    }
    root.calls = Box::new([outer]);
    body.calls = Box::new([recursive]);
    for prefix in [10, 11, 12, 13] {
        body.recovery[prefix].incoming = true;
    }
    for (program, code) in [(0, &mut root), (1, &mut body)] {
        for (prefix, map) in code.recovery.iter_mut().enumerate() {
            map.continuation.site = Site::new(Unit::Main, vec![program, prefix as u32]);
        }
    }
    programs::Programs::bind(Box::new([Arc::new(root), Arc::new(body)])).expect("fixture registry")
}

#[test]
fn calls_and_recovery_use_each_frames_own_code_and_the_callers_plan() {
    for (n, bound, depth) in [
        (0, 4, 1),
        (0, 7, 1),
        (8, u64::MAX, 8),
        (8, u64::MAX, 9),
        (1024, u64::MAX, 1025),
    ] {
        let mut state = State::new();
        let mut lease = RegionLease::acquire_fixture_programs(
            &mut state,
            multicode_recursive(n),
            CodeId(0),
            Limits {
                charge: bound,
                depth,
                ..limits()
            },
        )
        .expect("entry");
        let stop = lease
            .run(StopAt::NextBoundary)
            .expect("indexed multi-code recursion");
        let prefix = lease.materialize().expect("qualified recovery maps");
        let image = prefix.reconstruction();
        if bound == 4 {
            assert_eq!(stop, RegionStop::Refusal);
            assert_eq!(
                (
                    image.counters.charge,
                    image.counters.depth,
                    image.counters.balance
                ),
                (4, 0, 10)
            );
            assert_eq!(image.frames.len(), 1);
            assert_eq!(image.prefix.program, CodeId(0));
            assert_eq!(
                image.frames[0].continuation.site,
                Site::new(Unit::Main, vec![0, 11])
            );
            assert_eq!(
                image.attempted.as_ref().expect("completion debt").site,
                Site::new(Unit::Main, vec![0, 10])
            );
            assert!(image.pending.expect("actual return").same(&Value::Int(0)));
            let returning = image.returning.as_ref().expect("completion not accepted");
            assert_eq!(returning.callee_invocation, 2);
            assert_eq!(returning.caller_program, CodeId(0));
            assert!(matches!(returning.completion.fee, Fee::Fixed(3)));
        } else if depth <= n as u32 {
            assert_eq!(stop, RegionStop::Refusal);
            assert_eq!(
                (
                    image.counters.charge,
                    image.counters.depth,
                    image.counters.balance
                ),
                (5 * u64::from(depth), depth, 10 + 8 * u64::from(depth))
            );
            assert_eq!(image.prefix.program, CodeId(1));
            assert_eq!(image.frames.len(), depth as usize + 1);
            assert!(role(image, 0, 0).same(&Value::Int(n)));
            for (index, frame) in image.frames.iter().enumerate().skip(1) {
                assert_eq!(frame.program, CodeId(1));
                assert_eq!(frame.continuation.site.path[0], 1);
                assert!(role(image, index, 9).same(&Value::Int(n - index as i64)));
            }
        } else {
            assert_eq!(stop, RegionStop::Complete);
            assert_eq!(image.frames.len(), 1);
            assert_eq!(image.frames[0].program, CodeId(0));
            assert!(image.payload.expect("caller result").same(&Value::Int(n)));
            assert_eq!(image.counters.charge, 9 * n as u64 + 7);
        }
    }
}

mod graph;

mod calls;

mod entry;
