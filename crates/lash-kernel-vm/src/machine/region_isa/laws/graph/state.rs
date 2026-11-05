use super::*;

/// Exercise C's same executor with real D storage and its single exit packet.
/// Hand-derived costs: list header 32, integer member 17, each guest ref 16. Pending
/// and physical operand aliases add no source holdings beyond the live roles.
pub(super) fn real_state_exit() {
    use crate::laws::embedder::Embedder;
    use lash_kernel_doc::TaskId;
    use lash_kernel_state::kernel_internals::logical::{PublicationValue, StateExit, StateLease};
    for extra in [64, 65, 66, 80, 81, 82] {
        for charge in [0, 1, 2, 3, 4] {
            for commit in [false, true] {
                let mut run = Embedder::new("main { return 0 }");
                let shared = run.machine.logical.clone();
                let caller = shared
                    .borrow_mut()
                    .enter_source(
                        TaskId::MAIN,
                        &mut run.machine.arena.borrow_mut(),
                        &run.machine.heap,
                        &[],
                        &[],
                    )
                    .unwrap();
                let before = shared.borrow().balance().unwrap().total().unwrap();
                let stamp = shared.borrow().stamp();
                let mut code = Arc::try_unwrap(fresh_list()).ok().expect("unique program");
                code.invocation = caller.id();
                code.recovery[0].roles = Box::new([]);
                let code = Arc::new(code);
                let mut state =
                    StateLease::acquire_shared(&mut run.machine.heap, &run.machine.arena).unwrap();
                let scalar = state.import(&Value::Int(7)).unwrap();
                assert_eq!(state.price(scalar).unwrap(), 17);
                let mut execution = Execution::acquire(
                    code,
                    Limits {
                        entry_balance: before,
                        memory: before + extra,
                        charge,
                        ..limits()
                    },
                    super::super::super::invocations::Invocations::fixture(caller.id() + 1, 1)
                        .unwrap(),
                    state,
                )
                .unwrap();
                let stop = execution.run(StopAt::NextBoundary).unwrap();
                assert!(shared.borrow().is_current(&stamp));
                assert_eq!(shared.borrow().balance().unwrap().total().unwrap(), before);
                let super::super::super::reconstruct::Finished {
                    image,
                    live,
                    view: lease,
                    graph,
                    invocations,
                } = execution.reconstruct().unwrap();
                assert!(
                    lease.price(scalar).is_err(),
                    "frozen lease cannot resolve execution prices"
                );
                let expected_delta = if charge == 0 || extra < 65 {
                    0
                } else if extra < 81 {
                    65
                } else {
                    81
                };
                assert_eq!(image.counters.balance, before + expected_delta);
                assert_eq!(image.counters.peak, before + expected_delta);
                assert_eq!(
                    stop,
                    if extra >= 81 && charge >= 3 {
                        RegionStop::Complete
                    } else {
                        RegionStop::Refusal
                    }
                );
                let published = live.publish(&graph).unwrap();
                assert_eq!(published.frames.len(), 1);
                let frame = published.frames.into_vec().pop().unwrap();
                assert_eq!((frame.program, frame.invocation), (CodeId(0), caller.id()));
                let expected_roots = if expected_delta == 0 {
                    0
                } else if expected_delta == 65 {
                    1
                } else {
                    2
                };
                assert_eq!(frame.roles.len(), expected_roots);
                let identity = image.frames[0].roles.first().map(|(_, value)| *value);
                for (_, value) in &frame.roles {
                    assert!(
                        matches!(value, PublicationValue::Prepared(_)),
                        "fresh object must keep its detached proof"
                    );
                }
                if expected_roots == 2 {
                    assert_eq!(frame.operands.len(), 2);
                    assert!(frame.pending.is_some());
                    assert!(
                        image.frames[0]
                            .operands
                            .iter()
                            .all(|value| value.same(&identity.unwrap()))
                    );
                    assert!(image.frames[0].pending.unwrap().same(&identity.unwrap()));
                }
                let outputs = frame
                    .roles
                    .into_vec()
                    .into_iter()
                    .map(|(slot, value)| (caller.owner(slot), value))
                    .collect();
                let exit = StateExit {
                    inputs: Box::new([]),
                    outputs,
                    graph,
                    invocations,
                    peak: image.counters.peak,
                    delta: i128::from(expected_delta),
                };
                let vm = std::cell::Cell::new(0);
                let prepared = lease.prepare(exit, before + extra).unwrap();
                assert_eq!(vm.get(), 0);
                if !commit {
                    drop(prepared);
                    assert!(shared.borrow().is_current(&stamp));
                    assert_eq!(shared.borrow().balance().unwrap().total().unwrap(), before);
                    assert_eq!(vm.get(), 0);
                    continue;
                }
                prepared.commit(|| vm.set(1)).unwrap();
                assert_eq!(vm.get(), 1);
                assert_eq!(
                    shared.borrow().balance().unwrap().total().unwrap(),
                    before + expected_delta
                );
                if let Some(value) = identity {
                    let body = run
                        .machine
                        .arena
                        .borrow_mut()
                        .item(value.handle().unwrap(), 0)
                        .unwrap()
                        .unwrap();
                    assert!(body.same(&Value::Int(if extra >= 81 && charge >= 2 {
                        23
                    } else {
                        7
                    })));
                }
            }
        }
    }
}
