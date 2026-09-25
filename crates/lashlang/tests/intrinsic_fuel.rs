//! Intrinsics charge their proportional work to the instruction budget.
//!
//! An intrinsic's dispatch is one instruction, but `JSON.parse` over a large
//! text is proportional work behind that one bytecode. The instruction budget
//! is the VM's only deterministic bound, so every intrinsic whose work grows
//! with its input or output charges one instruction per unit of that work.
//!
//! Each case below runs one intrinsic over a small input and a large one under
//! the same budget. The small run must finish, which shows the budget covers
//! the program around the call. The large run must exhaust the budget, which
//! it can only do if the intrinsic charged for its input: the two programs
//! differ only in the input's size. Add an intrinsic here when it gains a
//! charge.
//!
//! Two tables cover two reachabilities. `CASES` are authored TypeScript: the
//! JavaScript intrinsics are heap-native, so a seeded `input` reaches the
//! method as a heap reference and the method's own charge is what spends the
//! budget. `AST_CASES` are the Lash builtins no dialect spells (ADR 0096),
//! built on the IR directly; there the generic intrinsic opcode exports its
//! operands first, and that boundary read is itself charged, so a large seed
//! exhausts at the boundary the call crosses rather than inside the builtin.
//! Either way the property under test is the same: the work is bounded by the
//! instruction budget.

use std::sync::Arc;

use lashlang::{
    AbilityOp, AbilityResult, ExecutionBound, ExecutionBounds, ExecutionHost, ExecutionHostError,
    ExecutionOutcome, Expr, LashlangAbilities, LashlangHostCatalog, LashlangHostEnvironment,
    Record, RuntimeError, State, TypeExpr, Value,
};

use crate::ast_support::{call, number, program, string, var};
use crate::execute_support::{ExecuteError, execute, execute_program};

/// The budget every case runs under: far above what the program around the
/// intrinsic spends, far below the large input's size.
const BUDGET: u64 = 5_000;
const SMALL: usize = 10;
const LARGE: usize = 50_000;

/// The host for the measured run: the instruction budget under test.
struct BudgetHost;

impl ExecutionHost for BudgetHost {
    async fn perform(&self, op: AbilityOp) -> Result<AbilityResult, ExecutionHostError> {
        match op {
            AbilityOp::Finish(value) | AbilityOp::Print(value) => Ok(AbilityResult::Value(value)),
            _ => Err(ExecutionHostError::new(
                "no other host abilities in this test",
            )),
        }
    }

    fn execution_bounds(&self) -> ExecutionBounds {
        ExecutionBounds::new(
            ExecutionBound::instructions(BUDGET),
            ExecutionBounds::memory_bounded_default().memory_limit,
        )
    }
}

fn environment() -> LashlangHostEnvironment {
    LashlangHostEnvironment::new(LashlangHostCatalog::new(), LashlangAbilities::all())
}

/// A cell written in TypeScript. `seed` is inserted as the `input` global
/// before the run, so the input's construction never pays the budget being
/// measured; `probe` runs the intrinsic once over the seeded value, or sizes
/// the input inside itself when `seed` is `None`. `large` overrides `LARGE`
/// for the cases whose honest accounting is quadratic — the budget refuses
/// them at a size whose work still runs in reasonable time.
struct Case {
    intrinsic: &'static str,
    seed: Option<fn(usize) -> Value>,
    probe: fn(usize) -> String,
    large: usize,
}

fn text(size: usize) -> Value {
    Value::String("x".repeat(size).into())
}

fn csv(size: usize) -> Value {
    Value::String("x,".repeat(size).into())
}

fn digits(size: usize) -> Value {
    Value::String("9".repeat(size).into())
}

fn json_list(size: usize) -> Value {
    Value::String(format!("[{}0]", "0,".repeat(size)).into())
}

fn numbers(size: usize) -> Value {
    Value::List(
        (0..size)
            .map(|index| Value::Number(index as f64))
            .collect::<Vec<_>>()
            .into(),
    )
}

fn pairs(size: usize) -> Value {
    Value::List(
        (0..size)
            .map(|index| {
                Value::List(vec![Value::Number(index as f64), Value::Number(index as f64)].into())
            })
            .collect::<Vec<_>>()
            .into(),
    )
}

