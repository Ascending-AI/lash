//! FIG-6372: helpers agree exactly with recorded real-language observations;
//! compared through the embedding API, with named semantic deviations.
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

#[path = "../shared/helper_differential/comparison.rs"]
mod comparison;

#[path = "../shared/helper_differential/runner.rs"]
mod runner;
use std::collections::{BTreeMap, BTreeSet};
use std::sync::OnceLock;

use lash_kernel_doc::Datum;
use runner::{Case, Runner, tag};
use serde::Deserialize;
use serde_json::{Value, json};

/// K-CHG-007: index comparison and conversion may read every integer word.
#[test]
fn number_index_charge_bounds_arbitrary_integer_comparisons() {
    use lash_ext_helper_compiler::library::{Dialect, Library};
    use lash_kernel_doc::Operand;
    let library = Library::of(Dialect::TypeScript).unwrap();
    let (_, function) = library
        .registry
        .iter()
        .find(|(_, f)| f.name() == "ts.number_index")
        .unwrap();
    let formula = function.definition().charge().unwrap().clone();
    for words in [1_u64, 16, 128, 1024] {
        let charged = formula.evaluate(&mut |operand, _| match operand {
            Operand::Param(name) if name.as_str() == "key" => 1 + words,
            Operand::Param(_) => 2 + words,
            Operand::Result => 1,
        });
        assert!(
            charged >= 4 + 2 * words,
            "{words} words need comparison and conversion work, got {charged}"
        );
    }
}

/// K-CHG-007: quoting includes scalar lookup, scanning and output bytes.
#[test]
fn text_repr_charge_bounds_unicode_search_and_output() {
    use lash_ext_helper_compiler::library::{Dialect, Library};
    use lash_kernel_doc::Operand;
    let library = Library::of(Dialect::Python).unwrap();
    let (_, function) = library
        .registry
        .iter()
        .find(|(_, f)| f.name() == "py.text_repr")
        .unwrap();
    let formula = function.definition().charge().unwrap().clone();
    for count in [1_u64, 16, 256] {
        // U+0378 is unassigned in Unicode 15 and is spelled \\u0378.
        let input_bytes = 2 * count;
        let output_bytes = 2 + 6 * count;
        let charged = formula.evaluate(&mut |operand, _| match operand {
            Operand::Param(_) => 1 + input_bytes,
            Operand::Result => 1 + output_bytes,
        });
        let required = 3 * input_bytes + 10 * count + output_bytes;
        assert!(
            charged >= required,
            "{count} unassigned scalars need {required} work, got {charged}"
        );
    }
}

/// K-LIB-006/007 and K-CHG-007: numeric keys compare before rounding,
/// preserve signed zero and overflow errors, and never coerce a nonnumber.
#[test]
fn number_index_native_matches_its_kernel_body() {
    use lash_kernel_doc::{Float, Integer, Literal};
    let native = Runner::new("typescript").unwrap();
    let body = Runner::bodies("typescript").unwrap();
    let mut keys = vec![
        Literal::Absent,
        Literal::Null,
        Literal::Bool(true),
        Literal::Text("0".into()),
    ];
    for value in [
        -1.0,
        -0.0,
        0.0,
        f64::from_bits(1),
        0.5,
        1.0,
        2.0,
        9007199254740992.0,
        f64::NAN,
        f64::INFINITY,
        f64::NEG_INFINITY,
    ] {
        keys.push(Literal::Float(Float::new(value)));
    }
    for value in [
        "-9223372036854775809",
        "0",
        "1",
        "9007199254740993",
        "9007199254740995",
        "9223372036854775807",
        "9223372036854775808",
    ] {
        keys.push(Literal::Int(Integer::parse(value).unwrap()));
    }
    keys.push(Literal::Int(
        Integer::parse(&format!("1{}", "0".repeat(400))).unwrap(),
    ));
    let mut lengths = vec![
        Literal::Int(0.into()),
        Literal::Int(2.into()),
        Literal::Int(Integer::parse("9007199254740993").unwrap()),
        Literal::Int(Integer::parse(&format!("2{}", "0".repeat(400))).unwrap()),
    ];
    for value in [
        -1.0,
        -0.0,
        1.0,
        2.0,
        9007199254740992.0,
        9007199254740996.0,
        f64::NAN,
        f64::INFINITY,
    ] {
        lengths.push(Literal::Float(Float::new(value)));
    }
    let mut cases = 0;
    for key in keys {
        for length in &lengths {
            let args = vec![key.clone(), length.clone()];
            assert_eq!(
                native.leaf("ts.number_index", args.clone()).unwrap(),
                body.leaf("ts.number_index", args.clone()).unwrap(),
                "{args:?}"
            );
            cases += 1;
        }
    }
    println!("numeric indexing: {cases} native/body pairs");
}

