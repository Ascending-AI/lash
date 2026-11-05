//! Corruption witnesses for FIG-7632: source proof fields cannot be substituted.
use super::*;
use crate::laws::embedder::{Embedder, Setup};

fn scalar_probes(
    registry: &mut doc::FunctionRegistry,
    _: &std::collections::BTreeMap<&'static str, FunctionId>,
) -> Vec<(&'static str, FunctionId)> {
    struct Scalar(doc::scalar::ScalarOp);
    impl Scalar {
        fn answer(op: doc::scalar::ScalarOp, args: &[Value]) -> Option<Value> {
            match args {
                [Value::Int(a), Value::Int(b)] => match op {
                    doc::scalar::ScalarOp::Add => a.checked_add(*b).map(Value::Int),
                    doc::scalar::ScalarOp::Lt => Some(Value::Bool(a < b)),
                    _ => None,
                },
                [Value::Float(a), Value::Float(b)] => match op {
                    doc::scalar::ScalarOp::Add => {
                        Some(Value::Float(doc::Float::new(a.get() + b.get())))
                    }
                    doc::scalar::ScalarOp::Lt => Some(Value::Bool(a.get() < b.get())),
                    _ => None,
                },
                _ => None,
            }
        }
    }
    impl doc::NativeFunction for Scalar {
        fn call(&self, call: doc::NativeCall<'_, '_>) -> Result<Value, doc::NativeError> {
            Self::answer(self.0, call.args.values())
                .ok_or_else(|| doc::NativeError::refused("type", "word inputs required"))
        }
        fn resource_contract(
            &self,
            args: doc::Args<'_>,
        ) -> Result<Option<doc::NativeResourceContract>, doc::NativeError> {
            Self::answer(self.0, args.values())
                .map(|value| doc::NativeResourceContract::returned(args, &value))
                .transpose()
        }
        fn specialization(&self) -> doc::NativeSpecialization {
            doc::NativeSpecialization {
                immediate: self.prepare(),
                immediate_resources: true,
                ..Default::default()
            }
        }
        fn prepare(&self) -> Option<doc::PreparedNative> {
            let entry = match self.0 {
                doc::scalar::ScalarOp::Add => {
                    |args: &[Value]| Self::answer(doc::scalar::ScalarOp::Add, args)
                }
                doc::scalar::ScalarOp::Lt => {
                    |args: &[Value]| Self::answer(doc::scalar::ScalarOp::Lt, args)
                }
                _ => return None,
            };
            Some(doc::PreparedNative {
                entry,
                charged_entry: None,
                operation: "isa.word",
                pair: Some(doc::scalar::Pair {
                    ints: Some(self.0),
                    floats: Some(self.0),
                }),
            })
        }
    }
    let mut probes: Vec<_> = [("isa.add", doc::scalar::ScalarOp::Add), ("isa.lt", doc::scalar::ScalarOp::Lt)].into_iter().map(|(name, op)| {
        let definition = doc::parse_definition(&format!("function {name}(a: Any, b: Any) -> {}\nkernel 1\ncharge sum(3, size(a), size(b), size(result))\nnative\n", if op.compares() { "Bool" } else { "Any" })).unwrap();
        (name, registry.register(definition, Some(Arc::new(Scalar(op)))).unwrap())
    }).collect();
    for (name, result) in [("isa.first", "a"), ("isa.second", "b")] {
        let definition = doc::parse_definition(&format!(
            "function {name}(a: Any, b: Any) -> Any\nkernel 1\ncharge 1\nnative\nbody {{ return {result} }}"
        ))
        .unwrap();
        probes.push((name, registry.register(definition, None).unwrap()));
    }
    probes
}

