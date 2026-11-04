#[path = "calls.rs"]
mod calls;
use super::*;
use crate::compile::room::charges::{ChargeProgram, FormulaValues};
use lash_kernel_doc::Value;
use lash_kernel_exec::{Op, OpRange, Weight};

/// Charge totals alone cannot authenticate the source's ordered closes. The
/// same compiled formula and zero/nonzero measurements serve tape and quiet
/// admission, including exact charge-bound and strict slice equality.
pub(super) fn compiled_charges() {
    calls::pure_calls();
    member_closes();
    constructor_debt();
    native_after_ordered_closes();
    for layout in [0, 1, 42, u64::MAX] {
        let text = super::super::layout::scalar_document(
            "let x = flat.add(0, flat.mul(2, 3)) set x = flat.add(x, 4) return x",
        );
        let (program, _) = Embedder::with(
            &text,
            Setup {
                layout: crate::Layout(layout),
                probes: Some(super::super::layout::scalar_probes),
                ..Setup::default()
            },
        )
        .program(true);
        let admitted = program
            .library
            .admit(&program.document, Default::default(), &program.selection)
            .unwrap_or_else(|_| panic!("charge source is admitted"));
        let exe = admitted
            .executable(
                &program.document,
                &program.library,
                &program.documents,
                &program.functions,
                &program.instances,
            )
            .unwrap_or_else(|_| panic!("charge source compiles"));
        let body = exe
            .islands
            .plans
            .iter()
            .find(|plan| plan.params.is_empty() && plan.body.statements.len() == 3)
            .unwrap();
        let statement = body.body.statements[0].id;
        let (_, stmt, _) = exe.statement(statement);
        let mut charges = ChargeProgram::compile(&exe, body.code, statement, stmt.ops).unwrap();
        assert_eq!(charges.verify(&exe, body.code), Ok(()));
        assert_eq!(
            charges
                .formulas()
                .map(|(_, _, count)| count)
                .collect::<Vec<_>>(),
            [2, 2]
        );
        let first = [Value::Int(2), Value::Int(3)];
        let second = [Value::Int(0), Value::Int(6)];
        let values = [
            FormulaValues {
                arguments: &first,
                result: Value::Int(6),
            },
            FormulaValues {
                arguments: &second,
                result: Value::Int(6),
            },
        ];
        let debt = charges.bind(&values).unwrap();
        assert_eq!(
            charges
                .bind_measured(&values, |_, _, _| {
                    panic!("immediate values must not trust a storage-supplied size")
                })
                .unwrap()
                .units(),
            23
        );
        let (stream, _, _) = exe.statement(statement);
        let close = (stmt.ops.start..stmt.ops.end())
            .find(|pc| {
                matches!(
                    stream.ops[*pc as usize],
                    Op::Call { .. } | Op::Operator { .. } | Op::Nested { .. }
                )
            })
            .unwrap()
            + 1;
        let native =
            ChargeProgram::native_completion(&exe, body.code, statement, close - 1).unwrap();
        assert_eq!(native.verify(&exe, body.code), Ok(()));
        let native_debt = native.bind(&values[..1]).unwrap();
        assert_eq!(
            native_debt.units(),
            9,
            "a native completion never repays the statement or read closes"
        );
        assert!(native_debt.matches(
            &exe.code(body.code).source_room,
            native.first(),
            native.first()
        ));
        let native_tape = crate::observe::ChargeTape::new();
        native_debt.record(&native_tape);
        assert_eq!(native_tape.steps(), [Weight::Atomic(9)]);
        let pending = (stmt.ops.start..close - 1)
            .rev()
            .find_map(|pc| {
                if let Op::Region { region, total } = stream.ops[pc as usize] {
                    Some(crate::compile::expr::Close {
                        region: region.0,
                        head: pc,
                        total,
                    })
                } else {
                    None
                }
            })
            .unwrap();
        let packet =
            ChargeProgram::native_packet(&exe, body.code, statement, close - 1, Some(pending))
                .unwrap();
        let packet_debt = packet.bind(&values[..1]).unwrap();
        assert_eq!(
            packet_debt.units(),
            9 + u64::from(pending.total),
            "the packet pays the original pending close exactly once before native completion"
        );
        let packet_tape = crate::observe::ChargeTape::new();
        packet_debt.record(&packet_tape);
        let expected_tape: Vec<_> = stream
            .regions
            .events(pending.region)
            .iter()
            .map(|event| event.weight)
            .chain(std::iter::once(Weight::Atomic(9)))
            .collect();
        assert_eq!(packet_tape.steps(), expected_tape);
        let precise_need = native_debt.native_allowance(7).unwrap();
        let packet_need = packet_debt.native_allowance(7).unwrap();
        assert!(!precise_need.strict && packet_need.strict);
        // This native-only declaration has no logical body/frame to reserve.
        assert_eq!((precise_need.depth, packet_need.depth), (0, 0));
        assert!(precise_need.close.is_none() && packet_need.close.is_none());
        for need in [&precise_need, &packet_need] {
            assert_eq!(need.safe_point, 7);
            let accepted = crate::admission::Window::new(0, need.units, need.units).admit(
                (),
                need.before,
                need.units,
                need.close,
                need.strict,
            );
            assert_eq!(
                accepted.is_some(),
                !need.strict,
                "precise native completion is inclusive; speculative packet entry is strict"
            );
        }
        assert!(
            ChargeProgram::native_packet(&exe, body.code, statement, close - 1, None).is_err(),
            "a packet cannot erase its pending source close"
        );
        let wrong = crate::compile::expr::Close {
            total: pending.total + 1,
            ..pending
        };
        assert!(
            ChargeProgram::native_packet(&exe, body.code, statement, close - 1, Some(wrong))
                .is_err(),
            "the close keeps its original source identity and total"
        );

        let prefix = ChargeProgram::compile(
            &exe,
            body.code,
            statement,
            OpRange {
                start: stmt.ops.start,
                len: close - stmt.ops.start,
            },
        )
        .unwrap();
        let suffix = ChargeProgram::compile(
            &exe,
            body.code,
            statement,
            OpRange {
                start: close,
                len: stmt.ops.end() - close,
            },
        )
        .unwrap();
        let final_call = (stmt.ops.start..stmt.ops.end())
            .rfind(|pc| {
                matches!(
                    stream.ops[*pc as usize],
                    Op::Call { .. } | Op::Operator { .. } | Op::Nested { .. }
                )
            })
            .unwrap()
            + 1;
        let write = ChargeProgram::compile(
            &exe,
            body.code,
            statement,
            OpRange {
                start: final_call,
                len: stmt.ops.end() - final_call,
            },
        )
        .unwrap();
        let at = crate::compile::room::Point {
            site: stmt.site.clone(),
            phase: crate::compile::room::Phase::Commit,
        };
        assert!(
            write
                .bind(&[])
                .unwrap()
                .matches(&exe.code(body.code).source_room, &at, &at),
            "a physical pin cannot turn a completed native into an unpaid source event"
        );
        let prefix_debt = prefix.bind(&values[..1]).unwrap();
        let suffix_debt = suffix.bind(&values[1..]).unwrap();
        assert_eq!(
            prefix_debt.units() + suffix_debt.units(),
            debt.units(),
            "an unpaid suffix never repays the statement or completed native"
        );
        let split_tape = crate::observe::ChargeTape::new();
        prefix_debt.record(&split_tape);
        suffix_debt.record(&split_tape);
        let complete_tape = crate::observe::ChargeTape::new();
        debt.record(&complete_tape);
        assert_eq!(split_tape.steps(), complete_tape.steps());
        let read = (stmt.ops.start..close)
            .find(|pc| matches!(stream.ops[*pc as usize], Op::Const { .. }))
            .unwrap();
        assert!(
            ChargeProgram::compile(
                &exe,
                body.code,
                statement,
                OpRange {
                    start: read + 1,
                    len: stmt.ops.end() - read - 1
                }
            )
            .is_err(),
            "a suffix cannot drop an unpaid source read"
        );
        let mut arena = lash_kernel_state::arena::Arena::new();
        let long = arena.text(&"a".repeat(64));
        let pair = [long, long];
        let detached = [
            FormulaValues {
                arguments: &pair,
                result: long,
            },
            FormulaValues {
                arguments: &pair,
                result: long,
            },
        ];
        assert!(charges.bind(&detached).is_none());
        let mut demanded = Vec::new();
        let measured = charges
            .bind_measured(&detached, |index, source, measure| {
                demanded.push((index, source, measure));
                // Actual UTF-8 bytes plus the canonical size sentinel.
                (measure == lash_kernel_doc::Measure::Size).then_some(65)
            })
            .unwrap();
        assert_eq!(measured.units(), 6 + 2 * (3 + 3 * 65));
        assert_eq!(demanded.len(), 6);
        assert!(
            charges
                .bind_measured(&detached, |_, source, _| {
                    (source != crate::compile::Source::Result).then_some(65)
                })
                .is_none(),
            "a missing detached result measure cannot become zero"
        );

        // One statement, five expression nodes, and the native formulas
        // 3+2+2+2 and 3+1+2+2. These prices come from this law's declarations.
        assert_eq!(debt.units(), 23);
        let tape = crate::observe::ChargeTape::new();
        debt.record(&tape);
        assert_eq!(
            tape.steps()
                .iter()
                .map(|weight| weight.units())
                .sum::<u64>(),
            23
        );
        assert_eq!(
            tape.steps()
                .iter()
                .filter(|weight| matches!(weight, Weight::Atomic(_)))
                .copied()
                .collect::<Vec<_>>(),
            [Weight::Atomic(9), Weight::Atomic(8)]
        );
        let window = |bound, slice| crate::admission::Window::new(7, bound, slice);
        assert!(window(30, 31).admit_charges((), &debt).is_some());
        assert!(window(29, 31).admit_charges((), &debt).is_none());
        assert!(window(30, 30).admit_charges((), &debt).is_none());
        assert!(
            crate::admission::Window::new(u64::MAX - 22, u64::MAX, u64::MAX)
                .admit_charges((), &debt)
                .is_none()
        );
        assert!(charges.bind(&values[..1]).is_none());
        let mut charged = 7;
        let mut counters = crate::machine::tasks::Occurrences::for_source_law(1);
        {
            let accepted = crate::admission::SourceResources {
                charged: &mut charged,
                counters: &mut counters,
                bound: 30,
                slice: 31,
                depth: 1,
                limit: 2,
                safe_point: 7,
            }
            .admit_region(&debt, &[0, 0], 1, 7, ())
            .unwrap()
            .unwrap();
            assert!(accepted.commit(|_| Err::<(), _>(())).is_err());
        }
        assert_eq!(charged, 7, "a rejected publication charges nothing");
        assert_eq!(counters.current(0), Some(0));
        crate::admission::SourceResources {
            charged: &mut charged,
            counters: &mut counters,
            bound: 30,
            slice: 31,
            depth: 1,
            limit: 2,
            safe_point: 7,
        }
        .admit_region(&debt, &[0, 0], 1, 7, ())
        .unwrap()
        .unwrap()
        .commit(|_| Ok::<_, ()>(()))
        .unwrap();
        assert_eq!(charged, 30);
        assert_eq!(counters.current(0), Some(2));
        let changed = [
            FormulaValues {
                arguments: &[Value::Int(0), Value::Int(0)],
                result: Value::Int(0),
            },
            FormulaValues {
                arguments: &[Value::Int(0), Value::Int(0)],
                result: Value::Int(0),
            },
        ];
        assert_eq!(charges.bind(&changed).unwrap().units(), 18);
        charges.corrupt_close_for_law();
        assert_eq!(
            charges.verify(&exe, body.code),
            Err(crate::compile::room::Refused::Exit)
        );
    }
}

