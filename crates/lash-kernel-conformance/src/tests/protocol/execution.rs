//! Execution protocols: formula probes, cell handovers and parked coordinates.

use std::sync::Arc;

use lash_kernel_doc::{
    Formula, FunctionRegistry, NativeCall, NativeError, NativeFunction, Operand, Value,
    parse_definition,
};
use lash_kernel_vm::{Bindings, End, KernelMachine, Machine};

use super::{Case, World, end, int, main, program, registry, start};

struct Null;
impl NativeFunction for Null {
    fn call(&self, _: NativeCall<'_>) -> Result<Value, NativeError> {
        Ok(Value::Null)
    }
}

pub(super) fn check(rule: &str, case: &Case) {
    match rule {
        "K-CHG-003" => formula(case),
        "K-EFF-011" => committed(case),
        "K-CHG-004" | "K-CHG-005" | "K-CHG-006" => measure(rule, case),
        "K-SES-001" | "K-SES-002" | "K-SES-003" => session(rule, case),
        "K-SITE-001" | "K-SITE-002" | "K-SITE-003" => sites(rule, case),
        "K-BND-003" | "K-LIB-010" => {
            let mut library = (*registry()).clone();
            if rule == "K-LIB-010" {
                lash_kernel_lib::register_numbers(&mut library).unwrap();
            }
            let mut runner = crate::MachineRunner::<KernelMachine>::new(Arc::new(library));
            crate::check_case(&mut runner, case).expect("rule observations");
        }
        _ => panic!("no execution protocol for {rule}"),
    }
}

fn measure(rule: &str, case: &Case) {
    let (name, formula) = match rule {
        "K-CHG-004" => ("size", "size(x)"),
        "K-CHG-005" => ("deep", "deep(x)"),
        "K-CHG-006" => ("magnitude", "magnitude(x)"),
        _ => unreachable!(),
    };
    let mut registry = FunctionRegistry::new();
    let definition = parse_definition(&format!(
        "function probe.{name}(x: Any) -> Any\nkernel 1\ncharge {formula}\nnative\n"
    ))
    .unwrap();
    registry.register(definition, Some(Arc::new(Null))).unwrap();
    let mut runner = crate::MachineRunner::<KernelMachine>::new(Arc::new(registry));
    crate::check_case(&mut runner, case).expect("the formula measures the written value");
}

fn formula(case: &Case) {
    // The rule fixes empty identities and saturating arithmetic independently
    // of a machine's arguments or heap layout.
    let evaluate = |formula: Formula| formula.evaluate(&mut |_, _| panic!("constant formula"));
    assert_eq!(evaluate(Formula::Sum(vec![])), 0);
    assert_eq!(evaluate(Formula::Product(vec![])), 1);
    assert_eq!(
        evaluate(Formula::Sum(vec![
            Formula::Constant(u64::MAX),
            Formula::Constant(1)
        ])),
        u64::MAX
    );
    assert_eq!(
        evaluate(Formula::Product(vec![
            Formula::Constant(u64::MAX),
            Formula::Constant(2)
        ])),
        u64::MAX
    );

    // Returning and raising bodies both use the result formula; a raised
    // arbitrary value is not that result and contributes zero.
    let body = if case.name.starts_with("return") {
        "return 7"
    } else {
        "throw 7"
    };
    {
        let mut library = FunctionRegistry::new();
        let definition = parse_definition(&format!(
            "function probe.result() -> Any\nkernel 1\ncharge sum(9, size(result))\nbody {{ {body} }}"
        )).unwrap();
        library.register(definition, None).unwrap();
        let mut runner = crate::MachineRunner::<KernelMachine>::new(Arc::new(library));
        crate::check_case(&mut runner, case).expect("return/raise charge formula");
    }
    assert_eq!(Formula::Size(Operand::Result).evaluate(&mut |_, _| 0), 0);
}

