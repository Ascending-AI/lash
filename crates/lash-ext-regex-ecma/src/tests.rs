//! The laws of the extension: the cold-and-warm gate (`docs/kernel/design.md`
//! §11, gate 5) and the rules each function states.

use std::collections::BTreeSet;
use std::sync::Arc;

use lash_kernel_doc::{
    Datum, ErrorDatum, FunctionDefinition, FunctionRegistry, GuardExceeded, Integer, Measure,
    NativeCall, NativeError, Operand, Value, WorkCounter,
};

use crate::{
    BRAND, Engine, LONE_SURROGATE_ERROR, MAX_GROUP_NESTING, MAX_PATTERN_UNITS, Operation,
    SYNTAX_ERROR, register,
};

use super::testsupport::{Fixture, fresh};
use lash_kernel_state::paged::{PagedHeap, SemanticHeap};

#[derive(Debug, PartialEq)]
enum Failure {
    Raised(lash_kernel_doc::ErrorDatum),
    Guard(GuardExceeded),
    Memory,
    Heap(lash_kernel_doc::heap::HeapFault),
}

fn int(value: i64) -> Value {
    Value::Int(value.into())
}

/// The work limit a call runs under.
#[derive(Clone, Copy, Debug)]
enum Limit {
    /// What the definition's guard formula gives for the arguments.
    Stated,
    Exactly(u64),
}

/// Everything a call shows: how it ended, the guard units it spent, and
/// what the definition's formula charges for it.
#[derive(Debug, PartialEq)]
struct Run {
    outcome: Result<Datum, Failure>,
    spent: u64,
    charge: u64,
}

fn measure(
    definition: &FunctionDefinition,
    heap: &PagedHeap,
    args: &[Value],
    result: Option<&Value>,
    operand: &Operand,
    measure: Measure,
) -> u64 {
    let value = match operand {
        Operand::Param(name) => {
            let index = definition
                .signature
                .params
                .iter()
                .position(|param| &param.name == name)
                .unwrap();
            args.get(index)
        }
        Operand::Result => result,
    };
    match (value, measure) {
        // A call that raises is charged with a result of size 0 (`K-CHG-003`).
        (None, _) => 0,
        (Some(value), Measure::Size) => heap.size(value),
        (Some(value), Measure::DeepSize) => heap.deep_size(value, &mut BTreeSet::new()),
        (Some(_), Measure::Magnitude) => panic!("no formula here takes a magnitude"),
        (Some(_), Measure::NestedSize) => panic!("no formula here takes a nested size"),
    }
}

/// Calls `operation` as a machine would: through the registry, under the
/// guard's limit, and charged by the definition's formula.
fn run(
    engine: &Arc<Engine>,
    operation: Operation,
    limit: Limit,
    arguments: &dyn Fn(&mut PagedHeap) -> Vec<Value>,
) -> Run {
    let mut registry = FunctionRegistry::new();
    let functions = register(&mut registry, engine).unwrap();
    let registered = registry.get(&functions.get(operation)).unwrap();
    let definition = registered.definition().as_ref();

    let mut heap = fresh();
    let args = arguments(&mut heap)
        .into_iter()
        .map(|value| heap.canonical_value(value).unwrap())
        .collect::<Vec<_>>();
    let before = args
        .iter()
        .map(|value| heap.datum(value))
        .collect::<Vec<_>>();
    let limit =
        match (limit, &definition.guard) {
            (Limit::Exactly(limit), _) => Some(limit),
            (Limit::Stated, Some(guard)) => Some(guard.limit.evaluate(&mut |operand, how| {
                measure(definition, &heap, &args, None, operand, how)
            })),
            (Limit::Stated, None) => None,
        };
    let mut counter = WorkCounter::new(limit);
    let result = registered.native().unwrap().call(NativeCall {
        args: &args,
        heap: &mut heap,
        counter: &mut counter,
    });
    // No function changes an object that existed (`K-LIB-007`).
    assert_eq!(
        args.iter()
            .map(|value| heap.datum(value))
            .collect::<Vec<_>>(),
        before
    );
    let charge = definition.charge.evaluate(&mut |operand, how| {
        measure(definition, &heap, &args, result.as_ref().ok(), operand, how)
    });
    Run {
        outcome: match result {
            Ok(value) => Ok(heap.datum(&value)),
            Err(NativeError::Raised(error)) => {
                let Datum::Error(error) = heap.datum(&Value::Error(error)) else {
                    unreachable!()
                };
                Err(Failure::Raised(*error))
            }
            Err(NativeError::Guard(error)) => Err(Failure::Guard(error)),
            Err(NativeError::Memory) => Err(Failure::Memory),
            Err(NativeError::Heap(error)) => Err(Failure::Heap(error)),
        },
        spent: counter.spent(),
        charge,
    }
}