/// K-CHG-003: optimizer shortcuts and spill storage must compute the recursive
/// declaration's price, including its saturating arithmetic and empty extrema.
#[test]
fn compiled_formulas_equal_the_recursive_oracle() {
    use crate::compile::{Plan, Source};
    use lash_kernel_doc::{Formula as F, Measure as M, Operand as O};
    use lash_kernel_value::{Name, Type, types::Param};
    let params: Vec<_> = (0..10)
        .map(|index| Param {
            name: Name::from(format!("p{index}").as_str()),
            ty: Type::Any,
            optional: false,
        })
        .collect();
    let operand = |at: usize| {
        if at == 10 {
            O::Result
        } else {
            O::Param(params[at].name.clone())
        }
    };
    let size = |at| F::Size(operand(at));
    let mut cases = vec![
        F::Sum(vec![size(0), size(1), F::Constant(7)]),
        F::Sum(vec![
            size(0),
            F::Min(vec![size(1), size(2)]),
            F::Constant(7),
        ]),
        F::Min(vec![size(0), F::Min(vec![size(1), size(2)])]),
        // Nine distinct measures force the nonlinear evaluator's heap storage.
        F::Max((0..9).map(size).collect()),
        F::Product((0..10).map(size).collect()),
        F::Sum(vec![size(0), size(0), size(10)]),
        F::Min(vec![]),
        F::Max(vec![]),
        F::Product(vec![]),
        F::Sum(vec![]),
    ];
    let mut deep = size(10);
    for index in 0..20 {
        deep = if index % 2 == 0 {
            F::Max(vec![size(index % 10), deep])
        } else {
            F::Min(vec![size(index % 10), deep])
        };
    }
    cases.push(deep);
    fn next(seed: &mut u64) -> u64 {
        *seed = seed
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        *seed
    }
    fn generate(seed: &mut u64, depth: usize, names: &[Param]) -> F {
        let choice = next(seed);
        if depth == 0 || choice.is_multiple_of(5) {
            let at = (next(seed) % 12) as usize;
            let operand = if at < names.len() {
                O::Param(names[at].name.clone())
            } else if at == 10 {
                O::Result
            } else {
                O::Param(Name::from("missing"))
            };
            return match choice % 6 {
                0 => F::Constant([0, 1, 2, 256, u64::MAX][(next(seed) % 5) as usize]),
                1 => F::Size(operand),
                2 => F::DeepSize(operand),
                3 => F::NestedSize(operand),
                _ => F::Magnitude(operand),
            };
        }
        if choice % 6 == 5 {
            return F::CeilLog2(Box::new(generate(seed, depth - 1, names)));
        }
        let terms = (0..(next(seed) % 4))
            .map(|_| generate(seed, depth - 1, names))
            .collect();
        match choice % 4 {
            0 => F::Sum(terms),
            1 => F::Product(terms),
            2 => F::Min(terms),
            _ => F::Max(terms),
        }
    }
    let mut seed = 0x7614_0030_0000_0003;
    for _ in 0..384 {
        cases.push(generate(&mut seed, 5, &params));
    }
    let mut spilled = 0;
    let mut nonlinear = 0;
    let mut stack_spill = 0;
    let mut linear = 0;
    // These are independent shape witnesses, not expected fee amounts. The
    // selected formulas cannot collapse into the optimizer's linear grammar.
    fn depth(formula: &F) -> usize {
        match formula {
            F::Sum(terms) | F::Product(terms) | F::Min(terms) | F::Max(terms) => terms
                .iter()
                .enumerate()
                .map(|(index, term)| usize::from(index > 0) + depth(term))
                .max()
                .unwrap_or(1),
            F::CeilLog2(term) => depth(term),
            _ => 1,
        }
    }
    for (index, formula) in cases.iter().enumerate() {
        if index < 2 {
            linear += 1;
        }
        if (2..5).contains(&index) || index == 10 {
            nonlinear += 1;
        }
        if index == 10 {
            assert!(depth(formula) > 16);
            stack_spill += 1;
        }
        for row in 0..8 {
            let amounts = [0_u64, 1, 2, 3, 8, 256, 1_u64 << 63, u64::MAX];
            let table: Vec<[u64; 4]> = (0..11)
                .map(|at| {
                    std::array::from_fn(|kind| {
                        if index == 3 {
                            (at as u64 + 1) * 17 + kind as u64
                        } else {
                            amounts[(at * 3 + kind + row) % amounts.len()]
                        }
                    })
                })
                .collect();
            let measure = |source: Source, kind: M| match source {
                Source::Nothing => 0,
                Source::Arg(at) => {
                    table[at][match kind {
                        M::Size => 0,
                        M::DeepSize => 1,
                        M::NestedSize => 2,
                        M::Magnitude => 3,
                    }]
                }
                Source::Result => {
                    table[10][match kind {
                        M::Size => 0,
                        M::DeepSize => 1,
                        M::NestedSize => 2,
                        M::Magnitude => 3,
                    }]
                }
            };
            let oracle = |formula: &F, shallow: bool| {
                formula.evaluate(&mut |operand, kind| {
                    let source = match operand {
                        O::Result => Source::Result,
                        O::Param(name) => params
                            .iter()
                            .position(|param| param.name == *name)
                            .map_or(Source::Nothing, Source::Arg),
                    };
                    if shallow && kind == M::NestedSize {
                        0
                    } else {
                        measure(
                            source,
                            if shallow && kind == M::DeepSize {
                                M::Size
                            } else {
                                kind
                            },
                        )
                    }
                })
            };
            let mut asked = Vec::new();
            let compiled = Plan::new(formula, &params).evaluate(|source, kind| {
                asked.push((source, kind));
                measure(source, kind)
            });
            assert_eq!(
                compiled,
                oracle(formula, false),
                "formula {index}, measurement row {row}: {formula:?}"
            );
            if index == 3 {
                assert_eq!(asked.len(), 9);
                spilled += 1;
            }
            let shallow = formula.shallow();
            assert_eq!(
                Plan::new(&shallow, &params).evaluate(measure),
                oracle(formula, true),
                "shallow formula {index}, row {row}"
            );
        }
    }
    assert_eq!((linear, nonlinear, spilled, stack_spill), (2, 4, 8, 1));
    eprintln!(
        "formula equivalence: {} declarations, {} evaluations; linear 2, nonlinear 4, 9-measurement rows 8, stack-spill 1",
        cases.len(),
        cases.len() * 16
    );
}

