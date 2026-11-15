//! Execution protocols: formula probes, cell handovers and parked coordinates.

use std::sync::Arc;

use lash_kernel_doc::{
    Formula, FunctionRegistry, NativeCall, NativeError, NativeFunction, Operand, Value,
    parse_definition,
};

use lash_kernel_vm::{End, KernelMachine, Machine, PreparedLibrary};

use super::{Case, World, end, int, main, program, registry, start};

struct Null;
impl NativeFunction for Null {
    fn call(&self, _: NativeCall<'_, '_>) -> Result<Value, NativeError> {
        Ok(Value::Null)
    }
}

pub(super) fn check(rule: &str, case: &Case) {
    match rule {
        "K-CHG-003" => formula(case),
        "K-EFF-011" => committed(case),
        "K-CHG-004" | "K-CHG-005" | "K-CHG-006" => measure(case),
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

pub(super) fn register_probes(registry: &mut FunctionRegistry) {
    for name in ["size", "deep", "magnitude"] {
        let definition = parse_definition(&format!(
            "function probe.{name}(x: Any) -> Any\nkernel 1\ncharge {name}(x)\nnative\n"
        ))
        .unwrap();
        registry.register(definition, Some(Arc::new(Null))).unwrap();
    }
}

fn measure(case: &Case) {
    let mut registry = FunctionRegistry::new();
    register_probes(&mut registry);
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
    // arbitrary value is not that result and contributes zero. The probe
    // states a native implementation, so its formula is its whole charge
    // whichever implementation runs (`K-CHG-007`); here its body runs.
    // K-CHG-002 adds 6 outside that body: try, do and finish statements,
    // the do action expression, the null expression and its copy-out.
    // Thus the totals are 6 + (9 + 2) = 17 on return and 6 + 9 = 15 on raise.
    let body = if case.name.starts_with("return") {
        "return 7"
    } else {
        "throw 7"
    };
    {
        let mut library = FunctionRegistry::new();
        let definition = parse_definition(&format!(
            "function probe.result() -> Any\nkernel 1\ncharge sum(9, size(result))\nnative\nbody {{ {body} }}"
        )).unwrap();
        library.register(definition, None).unwrap();
        let mut runner = crate::MachineRunner::<KernelMachine>::new(Arc::new(library));
        crate::check_case(&mut runner, case).expect("return/raise charge formula");
    }
    assert_eq!(Formula::Size(Operand::Result).evaluate(&mut |_, _| 0), 0);
}

fn session(rule: &str, case: &Case) {
    use crate::pages::Session;
    let library = registry();
    let mut session = Session::default();
    let mut machine = start(
        program(case, library.clone()),
        case.environment.bounds,
        &session,
    );
    let mut world = World::default();
    let End::Finished(first) = end(&mut machine, &mut world) else {
        panic!("first cell did not finish")
    };
    assert_eq!(
        crate::ExpectedEnd::Finished(first.result.clone()),
        case.expected.end
    );
    session.root = first.carry;
    let names = |session: &Session| {
        session
            .names()
            .iter()
            .map(|name| name.as_str().to_owned())
            .collect::<Vec<_>>()
    };
    match rule {
        "K-SES-001" => {
            assert_eq!(names(&session), ["x"]);
            assert!(first.not_carried.is_empty());
            let mut next = case.clone();
            next.document =
                "kernel 1\nnumbers by_spelling\nmain { set x = 9 let y = x finish y }".into();
            let mut machine = start(program(&next, library), next.environment.bounds, &session);
            let End::Finished(second) = end(&mut machine, &mut world) else {
                panic!("second cell")
            };
            assert_eq!(second.result, int(9));
            session.root = second.carry;
            assert_eq!(names(&session), ["x", "y"]);
        }
        "K-SES-002" => {
            // K-SES-002 is observed as behaviour: a write through one carried
            // binding is read through the other in the next cell.
            let mut aliased = |session: &mut Session, write: &str, read: &str, value: i64| {
                let mut next = case.clone();
                next.document = format!(
                    "kernel 1\nnumbers by_spelling\nmain {{ set {write}[0] = {value} finish {read}[0] }}"
                );
                let mut machine = start(
                    program(&next, library.clone()),
                    next.environment.bounds,
                    session,
                );
                let End::Finished(next) = end(&mut machine, &mut world) else {
                    panic!("aliasing cell")
                };
                assert_eq!(next.result, int(value));
                session.root = next.carry;
            };
            aliased(&mut session, "b", "a", 9);
            aliased(&mut session, "a", "b", 11);
        }
        "K-SES-003" => {
            assert_eq!(
                first
                    .not_carried
                    .iter()
                    .map(|(n, _)| n.as_str())
                    .collect::<Vec<_>>(),
                ["closure", "nested", "task"]
            );
            assert_eq!(names(&session), ["got", "kept"]);
        }
        _ => unreachable!(),
    }
}

fn sites(rule: &str, case: &Case) {
    use lash_kernel_doc::{ActKind, Action, Expr, Node, Stmt};
    use lash_kernel_state::paged::run::continuation::{PointPhase, RunTask, SlotRole};
    use lash_kernel_value::sites::{Site, TaskIdentity, Unit};
    let library = PreparedLibrary::new(registry());
    let mut inspected = 0;
    let actual = crate::machine::run_case::<KernelMachine>(&library, case, &mut |mut at| {
        inspected += 1;
        let saved = at.machine.seal(crate::pages::BUDGET).unwrap().unwrap();
        // The coordinates are read from the saved run itself: where each
        // frame stands, the slots it holds by declaring site, and the
        // occurrences counted per action site.
        let (run, _) = lash_kernel_state::paged::run::continuation::open(
            Box::new(at.source.clone()),
            lash_kernel_vm::kernel_internals::sealed_root(&saved).heap,
        )
        .unwrap();
        let document = &at.program.document;
        let in_f = |path: &[u32]| Site {
            unit: Unit::Library(
                document
                    .functions
                    .iter()
                    .find(|(_, definition)| definition.name.as_str() == "f")
                    .map(|(id, _)| *id)
                    .expect("written f body"),
            ),
            path: path.to_vec(),
        };
        // K-SITE-001: a frame stands at a statement or at an action nested
        // in one, never at a block or another expression.
        let stands = |task: &RunTask| {
            for frame in &task.frames {
                match document.node(&frame.site) {
                    Some(Node::Stmt(_)) => {}
                    Some(Node::Action(_) | Node::Expr(Expr::Act(_))) => {
                        assert_ne!(frame.phase, PointPhase::Statement, "{frame:?}");
                    }
                    other => panic!("a frame stands at {other:?}"),
                }
            }
        };
        let occurrences = |task: &RunTask, site: &Site| {
            task.occurrences
                .iter()
                .find(|occurrence| occurrence.site == *site)
                .map(|occurrence| occurrence.count)
        };
        let effect = at
            .requests
            .iter()
            .map(|r| {
                let lash_kernel_vm::Request::Effect(r) = r;
                lash_kernel_value::kernel_internals::effect_parts(&r.identity)
            })
            .next()
            .unwrap();
        match rule {
            "K-SITE-001" => {
                let [task] = run.tasks.as_slice() else {
                    panic!("one task")
                };
                stands(task);
                let [outer, inner] = task.frames.as_slice() else {
                    panic!("main and f")
                };
                // main stands at its `invoke f()`, f at its waited perform.
                assert_eq!(
                    (&outer.site, outer.phase),
                    (&main(&[0, 0]), PointPhase::Call)
                );
                assert_eq!(
                    (&inner.site, inner.phase),
                    (&in_f(&[0, 1, 0, 0]), PointPhase::Issued)
                );
                // The `try` whose finally f runs holds its completion under
                // the try's own site; every held site is a statement.
                for slot in &inner.slots {
                    assert!(
                        matches!(document.node(&in_f(&slot.site)), Some(Node::Stmt(_))),
                        "{slot:?}"
                    );
                }
                assert!(inner.slots.iter().any(|slot| slot.role == SlotRole::Held
                    && matches!(
                        document.node(&in_f(&slot.site)),
                        Some(Node::Stmt(Stmt::Try(_)))
                    )));
            }
            "K-SITE-002" => {
                let [parent, child] = run.tasks.as_slice() else {
                    panic!("main and its spawn")
                };
                stands(parent);
                stands(child);
                let TaskIdentity::Spawned(spawn) = &child.identity else {
                    panic!("child identity")
                };
                assert_eq!(spawn.site, main(&[0, 0]));
                assert_eq!(occurrences(parent, &spawn.site), Some(1));
                assert_eq!(effect.site, in_f(&[0, 1, 0, 0]));
                assert_eq!(occurrences(child, &effect.site), Some(1));
                assert_eq!(child.frames.last().unwrap().site, effect.site);
                match document.node(&effect.site) {
                    Some(Node::Action(Action::Perform { .. })) => {}
                    Some(Node::Expr(Expr::Act(act))) => {
                        assert!(matches!(act.kind, ActKind::Perform { .. }));
                    }
                    other => panic!("the wait stands at {other:?}"),
                }
            }
            "K-SITE-003" => {
                let [task] = run.tasks.as_slice() else {
                    panic!("one task")
                };
                stands(task);
                assert_eq!(effect.loops[0].site, main(&[0]));
                assert!(matches!(
                    document.node(&effect.loops[0].site),
                    Some(Node::Stmt(Stmt::For { .. }))
                ));
                // The loop's saved state is named by the same site.
                let loops: Vec<_> = task.frames[0]
                    .slots
                    .iter()
                    .filter(|slot| matches!(slot.role, SlotRole::Loop(_)))
                    .map(|slot| main(&slot.site))
                    .collect();
                assert!(!loops.is_empty());
                assert!(loops.iter().all(|site| *site == main(&[0])));
            }
            _ => unreachable!(),
        }
        Ok(crate::pages::open::<KernelMachine>(
            at.program.clone(),
            at.bounds,
            lash_kernel_vm::kernel_internals::sealed_root(&saved),
            at.source,
        )
        .unwrap())
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
    let session = crate::pages::Session::default();
    let mut machine = start(written.clone(), case.environment.bounds, &session);
    let mut world = World::default();
    let lash_kernel_vm::Step::Parked(park) = machine.run(&mut world, u64::MAX).unwrap() else {
        panic!("effect park")
    };
    assert_eq!(park.requests.len(), 1);
    let lash_kernel_vm::Request::Effect(request) = &park.requests[0];
    let admitted = machine.seal(crate::pages::BUDGET).unwrap().unwrap();
    let outcome = lash_kernel_vm::Outcome::Completed(int(7));
    machine.deliver(request.wait, outcome.clone()).unwrap();
    let committed = machine.seal(crate::pages::BUDGET).unwrap().unwrap();
    let (run, _) = lash_kernel_state::paged::run::continuation::open(
        Box::new(session.source.clone()),
        lash_kernel_vm::kernel_internals::sealed_root(&committed).heap,
    )
    .unwrap();
    let wait = lash_kernel_doc::kernel_internals::wait_number(request.wait);
    assert_eq!(
        run.waits
            .iter()
            .find(|saved| saved.number == wait)
            .map(|saved| &saved.phase),
        Some(&lash_kernel_state::paged::run::continuation::WaitPhase::Completed(int(7)))
    );
    let mut restored = crate::pages::open::<KernelMachine>(
        written.clone(),
        case.environment.bounds.into(),
        lash_kernel_vm::kernel_internals::sealed_root(&committed),
        &session.source,
    )
    .unwrap();
    let End::Finished(result) = end(&mut restored, &mut world) else {
        panic!("committed result")
    };
    assert_eq!(result.result, int(7));
    let charge = restored.meters().charged.expect("exact charge");
    let mut restored = crate::pages::open::<KernelMachine>(
        written,
        case.environment.bounds.into(),
        lash_kernel_vm::kernel_internals::sealed_root(&admitted),
        &session.source,
    )
    .unwrap();
    restored.deliver(request.wait, outcome).unwrap();
    let End::Finished(result) = end(&mut restored, &mut world) else {
        panic!("delivered result")
    };
    assert_eq!(result.result, int(7));
    assert_eq!(restored.meters().charged.expect("exact charge"), charge);
    assert!(world.prints.is_empty());
}