fn cold() -> Arc<Engine> {
    Arc::new(Engine::new(8))
}

/// Runs a call on a cold engine, again on the same engine now that it holds
/// the program, and on an engine that keeps none, and requires all three to
/// agree on the result, the units spent and the charge.
fn cold_and_warm(
    operation: Operation,
    limit: Limit,
    arguments: &dyn Fn(&mut PagedHeap) -> Vec<Value>,
) -> Run {
    let engine = cold();
    assert_eq!(engine.cached_patterns(), 0);
    let first = run(&engine, operation, limit, arguments);
    let second = run(&engine, operation, limit, arguments);
    let uncached_engine = Arc::new(Engine::new(0));
    let uncached = run(&uncached_engine, operation, limit, arguments);
    assert_eq!(uncached_engine.cached_patterns(), 0);
    assert_eq!(first, second, "{operation:?}: cold against warm");
    assert_eq!(first, uncached, "{operation:?}: cold against uncached");
    first
}

const MATCHING: [Operation; 5] = [
    Operation::Exec,
    Operation::Test,
    Operation::MatchAll,
    Operation::Replace,
    Operation::Split,
];

fn arguments<'a>(
    operation: Operation,
    pattern: &'a str,
    flags: &'a str,
    input: &'a str,
) -> impl Fn(&mut PagedHeap) -> Vec<Value> + 'a {
    move |heap| {
        let mut args = vec![heap.regex(pattern, flags, 0), Value::text(input)];
        if operation == Operation::Replace {
            args.push(Value::text("[$&]"));
        }
        args
    }
}

fn raised(outcome: &Result<Datum, Failure>) -> &ErrorDatum {
    match outcome {
        Err(Failure::Raised(error)) => error,
        other => panic!("expected a raised error, got {other:?}"),
    }
}

/// `K-BND-001`, `K-LIB-007`: what a call keeps follows its matches times
/// its groups, so it is reserved as it is kept. A heap with no room for it
/// refuses the call, which the guard's work limit alone would let finish.
#[test]
fn a_regex_expansion_refuses_under_the_real_census() {
    let input = "a".repeat(200);
    for operation in [Operation::MatchAll, Operation::Replace, Operation::Split] {
        let arguments = arguments(operation, "(a)", "g", &input);
        let roomy = run(&cold(), operation, Limit::Stated, &arguments);
        assert!(roomy.outcome.is_ok());
        let tight = run(&cold(), operation, Limit::Stated, &|heap| {
            let args = arguments(heap);
            super::testsupport::limit_to_extra(heap, &args, 1 << 10);
            args
        });
        assert_eq!(tight.outcome, Err(Failure::Memory), "{operation:?}");
    }
}

/// Gate 5: results, charges and the point of guard failure are equal with
/// the cache cold and warm, at the stated limit, at the smallest limit that
/// lets the call finish, and one unit below it.
#[test]
fn results_charges_and_guard_failures_are_equal_cold_and_warm() {
    let backtracking = "a".repeat(14);
    let cases = [
        ("a(b+)(?<tail>c)?", "g", "xxabbbc ab"),
        ("(a+)+b", "", backtracking.as_str()),
    ];
    for (pattern, flags, input) in cases {
        for operation in MATCHING {
            let arguments = arguments(operation, pattern, flags, input);
            let stated = cold_and_warm(operation, Limit::Stated, &arguments);
            assert!(
                stated.outcome.is_ok(),
                "{operation:?}: {:?}",
                stated.outcome
            );
            let needed = stated.spent;
            assert!(needed > 0);

            let at = cold_and_warm(operation, Limit::Exactly(needed), &arguments);
            assert_eq!(at, stated, "{operation:?} at its threshold");

            let below = cold_and_warm(operation, Limit::Exactly(needed - 1), &arguments);
            assert_eq!(
                below.outcome,
                Err(Failure::Guard(GuardExceeded { limit: needed - 1 })),
                "{operation:?} one unit below its threshold"
            );
        }
    }
}