/// Member publication authenticates its unpaid source close before it writes.
fn member_closes() {
    for layout in [0, 1, 42, u64::MAX] {
        let (program, _) = Embedder::with(
            "main { let values = [1, 2] set values[0] = 3 return values }",
            Setup {
                layout: crate::Layout(layout),
                ..Setup::default()
            },
        )
        .program(false);
        let admitted = program
            .library
            .admit(&program.document, Default::default(), &program.selection)
            .unwrap_or_else(|_| panic!("member source is admitted and compiled"));
        let exe = admitted
            .executable(
                &program.document,
                &program.library,
                &program.documents,
                &program.functions,
                &program.instances,
            )
            .unwrap_or_else(|_| panic!("member source is admitted and compiled"));
        let body = exe
            .islands
            .plans
            .iter()
            .find(|plan| plan.params.is_empty() && plan.body.statements.len() == 3)
            .unwrap();
        let statement = body.body.statements[1].id;
        let (stream, stmt, _) = exe.statement(statement);
        let pc = (stmt.ops.start..stmt.ops.end())
            .find(|pc| matches!(stream.ops[*pc as usize], Op::SetIndex { .. }))
            .unwrap();
        let pending = (stmt.ops.start..pc)
            .rev()
            .find_map(|head| match stream.ops[head as usize] {
                Op::Region { region, total } => Some(crate::compile::expr::Close {
                    region: region.0,
                    head,
                    total,
                }),
                _ => None,
            })
            .unwrap();
        let charges =
            ChargeProgram::event_packet(&exe, body.code, statement, pc, Some(pending)).unwrap();
        let debt = charges.bind(&[]).unwrap();
        assert_eq!(
            debt.units(),
            u64::from(pending.total),
            "a member pays its original fixed close exactly once"
        );
        let tape = crate::observe::ChargeTape::new();
        debt.record(&tape);
        assert_eq!(
            tape.steps(),
            stream
                .regions
                .events(pending.region)
                .iter()
                .map(|event| event.weight)
                .collect::<Vec<_>>()
        );
        assert!(debt.native_allowance(7).unwrap().strict);
        assert!(
            ChargeProgram::event_packet(&exe, body.code, statement, pc, None).is_err(),
            "a member cannot erase its unpaid prefix"
        );
        assert!(
            ChargeProgram::event_packet(
                &exe,
                body.code,
                statement,
                pc,
                Some(crate::compile::expr::Close {
                    total: pending.total + 1,
                    ..pending
                })
            )
            .is_err(),
            "equal event coordinates cannot authenticate another close"
        );
    }
}