/// A list of `size` members spread across inner lists of fixed width, so the
/// members a `flat` descends through still scale with the input while the heap
/// object count does not: exporting the seed walks one object per inner list,
/// and the debug-build boundary-cache invariant runs once per exported object.
fn nested(size: usize) -> Value {
    const WIDTH: usize = 64;
    let inner = (0..size / WIDTH.max(1))
        .map(|base| {
            Value::List(
                (0..WIDTH)
                    .map(|index| Value::Number((base * WIDTH + index) as f64))
                    .collect::<Vec<_>>()
                    .into(),
            )
        })
        .collect::<Vec<_>>();
    if inner.is_empty() {
        return Value::List(vec![Value::List(vec![].into())].into());
    }
    Value::List(inner.into())
}

fn record(size: usize) -> Value {
    Value::Record(Arc::new(Record::from_iter(
        (0..size).map(|index| (format!("k{index}"), Value::Number(index as f64))),
    )))
}

fn url(size: usize) -> Value {
    Value::String(format!("https://a.example/{}", "a".repeat(size)).into())
}

fn params(size: usize) -> Value {
    Value::String("a=1&".repeat(size).into())
}

fn encoded(size: usize) -> Value {
    Value::String("%20".repeat(size).into())
}

fn date(size: usize) -> Value {
    Value::String(format!("2020-01-01T00:00:00.{}Z", "1".repeat(size)).into())
}

