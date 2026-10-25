//! Scheduler witnesses shared by layout, observation, identity and limiter laws.
use super::embedder::{Embedder, Setup, int, result, text};
use crate::{End, Layout, Machine, Step};
use lash_kernel_doc::Datum;

/// K-TASK / K-EVAL-008 / K-MACH-008: scheduler handoffs retain their
/// effects, ordered operands, charge and durable continuation across a cold park.
#[test]
fn layouts_agree() {
    use crate::perf_witness::execution::Owner;
    for cancel in [false, true] {
        let source = document(cancel);
        let run = |layout: Layout, cold: bool| {
            let mut embedder = Embedder::with(
                &source,
                Setup {
                    layout,
                    ..Setup::default()
                },
            );
            embedder.machine.count_execution();
            let tape = crate::ChargeTape::new();
            embedder.machine.record_charges(tape.clone());
            let mut saved = Vec::new();
            let end = loop {
                match embedder.run() {
                    Step::Ended(mut end) => {
                        if let End::Finished(done) = &mut end {
                            done.carry.take();
                        }
                        break end;
                    }
                    Step::Slice => (),
                    Step::Parked(_) => {
                        saved.push(embedder.machine.semantic_checkpoint());
                        if cold {
                            embedder.reopen(layout, true);
                            embedder.machine.record_charges(tape.clone());
                        }
                        embedder.deliver_next();
                    }
                }
            };
            let census = embedder.machine.take_execution_census();
            if layout == Layout(0) && !cold {
                assert!(
                    census
                        .slow_exits
                        .get(&Owner::RegisterProgram)
                        .is_none_or(|reasons| !reasons
                            .contains_key(&crate::perf_witness::execution::Reason::ColdBranch)),
                    "scheduler actions do not hand their operation to the source driver: {census:?}"
                );
                assert_eq!(
                    census
                        .statements
                        .get(&Owner::GeneralStatementDriver)
                        .copied()
                        .unwrap_or(0),
                    0,
                    "every scheduler form and cleanup is owned by the register program: {census:?}"
                );
                assert!(
                    census
                        .statements
                        .get(&Owner::RegisterProgram)
                        .copied()
                        .unwrap_or(0)
                        >= if cancel { 6 } else { 11 },
                    "scheduler forms must run in the register program: {census:?}"
                );
            }
            (
                end,
                embedder.requests,
                saved,
                embedder.machine.meters().charged,
                lash_kernel_exec::Weight::expand(tape.steps()),
            )
        };
        let reference = run(Layout(0).without_regions(), false);
        if cancel {
            assert!(matches!(reference.0, End::Error(_)));
            assert!(
                reference
                    .1
                    .iter()
                    .any(|request| super::embedder::label(request) == "cleanup")
            );
        } else {
            assert_eq!(
                result(reference.0.clone()),
                Datum::Tuple(vec![int(11), text("worker"), int(22)])
            );
        }
        for layout in [Layout(0), Layout(1), Layout(0x5eed)] {
            assert_eq!(run(layout, false), reference);
            assert_eq!(run(layout, true), reference);
        }
    }
}

pub(super) fn document(cancel: bool) -> String {
    let worker = if cancel {
        "function worker() -> Any\nkernel 1\nmetering statements\nbody { try { let reply = perform 0(\"blocked\") as Any return reply } finally { do perform 0(\"cleanup\") as Any } }"
    } else {
        "function worker() -> Any\nkernel 1\nmetering statements\nbody { return (11, perform 0(\"worker\") as Any, 22) }"
    };
    let scheduler = if cancel {
        "function scheduler() -> Any\nkernel 1\nmetering statements\nbody { let child = spawn call worker() do cancel child let reply = join child return reply }"
    } else {
        "function scheduler() -> Any\nkernel 1\nmetering statements\nbody { let child = spawn call worker() let i = 0 while num.lt(i, 2) { do yield set i = num.add(i, 1) } let reply = join child return reply }"
    };
    format!(
        "definition {}\ndefinition {}\nmain {{ return call scheduler() }}",
        serde_json::to_string(worker).unwrap(),
        serde_json::to_string(scheduler).unwrap()
    )
}