/// K-CHG-002: conditional arms close their own debt before the joined suffix.
#[test]
fn conditional_constructor_debt() {
    use crate::compile::expr::{Expr, Kind};

    fn check(expr: &Expr, stream: &crate::compile::Stream, range: OpRange) -> usize {
        let mut witnessed = 0;
        if let Kind::Closure { ahead, miss, .. }
        | Kind::Make { ahead, miss, .. }
        | Kind::Record { ahead, miss, .. } = &expr.kind
        {
            let prefix =
                crate::compile::expr::ConstructorPrefix::compile(stream, range, expr.pc).unwrap();
            assert_eq!(
                (
                    prefix.unpaid(),
                    crate::compile::expr::units(prefix.pending())
                ),
                (*miss, *ahead),
                "a constructor must retain only the selected arm's unpaid closes at pc {}",
                expr.pc
            );
            witnessed += 1;
        }
        for operand in expr.operands() {
            witnessed += check(operand, stream, range);
        }
        if let Kind::Cond {
            then, otherwise, ..
        } = &expr.kind
        {
            for arm in [then, otherwise] {
                witnessed += check(arm.value.as_ref().unwrap(), stream, range);
            }
        }
        witnessed
    }

    for layout in [0, 2, 0x5eed] {
        let (program, _) = Embedder::with(
            "main { let run = fn(flag) { let made = (if(flag, fn() { return 1 }, fn() { return 2 }), fn() { return 3 }) return made } return apply run(true) }",
            Setup { layout: crate::Layout(layout), ..Setup::default() },
        ).program(true);
        let admitted = program
            .library
            .admit(&program.document, Default::default(), &program.selection)
            .unwrap_or_else(|_| panic!("conditional constructor source admitted"));
        let exe = admitted
            .executable(
                &program.document,
                &program.library,
                &program.documents,
                &program.functions,
                &program.instances,
            )
            .unwrap_or_else(|_| panic!("conditional constructor source compiled"));
        let body = exe
            .islands
            .plans
            .iter()
            .find(|plan| plan.params.len() == 1 && plan.body.statements.len() == 2)
            .unwrap();
        let statement = &body.body.statements[0];
        let (stream, stmt, _) = exe.statement(statement.id);
        let mut source = crate::compile::expr::statement(stream, stmt.ops, false).unwrap();
        let Op::Bind { src, .. } = source.last else {
            panic!("the conditional constructor declaration binds its value")
        };
        let value = source.take(src).unwrap();
        assert_eq!(
            check(&value, stream, stmt.ops),
            4,
            "both arms, the following closure and the tuple must be checked"
        );
    }
}