const CASES: &[Case] = &[
    Case {
        intrinsic: "JSON.parse",
        seed: Some(json_list),
        probe: |_| r#"finish(JSON.parse(input).length);"#.to_string(),
        large: LARGE,
    },
    Case {
        intrinsic: "JSON.stringify",
        seed: Some(text),
        probe: |_| r#"finish(JSON.stringify(input).length);"#.to_string(),
        large: LARGE,
    },
    Case {
        intrinsic: "string.includes",
        seed: Some(text),
        probe: |_| r#"finish(input.includes("y"));"#.to_string(),
        large: LARGE,
    },
    Case {
        intrinsic: "string.slice",
        seed: Some(text),
        probe: |_| r#"finish(input.slice(1).length);"#.to_string(),
        large: LARGE,
    },
    Case {
        intrinsic: "string.replaceAll",
        seed: Some(text),
        probe: |_| r#"finish(input.replaceAll("x", "y").length);"#.to_string(),
        large: LARGE,
    },
    Case {
        intrinsic: "string.split",
        seed: Some(csv),
        probe: |_| r#"finish(input.split(",").length);"#.to_string(),
        large: LARGE,
    },
    Case {
        intrinsic: "string.toUpperCase",
        seed: Some(text),
        probe: |_| r#"finish(input.toUpperCase().length);"#.to_string(),
        large: LARGE,
    },
    Case {
        intrinsic: "string.concat",
        seed: Some(text),
        probe: |_| r#"finish(input.concat(input).length);"#.to_string(),
        large: LARGE,
    },
    Case {
        intrinsic: "string.repeat",
        seed: None,
        probe: |size| format!(r#"finish("x".repeat({size}).length);"#),
        large: LARGE,
    },
    Case {
        intrinsic: "string.padStart",
        seed: None,
        probe: |size| format!(r#"finish("x".padStart({size}).length);"#),
        large: LARGE,
    },
    Case {
        intrinsic: "array.join",
        seed: Some(numbers),
        probe: |_| r#"finish(input.join("-").length);"#.to_string(),
        large: LARGE,
    },
    Case {
        intrinsic: "array.includes",
        seed: Some(numbers),
        probe: |_| r#"finish(input.includes(-1));"#.to_string(),
        large: LARGE,
    },
    Case {
        intrinsic: "array.concat",
        seed: Some(numbers),
        probe: |_| r#"finish(input.concat(input).length);"#.to_string(),
        large: LARGE,
    },
    Case {
        intrinsic: "array.flat",
        seed: Some(nested),
        probe: |_| r#"finish(input.flat().length);"#.to_string(),
        large: LARGE,
    },
    Case {
        intrinsic: "array.slice",
        seed: Some(numbers),
        probe: |_| r#"finish(input.slice(1).length);"#.to_string(),
        large: LARGE,
    },
    Case {
        intrinsic: "array.toString",
        seed: Some(numbers),
        probe: |_| r#"finish(input.toString().length);"#.to_string(),
        large: LARGE,
    },
    Case {
        intrinsic: "array.sort",
        seed: Some(numbers),
        probe: |_| r#"finish(input.sort().length);"#.to_string(),
        large: LARGE,
    },
    Case {
        intrinsic: "Object.keys",
        seed: Some(record),
        probe: |_| r#"finish(Object.keys(input).length);"#.to_string(),
        large: LARGE,
    },
    Case {
        intrinsic: "Object.assign",
        seed: Some(record),
        probe: |_| r#"finish(Object.keys(Object.assign({}, input)).length);"#.to_string(),
        large: LARGE,
    },
    Case {
        intrinsic: "Array.from",
        seed: None,
        probe: |size| format!(r#"finish(Array.from({{length: {size}}}).length);"#),
        large: LARGE,
    },
    Case {
        intrinsic: "Math.max spread",
        seed: Some(numbers),
        probe: |_| r#"finish(Math.max(...input));"#.to_string(),
        large: LARGE,
    },
    Case {
        intrinsic: "new Set",
        seed: Some(numbers),
        probe: |_| r#"finish(new Set(input).size);"#.to_string(),
        // The Set dedup reads every member kept so far per insert, so the
        // honest charge — and the wall time — is quadratic in the input.
        large: 4_000,
    },
    Case {
        intrinsic: "new Map",
        seed: Some(pairs),
        probe: |_| r#"finish(new Map(input).size);"#.to_string(),
        large: 4_000,
    },
    Case {
        intrinsic: "new AggregateError",
        seed: Some(numbers),
        probe: |_| r#"finish(new AggregateError(input).name);"#.to_string(),
        large: LARGE,
    },
    Case {
        intrinsic: "Set.forEach",
        seed: Some(numbers),
        probe: |_| r#"const s = new Set(input); s.forEach((v) => v); finish(0);"#.to_string(),
        large: 4_000,
    },
    Case {
        intrinsic: "new URL",
        seed: Some(url),
        probe: |_| r#"finish(new URL(input).href.length);"#.to_string(),
        large: LARGE,
    },
    Case {
        intrinsic: "URL.canParse",
        seed: Some(url),
        probe: |_| r#"finish(URL.canParse(input));"#.to_string(),
        large: LARGE,
    },
    Case {
        intrinsic: "new URLSearchParams",
        seed: Some(params),
        probe: |_| r#"finish(new URLSearchParams(input).toString().length);"#.to_string(),
        large: LARGE,
    },
    Case {
        intrinsic: "encodeURIComponent",
        seed: Some(text),
        probe: |_| r#"finish(encodeURIComponent(input).length);"#.to_string(),
        large: LARGE,
    },
    Case {
        intrinsic: "decodeURIComponent",
        seed: Some(encoded),
        probe: |_| r#"finish(decodeURIComponent(input).length);"#.to_string(),
        large: LARGE,
    },
    Case {
        intrinsic: "RegExp exec",
        seed: Some(text),
        probe: |_| r#"finish((new RegExp("x").exec(input) || []).length);"#.to_string(),
        large: LARGE,
    },
    // `new RegExp` is charged for compiling its pattern but takes no case
    // here: patterns cap at 4_096 UTF-16 code units, so under this budget the
    // constructor always throws TS_REGEX_PATTERN_TOO_LONG before its charge
    // could exhaust. The family's scan side is covered by `RegExp exec` and
    // `string.match` above and below.
    Case {
        intrinsic: "string.match",
        seed: Some(text),
        probe: |_| r#"finish((input.match(new RegExp("x")) || []).length);"#.to_string(),
        large: LARGE,
    },
    Case {
        intrinsic: "Date.parse",
        seed: Some(date),
        probe: |_| r#"finish(Date.parse(input));"#.to_string(),
        large: LARGE,
    },
    Case {
        intrinsic: "new Date",
        seed: Some(date),
        probe: |_| r#"finish(new Date(input).getTime());"#.to_string(),
        large: LARGE,
    },
    Case {
        intrinsic: "Number",
        seed: Some(digits),
        probe: |_| r#"finish(Number(input));"#.to_string(),
        large: LARGE,
    },
    Case {
        intrinsic: "string +",
        seed: Some(text),
        probe: |_| r#"finish((input + input).length);"#.to_string(),
        large: LARGE,
    },
    Case {
        intrinsic: "string +=",
        seed: Some(text),
        probe: |_| r#"let s = "a"; s += input; finish(s.length);"#.to_string(),
        large: LARGE,
    },
    Case {
        intrinsic: "string ===",
        seed: Some(text),
        probe: |_| r#"finish(input === input);"#.to_string(),
        large: LARGE,
    },
    Case {
        intrinsic: "console.log",
        seed: Some(text),
        probe: |_| r#"console.log(input); finish(0);"#.to_string(),
        large: LARGE,
    },
];

/// A Lash builtin with no TypeScript spelling, spelled on the IR. `seed` is
/// inserted as the `input` global unmeasured; `program` is the body run under
/// the budget.
struct AstCase {
    intrinsic: &'static str,
    seed: fn(usize) -> Value,
    program: fn(usize) -> Vec<Expr>,
}

const AST_CASES: &[AstCase] = &[
    AstCase {
        intrinsic: "len",
        seed: text,
        program: |_| vec![Expr::Finish(Box::new(call("len", vec![var("input")])))],
    },
    AstCase {
        intrinsic: "keys",
        seed: record,
        program: |_| vec![Expr::Finish(Box::new(call("keys", vec![var("input")])))],
    },
    AstCase {
        intrinsic: "values",
        seed: record,
        program: |_| vec![Expr::Finish(Box::new(call("values", vec![var("input")])))],
    },
    AstCase {
        intrinsic: "contains",
        seed: text,
        program: |_| {
            vec![Expr::Finish(Box::new(call(
                "contains",
                vec![var("input"), string("y")],
            )))]
        },
    },
    AstCase {
        intrinsic: "find",
        seed: text,
        program: |_| {
            vec![Expr::Finish(Box::new(call(
                "find",
                vec![var("input"), string("y")],
            )))]
        },
    },
    AstCase {
        intrinsic: "grep_text",
        seed: text,
        program: |_| {
            vec![Expr::Finish(Box::new(call(
                "grep_text",
                vec![var("input"), string("y")],
            )))]
        },
    },
    AstCase {
        intrinsic: "starts_with",
        seed: text,
        program: |_| {
            vec![Expr::Finish(Box::new(call(
                "starts_with",
                vec![var("input"), string("x")],
            )))]
        },
    },
    AstCase {
        intrinsic: "ends_with",
        seed: text,
        program: |_| {
            vec![Expr::Finish(Box::new(call(
                "ends_with",
                vec![var("input"), string("x")],
            )))]
        },
    },
    AstCase {
        intrinsic: "split",
        seed: csv,
        program: |_| {
            vec![Expr::Finish(Box::new(call(
                "split",
                vec![var("input"), string(",")],
            )))]
        },
    },
    AstCase {
        intrinsic: "join",
        seed: numbers,
        program: |_| {
            vec![Expr::Finish(Box::new(call(
                "join",
                vec![var("input"), string("-")],
            )))]
        },
    },
    AstCase {
        intrinsic: "trim",
        seed: text,
        program: |_| vec![Expr::Finish(Box::new(call("trim", vec![var("input")])))],
    },
    AstCase {
        intrinsic: "slice",
        seed: text,
        program: |_| {
            vec![Expr::Finish(Box::new(call(
                "slice",
                vec![var("input"), number(1.0), number(1e9)],
            )))]
        },
    },
    AstCase {
        intrinsic: "to_string",
        seed: record,
        program: |_| {
            vec![Expr::Finish(Box::new(call(
                "to_string",
                vec![var("input")],
            )))]
        },
    },
    AstCase {
        intrinsic: "to_int",
        seed: digits,
        program: |_| vec![Expr::Finish(Box::new(call("to_int", vec![var("input")])))],
    },
    AstCase {
        intrinsic: "to_float",
        seed: digits,
        program: |_| vec![Expr::Finish(Box::new(call("to_float", vec![var("input")])))],
    },
    AstCase {
        intrinsic: "json_parse",
        seed: json_list,
        program: |_| {
            vec![Expr::Finish(Box::new(call(
                "json_parse",
                vec![var("input")],
            )))]
        },
    },
    AstCase {
        intrinsic: "format",
        seed: text,
        program: |_| vec![Expr::Finish(Box::new(call("format", vec![var("input")])))],
    },
    AstCase {
        intrinsic: "validate",
        seed: text,
        program: |_| {
            vec![Expr::Finish(Box::new(call(
                "validate",
                vec![var("input"), Expr::TypeLiteral(Box::new(TypeExpr::Any))],
            )))]
        },
    },
    AstCase {
        intrinsic: "range",
        seed: |_| Value::Null,
        program: |size| {
            vec![Expr::Finish(Box::new(call(
                "range",
                vec![number(0.0), number(size as f64)],
            )))]
        },
    },
    AstCase {
        intrinsic: "push",
        seed: numbers,
        program: |_| {
            vec![Expr::Finish(Box::new(call(
                "push",
                vec![var("input"), number(1.0)],
            )))]
        },
    },
    AstCase {
        intrinsic: "sort",
        seed: numbers,
        program: |_| vec![Expr::Finish(Box::new(call("sort", vec![var("input")])))],
    },
    AstCase {
        intrinsic: "sum",
        seed: numbers,
        program: |_| vec![Expr::Finish(Box::new(call("sum", vec![var("input")])))],
    },
    AstCase {
        intrinsic: "min",
        seed: numbers,
        program: |_| vec![Expr::Finish(Box::new(call("min", vec![var("input")])))],
    },
    AstCase {
        intrinsic: "max",
        seed: numbers,
        program: |_| vec![Expr::Finish(Box::new(call("max", vec![var("input")])))],
    },
    AstCase {
        intrinsic: "lower",
        seed: text,
        program: |_| vec![Expr::Finish(Box::new(call("lower", vec![var("input")])))],
    },
    AstCase {
        intrinsic: "upper",
        seed: text,
        program: |_| vec![Expr::Finish(Box::new(call("upper", vec![var("input")])))],
    },
    AstCase {
        intrinsic: "replace",
        seed: text,
        program: |_| {
            vec![Expr::Finish(Box::new(call(
                "replace",
                vec![var("input"), string("x"), string("y")],
            )))]
        },
    },
    AstCase {
        intrinsic: "unique",
        seed: numbers,
        program: |_| vec![Expr::Finish(Box::new(call("unique", vec![var("input")])))],
    },
    AstCase {
        intrinsic: "reverse",
        seed: numbers,
        program: |_| vec![Expr::Finish(Box::new(call("reverse", vec![var("input")])))],
    },
];

async fn run_case(case: &Case, size: usize) -> Result<ExecutionOutcome, ExecuteError> {
    let mut state = State::new();
    if let Some(seed) = case.seed {
        state
            .insert_global("input", seed(size))
            .map_err(ExecuteError::Runtime)?;
    }
    execute(&(case.probe)(size), &mut state, &BudgetHost, environment()).await
}

async fn run_ast_case(case: &AstCase, size: usize) -> Result<ExecutionOutcome, ExecuteError> {
    let mut state = State::new();
    state
        .insert_global("input", (case.seed)(size))
        .map_err(ExecuteError::Runtime)?;
    execute_program(&program((case.program)(size)), &mut state, &BudgetHost).await
}

fn assert_fits(name: &str, outcome: &Result<ExecutionOutcome, ExecuteError>) {
    assert!(
        matches!(outcome, Ok(ExecutionOutcome::Finished(_))),
        "{name}: a small input must fit the budget, got {outcome:?}"
    );
}

fn assert_exhausts(name: &str, outcome: &Result<ExecutionOutcome, ExecuteError>) {
    assert!(
        matches!(
            outcome,
            Err(ExecuteError::Runtime(
                RuntimeError::InstructionBudgetExceeded { limit: BUDGET }
            ))
        ),
        "{name}: a large input must exhaust the budget, got {outcome:?}"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn every_charged_intrinsic_spends_the_budget_in_proportion_to_its_input() {
    for case in CASES {
        assert_fits(case.intrinsic, &run_case(case, SMALL).await);
        assert_exhausts(case.intrinsic, &run_case(case, case.large).await);
    }
    for case in AST_CASES {
        assert_fits(case.intrinsic, &run_ast_case(case, SMALL).await);
        assert_exhausts(case.intrinsic, &run_ast_case(case, LARGE).await);
    }
}

/// The instruction a budget runs out on is a function of the program and its
/// input, so two runs of one cell report the same exhaustion.
#[tokio::test(flavor = "current_thread")]
async fn budget_exhaustion_is_identical_across_runs() {
    for case in CASES {
        let first = run_case(case, case.large).await;
        let second = run_case(case, case.large).await;
        assert_eq!(first, second, "{}", case.intrinsic);
    }
    for case in AST_CASES {
        let first = run_ast_case(case, LARGE).await;
        let second = run_ast_case(case, LARGE).await;
        assert_eq!(first, second, "{}", case.intrinsic);
    }
}
