use super::*;

pub(super) fn local_edit() -> Arc<Code> {
    use crate::machine::region_isa::code::{HeapAction, HeapKind};
    let mut code = code(
        vec![
            instruction(Op::Heap { site: 0 }, 1, 1, cost(1, 0, 0, 0)),
            instruction(
                Op::Read {
                    base: 1,
                    member: ReadSelector::List(0),
                    dst: Register::Int(1),
                },
                2,
                2,
                cost(1, 0, 0, 0),
            ),
            instruction(
                Op::Return {
                    value: Register::Int(1),
                },
                2,
                3,
                Cost::ZERO,
            ),
        ],
        vec![
            (Register::Value(0), object(99)),
            (Register::Value(1), object(99)),
            (Register::Int(0), Value::Int(23)),
        ],
        &[
            (Slot::Binding(0), Register::Value(0)),
            (Slot::Binding(1), Register::Value(1)),
            (Slot::Binding(2), Register::Int(0)),
        ],
        4,
    );
    code.dependencies = Box::new([ReadDependency {
        identity: object(99),
        members: Box::new([Value::Int(7)]),
        fields: Box::new([]),
    }]);
    code.heap = Box::new([HeapAction {
        kind: HeapKind::Member(0),
        args: Box::new([Register::Value(0), Register::Int(0)]),
        destination: None,
    }]);
    Arc::new(code)
}

fn field_edit() -> Arc<Code> {
    use crate::machine::region_isa::code::HeapKind;
    let mut code = Arc::try_unwrap(local_edit())
        .ok()
        .expect("unique field fixture");
    let identity = Value::Record(lash_kernel_doc::arena::ArenaHandle::new(99, 1));
    code.initial[0].1 = identity;
    code.initial[1].1 = identity;
    code.dependencies[0].identity = identity;
    code.dependencies[0].fields = Box::new([Arc::from("field")]);
    code.fields = Box::new([Arc::from("field")]);
    code.heap[0].kind = HeapKind::Field(0);
    let Op::Read { member, .. } = &mut code.instructions[1].op else {
        unreachable!()
    };
    *member = ReadSelector::Field(0);
    Arc::new(code)
}

#[test]
fn local_alias_edits_recover_the_reached_graph_and_publish_once() {
    check_called_fresh_graph();
    for (cap, field) in [0, 1, 2, 3]
        .into_iter()
        .flat_map(|cap| [(cap, false), (cap, true)])
    {
        let mut state = State::new();
        let view = state.view();
        let mut lease = RegionLease::acquire_fixture(
            &mut state,
            if field { field_edit() } else { local_edit() },
            Limits {
                charge: cap,
                ..limits()
            },
        )
        .expect("entry");
        let stop = lease
            .run(StopAt::NextBoundary)
            .expect("local edit and aliased read");
        assert_eq!(
            stop,
            if cap < 2 {
                RegionStop::Refusal
            } else {
                RegionStop::Complete
            }
        );
        view.inspect_graph(|graph| assert!(graph.is_none()));
        let prefix = lease.materialize().expect("freeze exact overlay");
        let FrozenGraph::Fixture(graph) = &prefix.graph else {
            unreachable!()
        };
        let body = graph.bodies[0].as_ref().expect("existing object retained");
        assert!(body.members[0].same(&Value::Int(if cap == 0 { 7 } else { 23 })));
        assert_eq!(body.version, u64::from(cap != 0));
        if cap >= 2 {
            assert!(
                prefix
                    .reconstruction()
                    .payload
                    .expect("aliased answer")
                    .same(&Value::Int(23))
            );
        }
        assert_eq!(prefix.reconstruction().counters.balance, 10);
        prefix
            .prepare_exit()
            .expect("one graph preparation")
            .commit()
            .expect("atomic graph/VM swap");
        view.inspect_graph(|graph| {
            let body = graph.expect("committed graph").bodies[0]
                .as_ref()
                .expect("retained body");
            assert!(body.members[0].same(&Value::Int(if cap == 0 { 7 } else { 23 })));
            assert_eq!(body.version, u64::from(cap != 0));
        });
        view.inspect(|_, version, roots| assert_eq!((version, roots), (1, 1)));
    }
    for n in [1, 17, 4096] {
        let mut state = State::new();
        let view = state.view();
        let mut lease =
            RegionLease::acquire_fixture(&mut state, repeated_edit(n), limits()).expect("entry");
        assert_eq!(
            lease
                .run(StopAt::NextBoundary)
                .expect("repeated local writes"),
            RegionStop::Complete
        );
        let execution = lease.execution.fixture();
        assert_eq!(
            (
                execution.frames.len(),
                execution.view.objects.len(),
                execution.view.objects[0].members.len()
            ),
            (1, 1, 1)
        );
        view.inspect(|image, revision, roots| {
            assert!(image.is_none());
            assert_eq!((revision, roots), (0, 0));
        });
        let prefix = lease.materialize().expect("current contents only");
        let image = prefix.reconstruction();
        let FrozenGraph::Fixture(graph) = &prefix.graph else {
            unreachable!()
        };
        let body = graph.bodies[0].as_ref().expect("entry object");
        assert!(
            body.members[0].same(&Value::Int(7)),
            "net-zero contents still retain every edit's version"
        );
        assert_eq!((body.version, body.retained), (n as u64 + 1, 2));
        assert_eq!(
            (
                image.counters.charge,
                image.counters.balance,
                image.counters.peak
            ),
            (5 * n as u64 + 1, 10, 10)
        );
        assert!(
            image
                .payload
                .expect("last alias read")
                .same(&Value::Int(n - 1))
        );
        prefix
            .prepare_exit()
            .expect("one overlay exit")
            .commit()
            .expect("publish once");
        view.inspect(|_, revision, roots| assert_eq!((revision, roots), (1, 1)));
    }
}