/// A pattern that backtracks past the limit its definition states ends at
/// that limit, with every unit spent, cold and warm alike.
#[test]
fn runaway_backtracking_ends_at_the_stated_limit_cold_and_warm() {
    let input = "a".repeat(40);
    for operation in MATCHING {
        let arguments = arguments(operation, "(a+)+b", "", &input);
        let run = cold_and_warm(operation, Limit::Stated, &arguments);
        let limit = crate::GUARD_BASE + crate::GUARD_PER_INPUT_UNIT * (1 + input.len() as u64);
        assert_eq!(
            run.outcome,
            Err(Failure::Guard(GuardExceeded { limit })),
            "{operation:?}"
        );
        assert_eq!(run.spent, limit, "{operation:?}");
    }
}

/// A pattern that does not compile raises the same error for the same
/// charge whether the engine has seen it or not, and spends no guard unit:
/// it is not a guard failure even under a limit of zero.
#[test]
fn a_pattern_that_fails_to_compile_is_equal_cold_and_warm() {
    for operation in MATCHING {
        let arguments = arguments(operation, "a(b", "", "ab");
        let stated = cold_and_warm(operation, Limit::Stated, &arguments);
        assert_eq!(raised(&stated.outcome).kind, SYNTAX_ERROR, "{operation:?}");
        assert_eq!(stated.spent, 0);
        let none = cold_and_warm(operation, Limit::Exactly(0), &arguments);
        assert_eq!(none, stated, "{operation:?} under a limit of zero");
    }
    let check = |heap: &mut PagedHeap| {
        let _ = heap;
        vec![Value::text("a(b"), Value::text("")]
    };
    let run = cold_and_warm(Operation::CompileCheck, Limit::Stated, &check);
    assert_eq!(raised(&run.outcome).kind, SYNTAX_ERROR);
}

/// The cache holds what the embedder allows and no more.
#[test]
fn the_cache_is_bounded_by_the_size_the_embedder_sets() {
    let engine = Engine::new(2);
    for pattern in ["a", "b", "c", "a(", "a"] {
        let _ = engine.check(pattern, "");
        assert!(engine.cached_patterns() <= 2);
    }
    assert_eq!(engine.cached_patterns(), 2);
}

fn call(operation: Operation, arguments: &dyn Fn(&mut PagedHeap) -> Vec<Value>) -> Datum {
    run(&cold(), operation, Limit::Stated, arguments)
        .outcome
        .unwrap()
}

fn error(operation: Operation, arguments: &dyn Fn(&mut PagedHeap) -> Vec<Value>) -> ErrorDatum {
    raised(&run(&cold(), operation, Limit::Stated, arguments).outcome).clone()
}

fn text(text: &str) -> Datum {
    Datum::Text(text.to_string())
}

fn integer(value: i64) -> Datum {
    Datum::Int(Integer::from(value))
}

fn fields(fields: &[(&str, Datum)]) -> Datum {
    Datum::Record(
        fields
            .iter()
            .map(|(name, value)| (name.to_string(), value.clone()))
            .collect(),
    )
}

fn found(index: i64, groups: &[Datum], named: Datum) -> Datum {
    fields(&[
        ("index", integer(index)),
        ("groups", Datum::List(groups.to_vec())),
        ("named", named),
    ])
}

fn exec(pattern: &str, flags: &str, last_index: i64, input: &str) -> Datum {
    call(Operation::Exec, &|heap| {
        vec![heap.regex(pattern, flags, last_index), Value::text(input)]
    })
}

/// `exec` returns the match and the next `lastIndex` instead of writing it
/// (the harness checks that no argument changed).
#[test]
fn exec_returns_the_next_last_index() {
    let at = |index, last_index| {
        fields(&[
            ("match", found(index, &[text("a")], Datum::Null)),
            ("lastIndex", integer(last_index)),
        ])
    };
    let none = |last_index| fields(&[("match", Datum::Null), ("lastIndex", integer(last_index))]);
    // Global: from `lastIndex`, which becomes the match's end.
    assert_eq!(exec("a", "g", 1, "aXaXa"), at(2, 3));
    // Nothing left, or a `lastIndex` past the input: zero.
    assert_eq!(exec("a", "g", 5, "aXaXa"), none(0));
    assert_eq!(exec("a", "g", 9, "aXaXa"), none(0));
    // Sticky: only exactly at `lastIndex`.
    assert_eq!(exec("a", "y", 1, "aXaXa"), none(0));
    assert_eq!(exec("a", "y", 2, "aXaXa"), at(2, 3));
    // Neither: from the start, and `lastIndex` is kept.
    assert_eq!(exec("a", "", 4, "aXaXa"), at(0, 4));
    assert_eq!(exec("b", "", 4, "aXaXa"), none(4));

    let test = call(Operation::Test, &|heap| {
        vec![heap.regex("a", "g", 1), Value::text("aXaXa")]
    });
    assert_eq!(
        test,
        fields(&[("matched", Datum::Bool(true)), ("lastIndex", integer(3))])
    );
}