fn fixture(check: impl FnOnce(&Executable, CodeId, RegionDraft)) {
    fixture_body(
        "function counter() -> Any\nkernel 1\nmetering statements\nbody { let i = 0 while isa.lt(i, 4) { set i = isa.add(i, 1) } return i }",
        check,
    );
}
fn fixture_body(definition: &str, check: impl FnOnce(&Executable, CodeId, RegionDraft)) {
    let body = crate::laws::embedder::fragment(definition);
    let run = Embedder::with(
        &format!("definition \"{body}\"\nmain {{ let answer = call counter() finish answer }}"),
        Setup {
            probes: Some(scalar_probes),
            ..Setup::default()
        },
    );
    let (program, _) = run.program(true);
    let admitted = program
        .library
        .admit(&program.document, Default::default(), &program.selection)
        .unwrap_or_else(|_| panic!("source admission"));
    let exe = admitted
        .executable(
            &program.document,
            &program.library,
            &program.documents,
            &program.functions,
            &program.instances,
        )
        .unwrap_or_else(|_| panic!("source executable"));
    let code = exe
        .declared
        .values()
        .copied()
        .find(|code| {
            exe.code(*code)
                .source_room
                .body
                .iter()
                .any(|stmt| matches!(stmt, doc::Stmt::While { .. }))
        })
        .unwrap();
    let draft = compile_region(&exe, code).unwrap();
    let plans = crate::compile::room::region::calls::region_isa::compile_calls(&exe, &draft)
        .unwrap_or_else(|reason| panic!("source fixture calls must compile: {reason:?}"));
    verify_region(&exe, code, compile_region(&exe, code).unwrap(), &plans)
        .unwrap_or_else(|reason| panic!("source fixture must verify: {reason:?}"));
    check(&exe, code, draft);
}
#[test]
fn rejects_an_understated_source_peak() {
    fixture(|exe, code, mut draft| {
        draft
            .steps
            .iter_mut()
            .find(|step| step.event.peak.fixed > 0)
            .unwrap()
            .event
            .peak
            .fixed -= 1;
        assert_eq!(
            verify_region(exe, code, draft, &[]).err(),
            Some(Refused::Peak)
        );
    });
}
#[test]
fn rejects_a_missing_reconstruction_prefix() {
    fixture(|exe, code, mut draft| {
        let mut maps = draft.recovery.into_vec();
        maps.pop();
        draft.recovery = maps.into();
        assert_eq!(
            verify_region(exe, code, draft, &[]).err(),
            Some(Refused::Exit)
        );
    });
}
#[test]
fn rejects_a_substituted_charge_identity() {
    fixture(|exe, code, mut draft| {
        let close = draft
            .steps
            .iter_mut()
            .flat_map(|step| step.charges.iter_mut())
            .find(|close| !close.regions.is_empty())
            .unwrap();
        close.regions[0].head = close.regions[0].head.checked_add(1).unwrap();
        assert_eq!(
            verify_region(exe, code, draft, &[]).err(),
            Some(Refused::Source)
        );
    });
}
#[test]
fn rejects_a_control_edge_outside_the_program() {
    fn corrupt(exe: &Executable, code: CodeId, mut draft: RegionDraft) {
        let branch = draft
            .ops
            .iter_mut()
            .find(|op| matches!(op, RegionOp::Branch { .. }))
            .unwrap();
        let RegionOp::Branch { yes, .. } = branch else {
            unreachable!()
        };
        *yes = Pc(u32::MAX);
        assert_eq!(
            verify_region(exe, code, draft, &[]).err(),
            Some(Refused::Path)
        );
    }
    fixture(corrupt);
    // A completed statement conditional must reach its following statement.
    // Its rejected arm also has to stop at the first operand prefix.
    fixture_body(
        "function counter() -> Any\nkernel 1\nmetering statements\nbody { let i = 0 if isa.lt(i, 0) { throw [1, 2] } while isa.lt(i, 4) { set i = isa.add(i, 1) } return i }",
        corrupt,
    );
}
#[test]
fn rejects_an_untyped_branch_register() {
    fixture(|exe, code, mut draft| {
        let branch = draft
            .ops
            .iter_mut()
            .find(|op| matches!(op, RegionOp::Branch { .. }))
            .unwrap();
        let RegionOp::Branch { test, .. } = branch else {
            unreachable!()
        };
        *test = Reg::new(draft.roles.len()).unwrap();
        assert_eq!(
            verify_region(exe, code, draft, &[]).err(),
            Some(Refused::Facts)
        );
    });
}

