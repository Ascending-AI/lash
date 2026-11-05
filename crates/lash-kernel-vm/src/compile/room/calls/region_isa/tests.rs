use super::callee::Substitution;
use super::*;
use crate::compile::region_isa::fixtures as drafts;
use crate::laws::embedder::Embedder;
use lash_kernel_state::kernel_internals::logical::{LeaseAllocation, LeaseMember, LeaseOp};

fn executable(run: &Embedder) -> Arc<Executable> {
    let (program, _) = run.program(true);
    program
        .library
        .admit(&program.document, Default::default(), &program.selection)
        .unwrap_or_else(|_| panic!("call plan admission"))
        .executable(
            &program.document,
            &program.library,
            &program.documents,
            &program.functions,
            &program.instances,
        )
        .unwrap_or_else(|_| panic!("call plan executable"))
}
fn call_site(exe: &Executable, caller: CodeId) -> (StmtId, u32) {
    let block = exe.block(exe.code(caller).body);
    for &statement in &block.stmts {
        let (_, stmt, _) = exe.statement(statement);
        for pc in stmt.ops.start..stmt.ops.end() {
            if resolved(exe, caller, statement, pc).is_ok() {
                return (statement, pc);
            }
        }
    }
    panic!("source has a body call")
}
fn values(plan: &CalleePlan, inputs: &[(Slot, Value)]) -> Vec<(String, Value)> {
    let formals = plan.source().owner_slots().unwrap();
    plan.substitutions()
        .iter()
        .map(|substitution| {
            let (parameter, value) = match substitution {
                Substitution::Role {
                    parameter,
                    argument,
                } => (
                    parameter.role.slot(),
                    inputs
                        .iter()
                        .find(|(slot, _)| *slot == argument.slot())
                        .expect("source argument must be used")
                        .1,
                ),
                Substitution::Absent { parameter } => (parameter.role.slot(), Value::Absent),
            };
            let name = plan
                .source()
                .params
                .iter()
                .find(|name| {
                    let owner = formals
                        .root(
                            &Owner::Binding {
                                declared: plan.source().site.clone(),
                                name: (*name).clone(),
                            },
                            TaskId::MAIN,
                            0,
                        )
                        .unwrap();
                    matches!(owner, logical::Owner::Value { slot, .. } if slot == parameter)
                })
                .unwrap();
            (name.as_str().to_owned(), value)
        })
        .collect()
}