fn session(rule: &str, case: &Case) {
    let library = registry();
    let mut machine = start(
        program(case, library.clone()),
        case.environment.bounds,
        Bindings::default(),
    );
    let mut world = World::default();
    let End::Finished(first) = end(&mut machine, &mut world) else {
        panic!("first cell did not finish")
    };
    assert_eq!(
        crate::ExpectedEnd::Finished(first.result.clone()),
        case.expected.end
    );
    match rule {
        "K-SES-001" => {
            assert_eq!(
                first
                    .bindings
                    .variables
                    .keys()
                    .map(|n| n.as_str())
                    .collect::<Vec<_>>(),
                ["x"]
            );
            assert!(first.not_carried.is_empty());
            let mut next = case.clone();
            next.document =
                "kernel 1\nnumbers by_spelling\nmain { set x = 9 let y = x finish y }".into();
            let mut machine = start(
                program(&next, library),
                next.environment.bounds,
                first.bindings,
            );
            let End::Finished(second) = end(&mut machine, &mut world) else {
                panic!("second cell did not finish")
            };
            assert_eq!(second.result, int(9));
            assert_eq!(
                second
                    .bindings
                    .variables
                    .keys()
                    .map(|n| n.as_str())
                    .collect::<Vec<_>>(),
                ["x", "y"]
            );
        }
        "K-SES-002" => {
            assert_eq!(first.bindings.objects.len(), 1);
            let mut next = case.clone();
            next.document =
                "kernel 1\nnumbers by_spelling\nmain { set b[0] = 9 finish a[0] }".into();
            let mut machine = start(
                program(&next, library),
                next.environment.bounds,
                first.bindings,
            );
            let End::Finished(second) = end(&mut machine, &mut world) else {
                panic!("second cell did not finish")
            };
            assert_eq!(second.result, int(9));
            assert_eq!(
                second.bindings.variables[&lash_kernel_doc::Name::new("a")],
                second.bindings.variables[&lash_kernel_doc::Name::new("b")]
            );
            assert_eq!(second.bindings.objects.len(), 1);
        }
        "K-SES-003" => {
            assert_eq!(
                first
                    .not_carried
                    .iter()
                    .map(|n| n.as_str())
                    .collect::<Vec<_>>(),
                ["closure", "nested", "task"]
            );
            assert_eq!(
                first
                    .bindings
                    .variables
                    .keys()
                    .map(|n| n.as_str())
                    .collect::<Vec<_>>(),
                ["got", "kept"]
            );
            assert!(first.bindings.objects.is_empty());
        }
        _ => unreachable!(),
    }
}

