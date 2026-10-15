//! JSON and number formatting natives (FIG-6144's laws, ported onto heap
//! values).
use lash_kernel_doc::{
    Datum, FunctionRegistry, Handle, MapType, Measure, NativeError, NumberPolicy, NumberToken,
    Operand, RecordType, RecordTypeField, Type, Value,
};

use super::super::{decode_number, parse_json, stringify_json};
use super::lab::{Lab, VALUE, b, deep, f, i, list, raised, run, t};

fn number(token: &str, ty: Type, policy: NumberPolicy) -> Result<Datum, String> {
    let mut lab = Lab::new();
    let token = NumberToken::new(token).unwrap();
    let result = lab.spy(|heap| decode_number(&token, &ty, policy, heap));
    match result {
        Ok(value) => Ok(lab.datum(&value)),
        error => Err(lab.kind(error)),
    }
}
fn parsed(lab: &mut Lab, text: &str, ty: &Type, policy: NumberPolicy) -> Result<Value, String> {
    let result = lab.spy(|heap| parse_json(text, ty, policy, heap));
    result.map_err(|error| lab.kind(Err::<(), _>(error)))
}
fn stringified(lab: &mut Lab, value: &Value) -> Result<String, String> {
    let result = lab.spy(|heap| stringify_json(value, heap));
    result.map_err(|error| lab.kind(Err::<(), _>(error)))
}
fn charge_of(name: &str, measure: &mut dyn FnMut(&Operand, Measure) -> u64) -> u64 {
    let mut registry = FunctionRegistry::new();
    super::super::register_text_json(&mut registry).unwrap();
    let (_, function) = registry
        .iter()
        .find(|(_, f)| f.definition().name.as_str() == name)
        .unwrap();
    function
        .definition()
        .charge
        .evaluate(&mut |operand, kind| measure(operand, kind))
}
fn big(digits: &str) -> Datum {
    i(digits.parse::<num_bigint::BigInt>().unwrap())
}

#[test]
fn k_eff_005_json_integer_past_u64_round_trips_without_float() {
    let mut lab = Lab::new();
    let text = "18446744073709551617000000000000000000001";
    let value = parsed(&mut lab, text, &Type::Int, NumberPolicy::Float).unwrap();
    assert_eq!(lab.datum(&value), big(text));
    assert_eq!(stringified(&mut lab, &value).unwrap(), text);
    assert_eq!(
        number("9007199254740993", Type::Int, NumberPolicy::Float),
        Ok(i(9_007_199_254_740_993u64))
    );
}

#[test]
fn k_eff_005_exponents_decode_under_each_stated_type() {
    let policy = NumberPolicy::BySpelling;
    assert_eq!(number("1.25e2", Type::Int, policy), Ok(i(125)));
    assert_eq!(
        number("18446744073709551617e2", Type::Int, policy),
        Ok(big("1844674407370955161700"))
    );
    assert_eq!(number("12000e-3", Type::Int, policy), Ok(i(12)));
    assert_eq!(
        number("-0.0e+9999999999999999999999", Type::Int, policy),
        Ok(i(0))
    );
    assert_eq!(number("1e3", Type::Float, policy), Ok(f(1000.0)));
    assert_eq!(number("1e3", Type::Number, policy), Ok(f(1000.0)));
    assert_eq!(number("-0", Type::Int, policy), Ok(i(0)));
    assert_eq!(number("-0", Type::Float, policy), Ok(f(-0.0)));
}

#[test]
fn k_eff_006_bare_numbers_follow_the_document_policy() {
    assert_eq!(
        number("9007199254740993", Type::Any, NumberPolicy::BySpelling),
        Ok(i(9_007_199_254_740_993u64))
    );
    assert_eq!(
        number("9007199254740993", Type::Any, NumberPolicy::Float),
        Ok(f(9_007_199_254_740_992.0))
    );
    assert_eq!(
        number("1.0", Type::Number, NumberPolicy::BySpelling),
        Ok(f(1.0))
    );
    let halfway = "1.00000000000000011102230246251565404236316680908203125";
    assert_eq!(
        number(halfway, Type::Float, NumberPolicy::Float),
        Ok(f(1.0))
    );
}

