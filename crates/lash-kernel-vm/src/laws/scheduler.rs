//! Scheduler witnesses shared by observation, identity and limiter laws.
use super::embedder::{Embedder, Setup, int, result, text};
use crate::{End, Machine, Step};
use lash_kernel_doc::Datum;

/// K-TASK / K-EVAL-008 / K-MACH-008: scheduler handoffs retain their
/// effects, ordered operands, charge and durable continuation across a cold park.
#[test]
fn cold_parks_agree() {
    for cancel in [false, true] {
        let source = document(cancel);
        let run = |cold: bool| {
            let mut embedder = Embedder::with(&source, Setup::default());

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
                            embedder.reopen(true);
                            embedder.machine.record_charges(tape.clone());
                        }
                        embedder.deliver_next();
                    }
                }
            };

            (
                end,
                embedder.requests,
                saved,
                embedder.machine.meters().charged,
                lash_kernel_exec::Weight::expand(tape.steps()),
            )
        };
        let reference = run(false);
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
        assert_eq!(run(true), reference);
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
