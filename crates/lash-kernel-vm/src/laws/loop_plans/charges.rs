//! Compiled completion formulas agree with their independent recursive oracle.
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