/// Amendment 2: a recovery map must retain its actual source-to-local binding.
#[test]
fn rejects_a_substituted_graph_identity_binding() {
    fixture(|exe, code, mut draft| {
        assert!(
            draft
                .recovery
                .iter_mut()
                .any(|map| map.graph.substitute_identity())
        );
        assert_eq!(
            verify_region(exe, code, draft, &[]).err(),
            Some(Refused::Exit)
        );
    });
}

fn heap_fixture(check: impl FnOnce(&Executable, CodeId, RegionDraft)) {
    fixture_body(
        "function counter() -> Any\nkernel 1\nmetering statements\nbody { let obj = {a: 1} set obj.b = 2 let i = 0 while isa.lt(i, 4) { set i = isa.add(i, 1) } return i }",
        check,
    );
}
/// Amendment 2: a constructor cannot skip its source operand release.
#[test]
fn rejects_a_missing_heap_release_phase() {
    fn corrupt(exe: &Executable, code: CodeId, mut draft: RegionDraft) {
        let kernel = draft
            .ops
            .iter_mut()
            .find_map(|op| match op {
                RegionOp::Heap { kernel, .. } => Some(kernel),
                _ => None,
            })
            .unwrap();
        kernel.phases = vec![kernel.phases[0].clone()].into();
        assert_eq!(
            verify_region(exe, code, draft, &[]).err(),
            Some(Refused::Source)
        );
    }
    heap_fixture(corrupt);
    for literal in [
        "\"a heap backed literal longer than short text\"",
        "b\"0011\"",
    ] {
        fixture_body(
            &format!(
                "function counter() -> Any\nkernel 1\nmetering statements\nbody {{ let text = {literal} let i = 0 while isa.lt(i, 4) {{ set i = isa.add(i, 1) }} return i }}"
            ),
            corrupt,
        );
    }
}
/// Amendment 2: member growth must retain its canonical transient peak.
#[test]
fn rejects_an_understated_heap_growth_peak() {
    heap_fixture(|exe, code, mut draft| {
        let coefficient = draft
            .ops
            .iter_mut()
            .find_map(|op| match op {
                RegionOp::Heap { kernel, .. } if matches!(kernel.kind, HeapKind::Field { .. }) => {
                    kernel.phases[0].peak.terms.iter_mut().find_map(
                        |(fact, coefficient)| match fact {
                            ResourceInput::SourceFact(
                                crate::compile::room::Measure::MemberGrowth(_),
                            ) => Some(coefficient),
                            _ => None,
                        },
                    )
                }
                _ => None,
            })
            .unwrap();
        *coefficient -= 1;
        assert_eq!(
            verify_region(exe, code, draft, &[]).err(),
            Some(Refused::Peak)
        );
    });
}

/// A scalar replacement does not make the replaced graph scalar.
#[test]
fn rejects_a_scalar_claim_for_a_graph_replacement() {
    fixture_body(
        "function counter() -> Any\nkernel 1\nmetering statements\nbody { let obj = {a: 1} set obj = 0 let i = 0 while isa.lt(i, 4) { set i = isa.add(i, 1) } return i }",
        |exe, code, mut draft| {
            let index = draft
                .ops
                .iter()
                .enumerate()
                .find_map(|(index, op)| {
                    if !matches!(op, RegionOp::Move { .. }) {
                        return None;
                    }
                    match resources::node(&draft.source, &draft.steps[index].at.site).unwrap() {
                        doc::Node::Stmt(doc::Stmt::Assign {
                            place: doc::Place::Variable(name),
                            ..
                        }) if name.as_str() == "obj" => Some(index),
                        _ => None,
                    }
                })
                .unwrap();
            draft.steps[index].scalar_resources = true;
            assert_eq!(
                verify_region(exe, code, draft, &[]).err(),
                Some(Refused::Retained)
            );
        },
    );
}

