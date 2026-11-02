use super::*;
use crate::compile::room::charges::{ChargeProgram, FormulaValues};
use lash_kernel_doc::Value;
use lash_kernel_exec::Weight;

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
