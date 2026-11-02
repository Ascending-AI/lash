use super::*;
use crate::compile::room::charges::{ChargeProgram, FormulaValues};
use lash_kernel_doc::Value;
use lash_kernel_exec::{Op, OpRange, Weight};

/// Charge totals alone cannot authenticate the source's ordered closes. The
/// same compiled formula and zero/nonzero measurements serve tape and quiet
/// admission, including exact charge-bound and strict slice equality.
pub(super) fn compiled_charges() {
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
