use super::code::{Call, Cost, Fee, Instruction, Op, ReadDependency, Recovery, Register};
use super::fixtures::State;
use super::*;
use crate::compile::room::Phase;
use lash_kernel_doc::scalar::ScalarOp;
use lash_kernel_state::kernel_internals::logical::Slot;
use lash_kernel_value::sites::{Site, Unit};
use std::sync::atomic::{AtomicU64, Ordering};

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
        continuation: point(index),
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
        entry_depth: 0,
    }
}
fn cost(charge: u64, delta: i128, peak: u64, depth: i32) -> Cost {
    Cost {
        fee: Fee::Fixed(charge),
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
        entry: 0,
        entry_prefix: 0,
        instructions: instructions.into_boxed_slice(),
        recovery: (0..prefixes).map(|i| map(i, roles)).collect(),
        calls: Box::new([]),
        dependencies: Box::new([]),
        registers: 5,
        frame_capacity: 1,
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
            assert_eq!(image.prefix.occurrence, expected as u64);
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
    assert_eq!((image.prefix.occurrence, image.counters.charge), (0, 0));
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
    assert_eq!((image.prefix.occurrence, image.counters.charge), (1, 0));
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
                    member: 0,
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
            (Register::Value(0), Value::Int(99)),
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
    code.recovery[1].pending = Some(Register::Int(4));
    code.recovery[4].operands = Box::new([Register::Bool(0)]);
    code.dependencies = Box::new([ReadDependency {
        identity: Value::Int(99),
        members: Box::new([Value::Int(7)]),
    }]);
    Arc::new(code)
}

#[test]
fn stable_scalar_and_member_loops_publish_once_with_bounded_recovery() {
    for n in [1, 17, 4096] {
        let code = read_loop(n);
        let census = Arc::clone(&code.semantic_operations);
        let mut state = State::new();
        let view = state.view();
        let mut lease = RegionLease::acquire_fixture(&mut state, code, limits()).expect("entry");
        let frame_capacity = lease.execution.frames.capacity();
        assert_eq!(
            lease.run(StopAt::NextBoundary).expect("loop"),
            RegionStop::Complete
        );
        assert_eq!(lease.execution.frames.capacity(), frame_capacity);
        assert_eq!(lease.execution.frames.len(), 1);
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
        10,
    );
    code.frame_capacity = n as usize + 2;
    let plan = |leave_prefix| Call {
        entry: 2,
        prefix: 8,
        arguments: Box::new([
            (Register::Int(0), Register::Int(0)),
            (Register::Int(1), Register::Int(1)),
            (Register::Int(2), Register::Int(2)),
        ]),
        result: Register::Int(0),
        destination: Register::Int(0),
        leave: cost(3, -8, 0, -1),
        leave_prefix,
    };
    code.calls = Box::new([plan(7), plan(9)]);
    Arc::new(code)
}

#[test]
fn affine_calls_use_the_executor_stack_and_preserve_exact_depth_prefixes() {
    for (n, depth) in [(8, 8), (8, 9), (8, 10), (1024, 1025)] {
        let mut state = State::new();
        let mut lease =
            RegionLease::acquire_fixture(&mut state, recursive(n), Limits { depth, ..limits() })
                .expect("entry");
        let stopped = lease
            .run(StopAt::NextBoundary)
            .expect("recursive execution");
        let materialized = lease.materialize().expect("active-frame recovery");
        let image = materialized.reconstruction();
        if depth <= n as u32 {
            assert_eq!(stopped, RegionStop::Refusal);
            assert_eq!(
                (
                    image.counters.charge,
                    image.counters.depth,
                    image.counters.balance
                ),
                (5 * u64::from(depth), depth, 10 + 8 * u64::from(depth))
            );
            assert_eq!(image.frames.len(), depth as usize + 1);
            assert!(role(image, depth as usize, 0).same(&Value::Int(n - i64::from(depth))));
            let invocations: Vec<_> = image.frames.iter().map(|frame| frame.invocation).collect();
            assert_eq!(invocations, (1..=u64::from(depth) + 1).collect::<Vec<_>>());
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
                (9 * n as u64 + 7, 0, 10, 10 + 8 * (n as u64 + 1))
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
}

#[test]
fn failed_or_abandoned_exits_leave_canonical_state_unchanged() {
    for failure in 0..5 {
        let mut state = State::new();
        if failure == 0 {
            state.fail_prepare();
        }
        if failure == 1 {
            state.fail_commit();
        }
        let view = state.view();
        let mut lease =
            RegionLease::acquire_fixture(&mut state, arithmetic(), limits()).expect("entry");
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
        view.publish_alias()
            .expect("dropping authority releases exclusion");
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
    let mut code = Arc::try_unwrap(read_loop(1)).ok().expect("unique fixture");
    code.initial.last_mut().expect("actual read base").1 = Value::Int(100);
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