/// A match lists the whole match and then each group, `null` for a group
/// that took no part, and the named groups by name.
#[test]
fn a_match_lists_its_groups_and_names() {
    assert_eq!(
        exec("(?<year>\\d{4})-(x)?(\\d\\d)", "", 0, "on 2026-10"),
        fields(&[
            (
                "match",
                found(
                    3,
                    &[text("2026-10"), text("2026"), Datum::Null, text("10")],
                    fields(&[("year", text("2026"))]),
                ),
            ),
            ("lastIndex", integer(0)),
        ])
    );
}

/// `match_all` visits a global regex's matches from `lastIndex`, moving past
/// an empty match by one code point under `u` and one code unit otherwise;
/// any other regex gives the one match `exec` finds.
#[test]
fn match_all_visits_every_match_of_a_global_regex() {
    let indexes = |pattern: &str, flags: &str, last_index: i64, input: &str| {
        let Datum::List(matches) = call(Operation::MatchAll, &|heap| {
            vec![heap.regex(pattern, flags, last_index), Value::text(input)]
        }) else {
            panic!("match_all returns a list");
        };
        matches
            .into_iter()
            .map(|found| match found {
                Datum::Record(fields) => fields[0].1.clone(),
                other => panic!("not a match: {other:?}"),
            })
            .collect::<Vec<_>>()
    };
    let integers = |values: &[i64]| {
        values
            .iter()
            .map(|value| integer(*value))
            .collect::<Vec<_>>()
    };
    assert_eq!(indexes("a", "g", 1, "aXaXa"), integers(&[2, 4]));
    assert_eq!(indexes("a", "gy", 0, "aaXa"), integers(&[0, 1]));
    assert_eq!(indexes("", "gu", 0, "a\u{1f600}b"), integers(&[0, 1, 3, 4]));
    assert_eq!(
        indexes("", "g", 0, "a\u{1f600}b"),
        integers(&[0, 1, 2, 3, 4])
    );
    assert_eq!(indexes("a", "", 3, "aXaXa"), integers(&[0]));
}

/// `replace` expands `$$`, `$&`, `` $` ``, `$'`, `$n`, `$nn` and `$<name>`,
/// replaces every match of a global regex and one match of any other.
#[test]
fn replace_expands_the_replacement_and_returns_the_next_last_index() {
    let replace = |pattern: &str, flags: &str, last_index: i64, input: &str, with: &str| {
        call(Operation::Replace, &|heap| {
            vec![
                heap.regex(pattern, flags, last_index),
                Value::text(input),
                Value::text(with),
            ]
        })
    };
    let result = |value: &str, last_index| {
        fields(&[("text", text(value)), ("lastIndex", integer(last_index))])
    };
    assert_eq!(
        replace(
            "(?<first>\\w+) (\\w+)",
            "",
            7,
            "<hello world>",
            "$2 $<first>|$$|$&|$`|$'|$3|$02|$<none>"
        ),
        result("<world hello|$|hello world|<|>|$3|world|>", 7)
    );
    assert_eq!(replace("a", "g", 2, "aXaXa", "-"), result("-X-X-", 0));
    assert_eq!(replace("a", "", 2, "aXaXa", "-"), result("-XaXa", 2));
    assert_eq!(replace("a", "y", 2, "aXaXa", "-"), result("aX-Xa", 3));
    assert_eq!(replace("a", "y", 1, "aXaXa", "-"), result("aXaXa", 0));
}

/// `split` returns the pieces between matches with each match's captures
/// spliced in, up to `limit` entries.
#[test]
fn split_splices_captures_and_honours_the_limit() {
    let split = |pattern: &str, input: &str, limit: Option<i64>| {
        call(Operation::Split, &|heap| {
            vec![
                heap.regex(pattern, "", 0),
                Value::text(input),
                limit.map_or(Value::Absent, int),
            ]
        })
    };
    let list = |items: &[Datum]| Datum::List(items.to_vec());
    assert_eq!(
        split("(\\d)", "a1b2c", None),
        list(&[text("a"), text("1"), text("b"), text("2"), text("c")])
    );
    assert_eq!(
        split("(\\d)", "a1b2c", Some(2)),
        list(&[text("a"), text("1")])
    );
    assert_eq!(split("(\\d)", "a1b2c", Some(0)), list(&[]));
    assert_eq!(
        split("(x)?b", "ab", None),
        list(&[text("a"), Datum::Null, text("")])
    );
    assert_eq!(
        split("", "abc", None),
        list(&[text("a"), text("b"), text("c")])
    );
    assert_eq!(split("x*", "", None), list(&[]));
    assert_eq!(split("x", "", None), list(&[text("")]));
}