/// K-CHG-002: re-entering a while statement at its backedge charges the
/// statement again. The source result is 4. Its charge is 2 + 1 + 5 + 15
/// + 39 + 4 + 12 + 35 + 2 = 115, from the declared form/native costs.
#[test]
fn a_backedge_keeps_values_and_does_not_recharge_loop_entry() {
    fixture(|exe, code, draft| {
        let code = verify_region(exe, code, draft, &[])
            .unwrap_or_else(|reason| panic!("verified fixture: {reason:?}"));
        for code in [Arc::clone(&code), code.as_generic()] {
            let (value, charge) = run_fixture(exe, &code);
            assert!(value.same(&Value::Int(4)));
            assert_eq!(charge, 115);
        }
    });
    // Float sizes are all one, so the same nine native formulas cost 9*6.
    // The 41 fixed form units remain: 41 + 54 = 95.
    fixture_body(
        "function counter() -> Any\nkernel 1\nmetering statements\nbody { let i = 0.0 while isa.lt(i, 4.0) { set i = isa.add(i, 1.0) } return i }",
        |exe, code, draft| {
            let code = verify_region(exe, code, draft, &[])
                .unwrap_or_else(|reason| panic!("float fixture: {reason:?}"));
            for code in [Arc::clone(&code), code.as_generic()] {
                let (value, charge) = run_fixture(exe, &code);
                assert!(value.same(&Value::Float(doc::Float::new(4.0))));
                assert_eq!(charge, 95);
            }
        },
    );
}

