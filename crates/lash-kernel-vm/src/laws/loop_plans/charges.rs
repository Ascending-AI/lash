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