fn sites(rule: &str, case: &Case) {
    let library = registry();
    let mut inspected = 0;
    let actual = crate::machine::run_case::<KernelMachine>(&library, case, &mut |mut at| {
        inspected += 1;
        let saved = at.machine.export().unwrap();
        match rule {
            "K-SITE-001" => {
                let task = &saved.tasks[0];
                assert_eq!(task.calls[0].call.statement, main(&[0]));
                assert_eq!(
                    task.calls[1].call.statement,
                    lash_kernel_doc::Site {
                        unit: lash_kernel_doc::Unit::Function(lash_kernel_doc::Name::new("f")),
                        path: vec![0, 1, 0],
                    }
                );
                assert_eq!(task.calls[1].call.finally[0].site.path, [0]);
                for call in &task.calls {
                    assert!(matches!(
                        at.program.document.node(&call.call.statement),
                        Some(lash_kernel_doc::Node::Stmt(_))
                    ));
                    for cleanup in &call.call.finally {
                        assert!(matches!(
                            at.program.document.node(&cleanup.site),
                            Some(lash_kernel_doc::Node::Stmt(lash_kernel_doc::Stmt::Try(_)))
                        ));
                    }
                }
            }
            "K-SITE-002" => {
                let child = &saved.tasks[1];
                let lash_kernel_doc::TaskIdentity::Spawned(spawn) = &child.handle.identity else {
                    panic!("child identity")
                };
                assert_eq!(spawn.site, main(&[0, 0]));
                assert_eq!(saved.tasks[0].handle.occurrences[0].site, spawn.site);
                let lash_kernel_state::TaskState::Performing(wait) = &child.handle.state else {
                    panic!("effect wait")
                };
                let lash_kernel_state::Request::Effect { identity, .. } = &wait.request else {
                    panic!("effect request")
                };
                assert_eq!(identity.site.path, [0, 1, 0, 0]);
                assert_eq!(child.handle.occurrences[0].site, identity.site);
                let mut statement = identity.site.clone();
                statement.path.pop();
                assert_eq!(statement, child.calls[0].call.statement);
                assert!(matches!(
                    at.program.document.node(&identity.site),
                    Some(lash_kernel_doc::Node::Action(_))
                ));
            }
            "K-SITE-003" => {
                let call = &saved.tasks[0].calls[0].call;
                assert_eq!(call.loops[0].site, main(&[0]));
                let lash_kernel_state::TaskState::Performing(wait) = &saved.tasks[0].handle.state
                else {
                    panic!("effect wait")
                };
                let lash_kernel_state::Request::Effect { identity, .. } = &wait.request else {
                    panic!("effect request")
                };
                assert_eq!(identity.loops[0].site, main(&[0]));
                assert!(matches!(
                    at.program.document.node(&call.loops[0].site),
                    Some(lash_kernel_doc::Node::Stmt(
                        lash_kernel_doc::Stmt::For { .. }
                    ))
                ));
            }
            _ => unreachable!(),
        }
        Ok(KernelMachine::import(at.program.clone(), at.bounds, saved).unwrap())
    })
    .expect("parked site document");
    assert_eq!(inspected, 1);
    case.expected
        .check(&actual)
        .expect("sites preserve execution after rebuild");
}

/// K-EFF-011: crash after delivery has committed but before guest code
/// consumes it. Restore the saved, committed wait rather than running the
/// effect again. An admitted wait restored before delivery takes the same
/// committed answer; neither restored machine emits another request.
fn committed(case: &Case) {
    let written = program(case, registry());
    let mut machine = start(
        written.clone(),
        case.environment.bounds,
        Bindings::default(),
    );
    let mut world = World::default();
    let lash_kernel_vm::Step::Parked(park) = machine.run(&mut world, u64::MAX).unwrap() else {
        panic!("effect park")
    };
    assert_eq!(park.requests.len(), 1);
    let lash_kernel_vm::Request::Effect(request) = &park.requests[0] else {
        panic!("effect request")
    };
    let admitted = machine.export().unwrap();
    let outcome = lash_kernel_vm::Outcome::Completed(int(7));
    machine.deliver(request.wait, outcome.clone()).unwrap();
    let committed = machine.export().unwrap();
    assert!(matches!(&committed.tasks[0].handle.state,
        lash_kernel_state::TaskState::Performing(wait)
        if wait.state == lash_kernel_state::PerformState::Committed(lash_kernel_state::Outcome::Completed(int(7)))
    ));
    let mut restored =
        KernelMachine::import(written.clone(), case.environment.bounds.into(), committed).unwrap();
    let End::Finished(result) = end(&mut restored, &mut world) else {
        panic!("committed result")
    };
    assert_eq!(result.result, int(7));
    let charge = restored.meters().charged;
    let mut restored =
        KernelMachine::import(written, case.environment.bounds.into(), admitted).unwrap();
    restored.deliver(request.wait, outcome).unwrap();
    let End::Finished(result) = end(&mut restored, &mut world) else {
        panic!("delivered result")
    };
    assert_eq!(result.result, int(7));
    assert_eq!(restored.meters().charged, charge);
    assert!(world.prints.is_empty());
}