/// Swapping same-priced arguments, dropping Absent padding, or taking caller
/// operands as formals changes a real call even when its memory total agrees.
#[test]
fn substitutions_preserve_source_arguments_and_saved_operands() {
    let definition = "definition \"function pick(a: Any, b: Any, c?: Any) -> Any\\nkernel 1\\nmetering statements\\nbody { if num.lt(a, b) { return a } return b }\"";
    for nested in [false, true] {
        let body = if nested {
            "return num.add(9, invoke pick(x, 5))"
        } else {
            "let answer = invoke pick(x, 5) return answer"
        };
        let run = Embedder::new(&format!("{definition}\nmain {{ let x = 3 {body} }}"));
        let exe = executable(&run);
        let caller = exe.main.unwrap();
        let site = call_site(&exe, caller);
        let draft = drafts::calls(&exe, caller, &[site]).unwrap();
        let plans = compile_calls(&exe, &draft).unwrap();
        let plan = &plans[0];
        plan.verify(&exe).unwrap();
        let RegionOp::Call { site: id, .. } = draft.ops()[0] else {
            unreachable!()
        };
        plan.clone().verify_region(&exe, &draft, id).unwrap();
        let target = plan.static_target().unwrap();
        let source = &exe.code(caller).source_room;
        let inputs = if nested {
            [3, 5]
                .into_iter()
                .enumerate()
                .map(|(index, n)| {
                    (
                        slot(source, &Owner::Operand(plan.at.site.child(index as u32))).unwrap(),
                        Value::Int(n),
                    )
                })
                .collect::<Vec<_>>()
        } else {
            [3, 5]
                .into_iter()
                .enumerate()
                .map(|(index, n)| {
                    (
                        slot(
                            source,
                            &Owner::Atom {
                                at: plan.at.site.clone(),
                                index: index as u32,
                            },
                        )
                        .unwrap(),
                        Value::Int(n),
                    )
                })
                .collect::<Vec<_>>()
        };
        let bound = values(target, &inputs);
        for (name, expected) in [
            ("a", Value::Int(3)),
            ("b", Value::Int(5)),
            ("c", Value::Absent),
        ] {
            let actual = bound.iter().find(|(formal, _)| formal == name).unwrap().1;
            assert!(actual.same(&expected), "{name}: {actual:?} != {expected:?}");
        }
        // A single source body is dispatched with its test intact. Both
        // outcomes execute from this entry, without a successful-path cache.
        assert!(Arc::ptr_eq(
            target.source(),
            &exe.code(target.code()).source_room
        ));
        assert_eq!(target.enter().phase, Phase::Enter);
        assert_eq!(target.leave().phase, Phase::Leave);
        assert_eq!(target.frame_price(), doc::LogicalHeldV1::FRAME);
        if nested {
            let mut parent = plan.at.site.clone();
            parent.path.pop();
            let expected = slot(source, &Owner::Operand(parent.child(0))).unwrap();
            assert_eq!(
                plan.saved_roles()
                    .iter()
                    .map(|role| role.slot())
                    .collect::<Vec<_>>(),
                [expected]
            );
        }
        // A forged same-priced source argument must fail call-plan validation.
        let mut forged = fixtures::plan(&exe, caller, site.0, site.1).unwrap();
        forged.targets[0].corrupt_substitution_for_law();
        assert!(forged.verify(&exe).is_err());
    }
    for shared in [false, true] {
        let capture = if shared {
            "let f = fn() { return a }"
        } else {
            ""
        };
        let definition = format!(
            "function echo(a: Any, b: Any) -> Any\nkernel 1\nmetering statements\nbody {{ {capture} return a }}"
        );
        let run = Embedder::new(&format!(
            "definition {}\nmain {{ let values = [1] return (values, invoke echo(values, values)) }}",
            serde_json::to_string(&definition).unwrap()
        ));
        let exe = executable(&run);
        let caller = exe.main.unwrap();
        let site = call_site(&exe, caller);
        let plan = fixtures::plan(&exe, caller, site.0, site.1).unwrap();
        let target = plan.static_target().unwrap();
        let first = target.substitutions()[0].parameter();
        assert_eq!(first.shared, shared);
        with_lease(|lease| {
            let one = lease.import(&Value::Int(1)).unwrap();
            let list = local(lease, LeaseAllocation::list(&[one]));
            let mut parameters = Vec::new();
            for substitution in target.substitutions() {
                let value = substitution.input(lease, |_| Some(list)).unwrap().unwrap();
                let binding = substitution.parameter();
                let stored = if binding.shared {
                    local(lease, LeaseAllocation::cell(&value))
                } else {
                    value
                };
                parameters.push((binding, stored));
            }
            let first_value = if first.shared {
                let cell = lease.object(parameters[0].1).unwrap();
                lease
                    .read(cell, &LeaseMember::Variable)
                    .unwrap_or_else(|_| panic!("shared argument cell"))
            } else {
                parameters[0].1
            };
            let object = lease.object(first_value).unwrap();
            let two = lease.import(&Value::Int(2)).unwrap();
            accept(
                lease,
                LeaseOp::Member {
                    object,
                    member: LeaseMember::List(0),
                    next: Some(two),
                },
            );
            let saved = plan.saved_roles()[0].read(|_| Some(list)).unwrap();
            let result = plan.result_role().read(|_| Some(first_value)).unwrap();
            for value in [list, parameters[1].1, saved, result] {
                assert_list_item(lease, value, 2);
            }
        });
    }
}

/// A sibling executable or an unreferenced/duplicated source coordinate must
/// never authenticate a call edge simply because its slot prices match.
#[test]
fn call_tables_reject_foreign_and_duplicate_source_edges() {
    let source = "definition \"function id(a: Any) -> Any\\nkernel 1\\nmetering statements\\nbody { return a }\"\nmain { return invoke id(7) }";
    let run = Embedder::new(source);
    let exe = executable(&run);
    let caller = exe.main.unwrap();
    let site = call_site(&exe, caller);
    let draft = drafts::calls(&exe, caller, &[site]).unwrap();
    let plan = compile_calls(&exe, &draft)
        .unwrap()
        .into_vec()
        .pop()
        .unwrap();
    let sibling = executable(&Embedder::new(source));
    assert!(plan.verify(&sibling).is_err());
    assert!(compile_calls(&sibling, &draft).is_err());
    let duplicate = drafts::calls(&exe, caller, &[site, site]).unwrap();
    assert!(compile_calls(&exe, &duplicate).is_err());
    let target = plan.static_target().unwrap();
    let owner = if matches!(exe.statement(site.0).0.ops[site.1 as usize], Op::Act) {
        Owner::Atom {
            at: plan.at.site.clone(),
            index: 0,
        }
    } else {
        Owner::Operand(plan.at.site.child(0))
    };
    let argument = slot(&exe.code(caller).source_room, &owner).unwrap();
    assert!(
        values(target, &[(argument, Value::Int(7))])[0]
            .1
            .same(&Value::Int(7))
    );
    with_lease(|lease| {
        let value = lease.import(&Value::Int(7)).unwrap();
        assert_eq!(
            target
                .debt()
                .completion(lease, &[value], value)
                .unwrap_or_else(|_| panic!("canonical call debt")),
            Some(0)
        );
    });
}