pub(super) fn fresh_list() -> Arc<Code> {
    use crate::machine::region_isa::code::{HeapAction, HeapKind};
    let mut code = code(
        vec![
            instruction(Op::Heap { site: 0 }, 1, 1, cost(1, 0, 0, 0)),
            instruction(Op::Heap { site: 1 }, 2, 2, Cost::ZERO),
            instruction(
                Op::Move {
                    dst: Register::Value(1),
                    src: Register::Value(0),
                },
                3,
                3,
                Cost::ZERO,
            ),
            instruction(Op::Heap { site: 2 }, 4, 4, cost(1, 0, 0, 0)),
            instruction(
                Op::Read {
                    base: 1,
                    member: ReadSelector::List(0),
                    dst: Register::Int(1),
                },
                5,
                5,
                cost(1, 0, 0, 0),
            ),
            instruction(
                Op::Return {
                    value: Register::Int(1),
                },
                5,
                6,
                Cost::ZERO,
            ),
        ],
        vec![
            (Register::Int(0), Value::Int(7)),
            (Register::Int(2), Value::Int(23)),
        ],
        &[
            (Slot::Binding(0), Register::Int(0)),
            (Slot::Binding(1), Register::Int(2)),
        ],
        7,
    );
    for prefix in [1, 2, 3, 4, 5, 6] {
        code.recovery[prefix].roles = Box::new([(Slot::Binding(0), Register::Value(0))]);
        code.recovery[prefix].pending = Some(Register::Value(0));
    }
    for prefix in [3, 4, 5, 6] {
        code.recovery[prefix].roles = Box::new([
            (Slot::Binding(0), Register::Value(0)),
            (Slot::Binding(1), Register::Value(1)),
        ]);
        code.recovery[prefix].operands = Box::new([Register::Value(0), Register::Value(1)]);
    }
    code.heap = Box::new([
        HeapAction {
            kind: HeapKind::List,
            args: Box::new([Register::Int(0)]),
            destination: Some(0),
        },
        HeapAction {
            kind: HeapKind::Hold(1),
            args: Box::new([Register::Value(0)]),
            destination: None,
        },
        HeapAction {
            kind: HeapKind::Member(0),
            args: Box::new([Register::Value(0), Register::Int(2)]),
            destination: None,
        },
    ]);
    Arc::new(code)
}

