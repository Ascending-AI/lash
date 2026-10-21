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
    use lash_kernel_doc::heap::PageValue;
    use lash_kernel_state::kernel_internals::tree::Tree;
    use lash_kernel_state::paged::{BindingRecord, BoundRecord};
    use lash_vocabulary::names::Name;
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
    let aliases = |session: &Session| {
        let heap = session.heap();
        let root =
            lash_kernel_state::kernel_internals::session_bindings(session.root.as_ref().unwrap());
        let table = Tree::<BindingRecord>::of_keyed(&root);
        let a = table
            .get(
                &lash_kernel_state::kernel_internals::metadata::metadata_key(&Name::new("a")),
                heap.pages(),
            )
            .unwrap()
            .unwrap();
        let b = table
            .get(
                &lash_kernel_state::kernel_internals::metadata::metadata_key(&Name::new("b")),
                heap.pages(),
            )
            .unwrap()
            .unwrap();
        let (BoundRecord::Value(PageValue::List(a)), BoundRecord::Value(PageValue::List(b))) =
            (a.value, b.value)
        else {
            panic!("list aliases")
        };
        assert_eq!(a, b);
    };
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
            aliases(&session);
            let mut next = case.clone();
            next.document =
                "kernel 1\nnumbers by_spelling\nmain { set b[0] = 9 finish a[0] }".into();
            let mut machine = start(program(&next, library), next.environment.bounds, &session);
            let End::Finished(second) = end(&mut machine, &mut world) else {
                panic!("second cell")
            };
            assert_eq!(second.result, int(9));
            session.root = second.carry;
            aliases(&session);
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
    let library = PreparedLibrary::new(registry());
    let mut inspected = 0;
    let actual = crate::machine::run_case::<KernelMachine>(&library, case, &mut |mut at| {
        inspected += 1;
        let saved = at.machine.seal(crate::pages::BUDGET).unwrap().unwrap();
        let heap = lash_kernel_state::kernel_internals::PagedHeap::open_header(
            lash_kernel_vm::kernel_internals::sealed_root(&saved).heap,
            Box::new(at.source.clone()),
            lash_kernel_state::paged::EditEpoch(2),
        )
        .unwrap();
        let tasks = lash_kernel_state::kernel_internals::tree::Tree::<
            lash_kernel_state::paged::TaskRecord,
        >::of_keyed(
            &lash_kernel_vm::kernel_internals::sealed_root(&saved).tasks
        );
        let task = |n| {
            tasks
                .get(
                    &lash_kernel_state::kernel_internals::directory::ordinal_key(n),
                    heap.pages(),
                )
                .unwrap()
                .unwrap()
        };
        let frame = |task: &lash_kernel_state::paged::TaskRecord, n| {
            task.frames
                .get(
                    &lash_kernel_state::kernel_internals::directory::ordinal_key(n),
                    heap.pages(),
                )
                .unwrap()
                .unwrap()
        };
        let effect = || {
            at.requests
                .iter()
                .find_map(|r| {
                    if let lash_kernel_vm::Request::Effect(r) = r {
                        Some(r)
                    } else {
                        None
                    }
                })
                .unwrap()
        };
        match rule {
            "K-SITE-001" => {
                let task = task(0);
                let outer = frame(&task, 0);
                let inner = frame(&task, 1);
                assert_eq!(*outer.statement.resolve(heap.pages()).unwrap(), main(&[0]));
                assert_eq!(
                    *inner.statement.resolve(heap.pages()).unwrap(),
                    lash_vocabulary::sites::Site {
                        unit: lash_vocabulary::sites::Unit::Function(
                            lash_vocabulary::names::Name::new("f")
                        ),
                        path: vec![0, 1, 0]
                    }
                );
                assert_eq!(
                    inner
                        .finally
                        .get(
                            &lash_kernel_state::kernel_internals::directory::ordinal_key(0),
                            heap.pages()
                        )
                        .unwrap()
                        .unwrap()
                        .site
                        .resolve(heap.pages())
                        .unwrap()
                        .path,
                    [0]
                );
                task.frames
                    .visit(heap.pages(), &mut |_, call| {
                        assert!(matches!(
                            at.program
                                .document
                                .node(&call.statement.resolve(heap.pages()).unwrap()),
                            Some(lash_kernel_doc::Node::Stmt(_))
                        ));
                        call.finally
                            .visit(heap.pages(), &mut |_, cleanup| {
                                assert!(matches!(
                                    at.program
                                        .document
                                        .node(&cleanup.site.resolve(heap.pages()).unwrap()),
                                    Some(lash_kernel_doc::Node::Stmt(lash_kernel_doc::Stmt::Try(
                                        _
                                    )))
                                ));
                                true
                            })
                            .unwrap();
                        true
                    })
                    .unwrap();
            }
            "K-SITE-002" => {
                let child = task(1);
                let lash_vocabulary::sites::TaskIdentity::Spawned(spawn) =
                    lash_kernel_vm::kernel_internals::task_identity(&at.machine, child.identity)
                        .unwrap()
                else {
                    panic!("child identity")
                };
                assert_eq!(spawn.site, main(&[0, 0]));
                assert_eq!(
                    task(0)
                        .occurrences
                        .get(
                            &lash_kernel_state::kernel_internals::metadata::metadata_key(
                                &spawn.site
                            ),
                            heap.pages()
                        )
                        .unwrap(),
                    Some(1)
                );
                assert!(matches!(
                    child.state,
                    lash_kernel_state::paged::TaskStateRecord::Waiting(_)
                ));
                let identity = lash_kernel_doc::kernel_internals::effect_parts(&effect().identity);
                assert_eq!(identity.site.path, [0, 1, 0, 0]);
                assert_eq!(
                    child
                        .occurrences
                        .get(
                            &lash_kernel_state::kernel_internals::metadata::metadata_key(
                                &identity.site
                            ),
                            heap.pages()
                        )
                        .unwrap(),
                    Some(1)
                );
                let mut statement = identity.site.clone();
                statement.path.pop();
                assert_eq!(
                    statement,
                    *frame(&child, 0).statement.resolve(heap.pages()).unwrap()
                );
                let Some(lash_kernel_doc::Node::Expr(lash_kernel_doc::Expr::Act(act))) =
                    at.program.document.node(&identity.site)
                else {
                    panic!("wait site is not an action expression")
                };
                assert!(matches!(act.kind, lash_kernel_doc::ActKind::Perform { .. }));
            }
            "K-SITE-003" => {
                let call = frame(&task(0), 0);
                let site = call
                    .loops
                    .get(
                        &lash_kernel_state::kernel_internals::directory::ordinal_key(0),
                        heap.pages(),
                    )
                    .unwrap()
                    .unwrap()
                    .site
                    .resolve(heap.pages())
                    .unwrap();
                assert_eq!(site.as_ref(), &main(&[0]));
                assert_eq!(
                    lash_kernel_doc::kernel_internals::effect_parts(&effect().identity).loops[0]
                        .site,
                    main(&[0])
                );
                assert!(matches!(
                    at.program.document.node(&site),
                    Some(lash_kernel_doc::Node::Stmt(
                        lash_kernel_doc::Stmt::For { .. }
                    ))
                ));
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
    let lash_kernel_vm::Request::Effect(request) = &park.requests[0] else {
        panic!("effect request")
    };
    let admitted = machine.seal(crate::pages::BUDGET).unwrap().unwrap();
    let outcome = lash_kernel_vm::Outcome::Completed(int(7));
    machine.deliver(request.wait, outcome.clone()).unwrap();
    let committed = machine.seal(crate::pages::BUDGET).unwrap().unwrap();
    let heap = lash_kernel_state::kernel_internals::PagedHeap::open_header(
        lash_kernel_vm::kernel_internals::sealed_root(&committed).heap,
        Box::new(session.source.clone()),
        lash_kernel_state::paged::EditEpoch(2),
    )
    .unwrap();
    let waits = lash_kernel_state::kernel_internals::tree::Tree::<
        lash_kernel_state::paged::WaitRecord,
    >::of_keyed(&lash_kernel_vm::kernel_internals::sealed_root(&committed).waits);
    assert!(matches!(
        waits
            .get(
                &lash_kernel_state::kernel_internals::directory::ordinal_key(
                    lash_kernel_doc::kernel_internals::wait_number(request.wait)
                ),
                heap.pages()
            )
            .unwrap()
            .unwrap()
            .phase,
        lash_kernel_state::paged::WaitPhase::Committed(
            lash_kernel_state::paged::CommittedOutcomeRecord::Completed(_)
        )
    ));
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