/// Closure identity sorts capture names, but actual cells follow the body's
/// lexical capture order. Confusing those orders rejects or swaps captures.
#[test]
fn captures_keep_cell_order_instead_of_sorted_identity_names() {
    let reversed = lash_kernel_value::Name::new("seed") < lash_kernel_value::Name::new("offset");
    let expression = if reversed {
        "num.add(offset, seed)"
    } else {
        "num.add(seed, offset)"
    };
    let maker = format!(
        "function make(seed: Any, offset: Any) -> Any\nkernel 1\nmetering statements\nbody {{ return fn(a, b) {{ return {expression} }} }}"
    );
    let callback = "function callback(f: Any, outer: Any) -> Any\nkernel 1\nmetering statements\nbody { let answer = (outer, apply f(3, 4)) return answer }";
    let source = format!(
        "definition {}\ndefinition {}\nmain {{ let f = invoke make(9, 7) return invoke callback(f, 0) }}",
        serde_json::to_string(&maker).unwrap(),
        serde_json::to_string(callback).unwrap()
    );
    let run = Embedder::new(&source);
    let (program, _) = run.program(true);
    let exe = executable(&run);
    let caller = (0..exe.code_count())
        .map(|index| CodeId(index as u32))
        .find(|code| {
            exe.code(*code).source_room.params.as_ref()
                == [
                    lash_kernel_value::Name::new("f"),
                    lash_kernel_value::Name::new("outer"),
                ]
        })
        .unwrap();
    let statement = exe.block(exe.code(caller).body).stmts[0];
    let (stream, stmt, _) = exe.statement(statement);
    let pc = (stmt.ops.start..stmt.ops.end())
        .rfind(|pc| matches!(stream.ops[*pc as usize], Op::Act | Op::Nested { .. }))
        .unwrap();
    let callback = crate::compile::room::publication::sort::CallbackProgram::compile(
        &exe, caller, statement, pc,
    )
    .unwrap();
    let plan = callback.call();
    let draft = drafts::calls(&exe, caller, &[(statement, pc)]).unwrap();
    let RegionOp::Call { site, .. } = draft.ops()[0] else {
        unreachable!()
    };
    callback.verify_region(&exe, &draft, site).unwrap();
    let target = plan
        .targets()
        .iter()
        .find(|target| exe.code(target.code()).captures.len() == 2)
        .unwrap();
    let body = exe.code(target.code());
    let actual_names = body
        .captures
        .iter()
        .map(|capture| body.slots[capture.inner as usize].name.clone())
        .collect::<Vec<_>>();
    let at = body.created.as_ref().unwrap();
    let origin = doc::snapshot::closure_at(
        &program.document,
        at,
        program.link,
        exe.helpers(),
        exe.selection(),
    )
    .unwrap();
    let identity = |link| {
        Arc::new(doc::arena::ClosureIdentity {
            capture_names: body.capture_names.clone(),
            callable: lash_kernel_value::CallableId {
                link,
                function: origin.function,
            },
            site: doc::linked_site::LinkedSite {
                link,
                site: at.clone(),
            },
        })
    };
    with_lease(|lease| {
        let cells = actual_names
            .iter()
            .map(|name| {
                let value = lease
                    .import(&Value::Int(if name.as_str() == "seed" { 9 } else { 7 }))
                    .unwrap();
                local(lease, LeaseAllocation::cell(&value))
            })
            .collect::<Vec<_>>();
        let actual = local(
            lease,
            LeaseAllocation::closure(identity(program.link), &cells),
        );
        let bound_roles = {
            let classified = callback
                .classify(&exe, lease, Some(actual))
                .unwrap_or_else(|_| panic!("local callable view"))
                .unwrap();
            assert_eq!(classified.plan().code(), target.code());
            let crate::compile::room::publication::sort::CallbackTarget::Value(bound) = classified
            else {
                panic!("source callback is dynamic");
            };
            bound.capture_roles().collect::<Vec<_>>()
        };
        for (role, cell) in &bound_roles {
            let name = actual_names
                .iter()
                .find(|name| {
                    slot(
                        target.source(),
                        &Owner::Binding {
                            declared: target.source().site.clone(),
                            name: (*name).clone(),
                        },
                    )
                    .unwrap()
                        == role.slot()
                })
                .unwrap();
            let cell = lease.object(*cell).unwrap();
            let value = lease
                .read(cell, &LeaseMember::Variable)
                .unwrap_or_else(|_| panic!("capture cell read"));
            assert!(
                lease
                    .scalar(value)
                    .unwrap()
                    .same(&Value::Int(if name.as_str() == "seed" { 9 } else { 7 }))
            );
        }
        // Replacing a captured cell's contents must be observed through the
        // very same captured role, rather than an entry-value snapshot.
        let one = lease.import(&Value::Int(1)).unwrap();
        let list = local(lease, LeaseAllocation::list(&[one]));
        let cell = lease.object(bound_roles[0].1).unwrap();
        accept(
            lease,
            LeaseOp::Member {
                object: cell,
                member: LeaseMember::Variable,
                next: Some(list),
            },
        );
        let alias = lease
            .read(cell, &LeaseMember::Variable)
            .unwrap_or_else(|_| panic!("changed capture read"));
        let list_object = lease.object(alias).unwrap();
        let changed = lease.import(&Value::Int(42)).unwrap();
        accept(
            lease,
            LeaseOp::Member {
                object: list_object,
                member: LeaseMember::List(0),
                next: Some(changed),
            },
        );
        assert_list_item(lease, list, 42);
        let foreign = local(
            lease,
            LeaseAllocation::closure(
                identity(lash_kernel_value::LinkId::from_bytes([99; 32])),
                &cells,
            ),
        );
        assert!(
            callback
                .classify(&exe, lease, Some(foreign))
                .unwrap_or_else(|_| panic!("foreign callable classification"))
                .is_none()
        );
    });
}

