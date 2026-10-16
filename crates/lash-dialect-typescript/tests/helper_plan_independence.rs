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