#[test]
fn k_eff_007_fractional_int_overflow_wrong_kind_and_union() {
    for token in ["1.1", "1e-3", "1e-999999999999999999999"] {
        assert_eq!(
            number(token, Type::Int, NumberPolicy::BySpelling),
            Err("effect_result".into())
        );
    }
    assert_eq!(
        number("1e999", Type::Float, NumberPolicy::Float),
        Err("effect_result".into())
    );
    assert_eq!(
        number("1", Type::Bool, NumberPolicy::BySpelling),
        Err("effect_result".into())
    );
    let union = Type::Union(vec![Type::Int, Type::Float]);
    assert_eq!(
        number("1.5", union.clone(), NumberPolicy::BySpelling),
        Ok(f(1.5))
    );
    assert_eq!(number("1.0", union, NumberPolicy::Float), Ok(i(1)));
}

#[test]
fn k_ljson_001_json_syntax_and_surrogate_pairs_are_strict() {
    let mut lab = Lab::new();
    let pair = parsed(&mut lab, r#""😀""#, &Type::Text, NumberPolicy::BySpelling).unwrap();
    assert_eq!(lab.datum(&pair), t("😀"));
    for text in [
        r#""\ud800""#,
        r#""\udc00""#,
        "[1,]",
        "01",
        "+1",
        "NaN",
        "true false",
        "{\"x\":}",
        "\u{a0}null",
        "1e",
        "1.",
    ] {
        assert_eq!(
            parsed(&mut lab, text, &Type::Any, NumberPolicy::BySpelling).unwrap_err(),
            "json_syntax",
            "{text}"
        );
    }
    let too_deep = format!("{}null{}", "[".repeat(65), "]".repeat(65));
    assert_eq!(
        parsed(&mut lab, &too_deep, &Type::Any, NumberPolicy::BySpelling).unwrap_err(),
        "json_depth"
    );
}

#[test]
fn k_eff_003_json_collections_are_fresh_and_ordered() {
    use lash_kernel_doc::NativeHeap as _;
    use lash_kernel_doc::heap::FieldName;
    let mut lab = Lab::new();
    let json = r#"{"z":1,"a":[2],"z":3}"#;
    let a = parsed(&mut lab, json, &Type::Any, NumberPolicy::BySpelling).unwrap();
    let z = parsed(&mut lab, json, &Type::Any, NumberPolicy::BySpelling).unwrap();
    let (Value::Record(a), Value::Record(z)) = (a, z) else {
        panic!("JSON objects are records")
    };
    assert_ne!(a, z);
    assert_eq!(
        stringified(&mut lab, &Value::Record(a)).unwrap(),
        r#"{"z":3,"a":[2]}"#
    );
    let inner = |id| match lab.heap.record_get(id, FieldName::Static("a")).unwrap() {
        Some(Value::List(list)) => list,
        other => panic!("a fresh list: {other:?}"),
    };
    assert_ne!(inner(a), inner(z));
}

#[test]
fn k_eff_007_json_structured_types_are_checked_before_allocation() {
    let mut lab = Lab::new();
    let ty = Type::Record(RecordType {
        fields: vec![RecordTypeField {
            name: "x".into(),
            ty: Type::List(Box::new(Type::Int)),
            optional: false,
        }],
        rest: None,
    });
    for json in ["{}", r#"{"x":[1.5]}"#, r#"{"x":[1],"extra":0}"#] {
        assert_eq!(
            parsed(&mut lab, json, &ty, NumberPolicy::Float).unwrap_err(),
            "effect_result"
        );
        assert_eq!(lab.allocated, 0);
    }
    let value = parsed(&mut lab, r#"{"x":[1.0,1e3]}"#, &ty, NumberPolicy::Float).unwrap();
    assert_eq!(stringified(&mut lab, &value).unwrap(), r#"{"x":[1,1000]}"#);
    let value = parsed(
        &mut lab,
        "[1,\"x\"]",
        &Type::Tuple(vec![Type::Int, Type::Text]),
        NumberPolicy::Float,
    )
    .unwrap();
    assert_eq!(lab.datum(&value), Datum::Tuple(vec![i(1), t("x")]));
    let value = parsed(
        &mut lab,
        "{\"x\":1}",
        &Type::Map(Box::new(MapType {
            key: Type::Text,
            value: Type::Int,
        })),
        NumberPolicy::Float,
    )
    .unwrap();
    assert!(matches!(value, Value::Map(_)));
}

#[test]
fn k_ljson_004_stringify_refuses_absent_nonfinite_keys_and_handles() {
    let mut lab = Lab::new();
    for datum in [
        Datum::Absent,
        Datum::Handle(Handle {
            kind: "host".into(),
            id: "x".into(),
        }),
        super::lab::bytes(&[1]),
    ] {
        let value = lab.value(datum.clone());
        assert_eq!(stringified(&mut lab, &value).unwrap_err(), "not_data");
        let nested = lab.value(list(vec![datum]));
        assert_eq!(stringified(&mut lab, &nested).unwrap_err(), "not_data");
    }
    for number in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
        let value = lab.value(f(number));
        assert_eq!(stringified(&mut lab, &value).unwrap_err(), "json_number");
    }
    let map = lab.value(Datum::Map(vec![(i(1), t("x"))]));
    assert_eq!(stringified(&mut lab, &map).unwrap_err(), "json_key");
    let zero = lab.value(f(-0.0));
    assert_eq!(stringified(&mut lab, &zero).unwrap(), "-0.0");
}

#[test]
fn k_ljson_005_cycles_are_refused_but_sharing_is_copied() {
    use lash_kernel_state::paged::{ObjectInit, SemanticHeap as _};
    let mut lab = Lab::new();
    let child = lab.value(list(vec![t("x")]));
    let shared = lab.list_of(vec![child.clone(), child]);
    assert_eq!(stringified(&mut lab, &shared).unwrap(), "[[\"x\"],[\"x\"]]");
    let cycle = lab.heap.allocate_object(ObjectInit::List(vec![])).unwrap();
    lab.heap.list_write(cycle, 0, Value::List(cycle)).unwrap();
    assert_eq!(
        stringified(&mut lab, &Value::List(cycle)).unwrap_err(),
        "cycle"
    );
}

/// K-LJSON-008 and K-CHG-003: direct decimal rendering obeys explicit
/// layout policy, reserves its buffer, and bounds conversions at every
/// occurrence of a shared subtree by its output size.
#[test]
fn k_ljson_008_direct_render_bounds_and_preserves_decimal_policy() {
    let mut lab = Lab::new();
    let kinds = lab.value(Datum::Set(vec![]));
    let layout = lab.value(Datum::Tuple(vec![i(-5), i(21), b(true), b(false)]));
    let numbers = [
        -0.0,
        0.0,
        1.25,
        -1.25,
        1e-6,
        1e-7,
        1e20,
        1e21,
        5e-324,
        f64::MAX,
        1000000000000000100.0,
    ];
    let child = lab.value(list(numbers.into_iter().map(f).collect()));
    let expected = "[0,0,1.25,-1.25,0.000001,1e-7,100000000000000000000,1e+21,5e-324,1.7976931348623157e+308,1000000000000000100]";
    let shared = lab.list_of(vec![child.clone(), child.clone()]);
    let args = [shared, kinds.clone(), Value::Absent, layout.clone()];
    lab.room(None);
    let result = lab.call("json.render", &args).unwrap();
    let text = format!("[{expected},{expected}]");
    assert_eq!(lab.datum(&result), t(&text));
    // No fragment slots or per-number texts: only the output and its buffer.
    assert_eq!(lab.reserved, 2 * text.len() as u64);
    lab.room(Some(lab.reserved - 1));
    assert!(matches!(
        lab.call("json.render", &args),
        Err(NativeError::Memory)
    ));
    lab.room(None);
    let render = |lab: &mut Lab, value: Datum, kinds: &Value, field: Datum, layout: Datum| {
        let (value, field, layout) = (lab.value(value), lab.value(field), lab.value(layout));
        lab.call("json.render", &[value, kinds.clone(), field, layout])
            .map(|result| lab.datum(&result))
    };
    let layout_datum = Datum::Tuple(vec![i(-5), i(21), b(true), b(false)]);
    let signed = Datum::Tuple(vec![i(1), i(1), b(false), b(true)]);
    assert_eq!(
        render(
            &mut lab,
            list(vec![f(-0.0), f(100.0), f(0.01)]),
            &kinds,
            Datum::Absent,
            signed
        )
        .unwrap(),
        t("[-0,1e2,1e-2]")
    );
    assert_eq!(
        render(
            &mut lab,
            big("123456789012345678901230000"),
            &kinds,
            Datum::Absent,
            layout_datum.clone()
        )
        .unwrap(),
        t("1.2345678901234567890123e+26")
    );
    let verbatim = lab.value(Datum::Set(vec![t("list")]));
    let plain = stringified(&mut lab, &child).unwrap();
    let rendered = lab
        .call(
            "json.render",
            &[child.clone(), verbatim, Value::Absent, layout.clone()],
        )
        .unwrap();
    assert_eq!(lab.datum(&rendered), t(&plain));
    let opaque = Datum::Record(vec![("mark".into(), t("opaque")), ("n".into(), f(2.0))]);
    assert_eq!(
        render(&mut lab, opaque, &kinds, t("mark"), layout_datum.clone()).unwrap(),
        t(r#"{"mark":"opaque","n":2.0}"#)
    );
    for bad in [Datum::Absent, f(f64::NAN)] {
        let value = lab.value(bad.clone());
        let expected = stringified(&mut lab, &value).unwrap_err();
        let result = render(&mut lab, bad, &kinds, Datum::Absent, layout_datum.clone());
        assert_eq!(lab.kind(result), expected);
    }
    let escaped = "\"\\\n\r\t\u{8}\u{c}\u{0}\u{1f}é😀";
    assert_eq!(
        render(
            &mut lab,
            t(escaped),
            &kinds,
            Datum::Absent,
            layout_datum.clone()
        )
        .unwrap(),
        t(&serde_json::to_string(escaped).unwrap())
    );
    let reversed = Datum::Tuple(vec![i(2), i(1), b(true), b(false)]);
    let result = render(&mut lab, i(1), &kinds, Datum::Absent, reversed);
    assert_eq!(lab.kind(result), "number_range");
    let result_size = 1 + text.len() as u64;
    let charge = charge_of("json.render", &mut |operand, _| match operand {
        Operand::Result => result_size,
        Operand::Param(_) => 1,
    });
    assert_eq!(charge, 5 + result_size);
    // Every numeric occurrence, including sharing, contributes output
    // bytes, independently of deduplicated input deep size. The fused
    // writer follows the strict stringify native's size-based work bound.
    assert!(result_size >= (numbers.len() * 2) as u64);
}

#[test]
fn k_ljson_006_native_parse_applies_explicit_number_kind_recursively() {
    let mut lab = Lab::new();
    let args = lab.values(&[t("[1e3,18446744073709551617]"), t("int"), t("float")]);
    let value = lab.call("json.parse", &args).unwrap();
    let text = lab.call("json.stringify", &[value]).unwrap();
    assert_eq!(lab.datum(&text), t("[1000,18446744073709551617]"));
    assert_eq!(
        run("json.parse", &[t("[1,1e3]"), t("number"), t("by_spelling")]),
        list(vec![i(1), f(1000.0)])
    );
}

#[test]
fn k_lfmt_001_precision_is_explicit_and_preserves_exact_integers() {
    assert_eq!(
        run("format.fixed", &[big("18446744073709551617"), i(2)]),
        t("18446744073709551617.00")
    );
    assert_eq!(run("format.fixed", &[f(2.5), i(0)]), t("2"));
    assert_eq!(run("format.fixed", &[f(-0.0), i(2)]), t("-0.00"));
    assert_eq!(run("format.scientific", &[f(125.0), i(2)]), t("1.25e2"));
    assert_eq!(raised("format.fixed", &[f(f64::NAN), i(2)]), "number_range");
}

/// K-LFMT-003: special values retain their kind/sign; finite extremes
/// retain the existing shortest digits, with no layout policy applied.
#[test]
fn k_lfmt_003_decimal_parts_preserve_specials_and_binary64_edges() {
    for (number, kind, negative, digits, exponent) in [
        (0.0, "finite", false, "0", 0),
        (-0.0, "finite", true, "0", 0),
        (f64::NAN, "nan", false, "", 0),
        (f64::INFINITY, "infinity", false, "", 0),
        (f64::NEG_INFINITY, "infinity", true, "", 0),
        (f64::from_bits(1), "finite", false, "5", -324),
        (
            f64::from_bits((1 << 52) - 1),
            "finite",
            false,
            "2225073858507201",
            -323,
        ),
        (
            f64::MIN_POSITIVE,
            "finite",
            false,
            "22250738585072014",
            -324,
        ),
        (f64::MAX, "finite", false, "17976931348623157", 292),
        (0.1 + 0.2, "finite", false, "30000000000000004", -17),
        (-0.125, "finite", true, "125", -3),
        (1e20, "finite", false, "1", 20),
    ] {
        assert_eq!(
            run("format.decimal_parts", &[f(number)]),
            Datum::Tuple(vec![t(kind), b(negative), t(digits), i(exponent)]),
        );
    }
    assert_eq!(raised("format.decimal_parts", &[i(1)]), "type_error");
    assert_eq!(raised("format.decimal_parts", &[]), "arity");
}

/// K-LFMT-003, K-CHG-005 and K-BND-001: shortest digits round trip;
/// neither nearest shorter significand does. Conversion uses bounded
/// scratch, copying is charged by result size, and output is reserved.
#[test]
fn k_lfmt_003_shortest_digits_are_bounded_charged_and_reserved() {
    let mut bits = 1_u64;
    for _ in 0..4096 {
        bits = bits
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        let number = f64::from_bits(bits);
        if !number.is_finite() || number == 0.0 {
            continue;
        }
        let mut lab = Lab::new();
        let value = lab.run("format.decimal_parts", &[f(number)]);
        let Datum::Tuple(parts) = &value else {
            panic!("decimal tuple");
        };
        let (Datum::Text(digits), Datum::Int(exponent)) = (&parts[2], &parts[3]) else {
            panic!("decimal digits and exponent");
        };
        let exponent: i32 = exponent.to_string().parse().unwrap();
        assert!((1..=17).contains(&digits.len()));
        assert!(!digits.starts_with('0'));
        assert!(!digits.ends_with('0'));
        assert_eq!(
            format!("{digits}e{exponent}")
                .parse::<f64>()
                .unwrap()
                .to_bits(),
            number.abs().to_bits()
        );
        assert_eq!(parts[1], b(number.is_sign_negative()));
        if digits.len() > 1 {
            let shorter: u64 = digits[..digits.len() - 1].parse().unwrap();
            for candidate in [shorter, shorter + 1] {
                assert_ne!(
                    format!("{candidate}e{}", exponent + 1)
                        .parse::<f64>()
                        .unwrap()
                        .to_bits(),
                    number.abs().to_bits()
                );
            }
        }
        let result_size = deep(&value);
        let charged = charge_of("format.decimal_parts", &mut |operand, _| match operand {
            Operand::Param(_) => 1,
            Operand::Result => result_size,
        });
        assert_eq!(charged, 66 + result_size);
        // At most 17 mantissa digits, three exponent digits and separators
        // are consumed; trimming and copying visit at most 17 digits each.
        assert!(charged >= 24 + 2 * digits.len() as u64);
        let held = 4 * VALUE + 6 + digits.len() as u64 + 2;
        assert_eq!(lab.reserved, held);
        let mut too_small = Lab::new();
        too_small.room(Some(held - 1));
        let args = too_small.values(&[f(number)]);
        assert!(matches!(
            too_small.call("format.decimal_parts", &args),
            Err(NativeError::Memory)
        ));
        assert_eq!(too_small.allocated, 0);
    }
}

#[test]
fn k_lfmt_002_radix_and_padding_are_language_neutral() {
    assert_eq!(run("format.radix", &[i(-255), i(16)]), t("-ff"));
    assert_eq!(run("format.radix", &[i(35), i(36)]), t("z"));
    for radix in [1, 37] {
        assert_eq!(raised("format.radix", &[i(1), i(radix)]), "number_range");
    }
    assert_eq!(
        run("format.pad", &[t("😀"), i(3), t("0"), t("start")]),
        t("00😀")
    );
    assert_eq!(
        raised("format.pad", &[t("x"), i(3), t("0"), t("center")]),
        "type_error"
    );
}

/// What a result holds, as a native function reserves it: a text by its
/// bytes, an integer by its magnitude, a list by its values.
fn held(lab: &Lab, value: &Value) -> u64 {
    match lab.datum(value) {
        Datum::Text(text) => text.len() as u64,
        Datum::Int(integer) => integer.bits().div_ceil(8),
        Datum::List(items) => items.len() as u64 * VALUE,
        other => panic!("no amplifier returns {other:?}"),
    }
}

/// `K-BND-001`, `K-LIB-007`: a function whose result follows a count, a
/// product of two sizes or shared structure reserves the result before it
/// builds it. A room the result does not fit refuses the call at its
/// reservation, with no object allocated; a room it fits is asked for at
/// least what the result holds.
#[test]
fn an_amplifier_reserves_its_result_before_it_builds_it() {
    const ROOM: u64 = 16 << 10;
    type Args = fn(&mut Lab) -> Vec<Value>;
    let cases: &[(&str, Args)] = &[
        ("text.repeat", |lab| lab.values(&[t("ab"), i(40_000)])),
        ("text.replace", |lab| {
            lab.values(&[t(&"x".repeat(300)), t("x"), t(&"y".repeat(300))])
        }),
        ("text.join", |lab| {
            lab.values(&[list(vec![t(""); 300]), t(&"-".repeat(300))])
        }),
        ("text.split", |lab| {
            lab.values(&[t(&"x".repeat(2_000)), t("")])
        }),
        ("text.to_code_points", |lab| {
            lab.values(&[t(&"x".repeat(2_000))])
        }),
        ("text.to_utf16_units", |lab| {
            lab.values(&[t(&"x".repeat(2_000))])
        }),
        ("text.pad_start", |lab| {
            lab.values(&[t(""), i(40_000), t("ab")])
        }),
        ("text.pad_end", |lab| {
            lab.values(&[t(""), i(40_000), t("ab")])
        }),
        ("format.pad", |lab| {
            lab.values(&[t(""), i(40_000), t("ab"), t("end")])
        }),
        ("format.fixed", |lab| lab.values(&[i(1), i(40_000)])),
        ("format.fixed", |lab| lab.values(&[f(1.5), i(40_000)])),
        ("format.scientific", |lab| lab.values(&[f(1.5), i(40_000)])),
        ("json.parse", |lab| {
            lab.values(&[t("1e40000"), t("int"), t("by_spelling")])
        }),
        ("json.parse", |lab| {
            let text = format!("[{}0]", "0,".repeat(1_999));
            lab.values(&[t(&text), t("int"), t("by_spelling")])
        }),
        ("format.radix", |lab| {
            lab.values(&[i(num_bigint::BigInt::from(1) << 16_000), i(2)])
        }),
        ("json.stringify", |lab| {
            let mut shared = lab.value(list(vec![t("xxxxxxxx")]));
            for _ in 0..12 {
                shared = lab.list_of(vec![shared.clone(), shared]);
            }
            vec![shared]
        }),
    ];
    // Each function that builds a result its room refuses, or allocates
    // an object before its reservation is refused.
    let mut unreserved = Vec::new();
    for (name, args) in cases {
        let mut lab = Lab::new();
        let args = args(&mut lab);
        let objects = lab.allocated;
        lab.room(Some(ROOM));
        let refused = matches!(lab.call(name, &args), Err(NativeError::Memory));
        if !refused || lab.allocated != objects {
            unreserved.push(*name);
            continue;
        }
        lab.room(None);
        let value = lab.call(name, &args).unwrap();
        assert!(
            lab.reserved >= held(&lab, &value),
            "{name} reserved {} for a result of {}",
            lab.reserved,
            held(&lab, &value)
        );
    }
    assert_eq!(unreserved, [""; 0]);
}

/// K-LJSON-007: classification charges only the nodes and key comparisons
/// it reaches, and allocates nothing even when the remaining room is zero.
#[test]
fn classification_prices_the_actual_walk_and_stops_at_the_first_refusal() {
    let mut lab = Lab::new();
    let plain = lab.value(list(vec![Datum::Null, b(true), t("large leaf")]));
    let absent = lab.value(Datum::Absent);
    let refused = lab.list_of(vec![absent, plain.clone()]);
    let oversized = lab.value(list(vec![Datum::Null; 128]));
    let enormous = lab.value(list(vec![Datum::Null; 100_000]));
    let enormous_record = lab.value(Datum::Record(
        (0..100_000)
            .map(|n| (format!("k{n}"), Datum::Null))
            .collect(),
    ));
    let field = lab.value(Datum::Record(vec![("a".into(), Datum::Null)]));
    let hook = lab.value(Datum::Record(vec![("toJSON".into(), Datum::Null)]));
    let policy = lab.values(&[
        i(128),
        i(8),
        Datum::Tuple(vec![t("toJSON"), t("brand")]),
        t("0"),
        t(":"),
    ]);
    let objects = lab.allocated;
    lab.room(Some(0));
    for (value, encoded) in [
        (Value::Null, 3),
        (plain, 9),
        (refused, 4),
        (oversized, 2),
        (enormous, 2),
        (enormous_record, 2),
        (field, 21),
        (hook, 16),
    ] {
        let mut args = vec![value];
        args.extend(policy.iter().cloned());
        let result = lab.call("json.classify", &args).unwrap();
        assert_eq!(lab.datum(&result), i(encoded));
        let charge = charge_of("json.classify", &mut |operand, measure| {
            assert_eq!(*operand, Operand::Result);
            assert_eq!(measure, Measure::Magnitude);
            encoded as u64
        });
        assert_eq!(charge, 1 + encoded as u64);
        assert_eq!(lab.allocated, objects);
        assert_eq!(lab.reserved, 0);
    }
    // Field names and key order are supplied policy, not kernel constants.
    assert_eq!(
        run(
            "json.classify",
            &[
                Datum::Record(vec![("brand".into(), Datum::Null)]),
                i(128),
                i(8),
                Datum::Tuple(vec![t("deny"), t("ban")]),
                t("z"),
                t("zz"),
            ]
        ),
        i(27)
    );
}