// A fixed test runner for the compiler's word fixture, not a production
// constructor, executor or resource oracle. E owns full prefix comparison.
fn run_fixture(exe: &Executable, code: &RegionCode) -> (Value, u64) {
    use crate::compile::room::charges::region_isa::{ChargeItem, ChargeMoment};
    let mut values = vec![None; code.registers().len()];
    let mut pc = code.entry();
    let mut charged = 0_u64;
    for _ in 0..512 {
        let op = code.op(pc).unwrap();
        let step = code.step(pc).unwrap();
        let fees = |moment, values: &[Option<Value>]| -> u64 {
            step.charges
                .iter()
                .filter(|close| close.moment == moment)
                .flat_map(|close| close.tape.iter())
                .map(|item| match item {
                    ChargeItem::Fixed { weight, .. } => weight.units(),
                    ChargeItem::Formula { .. } => match op {
                        RegionOp::IntBinary {
                            kernel,
                            lhs,
                            rhs,
                            dst,
                            ..
                        } => {
                            let (
                                Some(Value::Int(lhs)),
                                Some(Value::Int(rhs)),
                                Some(Value::Int(result)),
                            ) = (
                                values[lhs.offset()],
                                values[rhs.offset()],
                                values[dst.offset()],
                            )
                            else {
                                panic!("word fee inputs");
                            };
                            kernel.charge(lhs, rhs, result)
                        }
                        RegionOp::Checked {
                            kernel:
                                CheckedKernel {
                                    kind:
                                        CheckedKind::IntTruth {
                                            lhs, rhs, units, ..
                                        },
                                },
                            ..
                        } => {
                            let (Some(Value::Int(lhs)), Some(Value::Int(rhs))) =
                                (values[lhs.offset()], values[rhs.offset()])
                            else {
                                panic!("truth fee inputs");
                            };
                            units[usize::from(lhs != 0) | usize::from(rhs != 0) << 1]
                        }
                        RegionOp::Checked {
                            kernel:
                                CheckedKernel {
                                    kind: CheckedKind::Native { .. },
                                },
                            args,
                            dst,
                            ..
                        } => {
                            let lhs = values[args[0].offset()].unwrap();
                            let rhs = values[args[1].offset()].unwrap();
                            let result = values[dst.offset()].unwrap();
                            let mask = doc::scalar::size_bit(lhs).unwrap()
                                | doc::scalar::size_bit(rhs).unwrap() << 1
                                | doc::scalar::size_bit(result).unwrap() << 3;
                            declared_fixture_fee(mask)
                        }
                        RegionOp::Checked {
                            kernel:
                                CheckedKernel {
                                    kind: CheckedKind::FloatBinary { units, .. },
                                },
                            ..
                        } => *units,
                        _ => panic!("fixture formula belongs to its actual semantic operation"),
                    },
                })
                .sum()
        };
        charged += fees(ChargeMoment::Before, &values);
        if let Some(guard) = &step.guard {
            match guard.kind() {
                CheckedKind::Truth { input, output } => {
                    let Some(Value::Bool(value)) = values[input.offset()] else {
                        panic!("truth guard");
                    };
                    values[output.offset()] = Some(Value::Bool(value));
                }
                CheckedKind::UnpackFloats { inputs, outputs } => {
                    for (input, output) in inputs.iter().zip(outputs) {
                        let Some(Value::Float(value)) = values[input.offset()] else {
                            panic!("float guard");
                        };
                        values[output.offset()] = Some(Value::Float(value));
                    }
                }
                CheckedKind::UnpackPair { inputs, outputs } => {
                    for (input, output) in inputs.iter().zip(outputs) {
                        let Some(Value::Int(value)) = values[input.offset()] else {
                            panic!("word guard");
                        };
                        values[output.offset()] = Some(Value::Int(value));
                    }
                }
                _ => panic!("fixture guard"),
            }
        }
        let next = match op {
            RegionOp::Constant { dst, value, next } => {
                values[dst.offset()] = Some(*value);
                *next
            }
            RegionOp::Move { dst, src, next } => {
                values[dst.offset()] = values[src.offset()];
                *next
            }
            RegionOp::IntBinary {
                kernel,
                lhs,
                rhs,
                dst,
                next,
            } => {
                let (Some(Value::Int(lhs)), Some(Value::Int(rhs))) =
                    (values[lhs.offset()], values[rhs.offset()])
                else {
                    panic!("word operands");
                };
                values[dst.offset()] = Some(Value::Int(kernel.apply(lhs, rhs).unwrap()));
                *next
            }
            RegionOp::Checked {
                kernel,
                args,
                dst,
                next,
            } => {
                match kernel.kind() {
                    CheckedKind::Local { slot } => {
                        let (_, source) = code
                            .roles()
                            .iter()
                            .find(|(candidate, _)| candidate == slot)
                            .unwrap();
                        values[dst.offset()] = values[source.offset()];
                    }
                    CheckedKind::IntTruth {
                        operation,
                        lhs,
                        rhs,
                        ..
                    } => {
                        let (Some(Value::Int(lhs)), Some(Value::Int(rhs))) =
                            (values[lhs.offset()], values[rhs.offset()])
                        else {
                            panic!("word comparison");
                        };
                        values[dst.offset()] = Some(Value::Bool(
                            doc::scalar::int_truth(*operation, lhs, rhs).unwrap(),
                        ));
                    }
                    CheckedKind::FloatBinary {
                        lhs,
                        rhs,
                        operation,
                        ..
                    } => {
                        let (Some(Value::Float(lhs)), Some(Value::Float(rhs))) =
                            (values[lhs.offset()], values[rhs.offset()])
                        else {
                            panic!("float operands");
                        };
                        values[dst.offset()] = Some(if operation.compares() {
                            Value::Bool(kernel.float_truth(lhs.get(), rhs.get()).unwrap())
                        } else {
                            Value::Float(doc::Float::new(
                                kernel.float_number(lhs.get(), rhs.get()).unwrap(),
                            ))
                        });
                    }
                    CheckedKind::Native { lib, .. } => {
                        let [lhs, rhs] = args.as_ref() else {
                            panic!("fixture arity");
                        };
                        let (Some(lhs), Some(rhs)) = (values[lhs.offset()], values[rhs.offset()])
                        else {
                            panic!("checked operands");
                        };
                        let pair = exe
                            .lib(*lib)
                            .specialization
                            .prepared
                            .as_ref()
                            .and_then(|(entry, _)| entry.pair())
                            .unwrap();
                        values[dst.offset()] = Some(match (lhs, rhs) {
                            (Value::Int(a), Value::Int(b)) => {
                                doc::scalar::ints(pair.ints.unwrap(), a, b).unwrap()
                            }
                            (Value::Float(a), Value::Float(b)) => {
                                doc::scalar::floats(pair.floats.unwrap(), a, b).unwrap()
                            }
                            _ => panic!("fixture numeric operands"),
                        });
                    }
                    CheckedKind::Phase | CheckedKind::LoopControl { .. } => {}
                    _ => panic!("fixture checked operation"),
                }
                *next
            }
            RegionOp::Branch { test, yes, no } => {
                let Some(Value::Bool(value)) = values[test.offset()] else {
                    panic!("branch truth");
                };
                if value { *yes } else { *no }
            }
            RegionOp::Return { value } => return (values[value.offset()].unwrap(), charged),
            RegionOp::Boundary { spec }
                if spec
                    .origin
                    .is_some_and(|origin| matches!(origin.op, Op::Return { .. })) =>
            {
                let owner = &step.event.inputs[0];
                let role = resources::slot(code.source(), owner).unwrap();
                let (_, value) = code.roles().iter().find(|(slot, _)| *slot == role).unwrap();
                return (values[value.offset()].unwrap(), charged);
            }
            _ => panic!(
                "fixture stopped at {pc:?}, source {:?}, opcode {:?}, node {:?}",
                step.at,
                std::mem::discriminant(op),
                resources::node(code.source(), &step.at.site).unwrap()
            ),
        };
        charged += fees(ChargeMoment::After, &values);
        pc = next;
    }
    panic!("compiled fixture exceeded its bounded control path")
}