/// A result that would hold half of a surrogate pair is a typed error; the
/// `u` flag matches the pair whole.
#[test]
fn half_a_surrogate_pair_in_a_result_is_a_typed_error() {
    let dot = |flags: &'static str| {
        move |heap: &mut PagedHeap| vec![heap.regex(".", flags, 0), Value::text("\u{1f600}")]
    };
    assert_eq!(error(Operation::Exec, &dot("")).kind, LONE_SURROGATE_ERROR);
    assert_eq!(
        call(Operation::Exec, &dot("u")),
        fields(&[
            ("match", found(0, &[text("\u{1f600}")], Datum::Null)),
            ("lastIndex", integer(0)),
        ])
    );
}

/// `compile_check` returns the regex record with its flags in canonical
/// order, and refuses what every other function refuses.
#[test]
fn compile_check_returns_the_regex_record_or_a_syntax_error() {
    let check = |pattern: &str, flags: &str| {
        let arguments = move |_: &mut PagedHeap| vec![Value::text(pattern), Value::text(flags)];
        run(&cold(), Operation::CompileCheck, Limit::Stated, &arguments).outcome
    };
    assert_eq!(
        check("a+", "yg").unwrap(),
        fields(&[
            ("brand", text(BRAND)),
            ("pattern", text("a+")),
            ("flags", text("gy")),
            ("lastIndex", integer(0)),
        ])
    );
    let too_long = "a".repeat(MAX_PATTERN_UNITS + 1);
    let too_deep = "(".repeat(MAX_GROUP_NESTING + 1) + &")".repeat(MAX_GROUP_NESTING + 1);
    for (pattern, flags) in [
        ("a", "gg"),
        ("a", "x"),
        ("a", "v"),
        ("a", "d"),
        ("a**", ""),
        (too_long.as_str(), ""),
        (too_deep.as_str(), ""),
    ] {
        assert_eq!(
            raised(&check(pattern, flags)).kind,
            SYNTAX_ERROR,
            "/{pattern}/{flags}"
        );
    }
}

/// A function is strict over its operands: a record that is not a regex,
/// or an operand of another kind, is a `type_error`, and a negative index
/// a `number_range`.
#[test]
fn operands_of_the_wrong_shape_are_typed_errors() {
    let kind =
        |arguments: &dyn Fn(&mut PagedHeap) -> Vec<Value>| error(Operation::Exec, arguments).kind;
    assert_eq!(
        kind(&|heap| {
            let regex = heap.record(&[
                ("brand", Value::text("regex.python")),
                ("pattern", Value::text("a")),
                ("flags", Value::text("")),
                ("lastIndex", int(0)),
            ]);
            vec![regex, Value::text("a")]
        }),
        "type_error"
    );
    assert_eq!(
        kind(&|heap| {
            let regex = heap.record(&[
                ("brand", Value::text(BRAND)),
                ("pattern", Value::text("a")),
                ("flags", Value::text("")),
                ("lastIndex", int(0)),
                ("extra", Value::Null),
            ]);
            vec![regex, Value::text("a")]
        }),
        "type_error"
    );
    assert_eq!(
        kind(&|_| vec![Value::text("a"), Value::text("a")]),
        "type_error"
    );
    assert_eq!(
        kind(&|heap| vec![heap.regex("a", "", 0), int(1)]),
        "type_error"
    );
    assert_eq!(
        kind(&|heap| vec![heap.regex("a", "g", -1), Value::text("a")]),
        "number_range"
    );
    assert_eq!(
        error(Operation::Split, &|heap| {
            vec![heap.regex("a", "", 0), Value::text("a"), int(-1)]
        })
        .kind,
        "number_range"
    );
}

/// REGEX-COMPILER-POLICY: the host narrows length and native group recursion together.
#[test]
fn a_host_pattern_policy_controls_compiler_admission() {
    let engine = Engine::new(0).with_limits(crate::PatternLimits::new(3, 1).unwrap());
    assert!(engine.check("abc", "").is_ok());
    assert!(matches!(
        engine.check("abcd", ""),
        Err(crate::SyntaxError::PatternTooLong { limit: 3, .. })
    ));
    let engine = Engine::new(0).with_limits(crate::PatternLimits::new(32, 1).unwrap());
    assert!(matches!(
        engine.check("((a))", ""),
        Err(crate::SyntaxError::PatternTooDeep { limit: 1 })
    ));
}