#[test]
fn fresh_graph_memory_prefixes_translate_all_aliases_through_one_packet() {
    for memory in [11, 12, 13] {
        for charge in [0, 1, 2, 3, 4] {
            let mut state = State::new();
            let view = state.view();
            let mut lease = RegionLease::acquire_fixture(
                &mut state,
                fresh_list(),
                Limits {
                    memory,
                    charge,
                    ..limits()
                },
            )
            .expect("entry");
            let stop = lease
                .run(StopAt::NextBoundary)
                .expect("typed allocation/edit/read");
            let prefix = lease.materialize().expect("one graph packet");
            let image = prefix.reconstruction();
            let FrozenGraph::Fixture(graph) = &prefix.graph else {
                unreachable!()
            };
            if memory < 12 || charge == 0 {
                assert_eq!(stop, RegionStop::Refusal);
                assert_eq!(
                    (
                        image.counters.balance,
                        image.counters.peak,
                        image.counters.charge
                    ),
                    (10, 10, 0)
                );
                assert!(graph.bodies.is_empty());
                assert_eq!(graph.next_identity, 1);
                assert!(image.pending.is_none());
            } else {
                assert_eq!((image.counters.balance, image.counters.peak), (12, 12));
                let body = graph.bodies[0].as_ref().expect("fresh body retained");
                let edited = charge >= 2;
                assert!(body.members[0].same(&Value::Int(if edited { 23 } else { 7 })));
                assert_eq!(body.version, u64::from(edited));
                assert_eq!(body.retained, 2);
                assert_eq!(graph.next_identity, 2);
                for (_, value) in &image.frames[0].roles {
                    assert!(value.same(&body.identity));
                }
                for value in &image.frames[0].operands {
                    assert!(value.same(&body.identity));
                }
                assert!(
                    image
                        .pending
                        .expect("same pending object")
                        .same(&body.identity)
                );
                if charge >= 3 {
                    assert_eq!(stop, RegionStop::Complete);
                    assert!(image.payload.expect("alias answer").same(&Value::Int(23)));
                } else {
                    assert_eq!(stop, RegionStop::Refusal);
                }
            }
            view.inspect_graph(|graph| assert!(graph.is_none()));
            prefix
                .prepare_exit()
                .expect("prepare complete graph")
                .commit()
                .expect("one publication");
            view.inspect(|_, version, roots| assert_eq!((version, roots), (1, 1)));
        }
    }
}

#[test]
fn lease_local_source_release_reclaims_acyclic_bodies_and_retains_self_cycles() {
    use crate::machine::region_isa::code::{HeapAction, HeapKind};
    for cycle in [false, true] {
        let mut code = code(
            vec![
                instruction(Op::Heap { site: 0 }, 1, 1, cost(1, 0, 0, 0)),
                instruction(Op::Heap { site: 1 }, 2, 2, cost(1, 0, 0, 0)),
                instruction(Op::Heap { site: 2 }, 3, 3, cost(1, 0, 0, 0)),
                instruction(Op::Exit, 3, 4, Cost::ZERO),
            ],
            vec![(Register::Int(0), Value::Int(0))],
            &[],
            5,
        );
        for prefix in [1, 2] {
            code.recovery[prefix].roles = Box::new([(Slot::Binding(0), Register::Value(0))]);
        }
        code.heap = Box::new([
            HeapAction {
                kind: HeapKind::List,
                args: Box::new([Register::Int(0)]),
                destination: Some(0),
            },
            HeapAction {
                kind: HeapKind::Member(0),
                args: vec![
                    Register::Value(0),
                    if cycle {
                        Register::Value(0)
                    } else {
                        Register::Int(0)
                    },
                ]
                .into_boxed_slice(),
                destination: None,
            },
            HeapAction {
                kind: HeapKind::Release(1),
                args: Box::new([Register::Value(0)]),
                destination: None,
            },
        ]);
        let mut state = State::new();
        let mut lease =
            RegionLease::acquire_fixture(&mut state, Arc::new(code), limits()).expect("entry");
        assert_eq!(
            lease
                .run(StopAt::NextBoundary)
                .expect("source release is local"),
            RegionStop::Complete
        );
        let prefix = lease.materialize().expect("retention graph freeze");
        let image = prefix.reconstruction();
        assert_eq!(
            (
                image.counters.charge,
                image.counters.peak,
                image.counters.balance
            ),
            (3, 12, if cycle { 12 } else { 10 })
        );
        assert!(image.frames[0].roles.is_empty());
        let FrozenGraph::Fixture(graph) = &prefix.graph else {
            unreachable!()
        };
        assert_eq!(graph.next_identity, 2);
        if cycle {
            let body = graph.bodies[0]
                .as_ref()
                .expect("source-retained self cycle");
            assert!(body.members[0].same(&body.identity));
            assert_eq!((body.retained, body.version), (1, 1));
        } else {
            assert!(
                graph.bodies[0].is_none(),
                "last-owner release must reclaim the fresh body"
            );
        }
    }
}