fn declared_fixture_fee(mask: u8) -> u64 {
    // All fixture natives state sum(3, size(a), size(b), size(result)).
    6 + u64::from(mask & 1 != 0) + u64::from(mask & 2 != 0) + u64::from(mask & 8 != 0)
}

/// Valid callee plans cannot authorize another source call's operation row.
#[test]
fn rejects_a_substituted_call_origin() {
    fixture_body(
        "function counter() -> Any\nkernel 1\nmetering statements\nbody { let i = isa.first(0, 1) let j = isa.second(2, 3) while isa.lt(i, 4) { set i = isa.add(i, 1) } return j }",
        |exe, code, mut draft| {
            let original = draft.clone();
            let first = draft.calls[0].origin;
            draft.calls[0].origin = draft.calls[1].origin;
            draft.calls[1].origin = first;
            draft.call_sources.swap(0, 1);
            let plans = crate::compile::room::region::calls::region_isa::compile_calls(exe, &draft)
                .unwrap_or_else(|reason| {
                    panic!("substituted source calls must remain valid B plans: {reason:?}")
                });
            assert_eq!(
                verify_region(exe, code, draft, &plans).err(),
                Some(Refused::Source)
            );
            let plans =
                crate::compile::room::region::calls::region_isa::compile_calls(exe, &original)
                    .unwrap();
            let mut invalid_statement = original;
            invalid_statement.calls[0].origin.statement = StmtId(u32::MAX);
            assert_eq!(
                verify_region(exe, code, invalid_statement, &plans).err(),
                Some(Refused::Source)
            );
        },
    );
}