/// K-CHG-002: an interior constructor cannot consume its region's future work.
/// Its pending identities must survive unchanged until the original close.
fn constructor_debt() {
    use crate::compile::expr::ConstructorPrefix;
    for layout in [0, 2, 0x5eed] {
        let (program, _) = Embedder::with(
            "main { let run = fn() { let made = ([1, 2], map{1: 2}, set{1, 2}, {left: 1}, fn() { return 7 }, \"a deferred literal larger than inline text capacity\") return made } return apply run() }",
            Setup { layout: crate::Layout(layout), ..Setup::default() },
        ).program(true);
        let admitted = program
            .library
            .admit(&program.document, Default::default(), &program.selection)
            .unwrap_or_else(|_| panic!("constructor debt source admitted"));
        let exe = admitted
            .executable(
                &program.document,
                &program.library,
                &program.documents,
                &program.functions,
                &program.instances,
            )
            .unwrap_or_else(|_| panic!("constructor debt source compiled"));
        let body = exe
            .islands
            .plans
            .iter()
            .find(|plan| {
                plan.params.is_empty()
                    && plan.body.statements.len() == 2
                    && exe.code(plan.code).site.unit != lash_kernel_value::sites::Unit::Main
            })
            .unwrap();
        let statement = body.body.statements[0].id;
        let (stream, stmt, _) = exe.statement(statement);
        let complete = ChargeProgram::compile(&exe, body.code, statement, stmt.ops).unwrap();
        let expected = crate::observe::ChargeTape::new();
        complete.bind(&[]).unwrap().record(&expected);
        let actual = crate::observe::ChargeTape::new();
        let mut constructors = 0;
        let mut inline_constants = 0;
        for pc in stmt.ops.start..stmt.ops.end() {
            let Some(prefix) = ConstructorPrefix::compile(stream, stmt.ops, pc) else {
                continue;
            };
            let original = crate::compile::room::charges::OriginalPrefix::compile(
                &exe,
                body.code,
                statement,
                prefix.pc(),
            )
            .unwrap();
            assert!(original.matches_constructor(
                stream,
                pc,
                prefix.unpaid(),
                crate::compile::expr::units(prefix.pending())
            ));
            assert!(!original.matches_constructor(
                stream,
                pc,
                pc + 1,
                crate::compile::expr::units(prefix.pending())
            ));
            // K-EVAL-004: recovery reads only previously evaluated roles. The
            // constructor's output comes from its accepted graph transaction.
            let at = stream.source_sites[pc as usize].as_ref().unwrap();
            let roles = stmt.source_roles_at(stream, pc + 1).unwrap();
            let needed: Vec<_> = roles
                .iter()
                .filter(|(_, site)| site.as_ref() != at.as_ref())
                .map(|(reg, _)| reg.0)
                .collect();
            let values = original.values(|reg| {
                assert!(
                    needed.contains(&reg),
                    "original recovery read an output or an unevaluated physical register"
                );
                Value::Int(7)
            });
            assert_eq!(values.len(), stmt.regs as usize);
            for reg in needed {
                assert!(values[reg as usize].same(&Value::Int(7)));
            }
            let charges =
                ChargeProgram::constructor_packet(&exe, body.code, statement, &prefix).unwrap();
            assert_eq!(charges.verify(&exe, body.code), Ok(()));
            assert_eq!(
                charges.constructor_quote(),
                Some(crate::compile::expr::units(prefix.pending()))
            );
            let debt = charges.bind(&[]).unwrap();
            assert!(debt.matches(
                &exe.code(body.code).source_room,
                charges.first(),
                charges.first()
            ));
            assert_eq!(debt.units(), 0, "an unclosed suffix is never paid ahead");
            let allowance = debt.native_allowance(17).unwrap();
            assert_eq!(
                (allowance.units, allowance.safe_point, allowance.strict),
                (0, 17, false)
            );
            debt.record(&actual);
            assert!(
                actual.steps().is_empty(),
                "pending work is not a paid tape prefix"
            );
            assert!(
                !prefix.pending().is_empty(),
                "the law must bind actual outstanding closes"
            );
            for duplicate in [false, true] {
                let mut wrong =
                    ChargeProgram::constructor_packet(&exe, body.code, statement, &prefix).unwrap();
                wrong.corrupt_constructor_close_for_law(duplicate);
                assert!(
                    wrong.verify(&exe, body.code).is_err(),
                    "a constructor cannot omit or duplicate an original pending close"
                );
            }
            if matches!(stream.ops[pc as usize], Op::Const { c, .. }
                if matches!(stream.consts[c.ix()], crate::compile::Constant::Resident(_)))
            {
                inline_constants += 1;
            } else {
                constructors += 1;
            }
        }
        // The RHS has a list, map, set, record, closure, deferred text and
        // outer tuple. Their inline integer operands are 2 + 2 + 2 + 1;
        // the closure body's return literal belongs to another statement.
        assert_eq!(
            constructors, 7,
            "all constructor/deferred-literal debt cases execute"
        );
        assert_eq!(
            inline_constants, 7,
            "all inline integer operand prefixes execute"
        );
        complete.bind(&[]).unwrap().record(&actual);
        assert_eq!(
            actual.steps(),
            expected.steps(),
            "the original close settles once after every constructor publication"
        );
    }
}