#[test]
fn allocation_fees_use_actual_sealed_member_measures_before_reservation() {
    use crate::machine::region_isa::code::{HeapAction, HeapKind};
    use crate::machine::region_isa::storage::FactMeasure;
    for count in [0, 1, 3] {
        let expected = 2 + 5 * count;
        for bound in [expected - 1, expected, expected + 1] {
            let fee = Cost {
                fee: Fee::Storage {
                    selector: FactMeasure::Members,
                    base: 2,
                    per_unit: 5,
                },
                ..Cost::ZERO
            };
            let mut code = code(
                vec![
                    instruction(Op::Heap { site: 0 }, 1, 1, fee),
                    instruction(Op::Exit, 1, 2, Cost::ZERO),
                ],
                vec![(Register::Int(0), Value::Int(7))],
                &[],
                3,
            );
            code.recovery[1].pending = Some(Register::Value(0));
            code.recovery[2].pending = Some(Register::Value(0));
            code.heap = Box::new([HeapAction {
                kind: HeapKind::List,
                args: vec![Register::Int(0); count as usize].into_boxed_slice(),
                destination: Some(0),
            }]);
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
            let stop = lease.run(StopAt::NextBoundary).expect("fact-priced phase");
            let prefix = lease.materialize().expect("current graph");
            let image = prefix.reconstruction();
            let FrozenGraph::Fixture(graph) = &prefix.graph else {
                unreachable!()
            };
            if bound < expected {
                assert_eq!(stop, RegionStop::Refusal);
                assert_eq!(
                    (
                        image.counters.charge,
                        image.counters.balance,
                        image.counters.peak,
                        graph.next_identity
                    ),
                    (0, 10, 10, 1)
                );
                assert!(graph.bodies.is_empty());
            } else {
                assert_eq!(stop, RegionStop::Complete);
                assert_eq!(
                    (
                        image.counters.charge,
                        image.counters.balance,
                        image.counters.peak,
                        graph.next_identity
                    ),
                    (expected, 11 + count, 11 + count, 2)
                );
                assert_eq!(
                    graph.bodies[0]
                        .as_ref()
                        .expect("actual allocation")
                        .members
                        .len(),
                    count as usize
                );
            }
        }
    }
}