#[test]
fn return_releases_entered_scopes_before_the_defining_frame() {
    fixture_body(
        "function counter() -> Any\nkernel 1\nmetering statements\nbody { let outer = 9 while isa.lt(0, 1) { let inner = 7 if isa.lt(0, 1) { return inner } else { return outer } } return outer }",
        |exe, code, draft| {
            let route = draft
                .departures
                .iter()
                .find(|route| route.leaves.len() == 3)
                .unwrap();
            let origin = route.origin;
            let at = draft.steps[origin.offset()].at.clone();
            let scope = |path| Point {
                site: Site {
                    unit: at.site.unit.clone(),
                    path,
                },
                phase: Phase::Leave,
            };
            let mut inner = at.site.path.clone();
            inner.pop();
            let mut body = inner.clone();
            body.pop();
            body.pop();
            let expected = [
                scope(inner),
                scope(body),
                Point {
                    site: draft.source.site.clone(),
                    phase: Phase::Leave,
                },
            ];
            let plans = crate::compile::room::region::calls::region_isa::compile_calls(exe, &draft)
                .unwrap();
            let region = verify_region(exe, code, draft.clone(), &plans).unwrap();
            assert_eq!(
                region.return_successor(origin),
                route.leaves.first().copied()
            );
            for (pc, expected) in route.leaves.iter().zip(&expected) {
                assert_eq!(&region.step(*pc).unwrap().at, expected);
                assert_eq!(region.departure_at(&at, expected), Some(*pc));
                let map = region.recovery(region.step(*pc).unwrap().after).unwrap();
                assert!(
                    map.roles
                        .iter()
                        .any(|(slot, _)| matches!(slot, Slot::Departure(_)))
                );
            }
            let last = *route.leaves.last().unwrap();
            assert!(matches!(region.op(last), Some(RegionOp::Exit { .. })));
            let mut bad = draft.clone();
            let RegionOp::Checked { next, .. } = &mut bad.ops[route.leaves[0].offset()] else {
                panic!("block Leave")
            };
            *next = last;
            assert_eq!(
                verify_region(exe, code, bad, &plans).err(),
                Some(Refused::Path)
            );
        },
    );
}

/// FIG-7632: tuple construction transfers operand occurrences; gross result
/// price must not invent an unaffordable intermediate balance.
#[test]
fn tuple_allocation_moves_inputs_at_the_source_peak() {
    fixture_body(
        "function counter() -> Any\nkernel 1\nmetering statements\nbody { let tuple = (7, 9) let i = 0 while isa.lt(i, 4) { set i = isa.add(i, 1) } return tuple }",
        |exe, code, draft| {
            let (index, kernel) = draft
                .ops
                .iter()
                .enumerate()
                .find_map(|(index, op)| match op {
                    RegionOp::Heap { kernel, .. } if kernel.kind == HeapKind::Tuple => {
                        Some((index, kernel))
                    }
                    _ => None,
                })
                .unwrap();
            let phase = &kernel.phases[0];
            let delta = phase
                .delta
                .evaluate(|fact| {
                    Some(match fact {
                        ResourceInput::Result => 50,
                        ResourceInput::Release(_) => 34,
                        _ => panic!("tuple has only its result and moved operands"),
                    })
                })
                .unwrap();
            assert_eq!(
                delta, 16,
                "the source transfers the two 17-byte operands into the 50-byte tuple"
            );
            assert_eq!(phase.peak.fixed, 16);
            assert!(phase.peak.terms.is_empty());
            assert_eq!(phase.reached, draft.steps[index].after);
            let before = 64 + 17 + 17;
            assert_eq!(before + delta, 114);
            assert_eq!(before + phase.peak.fixed, 114);
            let mut bad = draft.clone();
            let RegionOp::Heap { kernel, .. } = &mut bad.ops[index] else {
                unreachable!()
            };
            kernel.phases[0].delta.terms = kernel.phases[0]
                .delta
                .terms
                .iter()
                .filter(|(fact, _)| !matches!(fact, ResourceInput::Release(_)))
                .cloned()
                .collect();
            assert_eq!(
                verify_region(exe, code, bad, &[]).err(),
                Some(Refused::Retained)
            );
        },
    );
}