/// K-LIB-006: Unicode 15 boundaries, CPython's extra whitespace,
/// non-word and out-of-scalar integers retain the source body's answer.
#[test]
fn unicode_native_leaves_match_their_kernel_bodies() {
    use lash_ext_helper_compiler::library::{Dialect, Library};
    use lash_kernel_doc::{Expr, Literal, Rhs, Stmt};
    let library = Library::of(Dialect::Python).unwrap();
    let native = Runner::new("python").unwrap();
    let body = Runner::bodies("python").unwrap();
    let mut pairs = 0;
    for name in ["py.text.whitespace", "py.text.printable"] {
        let (_, function) = library
            .registry
            .iter()
            .find(|(_, f)| f.name() == name)
            .unwrap();
        let mut points = BTreeSet::from([
            -1,
            0,
            9,
            13,
            28,
            32,
            126,
            127,
            0x10ffff,
            0x110000,
            i64::MIN,
            i64::MAX,
        ]);
        for statement in &function.definition().body().unwrap().block {
            if let Stmt::Let {
                name,
                value: Rhs::Expr(Expr::List(ranges)),
            } = statement
            {
                if name.as_str() != "ranges" {
                    continue;
                }
                for range in ranges {
                    if let Expr::Tuple(pair) = range {
                        for bound in pair {
                            if let Expr::Literal(Literal::Int(point)) = bound {
                                let point = point.word().unwrap();
                                points.extend([point - 1, point, point + 1]);
                            }
                        }
                    }
                }
            }
        }
        for point in points {
            let args = vec![Literal::Int(point.into())];
            assert_eq!(
                native.leaf(name, args.clone()).unwrap(),
                body.leaf(name, args).unwrap(),
                "{name} {point}"
            );
            pairs += 1;
        }
        for point in [
            "-999999999999999999999999999",
            "999999999999999999999999999",
        ] {
            let args = vec![Literal::Int(
                lash_kernel_doc::Integer::parse(point).unwrap(),
            )];
            assert_eq!(
                native.leaf(name, args.clone()).unwrap(),
                body.leaf(name, args).unwrap()
            );
            pairs += 1;
        }
    }
    println!("Unicode lookup: {pairs} native/body pairs");
}

/// K-LIB-006/007: quote choice, escapes and Unicode encoding boundaries
/// agree with the kernel reference, at the same completion charge.
#[test]
fn text_quoting_native_matches_its_kernel_body_and_charge_bound() {
    use lash_ext_helper_compiler::library::{Dialect, Library};
    use lash_kernel_doc::{Literal, Operand, WorkCounter};
    let native = Runner::new("python").unwrap();
    let body = Runner::bodies("python").unwrap();
    let mut texts: Vec<String> = [
        "",
        "plain",
        "it's",
        "say \"hi\"",
        "'\"\\\t\n\r\0",
        "é😀\u{378}\u{a0}\u{200d}\u{e000}\u{10ffff}",
        "\u{ff}\u{100}\u{ffff}\u{10000}",
    ]
    .into_iter()
    .map(str::to_owned)
    .collect();
    texts.extend([
        "\u{7f}".repeat(64),
        "a".repeat(15),
        "\u{378}".repeat(256),
        "a😀\n".repeat(2048),
    ]);
    let library = Library::of(Dialect::Python).unwrap();
    let (_, function) = library
        .registry
        .iter()
        .find(|(_, f)| f.name() == "py.text_repr")
        .unwrap();
    let formula = function.definition().charge().unwrap();
    let implementation = lash_ext_py_core::native("py.text_repr").unwrap();
    for text in &texts {
        let args = vec![Literal::Text(text.clone())];
        let actual = native.leaf("py.text_repr", args.clone()).unwrap();
        assert_eq!(actual, body.leaf("py.text_repr", args).unwrap(), "{text:?}");
        let Ok(Datum::Text(result)) = actual.0 else {
            panic!("quoting did not return text")
        };
        let mut heap = lash_kernel_state::arena::Bench::new();
        let value = heap.build(&Datum::Text(text.clone())).unwrap();
        let mut counter = WorkCounter::new(None);
        let called = heap.call(&*implementation, &[value], 64 << 20, &mut counter);
        assert_eq!(
            heap.datum(&called.answer.unwrap()).unwrap(),
            Datum::Text(result.clone())
        );
        let charge = formula.evaluate(&mut |operand, _| match operand {
            Operand::Param(_) => 1 + text.len() as u64,
            Operand::Result => 1 + result.len() as u64,
        });
        assert!(
            counter.spent() <= charge,
            "native work {} exceeds {charge}",
            counter.spent()
        );
    }
    for value in [
        Literal::Absent,
        Literal::Null,
        Literal::Bool(true),
        Literal::Int(1.into()),
    ] {
        let args = vec![value];
        assert_eq!(
            native.leaf("py.text_repr", args.clone()).unwrap(),
            body.leaf("py.text_repr", args).unwrap()
        );
    }
    println!(
        "text quoting: {} text and 4 typed-error native/body pairs; measured-work bounds",
        texts.len()
    );
}