fn repeated_edit(n: i64) -> Arc<Code> {
    use crate::machine::region_isa::code::{HeapAction, HeapKind};
    let mut code = code(
        vec![
            instruction(Op::Heap { site: 0 }, 1, 1, cost(1, 0, 0, 0)),
            instruction(
                Op::Read {
                    base: 1,
                    member: ReadSelector::List(0),
                    dst: Register::Int(4),
                },
                2,
                2,
                cost(1, 0, 0, 0),
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
            instruction(Op::Heap { site: 1 }, 6, 6, cost(1, 0, 0, 0)),
            instruction(
                Op::Return {
                    value: Register::Int(4),
                },
                6,
                7,
                Cost::ZERO,
            ),
        ],
        vec![
            (Register::Int(0), Value::Int(0)),
            (Register::Int(1), Value::Int(n)),
            (Register::Int(2), Value::Int(1)),
            (Register::Int(3), Value::Int(7)),
            (Register::Value(0), object(99)),
            (Register::Value(1), object(99)),
        ],
        &[
            (Slot::Binding(0), Register::Value(0)),
            (Slot::Binding(1), Register::Value(1)),
            (Slot::Binding(2), Register::Int(0)),
        ],
        8,
    );
    code.dependencies = Box::new([ReadDependency {
        identity: object(99),
        members: Box::new([Value::Int(7)]),
        fields: Box::new([]),
    }]);
    code.heap = Box::new([
        HeapAction {
            kind: HeapKind::Member(0),
            args: Box::new([Register::Value(0), Register::Int(0)]),
            destination: None,
        },
        HeapAction {
            kind: HeapKind::Member(0),
            args: Box::new([Register::Value(0), Register::Int(3)]),
            destination: None,
        },
    ]);
    Arc::new(code)
}

fn called_fresh_graph() -> Arc<programs::Programs> {
    use crate::machine::region_isa::code::{HeapAction, HeapKind};
    let mut root = code(
        vec![
            instruction(Op::Heap { site: 0 }, 1, 1, cost(1, 0, 0, 0)),
            instruction(Op::Heap { site: 1 }, 2, 2, Cost::ZERO),
            instruction(Op::Call { site: 0 }, 3, 3, cost(2, 8, 8, 1)),
            instruction(
                Op::Return {
                    value: Register::Value(1),
                },
                3,
                7,
                Cost::ZERO,
            ),
        ],
        vec![(Register::Int(0), Value::Int(7))],
        &[],
        8,
    );
    for prefix in 1..8 {
        root.recovery[prefix].roles = Box::new([(Slot::Binding(0), Register::Value(0))]);
    }
    root.recovery[2].operands = Box::new([Register::Value(0)]);
    for prefix in [4, 5] {
        root.recovery[prefix].incoming = true;
    }
    for prefix in [6, 7] {
        root.recovery[prefix].roles = Box::new([
            (Slot::Binding(0), Register::Value(0)),
            (Slot::Binding(1), Register::Value(1)),
        ]);
    }
    root.heap = Box::new([
        HeapAction {
            kind: HeapKind::List,
            args: Box::new([Register::Int(0)]),
            destination: Some(0),
        },
        HeapAction {
            kind: HeapKind::Hold(1),
            args: Box::new([Register::Value(0)]),
            destination: None,
        },
    ]);
    root.calls = Box::new([Call {
        target: CodeId(1),
        before: Cost::ZERO,
        before_prefix: 2,
        entry: 0,
        prefix: 0,
        arguments: Box::new([(Register::Value(0), Register::Value(0))]),
        result: Register::Value(0),
        destination: Register::Value(1),
        leave: cost(0, -8, 0, -1),
        leave_prefix: 4,
        completion: cost(3, 0, 0, 0),
        completion_prefix: 5,
        restore: Cost::ZERO,
        restore_prefix: 6,
    }]);
    let mut callee = code(
        vec![
            instruction(
                Op::Constant {
                    dst: Register::Value(1),
                    value: Value::Int(23),
                },
                1,
                1,
                Cost::ZERO,
            ),
            instruction(Op::Heap { site: 0 }, 2, 2, cost(1, 0, 0, 0)),
            instruction(
                Op::Return {
                    value: Register::Value(0),
                },
                2,
                3,
                Cost::ZERO,
            ),
        ],
        vec![],
        &[(Slot::Binding(0), Register::Value(0))],
        4,
    );
    callee.id = CodeId(1);
    callee.heap = Box::new([HeapAction {
        kind: HeapKind::Member(0),
        args: Box::new([Register::Value(0), Register::Value(1)]),
        destination: None,
    }]);
    programs::Programs::bind(Box::new([Arc::new(root), Arc::new(callee)]))
        .expect("graph call registry")
}

fn check_called_fresh_graph() {
    for cap in [3, 4, 6, 7, 8] {
        let mut state = State::new();
        let view = state.view();
        let mut lease = RegionLease::acquire_fixture_programs(
            &mut state,
            called_fresh_graph(),
            CodeId(0),
            Limits {
                charge: cap,
                ..limits()
            },
        )
        .expect("fresh argument entry");
        let stop = lease
            .run(StopAt::NextBoundary)
            .expect("callee edits the shared lease graph");
        assert_eq!(
            stop,
            if cap < 7 {
                RegionStop::Refusal
            } else {
                RegionStop::Complete
            }
        );
        view.inspect_graph(|graph| assert!(graph.is_none()));
        let prefix = lease
            .materialize()
            .expect("one graph mapping for caller and callee");
        let image = prefix.reconstruction();
        let FrozenGraph::Fixture(graph) = &prefix.graph else {
            unreachable!()
        };
        let body = graph.bodies[0]
            .as_ref()
            .expect("same retained fresh argument");
        assert!(body.members[0].same(&Value::Int(if cap == 3 { 7 } else { 23 })));
        assert_eq!(body.retained, 2);
        assert_eq!(image.counters.peak, 20);
        assert_eq!(
            (
                image.counters.balance,
                image.counters.depth,
                image.frames.len()
            ),
            if cap == 3 { (20, 1, 2) } else { (12, 0, 1) }
        );
        assert!(role(image, 0, 0).same(&body.identity));
        if cap == 3 {
            assert!(role(image, 1, 0).same(&body.identity));
            assert!(image.pending.is_none());
        } else if cap < 7 {
            assert!(
                image
                    .pending
                    .expect("post-Leave graph result")
                    .same(&body.identity)
            );
            assert!(
                image
                    .returning
                    .as_ref()
                    .expect("unpaid completion")
                    .value
                    .same(&body.identity)
            );
            assert_eq!(image.counters.charge, 4);
        } else {
            assert!(role(image, 0, 1).same(&body.identity));
            assert!(image.payload.expect("returned alias").same(&body.identity));
            assert_eq!(image.counters.charge, 7);
        }
        prefix
            .prepare_exit()
            .expect("one complete graph exit")
            .commit()
            .expect("atomic publication");
        view.inspect(|_, version, roots| assert_eq!((version, roots), (1, 1)));
    }
}