/// FIG-7632: an accepted allocation pending release needs its canonical output
/// root. A sink-only result disappears from StateExit's mapped roles.
#[test]
fn allocated_pending_values_have_their_source_output_role() {
    heap_fixture(|exe, code, draft| {
        let (index, dst) = draft
            .ops
            .iter()
            .enumerate()
            .find_map(|(index, op)| match op {
                RegionOp::Heap {
                    kernel,
                    dst: Some(dst),
                    ..
                } if matches!(kernel.kind, HeapKind::Record(_)) => Some((index, *dst)),
                _ => None,
            })
            .unwrap();
        let step = &draft.steps[index];
        let output = step.event.output.as_ref().unwrap();
        let slot = resources::slot(&draft.source, output).unwrap();
        let pending = &draft.recovery[step.pending.offset()];
        assert_eq!(pending.pending, Some(dst));
        assert!(pending.roles.contains(&(slot, dst)));
        let before = &draft.recovery[step.before.offset()];
        assert!(before.roles.iter().all(|role| pending.roles.contains(role)));
        let after = &draft.recovery[step.after.offset()];
        assert!(after.roles.contains(&(slot, dst)));
        let mut bad = draft.clone();
        bad.recovery[step.pending.offset()].roles = bad.recovery[step.pending.offset()]
            .roles
            .iter()
            .filter(|(role, _)| *role != slot)
            .copied()
            .collect();
        assert_eq!(
            verify_region(exe, code, bad, &[]).err(),
            Some(Refused::Exit)
        );
    });
}

/// K-EVAL-008: callee completion precedes the caller's distinct Restore.
#[test]
fn a_guest_call_restores_its_incoming_value_before_continuing() {
    fixture_body(
        "function counter() -> Any\nkernel 1\nmetering statements\nbody { let i = isa.first(0, 1) let j = isa.second(2, 3) while isa.lt(i, 4) { set i = isa.add(i, 1) } return j }",
        |exe, code, draft| {
            let (index, next) = draft
                .ops
                .iter()
                .enumerate()
                .find_map(|(index, op)| match op {
                    RegionOp::Call { next, .. } => Some((index, *next)),
                    _ => None,
                })
                .unwrap();
            let expected = Point {
                site: draft.steps[index].at.site.clone(),
                phase: Phase::Restore,
            };
            assert_eq!(draft.steps[next.offset()].at, expected);
            let RegionOp::Move {
                dst,
                src,
                next: continuation,
            } = draft.ops[next.offset()]
            else {
                panic!("Restore must transfer the returned incoming value")
            };
            let output = draft.steps[next.offset()].event.output.as_ref().unwrap();
            assert_eq!(lower::reg(&draft.roles, &draft.source, output), Ok(dst));
            assert_eq!(src.offset(), draft.roles.len());
            assert_eq!(
                draft.recovery[draft.steps[index].after.offset()].pending,
                Some(src)
            );
            assert_eq!(
                draft.recovery[draft.steps[next.offset()].before.offset()].pending,
                Some(src)
            );
            assert_eq!(
                draft.recovery[draft.steps[next.offset()].after.offset()].pending,
                None
            );
            let plans = crate::compile::room::region::calls::region_isa::compile_calls(exe, &draft)
                .unwrap();
            let mut bad = draft;
            let RegionOp::Call { next, .. } = &mut bad.ops[index] else {
                unreachable!()
            };
            *next = continuation;
            assert_eq!(
                verify_region(exe, code, bad, &plans).err(),
                Some(Refused::Path)
            );
        },
    );
}