#[derive(Deserialize)]
struct Expected {
    engine: String,
    cases: BTreeMap<String, Value>,
}
#[derive(Deserialize)]
struct Deviation {
    rule: String,
    reason: String,
    case: String,
    expected: Value,
}
struct Results {
    mismatches: Vec<String>,
}
fn corpus(dialect: &str) -> Results {
    let cases: Vec<Case> =
        serde_json::from_str(include_str!("helper_differential/cases.json")).unwrap();
    let expected: Expected = serde_json::from_str(match dialect {
        "typescript" => include_str!("helper_differential/typescript.expected.json"),
        "python" => include_str!("helper_differential/python.expected.json"),
        _ => unreachable!(),
    })
    .unwrap();
    assert_eq!(
        expected.engine,
        if dialect == "typescript" {
            "v25.2.1"
        } else {
            "3.12.3"
        }
    );
    let deviations: Vec<Deviation> =
        serde_json::from_str(include_str!("helper_differential/deviations.json")).unwrap();
    let runner = Runner::new(dialect).unwrap();
    let mut mismatches = vec![];
    let mut ids = BTreeSet::new();
    for case in cases.iter().filter(|case| case.dialect == dialect) {
        assert!(ids.insert(case.id.clone()), "duplicate {}", case.id);
        let reference = &expected.cases[&case.id];
        match runner.run(case) {
            Err(error) => mismatches.push(format!("{}: {error}", case.id)),
            Ok(observed) => {
                let approximate = comparison::implementation_approximated(&observed.helpers);
                let deviation = deviations.iter().find(|d| d.case == case.id);
                let wanted = deviation.map_or(reference, |d| {
                    assert!(!d.reason.is_empty() && !d.rule.is_empty());
                    assert_ne!(&d.expected, reference, "{}: stale deviation", case.id);
                    &d.expected
                });
                if !comparison::matches(&observed.result, wanted, approximate) {
                    println!("diff\t{}\t{}\t{}", case.id, reference, observed.result);
                    mismatches.push(format!(
                        "{}: source={}\nreference={reference}\nactual={}",
                        case.id, case.source, observed.result
                    ));
                }
            }
        }
    }
    assert!(!ids.is_empty(), "the corpus must not be empty");
    for deviation in deviations
        .iter()
        .filter(|d| d.case.starts_with(&format!("{dialect}/")))
    {
        assert!(
            ids.contains(&deviation.case),
            "orphan deviation {}",
            deviation.case
        );
    }
    assert_eq!(ids, expected.cases.keys().cloned().collect());
    println!(
        "{dialect}: {} corpus cases, {} mismatches",
        ids.len(),
        mismatches.len()
    );
    Results { mismatches }
}
fn results(dialect: &str) -> &'static Results {
    static TS: OnceLock<Results> = OnceLock::new();
    static PY: OnceLock<Results> = OnceLock::new();
    match dialect {
        "typescript" => TS.get_or_init(|| corpus(dialect)),
        "python" => PY.get_or_init(|| corpus(dialect)),
        _ => unreachable!(),
    }
}
#[test]
fn typescript_helpers_match_node() {
    let r = results("typescript");
    assert!(r.mismatches.is_empty(), "{}", r.mismatches.join("\n"));
}
#[test]
fn python_helpers_match_cpython() {
    let r = results("python");
    assert!(r.mismatches.is_empty(), "{}", r.mismatches.join("\n"));
}
/// K-HOST-003 and FIG-6368: range and type alone admit division by 2^64.
#[test]
fn host_random_matches_injected_top_53_bits_exactly() {
    let runner = Runner::new("typescript").unwrap();
    for bits in [0, 1, 2047, 2048, 1 << 63, u64::MAX] {
        let case = Case {
            id: format!("bits-{bits}"),
            dialect: "typescript".into(),
            source: "await host.observe(Math.random());".into(),
            bits,
            stride: 0,
            nanoseconds: 0,
        };
        let value = (bits >> 11) as f64 * 2.0_f64.powi(-53);
        assert_eq!(
            runner.run(&case).unwrap().result,
            json!({"type":"observations", "value":[tag(&Datum::Float(value.into()))]}),
            "bits={bits}"
        );
    }
}
/// K-HOST-003: a counter that traverses all top-53-bit buckets visits both halves.
#[test]
fn host_random_counter_draws_cover_both_halves_exactly() {
    let runner = Runner::new("typescript").unwrap();
    let stride = 1_u64 << 52;
    let case = Case {
        id: "counter-4096".into(),
        dialect: "typescript".into(),
        source: "for (let i=0; i<4096; i++) { await host.observe(Math.random()); }".into(),
        bits: 0,
        stride,
        nanoseconds: 0,
    };
    let expected: Vec<_> = (0..4096_u64)
        .map(|i| {
            tag(&Datum::Float(
                (((i * stride) >> 11) as f64 * 2.0_f64.powi(-53)).into(),
            ))
        })
        .collect();
    assert_eq!(
        runner.run(&case).unwrap().result,
        json!({"type":"observations", "value":expected})
    );
    let lower = expected
        .iter()
        .filter(|v| {
            let bits = u64::from_str_radix(v["bits"].as_str().unwrap(), 16).unwrap();
            f64::from_bits(bits) < 0.5
        })
        .count();
    assert_eq!((lower, expected.len() - lower), (2048, 2048));
}
/// K-HOST-002: epoch milliseconds and UTC fields come from the injected instant.
#[test]
fn host_clock_helpers_match_injected_instant_exactly() {
    let runner = Runner::new("typescript").unwrap();
    // Leap day, sub-millisecond truncation, epoch, and pre-epoch time.
    for (ns, ms, iso, fields) in [
        (
            0_i64,
            0.0,
            "1970-01-01T00:00:00.000Z",
            vec![1970., 0., 1., 4., 0., 0., 0., 0.],
        ),
        (
            1709210096789123456,
            1709210096789.,
            "2024-02-29T12:34:56.789Z",
            vec![2024., 1., 29., 4., 12., 34., 56., 789.],
        ),
        (
            -1_000_000,
            -1.,
            "1969-12-31T23:59:59.999Z",
            vec![1969., 11., 31., 3., 23., 59., 59., 999.],
        ),
    ] {
        let case = Case { id: "clock".into(), dialect: "typescript".into(), bits: 0, stride: 0, nanoseconds: ns,
            source: "const d = new Date(); await host.observe([Date.now(), d.getTime(), d.toISOString(), [d.getUTCFullYear(),d.getUTCMonth(),d.getUTCDate(),d.getUTCDay(),d.getUTCHours(),d.getUTCMinutes(),d.getUTCSeconds(),d.getUTCMilliseconds()], Date() === d.toString()]);".into() };
        let expected = Datum::List(vec![
            Datum::Float(ms.into()),
            Datum::Float(ms.into()),
            Datum::Text(iso.into()),
            Datum::List(fields.into_iter().map(|n| Datum::Float(n.into())).collect()),
            Datum::Bool(true),
        ]);
        assert_eq!(
            runner.run(&case).unwrap().result,
            json!({"type":"observations", "value":[tag(&expected)]})
        );
    }
}

/// FIG-6372's ECMA-262 §21.3.2 ruling: approximation never covers special
/// values, exact helper classes, or errors greater than one ULP.
#[test]
fn implementation_approximated_comparison_keeps_its_one_ulp_boundary() {
    let value = |bits| tag(&Datum::Float(f64::from_bits(bits).into()));
    let one = 1.0_f64.to_bits();
    assert!(!comparison::matches(&value(one), &value(one + 1), false));
    assert!(comparison::matches(&value(one), &value(one + 1), true));
    assert!(!comparison::matches(&value(one), &value(one + 2), true));
    for special in [0.0_f64, -0.0, f64::INFINITY, f64::NEG_INFINITY, f64::NAN] {
        let bits = special.to_bits();
        assert!(comparison::matches(&value(bits), &value(bits), true));
        if !special.is_nan() {
            assert!(!comparison::matches(
                &value(bits),
                &value(bits ^ (1 << 63)),
                true
            ));
        }
    }
    assert!(!comparison::implementation_approximated(&BTreeSet::from([
        "ts.math.sqrt".into()
    ])));
    assert!(comparison::implementation_approximated(&BTreeSet::from([
        "ts.math.exp".into()
    ])));
}

#[path = "../src/tests/linked.rs"]
mod linked;
