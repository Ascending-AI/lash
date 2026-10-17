//! PLAN-INDEP over the FIG-6372 corpus (FIG-6440, `K-LIB-006`,
//! `K-CHG-007`): which path answers an operation is invisible to a run.
#![expect(
    clippy::unwrap_used,
    reason = "integration law setup treats malformed corpus and machine failures as failed laws"
)]

#[expect(
    dead_code,
    reason = "the adapter only needs the ordinary start fixture"
)]
#[path = "../src/tests/pages.rs"]
mod pages;

#[expect(
    dead_code,
    reason = "these laws run lowered programs, not the census or the fuzzer's entry"
)]
#[path = "../shared/helper_differential/runner.rs"]
mod runner;

use lash_kernel_vm::Layout;
use runner::{Case, Lowered, Runner};

/// Every corpus case of `dialect` whose index has `parity` gives the same
/// result and charge with its operations' natives answering through their
/// prepared entries (layout 0), uninterrupted, as when it starts with every
/// native declining to its kernel body (a layout of 3 modulo 4) and is
/// sealed and opened cold at each park, by turns, with natives answering
/// through the general call (an odd layout), through prepared entries, and
/// declining again.
/// Each layout shares its prepared library across cases and cold opens;
/// the machine and its heap access are fresh at every open.
fn runs_alike_whichever_path_answers(dialect: &str, parity: Option<usize>) {
    let cases: Vec<Case> =
        serde_json::from_str(include_str!("helper_differential/cases.json")).unwrap();
    let runner = Runner::new(dialect).unwrap();
    let plan = [Layout(3), Layout(1), Layout(0)];
    let mut compared = 0;
    for (index, case) in cases
        .iter()
        .filter(|case| case.dialect == dialect)
        .enumerate()
    {
        if parity.is_some_and(|parity| index % 2 != parity) {
            continue;
        }
        let Lowered::Program(program) = runner.lower(case).unwrap() else {
            continue;
        };
        let observe = |layouts: &[Layout]| {
            runner
                .execute(case, &program, layouts)
                .map(|observed| (observed.result, observed.charged))
        };
        assert_eq!(observe(&plan), observe(&[Layout(0)]), "{}", case.id);
        compared += 1;
    }
    assert!(compared > 0, "the corpus must not be empty");
    println!("{dialect}: plan-independence comparisons={compared}");
}

#[test]
fn typescript_even_cases_run_alike_whichever_path_answers() {
    runs_alike_whichever_path_answers("typescript", Some(0));
}

#[test]
fn typescript_odd_cases_run_alike_whichever_path_answers() {
    runs_alike_whichever_path_answers("typescript", Some(1));
}

#[test]
fn python_cases_run_alike_whichever_path_answers() {
    runs_alike_whichever_path_answers("python", None);
}

/// PLAN-INDEP for FIG-6640: numeric native guards decline coercions, a list
/// cursor maps holes and hands arbitrary values through unchanged, while
/// text, Map and Set cursors keep the complete item body and machine deref.
#[test]
fn numeric_and_list_operations_run_alike_after_guard_misses() {
    let runner = Runner::new("typescript").unwrap();
    let sources = [
        "const marks = []; const box = {valueOf() { marks.push('valueOf'); return 7; }}; await observe([10 - 3, 10 - box, Number(box), marks.join(',')]);",
        "const out = []; for (const x of [1, , 3]) { out.push(x === undefined ? 'hole' : x); await observe(x); } const xs: number[] = 'ab' as any; for (const x of xs) { out.push(x); } await observe(out);",
        "const key = {value: 1}; const entriesMap = new Map([[key, 'entry']]); for (const [entryKey, v] of entriesMap) { await observe(v); await observe(entryKey === key); } const membersSet = new Set([key]); for (const entryKey of membersSet) { await observe(entryKey === key); }",
    ];
    for (index, source) in sources.into_iter().enumerate() {
        let case = Case {
            id: format!("FIG-6640-{index}"),
            dialect: "typescript".into(),
            source: source.into(),
            bits: 0,
            stride: 0,
            nanoseconds: 0,
        };
        let program = match runner.lower(&case).unwrap() {
            Lowered::Program(program) => program,
            Lowered::Refused(observed) => panic!("{} must lower: {:?}", case.id, observed.result),
        };
        let reference = runner.execute(&case, &program, &[Layout(0)]).unwrap();
        assert_eq!(
            reference.result["type"], "observations",
            "{}: {:?}",
            case.id, reference.result
        );
        for plan in [
            &[Layout(1)][..],
            &[Layout(2)],
            &[Layout(3)],
            &[Layout(0x5eed)],
            &[Layout(0), Layout(2), Layout(3), Layout(1), Layout(0x5eed)],
        ] {
            let observed = runner.execute(&case, &program, plan).unwrap();
            assert_eq!(
                (&observed.result, observed.charged),
                (&reference.result, reference.charged),
                "{}: {plan:?}",
                case.id
            );
        }
    }
}