/// K-CHG-001: a packet which emitted several original closes owes only its
/// native formula. The compiler authenticates their complete ordered identity.
fn native_after_ordered_closes() {
    let mut witnessed = 0;
    for layout in [0, 2, 0x5eed] {
        let text = r#"main { let f = fn(input) {
            let first = json.decode(input, "number", "by_spelling", "record")
            let second = json.decode(input, "number", "by_spelling", "record")
            return (first, second)
        } return apply f("null") }"#;
        let (mut program, library) = Embedder::with(
            text,
            Setup {
                layout: crate::Layout(layout),
                probes: Some(crate::laws::layout::helper_probes),
                ..Setup::default()
            },
        )
        .program(false);
        program.library = crate::PreparedLibrary::with_layout(
            std::sync::Arc::clone(&library.registry),
            crate::Layout(layout),
        );
        let admitted = program
            .library
            .admit(&program.document, Default::default(), &program.selection)
            .unwrap_or_else(|_| panic!("ordered native source admitted"));
        let exe = admitted
            .executable(
                &program.document,
                &program.library,
                &program.documents,
                &program.functions,
                &program.instances,
            )
            .unwrap_or_else(|_| panic!("ordered native compiled"));
        for plan in exe.islands.plans.iter() {
            if exe.code(plan.code).params.len() != 1 {
                continue;
            }
            for statement in &plan.body.statements {
                let (stream, stmt, _) = exe.statement(statement.id);
                let mut paid = Vec::new();
                for pc in stmt.ops.start..stmt.ops.end() {
                    let op = stream.ops[pc as usize];
                    if let Op::Region { region, total } = op {
                        paid.push(crate::compile::expr::Close {
                            region: region.0,
                            head: pc,
                            total,
                        });
                    } else if !op.kind().in_region() {
                        if matches!(op, Op::Call { .. }) && paid.len() > 1 {
                            let charges = ChargeProgram::native_after_closes(
                                &exe,
                                plan.code,
                                statement.id,
                                pc,
                                &paid,
                            )
                            .unwrap();
                            assert_eq!(charges.verify(&exe, plan.code), Ok(()));
                            let arguments =
                                ["null", "number", "by_spelling", "record"].map(|text| {
                                    Value::Text(
                                        lash_kernel_doc::arena::ShortText::new(text).unwrap(),
                                    )
                                });
                            let values = [FormulaValues {
                                arguments: &arguments,
                                result: Value::Null,
                            }];
                            let debt = charges.bind(&values).unwrap();
                            let completion =
                                ChargeProgram::native_completion(&exe, plan.code, statement.id, pc)
                                    .unwrap();
                            assert_eq!(debt.units(), completion.bind(&values).unwrap().units());
                            let tape = crate::observe::ChargeTape::new();
                            debt.record(&tape);
                            let expected = crate::observe::ChargeTape::new();
                            completion.bind(&values).unwrap().record(&expected);
                            assert_eq!(
                                tape.steps(),
                                expected.steps(),
                                "already emitted closes cannot replay"
                            );
                            assert!(debt.native_allowance(7).unwrap().strict);
                            assert!(
                                ChargeProgram::native_after_closes(
                                    &exe,
                                    plan.code,
                                    statement.id,
                                    pc,
                                    &paid[1..]
                                )
                                .is_err(),
                                "missing original close"
                            );
                            let mut reordered = paid.clone();
                            reordered.swap(0, 1);
                            assert!(
                                ChargeProgram::native_after_closes(
                                    &exe,
                                    plan.code,
                                    statement.id,
                                    pc,
                                    &reordered
                                )
                                .is_err(),
                                "reordered original close"
                            );
                            let mut wrong = paid.clone();
                            wrong[0].total += 1;
                            assert!(
                                ChargeProgram::native_after_closes(
                                    &exe,
                                    plan.code,
                                    statement.id,
                                    pc,
                                    &wrong
                                )
                                .is_err(),
                                "another close total"
                            );
                            witnessed += 1;
                        }
                        paid.clear();
                    }
                }
            }
        }
    }
    assert!(
        witnessed > 0,
        "the owning corpus must compile actual multiple-close native sites"
    );
}
