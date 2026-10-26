//! K-LIB-006 for the text/JSON natives (FIG-6144's corpus law, ported onto
//! heap values): every definition has a named case whose result, charge and
//! determinism are specified independently of the implementation.
use std::collections::BTreeMap;

use lash_kernel_doc::{Datum, ErrorDatum, FunctionRegistry, NativeError, WorkCounter};

use super::super::register_text_json;
use super::lab::{Lab, deep, unicode};
use crate::testsupport;

/// The error a call raised, as data; `None` for any other answer.
fn error_datum(lab: &Lab, error: &NativeError) -> Option<ErrorDatum> {
    match testsupport::error_datum(&lab.heap, error)? {
        Datum::Error(error) => Some(*error),
        _ => None,
    }
}

/// Whether a datum holds no other value.
fn scalar(datum: &lash_kernel_doc::Datum) -> bool {
    use lash_kernel_doc::Datum;
    !matches!(
        datum,
        Datum::Tuple(_)
            | Datum::List(_)
            | Datum::Map(_)
            | Datum::Set(_)
            | Datum::Record(_)
            | Datum::Error(_)
    )
}

#[test]
fn k_lib_006_native_corpus_pins_results_charges_and_determinism() {
    use lash_kernel_doc::{Bytes, Datum, Float, Integer, Measure, Operand};

    use num_traits::ToPrimitive;
    let t = |s: &str| Datum::Text(s.to_owned());
    let i = |n: i64| Datum::Int(Integer::from(n));
    let b = |bytes: &[u8]| Datum::Bytes(Bytes::new(bytes.to_vec()));
    let f = |n: f64| Datum::Float(Float::new(n));
    // Expected answers are specified independently of the implementation.
    // The printed shards contain concrete content identities and charges.
    let mut cases = vec![
        (
            "K-VAL-007",
            "text.len".to_owned(),
            vec![t("a😀z")],
            Ok(i(3)),
        ),
        (
            "K-VAL-007",
            "text.utf16_len".to_owned(),
            vec![t("a😀z")],
            Ok(i(4)),
        ),
        (
            "K-VAL-007",
            "text.get".to_owned(),
            vec![t("a😀z"), i(1)],
            Ok(t("😀")),
        ),
        (
            "K-VAL-007",
            "text.utf16_get".to_owned(),
            vec![t("a😀z"), i(2)],
            Ok(i(0xde00)),
        ),
        (
            "K-LTXT-002",
            "text.slice".to_owned(),
            vec![t("a😀z"), i(1), i(2)],
            Ok(t("😀")),
        ),
        (
            "K-VAL-007",
            "text.utf16_slice".to_owned(),
            vec![t("a😀z"), i(1), i(3)],
            Ok(t("😀")),
        ),
        (
            "K-VAL-007",
            "text.utf16_slice".to_owned(),
            vec![t("a😀z"), i(1), i(2)],
            Err("text_boundary"),
        ),
        (
            "K-LTXT-003",
            "text.find".to_owned(),
            vec![t("😀z"), t("z"), i(0)],
            Ok(i(1)),
        ),
        (
            "K-LTXT-003",
            "text.utf16_find".to_owned(),
            vec![t("😀z"), t("z"), i(0)],
            Ok(i(2)),
        ),
        (
            "K-VAL-030",
            "text.compare".to_owned(),
            vec![t("😀"), t("\u{e000}")],
            Ok(i(1)),
        ),
        (
            "K-VAL-030",
            "text.utf16_compare".to_owned(),
            vec![t("😀"), t("\u{e000}")],
            Ok(i(-1)),
        ),
        (
            "K-LTXT-005",
            "text.concat".to_owned(),
            vec![t("a"), t("😀")],
            Ok(t("a😀")),
        ),
        // NFC composes U+0065 U+0301 into U+00E9 (Python unicodedata.normalize).
        // Charge: 1 + (1 + 3 input bytes) + (1 + 3 form bytes) + (1 + 2 result bytes) = 12.
        (
            "K-LTXT-010",
            "text.normalize_u17_0_0".to_owned(),
            vec![t("e\u{301}"), t("NFC")],
            Ok(t("\u{e9}")),
        ),
        (
            "K-LTXT-008",
            "text.starts_with".to_owned(),
            vec![t("😀a"), t("😀")],
            Ok(Datum::Bool(true)),
        ),
        (
            "K-LTXT-008",
            "text.ends_with".to_owned(),
            vec![t("😀a"), t("a")],
            Ok(Datum::Bool(true)),
        ),
        (
            "K-LTXT-005",
            "text.split".to_owned(),
            vec![t("a..b.."), t("..")],
            Ok(Datum::List(vec![t("a"), t("b"), t("")])),
        ),
        (
            "K-LTXT-005",
            "text.join".to_owned(),
            vec![Datum::List(vec![t("a"), t("😀")]), t("|")],
            Ok(t("a|😀")),
        ),
        (
            "K-LTXT-012",
            "text.join_or_index".to_owned(),
            vec![Datum::List(vec![t("a"), t("😀")]), t("|")],
            Ok(t("a|😀")),
        ),
        (
            "K-LTXT-012",
            "text.join_or_index".to_owned(),
            vec![Datum::List(vec![t("a"), Datum::Null, t("z")]), t("|")],
            Ok(i(1)),
        ),
        (
            "K-LTXT-005",
            "text.replace".to_owned(),
            vec![t("aaa"), t("aa"), t("x")],
            Ok(t("xa")),
        ),
        (
            "K-LTXT-007",
            "text.repeat".to_owned(),
            vec![t("😀a"), i(2)],
            Ok(t("😀a😀a")),
        ),
        (
            "K-LTXT-007",
            "text.pad_start".to_owned(),
            vec![t("😀"), i(4), t("ab")],
            Ok(t("aba😀")),
        ),
        (
            "K-LTXT-007",
            "text.pad_end".to_owned(),
            vec![t("😀"), i(3), t("ab")],
            Ok(t("😀ab")),
        ),
        (
            "K-LTXT-009",
            "text.to_code_points".to_owned(),
            vec![t("😀a")],
            Ok(Datum::List(vec![i(0x1f600), i(97)])),
        ),
        (
            "K-LTXT-009",
            "text.to_utf16_units".to_owned(),
            vec![t("😀a")],
            Ok(Datum::List(vec![i(0xd83d), i(0xde00), i(97)])),
        ),
        (
            "K-VAL-006",
            "text.from_code_points".to_owned(),
            vec![Datum::List(vec![i(0x1f600), i(97)])],
            Ok(t("😀a")),
        ),
        (
            "K-VAL-006",
            "text.from_code_points".to_owned(),
            vec![Datum::List(vec![i(0xd800)])],
            Err("invalid_scalar"),
        ),
        (
            "K-VAL-006",
            "text.from_utf16_units".to_owned(),
            vec![Datum::List(vec![i(0xd83d), i(0xde00), i(97)])],
            Ok(t("😀a")),
        ),
        (
            "K-VAL-006",
            "text.from_utf16_units".to_owned(),
            vec![Datum::List(vec![i(0xd800)])],
            Err("invalid_utf16"),
        ),
        (
            "K-VAL-008",
            "bytes.from_octets".to_owned(),
            vec![Datum::List(vec![i(0), i(255)])],
            Ok(b(&[0, 255])),
        ),
        (
            "K-VAL-008",
            "bytes.slice".to_owned(),
            vec![b(&[0, 128, 255]), i(-2), i(9)],
            Ok(b(&[128, 255])),
        ),
        (
            "K-VAL-008",
            "bytes.concat".to_owned(),
            vec![b(&[0]), b(&[255])],
            Ok(b(&[0, 255])),
        ),
        (
            "K-VAL-008",
            "bytes.compare".to_owned(),
            vec![b(&[0]), b(&[0, 1])],
            Ok(i(-1)),
        ),
        (
            "K-LBYTES-002",
            "bytes.utf8_encode".to_owned(),
            vec![t("😀")],
            Ok(b(&[0xf0, 0x9f, 0x98, 0x80])),
        ),
        (
            "K-LBYTES-002",
            "bytes.utf8_decode".to_owned(),
            vec![b(&[0xf0, 0x9f, 0x98, 0x80])],
            Ok(t("😀")),
        ),
        (
            "K-LBYTES-002",
            "bytes.utf8_decode".to_owned(),
            vec![b(&[0xed, 0xa0, 0x80])],
            Err("invalid_utf8"),
        ),
        (
            "K-EFF-005",
            "json.parse".to_owned(),
            vec![t("18446744073709551617e2"), t("int"), t("float")],
            Ok(Datum::Int(
                Integer::parse("1844674407370955161700").unwrap(),
            )),
        ),
        (
            "K-EFF-006",
            "json.parse".to_owned(),
            vec![t("[1,1e3]"), t("number"), t("by_spelling")],
            Ok(Datum::List(vec![i(1), f(1000.0)])),
        ),
        (
            "K-EFF-007",
            "json.parse".to_owned(),
            vec![t("1.5"), t("int"), t("by_spelling")],
            Err("effect_result"),
        ),
        (
            "K-LJSON-001",
            "json.parse".to_owned(),
            vec![t(r#""\ud800""#), t("number"), t("by_spelling")],
            Err("json_syntax"),
        ),
        (
            "K-LJSON-004",
            "json.stringify".to_owned(),
            vec![Datum::Int(Integer::parse("18446744073709551617").unwrap())],
            Ok(t("18446744073709551617")),
        ),
        (
            "K-LJSON-004",
            "json.stringify".to_owned(),
            vec![f(f64::NAN)],
            Err("json_number"),
        ),
        (
            "K-LJSON-004",
            "json.stringify".to_owned(),
            vec![Datum::Absent],
            Err("not_data"),
        ),
        (
            "K-LJSON-008",
            "json.render".to_owned(),
            vec![
                Datum::List(vec![t("x"), i(2), Datum::List(vec![f(2.5), Datum::Null])]),
                Datum::Set(vec![]),
                Datum::Absent,
                Datum::Tuple(vec![i(-5), i(21), Datum::Bool(true), Datum::Bool(false)]),
            ],
            Ok(t(r#"["x",2,[2.5,null]]"#)),
        ),
        // One record node, four two-unit comparisons, one list node and
        // two scalar nodes: work = 12, encoded admission = 2 * 12 + 1.
        (
            "K-LJSON-007",
            "json.classify".to_owned(),
            vec![
                Datum::Record(vec![(
                    "a".into(),
                    Datum::List(vec![Datum::Null, Datum::Bool(true)]),
                )]),
                i(8),
                Datum::Tuple(vec![t("toJSON"), t("brand")]),
                t("0"),
                t(":"),
                Datum::Bool(true),
            ],
            Ok(i(25)),
        ),
        // The first excluded field refuses before its value or any later
        // field is read: work = 1 + (1 + 6), encoded refusal = 2 * 8.
        (
            "K-LJSON-007",
            "json.classify".to_owned(),
            vec![
                Datum::Record(vec![
                    ("toJSON".into(), Datum::List(vec![i(7), i(8)])),
                    ("later".into(), Datum::List(vec![Datum::Null; 8])),
                ]),
                i(8),
                Datum::Tuple(vec![t("toJSON"), t("brand")]),
                t("0"),
                t(":"),
                Datum::Bool(true),
            ],
            Ok(i(16)),
        ),
        (
            "K-LFMT-003",
            "format.decimal_parts".to_owned(),
            vec![f(-0.125)],
            Ok(Datum::Tuple(vec![
                t("finite"),
                Datum::Bool(true),
                t("125"),
                i(-3),
            ])),
        ),
        (
            "K-LFMT-001",
            "format.fixed".to_owned(),
            vec![f(2.5), i(0)],
            Ok(t("2")),
        ),
        (
            "K-LFMT-001",
            "format.scientific".to_owned(),
            vec![f(125.0), i(2)],
            Ok(t("1.25e2")),
        ),
        (
            "K-LFMT-002",
            "format.radix".to_owned(),
            vec![i(-255), i(16)],
            Ok(t("-ff")),
        ),
        (
            "K-LFMT-002",
            "format.pad".to_owned(),
            vec![t("😀"), i(3), t("0"), t("start")],
            Ok(t("00😀")),
        ),
    ];
    for (name, input, expected) in [
        ("trim", "\u{2003}a\u{85}", "a"),
        ("trim_start", "\u{2003}a ", "a "),
        ("trim_end", " a\u{85}", " a"),
        ("lower", "İΟΣ", "i\u{307}ος"),
        ("upper", "straße", "STRASSE"),
    ] {
        cases.push(("K-LTXT-006", unicode(name), vec![t(input)], Ok(t(expected))));
    }
    cases.push((
        "K-LTXT-011",
        "text.trim_set".to_owned(),
        vec![t("aa😀aaa"), t("a"), Datum::Bool(true), Datum::Bool(true)],
        Ok(t("😀")),
    ));
    // K-VAL-008/009: byte indexing is strict and timestamp construction/extraction
    // preserves the signed nanosecond count, without a host read.
    cases.extend([
        (
            "K-VAL-008",
            "bytes.at".to_owned(),
            vec![b(&[0, 255]), i(1)],
            Ok(i(255)),
        ),
        (
            "K-VAL-008",
            "bytes.at".to_owned(),
            vec![b(&[0, 255]), i(-1)],
            Err("index"),
        ),
        (
            "K-VAL-008",
            "bytes.at".to_owned(),
            vec![b(&[0, 255]), i(2)],
            Err("index"),
        ),
        (
            "K-VAL-009",
            "timestamp.from_nanoseconds".to_owned(),
            vec![i(-123)],
            Ok(Datum::Timestamp(lash_kernel_doc::Timestamp {
                nanoseconds: (-123).into(),
            })),
        ),
        (
            "K-VAL-009",
            "timestamp.nanoseconds".to_owned(),
            vec![Datum::Timestamp(lash_kernel_doc::Timestamp {
                nanoseconds: (-123).into(),
            })],
            Ok(i(-123)),
        ),
        (
            "K-VAL-009",
            "timestamp.nanoseconds".to_owned(),
            vec![i(-123)],
            Err("type_error"),
        ),
    ]);
    let mut registry = FunctionRegistry::new();
    register_text_json(&mut registry).unwrap();
    let mut exercised = std::collections::BTreeSet::new();
    let mut shards: BTreeMap<&str, Vec<serde_json::Value>> = BTreeMap::new();
    for (ordinal, (rule, name, args, expected)) in cases.into_iter().enumerate() {
        let result_magnitude = match &expected {
            Ok(Datum::Int(value)) => value.to_u64().unwrap_or(0),
            _ => 0,
        };
        let (id, registered) = registry
            .iter()
            .find(|(_, f)| f.definition().name.as_str() == name)
            .unwrap();
        exercised.insert(*id);
        let mut lab = Lab::new();
        let values = lab.values(&args);
        let mut counter = WorkCounter::new(None);
        let actual = lab.counted(&name, &values, &mut counter);
        let (outcome, result_size, result_scalar) = match (actual, expected) {
            (Ok(value), Ok(expected)) => {
                let actual = lab.datum(&value);
                assert_eq!(actual, expected, "{name}");
                let scalar = scalar(&actual);
                (
                    serde_json::json!({"returned":actual}),
                    deep(&actual),
                    scalar,
                )
            }
            (Err(error), Err(kind)) if error_datum(&lab, &error).is_some() => {
                let error = error_datum(&lab, &error).unwrap();
                assert_eq!(error.kind, kind, "{name}");
                (serde_json::json!({"raised":error}), 0, true)
            }
            (actual, expected) => panic!("{name}: {actual:?} != {expected:?}"),
        };
        let param = |name: &lash_kernel_value::names::Name| {
            registered
                .definition()
                .signature
                .params
                .iter()
                .position(|p| p.name == *name)
                .and_then(|index| args.get(index))
        };
        let charged = registered
            .definition()
            .charge()
            .expect("formula-metered library function")
            .evaluate(&mut |operand, measure| match (operand, measure) {
                (Operand::Result, Measure::Magnitude) => result_magnitude,
                (Operand::Result, Measure::DeepSize) => result_size,
                // A value holding no other value has its deep size as size.
                (Operand::Result, Measure::Size) if result_scalar => result_size,
                (Operand::Param(param_name), Measure::Size)
                    if name == "text.join_or_index" && param_name.as_str() == "items" =>
                {
                    let Some(Datum::List(items)) = param(param_name) else {
                        panic!("list input");
                    };
                    1 + items.len() as u64
                }
                // An omitted optional argument measures nothing.
                (Operand::Param(name), Measure::DeepSize) => param(name).map_or(0, deep),
                (Operand::Param(name), Measure::Size) if param(name).is_none_or(scalar) => {
                    param(name).map_or(0, deep)
                }
                _ => panic!("unexpected native corpus measure: {operand:?} {measure:?}"),
            });
        let expected_charge = if name == "json.classify" {
            1 + result_magnitude
        } else if name == "text.join_or_index" {
            let Datum::List(items) = &args[0] else {
                panic!("list input");
            };
            1 + 2 * (1 + items.len() as u64) + deep(&args[1]) + 2 * result_size
        } else if name == "json.render" {
            1 + args.iter().map(deep).sum::<u64>() + result_size
        } else if name == "text.trim_set" {
            8 + (1 + deep(&args[0])) * (1 + deep(&args[1])) + result_size
        } else {
            (if name == "format.decimal_parts" {
                65
            } else {
                1
            }) + args.iter().map(deep).sum::<u64>()
                + result_size
        };
        assert_eq!(charged, expected_charge);
        assert_eq!(counter.spent(), 0);
        // A second call on the same implementation and heap must agree, and
        // neither call may change any input object (K-LIB-006, K-LIB-007).
        let mut warm_counter = WorkCounter::new(None);
        let warm = lab.counted(&name, &values, &mut warm_counter);
        let warm = match warm {
            Ok(value) => serde_json::json!({"returned": lab.datum(&value)}),
            Err(error) if error_datum(&lab, &error).is_some() => {
                serde_json::json!({"raised": error_datum(&lab, &error)})
            }
            error => panic!("unexpected warm outcome: {error:?}"),
        };
        assert_eq!(warm, outcome);
        assert_eq!(warm_counter.spent(), 0);
        assert_eq!(
            values
                .iter()
                .map(|value| lab.datum(value))
                .collect::<Vec<_>>(),
            args
        );
        shards.entry(rule).or_default().push(serde_json::json!({
            "name":format!("{name}.{ordinal}"), "function":id, "args":args,
            "expected":{"outcome":outcome,"charged":charged,"work":0}
        }));
    }
    assert_eq!(
        exercised.len(),
        registry.len(),
        "every native definition needs a named corpus case"
    );
    for (rule, cases) in shards {
        println!(
            "NATIVE_CORPUS {}",
            serde_json::json!({"rule":rule,"cases":cases})
        );
    }
}
