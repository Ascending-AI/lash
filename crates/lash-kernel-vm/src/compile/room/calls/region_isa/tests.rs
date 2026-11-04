use super::callee::Substitution;
use super::*;
use crate::compile::region_isa::fixtures as drafts;
use crate::laws::embedder::Embedder;

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
                    *parameter,
                    inputs
                        .iter()
                        .find(|(slot, _)| slot == argument)
                        .expect("source argument must be used")
                        .1,
                ),
                Substitution::Absent { parameter } => (*parameter, Value::Absent),
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
            assert_eq!(plan.saved_roles(), [expected]);
        }
        // A forged same-priced source argument must fail call-plan validation.
        let mut forged = fixtures::plan(&exe, caller, site.0, site.1).unwrap();
        forged.targets[0].corrupt_substitution_for_law();
        assert!(forged.verify(&exe).is_err());
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
    assert_eq!(
        target.debt().completion(&[Value::Int(7)], Value::Int(7)),
        Some(0)
    );
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
    let plan = fixtures::plan(&exe, caller, statement, pc).unwrap();
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
    let mut arena = Arena::new();
    let cells = actual_names
        .iter()
        .map(|name| arena.new_cell(Value::Int(if name.as_str() == "seed" { 9 } else { 7 })))
        .collect::<Vec<_>>();
    let actual = Value::Closure(arena.new_closure(identity(program.link), cells.clone()));
    let bound = plan.resolve_callable(&exe, &arena, actual).unwrap();
    let actual_values = bound
        .captures()
        .iter()
        .map(|binding| {
            let cell = cells[binding.index as usize];
            let name = actual_names
                .iter()
                .find(|name| {
                    slot(
                        bound.source(),
                        &Owner::Binding {
                            declared: bound.source().site.clone(),
                            name: (*name).clone(),
                        },
                    )
                    .unwrap()
                        == binding.slot
                })
                .unwrap();
            (name.as_str(), arena.cell(cell).unwrap())
        })
        .collect::<Vec<_>>();
    for (name, value) in actual_values {
        assert!(value.same(&Value::Int(if name == "seed" { 9 } else { 7 })));
    }
    let foreign = Value::Closure(arena.new_closure(
        identity(lash_kernel_value::LinkId::from_bytes([99; 32])),
        cells,
    ));
    assert!(plan.resolve_callable(&exe, &arena, foreign).is_none());
}