fn with_lease(test: impl FnOnce(&mut StateLease<'_>)) {
    use lash_kernel_state::paged::{EditEpoch, SemanticHeap};
    let mut heap = PagedHeap::fresh(Box::new(NoPages), EditEpoch(1));
    let mut arena = Arena::new();
    let mut lease = StateLease::acquire(&mut heap, &mut arena).unwrap();
    test(&mut lease);
}
struct NoPages;
impl lash_kernel_state::paged::PageSource for NoPages {
    fn read(
        &mut self,
        _: &[doc::heap::PageRef],
    ) -> Result<
        Vec<Result<lash_kernel_state::paged::PageAnswer, doc::heap::HeapFault>>,
        doc::heap::HeapFault,
    > {
        Err(doc::heap::HeapFault::Malformed(
            "local call used canonical page reads".into(),
        ))
    }
    fn seal(
        &mut self,
        _: lash_kernel_state::paged::PageBody,
    ) -> Result<doc::heap::PageRef, doc::heap::HeapFault> {
        Err(doc::heap::HeapFault::Malformed(
            "local call published a canonical page".into(),
        ))
    }
}
fn local(lease: &mut StateLease<'_>, allocation: LeaseAllocation<'_>) -> LeaseValue {
    let value = lease
        .stage(LeaseOp::Allocate(allocation))
        .unwrap_or_else(|_| panic!("local allocation stage"))
        .reserve()
        .unwrap()
        .accept()
        .unwrap();
    accept(lease, LeaseOp::Hold { value, count: 1 });
    value
}
fn accept(lease: &mut StateLease<'_>, op: LeaseOp<'_>) {
    lease
        .stage(op)
        .unwrap_or_else(|_| panic!("local call phase"))
        .reserve()
        .unwrap()
        .accept();
}
fn assert_list_item(lease: &mut StateLease<'_>, value: LeaseValue, expected: i64) {
    let object = lease.object(value).unwrap();
    let actual = lease
        .read(object, &LeaseMember::List(0))
        .unwrap_or_else(|_| panic!("shared local list read"));
    assert!(lease.scalar(actual).unwrap().same(&Value::Int(expected)));
}
