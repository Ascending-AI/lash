use std::collections::BTreeMap;
use std::ops::ControlFlow;

use lash_kernel_doc::{
    Bytes, Element, Float, FunctionRegistry, Handle, MapType, NativeCall, NativeError, NativeHeap,
    NumberPolicy, NumberToken, Object, ObjectId, RecordType, RecordTypeField, Type, Value,
    WorkCounter,
};

use super::{decode_number, int, parse_json, register_text_json, stringify_json};
use crate::tests::Room;

#[derive(Default)]
struct Heap(BTreeMap<ObjectId, Object>, Room);

impl NativeHeap for Heap {
    fn len(&self, object: ObjectId) -> usize {
        match self.0.get(&object) {
            Some(Object::List(items) | Object::Set(items)) => items.len(),
            Some(Object::Map(items)) => items.len(),
            Some(Object::Record(items)) => items.len(),
            _ => 0,
        }
    }
    fn list_get(&self, object: ObjectId, index: usize) -> Option<Value> {
        match self.0.get(&object) {
            Some(Object::List(items)) => items.get(index).cloned(),
            _ => None,
        }
    }
    fn map_get(&self, object: ObjectId, key: &Value) -> Option<Value> {
        match self.0.get(&object) {
            Some(Object::Map(items)) => {
                items.iter().find(|(k, _)| k == key).map(|(_, v)| v.clone())
            }
            _ => None,
        }
    }
    fn set_contains(&self, object: ObjectId, member: &Value) -> bool {
        matches!(self.0.get(&object), Some(Object::Set(items)) if items.contains(member))
    }
    fn record_get(&self, object: ObjectId, field: &str) -> Option<Value> {
        match self.0.get(&object) {
            Some(Object::Record(items)) => items
                .iter()
                .find(|(k, _)| k == field)
                .map(|(_, v)| v.clone()),
            _ => None,
        }
    }
    fn visit(&self, object: ObjectId, visitor: &mut dyn FnMut(Element<'_>) -> ControlFlow<()>) {
        if let Some(object) = self.0.get(&object) {
            let elements: Vec<_> = match object {
                Object::List(items) | Object::Set(items) => {
                    items.iter().map(Element::Item).collect()
                }
                Object::Map(items) => items
                    .iter()
                    .map(|(key, value)| Element::Entry { key, value })
                    .collect(),
                Object::Record(items) => items
                    .iter()
                    .map(|(name, value)| Element::Field { name, value })
                    .collect(),
                _ => vec![],
            };
            for element in elements {
                if visitor(element).is_break() {
                    break;
                }
            }
        }
    }
    fn allocate(&mut self, object: Object) -> Result<ObjectId, NativeError> {
        let id = ObjectId(u64::try_from(self.0.len()).unwrap());
        self.0.insert(id, object);
        Ok(id)
    }
    fn reserve(&mut self, values: u64, bytes: u64) -> Result<(), NativeError> {
        self.1.reserve(values, bytes)
    }
}

fn call(heap: &mut Heap, name: &str, args: &[Value]) -> Result<Value, NativeError> {
    let mut registry = FunctionRegistry::new();
    register_text_json(&mut registry).unwrap();
    let (_, registered) = registry
        .iter()
        .find(|(_, function)| function.definition.name.as_str() == name)
        .unwrap();
    registered.native.as_ref().unwrap().call(NativeCall {
        args,
        heap,
        counter: &mut WorkCounter::new(None),
    })
}

fn unary(name: &str, value: Value) -> Value {
    call(&mut Heap::default(), name, &[value]).unwrap()
}
fn s(text: &str) -> Value {
    Value::text(text)
}
fn list(heap: &mut Heap, items: Vec<Value>) -> Value {
    Value::List(heap.allocate(Object::List(items)).unwrap())
}
fn items(heap: &Heap, value: Value) -> Vec<Value> {
    let Value::List(id) = value else {
        panic!("expected list");
    };
    (0..heap.len(id))
        .map(|i| heap.list_get(id, i).unwrap())
        .collect()
}
fn error_kind<T: std::fmt::Debug>(result: Result<T, NativeError>) -> String {
    let NativeError::Raised(error) = result.unwrap_err() else {
        panic!("expected raised error");
    };
    error.kind
}
fn unicode(name: &str) -> String {
    let (a, b, c) = char::UNICODE_VERSION;
    format!("text.{name}_u{a}_{b}_{c}")
}
fn number(token: &str, ty: Type, policy: NumberPolicy) -> Result<Value, NativeError> {
    decode_number(
        &NumberToken::new(token).unwrap(),
        &ty,
        policy,
        &mut Heap::default(),
    )
}

#[test]
fn k_val_007_astral_length_and_index_have_distinct_addressing() {
    let text = s("a😀z");
    assert_eq!(unary("text.len", text.clone()), int(3));
    assert_eq!(unary("text.utf16_len", text.clone()), int(4));
    assert_eq!(
        call(&mut Heap::default(), "text.get", &[text.clone(), int(1)]).unwrap(),
        s("😀")
    );
    assert_eq!(
        call(
            &mut Heap::default(),
            "text.utf16_get",
            &[text.clone(), int(1)]
        )
        .unwrap(),
        int(0xd83d)
    );
    assert_eq!(
        call(
            &mut Heap::default(),
            "text.utf16_get",
            &[text.clone(), int(2)]
        )
        .unwrap(),
        int(0xde00)
    );
    assert_eq!(
        call(&mut Heap::default(), "text.get", &[text.clone(), int(-1)]).unwrap(),
        s("z")
    );
    assert_eq!(
        error_kind(call(&mut Heap::default(), "text.get", &[text, int(3)])),
        "index_out_of_range"
    );
}

#[test]
fn k_val_007_utf16_slice_refuses_split_pair_boundaries() {
    let mut heap = Heap::default();
    assert_eq!(
        call(&mut heap, "text.slice", &[s("a😀z"), int(1), int(2)]).unwrap(),
        s("😀")
    );
    assert_eq!(
        call(&mut heap, "text.utf16_slice", &[s("a😀z"), int(1), int(3)]).unwrap(),
        s("😀")
    );
    for (start, end) in [(1, 2), (2, 3), (2, 2), (2, 1)] {
        assert_eq!(
            error_kind(call(
                &mut heap,
                "text.utf16_slice",
                &[s("a😀z"), int(start), int(end)]
            )),
            "text_boundary"
        );
    }
    assert_eq!(
        call(&mut heap, "text.slice", &[s("abc"), int(-99), int(99)]).unwrap(),
        s("abc")
    );
    assert_eq!(
        call(&mut heap, "text.slice", &[s("abc"), int(2), int(1)]).unwrap(),
        s("")
    );
}

#[test]
fn k_ltxt_003_search_reports_the_selected_address_space() {
    let mut heap = Heap::default();
    assert_eq!(
        call(&mut heap, "text.find", &[s("😀z😀z"), s("z"), int(2)]).unwrap(),
        int(3)
    );
    assert_eq!(
        call(&mut heap, "text.utf16_find", &[s("😀z😀z"), s("z"), int(3)]).unwrap(),
        int(5)
    );
    assert_eq!(
        call(&mut heap, "text.utf16_find", &[s("😀z"), s("😀"), int(1)]).unwrap(),
        int(-1)
    );
    assert_eq!(
        call(&mut heap, "text.find", &[s("😀z"), s(""), int(99)]).unwrap(),
        int(2)
    );
    assert_eq!(
        call(&mut heap, "text.utf16_find", &[s("😀z"), s(""), int(99)]).unwrap(),
        int(3)
    );
}

#[test]
fn k_val_030_code_point_order_differs_from_unit_order() {
    let mut heap = Heap::default();
    assert_eq!(
        call(&mut heap, "text.compare", &[s("😀"), s("\u{e000}")]).unwrap(),
        int(1)
    );
    assert_eq!(
        call(&mut heap, "text.utf16_compare", &[s("😀"), s("\u{e000}")]).unwrap(),
        int(-1)
    );
    assert_eq!(
        call(&mut heap, "text.compare", &[s("a"), s("aa")]).unwrap(),
        int(-1)
    );
}

#[test]
fn k_ltxt_005_literal_split_join_replace_and_concat() {
    let mut heap = Heap::default();
    assert_eq!(
        call(&mut heap, "text.concat", &[s("a"), s("😀")]).unwrap(),
        s("a😀")
    );
    let result = call(&mut heap, "text.split", &[s("a..b.."), s("..")]).unwrap();
    assert_eq!(items(&heap, result.clone()), vec![s("a"), s("b"), s("")]);
    assert_eq!(
        call(&mut heap, "text.join", &[result, s("|")]).unwrap(),
        s("a|b|")
    );
    let result = call(&mut heap, "text.split", &[s("😀a"), s("")]).unwrap();
    assert_eq!(items(&heap, result), vec![s("😀"), s("a")]);
    let result = call(&mut heap, "text.split", &[s(""), s("")]).unwrap();
    assert!(items(&heap, result).is_empty());
    assert_eq!(
        call(&mut heap, "text.replace", &[s("aaa"), s("aa"), s("x")]).unwrap(),
        s("xa")
    );
    assert_eq!(
        call(&mut heap, "text.replace", &[s("😀a"), s(""), s("-")]).unwrap(),
        s("-😀-a-")
    );
}

#[test]
fn k_ltxt_006_case_and_whitespace_are_versioned_and_locale_free() {
    assert_eq!(unary(&unicode("lower"), s("İΟΣ")), s("i\u{307}ος"));
    assert_eq!(unary(&unicode("upper"), s("straße")), s("STRASSE"));
    assert_eq!(unary(&unicode("trim"), s("\u{2003} a \u{85}")), s("a"));
    assert_eq!(unary(&unicode("trim_start"), s("\u{2003}a ")), s("a "));
    assert_eq!(unary(&unicode("trim_end"), s(" a\u{85}")), s(" a"));
    assert_eq!(
        unary(&unicode("trim"), s("\u{feff}a\u{feff}")),
        s("\u{feff}a\u{feff}")
    );
    println!("Unicode data: {:?}", char::UNICODE_VERSION);
}

#[test]
fn k_ltxt_007_repeat_and_padding_count_scalars() {
    let mut heap = Heap::default();
    assert_eq!(
        call(&mut heap, "text.repeat", &[s("😀a"), int(2)]).unwrap(),
        s("😀a😀a")
    );
    assert_eq!(
        call(&mut heap, "text.repeat", &[s("a"), int(0)]).unwrap(),
        s("")
    );
    assert_eq!(
        error_kind(call(&mut heap, "text.repeat", &[s("a"), int(-1)])),
        "number_range"
    );
    assert_eq!(
        call(&mut heap, "text.pad_start", &[s("😀"), int(4), s("ab")]).unwrap(),
        s("aba😀")
    );
    assert_eq!(
        call(&mut heap, "text.pad_end", &[s("😀"), int(3), s("xy")]).unwrap(),
        s("😀xy")
    );
    assert_eq!(
        call(&mut heap, "text.pad_start", &[s("abc"), int(2), s("x")]).unwrap(),
        s("abc")
    );
    assert_eq!(
        call(&mut heap, "text.pad_end", &[s("a"), int(9), s("")]).unwrap(),
        s("a")
    );
}

#[test]
fn k_ltxt_008_prefix_suffix_and_empty_patterns() {
    let mut heap = Heap::default();
    assert_eq!(
        call(&mut heap, "text.starts_with", &[s("😀a"), s("😀")]).unwrap(),
        Value::Bool(true)
    );
    assert_eq!(
        call(&mut heap, "text.ends_with", &[s("😀a"), s("a")]).unwrap(),
        Value::Bool(true)
    );
    assert_eq!(
        call(&mut heap, "text.ends_with", &[s("a"), s("")]).unwrap(),
        Value::Bool(true)
    );
    assert_eq!(
        call(&mut heap, "text.starts_with", &[s("a"), s("b")]).unwrap(),
        Value::Bool(false)
    );
}

#[test]
fn k_val_006_scalar_and_utf16_conversion_refuse_lone_surrogates() {
    let mut heap = Heap::default();
    for (to, from, expected) in [
        (
            "text.to_code_points",
            "text.from_code_points",
            vec![int(0x1f600), int(97)],
        ),
        (
            "text.to_utf16_units",
            "text.from_utf16_units",
            vec![int(0xd83d), int(0xde00), int(97)],
        ),
    ] {
        let result = call(&mut heap, to, &[s("😀a")]).unwrap();
        assert_eq!(items(&heap, result.clone()), expected);
        assert_eq!(call(&mut heap, from, &[result]).unwrap(), s("😀a"));
    }
    let bad = list(&mut heap, vec![int(0xd800)]);
    assert_eq!(
        error_kind(call(
            &mut heap,
            "text.from_code_points",
            std::slice::from_ref(&bad)
        )),
        "invalid_scalar"
    );
    assert_eq!(
        error_kind(call(&mut heap, "text.from_utf16_units", &[bad])),
        "invalid_utf16"
    );
    let bad = list(&mut heap, vec![int(0x110000)]);
    assert_eq!(
        error_kind(call(&mut heap, "text.from_code_points", &[bad])),
        "invalid_scalar"
    );
}

#[test]
fn k_val_008_octets_slice_concat_and_compare() {
    let mut heap = Heap::default();
    let values = list(&mut heap, vec![int(0), int(128), int(255)]);
    let bytes = call(&mut heap, "bytes.from_octets", &[values]).unwrap();
    assert_eq!(bytes, Value::Bytes(Bytes::new(vec![0, 128, 255])));
    assert_eq!(
        call(&mut heap, "bytes.slice", &[bytes.clone(), int(-2), int(99)]).unwrap(),
        Value::Bytes(Bytes::new(vec![128, 255]))
    );
    assert_eq!(
        call(
            &mut heap,
            "bytes.concat",
            &[bytes.clone(), Value::Bytes(Bytes::new(vec![1]))]
        )
        .unwrap(),
        Value::Bytes(Bytes::new(vec![0, 128, 255, 1]))
    );
    assert_eq!(
        call(
            &mut heap,
            "bytes.compare",
            &[
                bytes.clone(),
                Value::Bytes(Bytes::new(vec![0, 128, 255, 1]))
            ]
        )
        .unwrap(),
        int(-1)
    );
    let bad = list(&mut heap, vec![int(256)]);
    assert_eq!(
        error_kind(call(&mut heap, "bytes.from_octets", &[bad])),
        "number_range"
    );
}

#[test]
fn k_lbytes_002_utf8_is_strict_without_replacement() {
    let encoded = unary("bytes.utf8_encode", s("😀\0"));
    assert_eq!(
        encoded,
        Value::Bytes(Bytes::new(vec![0xf0, 0x9f, 0x98, 0x80, 0]))
    );
    assert_eq!(unary("bytes.utf8_decode", encoded), s("😀\0"));
    for bytes in [
        vec![0xc0, 0x80],
        vec![0xed, 0xa0, 0x80],
        vec![0xf0, 0x9f],
        vec![0xff],
    ] {
        assert_eq!(
            error_kind(call(
                &mut Heap::default(),
                "bytes.utf8_decode",
                &[Value::Bytes(Bytes::new(bytes))]
            )),
            "invalid_utf8"
        );
    }
}

#[test]
fn k_eff_005_json_integer_past_u64_round_trips_without_float() {
    let mut heap = Heap::default();
    let text = "18446744073709551617000000000000000000001";
    let value = parse_json(text, &Type::Int, NumberPolicy::Float, &mut heap).unwrap();
    assert_eq!(value, int(text.parse::<num_bigint::BigInt>().unwrap()));
    assert_eq!(stringify_json(&value, &mut heap).unwrap(), text);
    assert_eq!(
        number("9007199254740993", Type::Int, NumberPolicy::Float).unwrap(),
        int(9_007_199_254_740_993u64)
    );
}

#[test]
fn k_eff_005_exponents_decode_under_each_stated_type() {
    let policy = NumberPolicy::BySpelling;
    assert_eq!(number("1.25e2", Type::Int, policy).unwrap(), int(125));
    assert_eq!(
        number("18446744073709551617e2", Type::Int, policy).unwrap(),
        int("1844674407370955161700"
            .parse::<num_bigint::BigInt>()
            .unwrap())
    );
    assert_eq!(number("12000e-3", Type::Int, policy).unwrap(), int(12));
    assert_eq!(
        number("-0.0e+9999999999999999999999", Type::Int, policy).unwrap(),
        int(0)
    );
    assert_eq!(
        number("1e3", Type::Float, policy).unwrap(),
        Value::Float(Float::new(1000.0))
    );
    assert_eq!(
        number("1e3", Type::Number, policy).unwrap(),
        Value::Float(Float::new(1000.0))
    );
    assert_eq!(number("-0", Type::Int, policy).unwrap(), int(0));
    assert_eq!(
        number("-0", Type::Float, policy).unwrap(),
        Value::Float(Float::new(-0.0))
    );
}

#[test]
fn k_eff_006_bare_numbers_follow_the_document_policy() {
    assert_eq!(
        number("9007199254740993", Type::Any, NumberPolicy::BySpelling).unwrap(),
        int(9_007_199_254_740_993u64)
    );
    assert_eq!(
        number("9007199254740993", Type::Any, NumberPolicy::Float).unwrap(),
        Value::Float(Float::new(9_007_199_254_740_992.0))
    );
    assert_eq!(
        number("1.0", Type::Number, NumberPolicy::BySpelling).unwrap(),
        Value::Float(Float::new(1.0))
    );
    let halfway = "1.00000000000000011102230246251565404236316680908203125";
    assert_eq!(
        number(halfway, Type::Float, NumberPolicy::Float).unwrap(),
        Value::Float(Float::new(1.0))
    );
}

#[test]
fn k_eff_007_fractional_int_overflow_wrong_kind_and_union() {
    for token in ["1.1", "1e-3", "1e-999999999999999999999"] {
        assert_eq!(
            error_kind(number(token, Type::Int, NumberPolicy::BySpelling)),
            "effect_result"
        );
    }
    assert_eq!(
        error_kind(number("1e999", Type::Float, NumberPolicy::Float)),
        "effect_result"
    );
    assert_eq!(
        error_kind(number("1", Type::Bool, NumberPolicy::BySpelling)),
        "effect_result"
    );
    assert_eq!(
        number(
            "1.5",
            Type::Union(vec![Type::Int, Type::Float]),
            NumberPolicy::BySpelling
        )
        .unwrap(),
        Value::Float(Float::new(1.5))
    );
    assert_eq!(
        number(
            "1.0",
            Type::Union(vec![Type::Int, Type::Float]),
            NumberPolicy::Float
        )
        .unwrap(),
        int(1)
    );
}

#[test]
fn k_ljson_001_json_syntax_and_surrogate_pairs_are_strict() {
    let mut heap = Heap::default();
    assert_eq!(
        parse_json(
            r#""\ud83d\ude00""#,
            &Type::Text,
            NumberPolicy::BySpelling,
            &mut heap
        )
        .unwrap(),
        s("😀")
    );
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
            error_kind(parse_json(
                text,
                &Type::Any,
                NumberPolicy::BySpelling,
                &mut heap
            )),
            "json_syntax",
            "{text}"
        );
    }
    let too_deep = format!("{}null{}", "[".repeat(65), "]".repeat(65));
    assert_eq!(
        error_kind(parse_json(
            &too_deep,
            &Type::Any,
            NumberPolicy::BySpelling,
            &mut heap
        )),
        "json_depth"
    );
}

#[test]
fn k_eff_003_json_collections_are_fresh_and_ordered() {
    let mut heap = Heap::default();
    let json = r#"{"z":1,"a":[2],"z":3}"#;
    let a = parse_json(json, &Type::Any, NumberPolicy::BySpelling, &mut heap).unwrap();
    let b = parse_json(json, &Type::Any, NumberPolicy::BySpelling, &mut heap).unwrap();
    assert_ne!(a.object(), b.object());
    assert_eq!(stringify_json(&a, &mut heap).unwrap(), r#"{"z":3,"a":[2]}"#);
    assert_ne!(
        heap.record_get(a.object().unwrap(), "a").unwrap().object(),
        heap.record_get(b.object().unwrap(), "a").unwrap().object()
    );
}

#[test]
fn k_eff_007_json_structured_types_are_checked_before_allocation() {
    let mut heap = Heap::default();
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
            error_kind(parse_json(json, &ty, NumberPolicy::Float, &mut heap)),
            "effect_result"
        );
        assert!(heap.0.is_empty());
    }
    let value = parse_json(r#"{"x":[1.0,1e3]}"#, &ty, NumberPolicy::Float, &mut heap).unwrap();
    assert_eq!(
        stringify_json(&value, &mut heap).unwrap(),
        r#"{"x":[1,1000]}"#
    );
    let value = parse_json(
        "[1,\"x\"]",
        &Type::Tuple(vec![Type::Int, Type::Text]),
        NumberPolicy::Float,
        &mut heap,
    )
    .unwrap();
    assert_eq!(value, Value::Tuple(vec![int(1), s("x")].into()));
    let value = parse_json(
        "{\"x\":1}",
        &Type::Map(Box::new(MapType {
            key: Type::Text,
            value: Type::Int,
        })),
        NumberPolicy::Float,
        &mut heap,
    )
    .unwrap();
    assert!(matches!(value, Value::Map(_)));
}

#[test]
fn k_ljson_004_stringify_refuses_absent_nonfinite_keys_and_handles() {
    let mut heap = Heap::default();
    for value in [
        Value::Absent,
        Value::Handle(std::sync::Arc::new(Handle {
            kind: "host".into(),
            id: "x".into(),
        })),
        Value::Bytes(Bytes::new(vec![1])),
    ] {
        assert_eq!(error_kind(stringify_json(&value, &mut heap)), "not_data");
        let nested = list(&mut heap, vec![value]);
        assert_eq!(error_kind(stringify_json(&nested, &mut heap)), "not_data");
    }
    for value in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
        assert_eq!(
            error_kind(stringify_json(&Value::Float(Float::new(value)), &mut heap)),
            "json_number"
        );
    }
    let map = Value::Map(heap.allocate(Object::Map(vec![(int(1), s("x"))])).unwrap());
    assert_eq!(error_kind(stringify_json(&map, &mut heap)), "json_key");
    assert_eq!(
        stringify_json(&Value::Float(Float::new(-0.0)), &mut heap).unwrap(),
        "-0.0"
    );
}

#[test]
fn k_ljson_005_cycles_are_refused_but_sharing_is_copied() {
    let mut heap = Heap::default();
    let child = list(&mut heap, vec![s("x")]);
    let shared = list(&mut heap, vec![child.clone(), child]);
    assert_eq!(
        stringify_json(&shared, &mut heap).unwrap(),
        "[[\"x\"],[\"x\"]]"
    );
    let id = heap.allocate(Object::List(vec![])).unwrap();
    heap.0.insert(id, Object::List(vec![Value::List(id)]));
    assert_eq!(
        error_kind(stringify_json(&Value::List(id), &mut heap)),
        "cycle"
    );
}

#[test]
fn k_ljson_006_native_parse_applies_explicit_number_kind_recursively() {
    let mut heap = Heap::default();
    let value = call(
        &mut heap,
        "json.parse",
        &[s("[1e3,18446744073709551617]"), s("int"), s("float")],
    )
    .unwrap();
    assert_eq!(
        call(&mut heap, "json.stringify", &[value]).unwrap(),
        s("[1000,18446744073709551617]")
    );
    let value = call(
        &mut heap,
        "json.parse",
        &[s("[1,1e3]"), s("number"), s("by_spelling")],
    )
    .unwrap();
    assert_eq!(
        items(&heap, value),
        vec![int(1), Value::Float(Float::new(1000.0))]
    );
}

#[test]
fn k_lfmt_001_precision_is_explicit_and_preserves_exact_integers() {
    let mut heap = Heap::default();
    assert_eq!(
        call(
            &mut heap,
            "format.fixed",
            &[
                int("18446744073709551617"
                    .parse::<num_bigint::BigInt>()
                    .unwrap()),
                int(2)
            ]
        )
        .unwrap(),
        s("18446744073709551617.00")
    );
    assert_eq!(
        call(
            &mut heap,
            "format.fixed",
            &[Value::Float(Float::new(2.5)), int(0)]
        )
        .unwrap(),
        s("2")
    );
    assert_eq!(
        call(
            &mut heap,
            "format.fixed",
            &[Value::Float(Float::new(-0.0)), int(2)]
        )
        .unwrap(),
        s("-0.00")
    );
    assert_eq!(
        call(
            &mut heap,
            "format.scientific",
            &[Value::Float(Float::new(125.0)), int(2)]
        )
        .unwrap(),
        s("1.25e2")
    );
    assert_eq!(
        error_kind(call(
            &mut heap,
            "format.fixed",
            &[Value::Float(Float::new(f64::NAN)), int(2)]
        )),
        "number_range"
    );
}

#[test]
fn k_lfmt_002_radix_and_padding_are_language_neutral() {
    let mut heap = Heap::default();
    assert_eq!(
        call(&mut heap, "format.radix", &[int(-255), int(16)]).unwrap(),
        s("-ff")
    );
    assert_eq!(
        call(&mut heap, "format.radix", &[int(35), int(36)]).unwrap(),
        s("z")
    );
    for radix in [1, 37] {
        assert_eq!(
            error_kind(call(&mut heap, "format.radix", &[int(1), int(radix)])),
            "number_range"
        );
    }
    assert_eq!(
        call(
            &mut heap,
            "format.pad",
            &[s("😀"), int(3), s("0"), s("start")]
        )
        .unwrap(),
        s("00😀")
    );
    assert_eq!(
        error_kind(call(
            &mut heap,
            "format.pad",
            &[s("x"), int(3), s("0"), s("center")]
        )),
        "type_error"
    );
}

#[test]
fn k_val_001_native_domains_never_coerce() {
    let mut heap = Heap::default();
    assert_eq!(
        error_kind(call(&mut heap, "text.len", &[int(1)])),
        "type_error"
    );
    assert_eq!(
        error_kind(call(
            &mut heap,
            "text.get",
            &[s("a"), Value::Float(Float::new(0.0))]
        )),
        "type_error"
    );
    let bad = list(&mut heap, vec![int(1)]);
    assert_eq!(
        error_kind(call(&mut heap, "text.join", &[bad, s(",")])),
        "type_error"
    );
    assert_eq!(
        error_kind(call(
            &mut heap,
            "bytes.slice",
            &[s("bytes"), int(0), int(1)]
        )),
        "type_error"
    );
    assert_eq!(error_kind(call(&mut heap, "text.len", &[])), "arity");
    assert_eq!(
        error_kind(call(&mut heap, "text.len", &[s("a"), s("b")])),
        "arity"
    );
}

fn load(heap: &mut Heap, datum: &lash_kernel_doc::Datum) -> Value {
    use lash_kernel_doc::Datum;
    match datum {
        Datum::Null => Value::Null,
        Datum::Absent => Value::Absent,
        Datum::Bool(value) => Value::Bool(*value),
        Datum::Int(value) => Value::Int(value.clone()),
        Datum::Float(value) => Value::Float(*value),
        Datum::Text(value) => Value::text(value.as_str()),
        Datum::Bytes(value) => Value::Bytes(value.clone()),
        Datum::List(values) => {
            let values = values.iter().map(|value| load(heap, value)).collect();
            list(heap, values)
        }
        Datum::Handle(value) => Value::Handle(std::sync::Arc::new(value.clone())),
        _ => panic!("unsupported native corpus input"),
    }
}

fn dump(heap: &Heap, value: &Value) -> lash_kernel_doc::Datum {
    use lash_kernel_doc::Datum;
    match value {
        Value::Null => Datum::Null,
        Value::Bool(value) => Datum::Bool(*value),
        Value::Int(value) => Datum::Int(value.clone()),
        Value::Float(value) => Datum::Float(*value),
        Value::Text(value) => Datum::Text(value.to_string()),
        Value::Bytes(value) => Datum::Bytes(value.clone()),
        Value::List(id) => Datum::List(
            (0..heap.len(*id))
                .map(|i| dump(heap, &heap.list_get(*id, i).unwrap()))
                .collect(),
        ),
        Value::Record(id) => {
            let mut fields = Vec::new();
            heap.visit(*id, &mut |element| {
                let Element::Field { name, value } = element else {
                    panic!("record field");
                };
                fields.push((name.to_owned(), dump(heap, value)));
                ControlFlow::Continue(())
            });
            Datum::Record(fields)
        }
        _ => panic!("unsupported native corpus result"),
    }
}

fn deep(datum: &lash_kernel_doc::Datum) -> u64 {
    use lash_kernel_doc::Datum;
    match datum {
        Datum::Int(integer) => 1 + integer.bits().div_ceil(64),
        Datum::Text(text) => 1 + u64::try_from(text.len()).unwrap(),
        Datum::Bytes(bytes) => 1 + u64::try_from(bytes.as_slice().len()).unwrap(),
        Datum::List(items) => {
            1 + u64::try_from(items.len()).unwrap() + items.iter().map(deep).sum::<u64>()
        }
        Datum::Record(fields) => {
            1 + u64::try_from(fields.len()).unwrap()
                + fields.iter().map(|(_, value)| deep(value)).sum::<u64>()
        }
        _ => 1,
    }
}

#[test]
fn k_lib_006_native_corpus_pins_results_charges_and_determinism() {
    use lash_kernel_doc::{Datum, ErrorDatum, Integer, Operand};
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
    let mut registry = FunctionRegistry::new();
    register_text_json(&mut registry).unwrap();
    let mut exercised = std::collections::BTreeSet::new();
    let mut shards: BTreeMap<&str, Vec<serde_json::Value>> = BTreeMap::new();
    for (ordinal, (rule, name, args, expected)) in cases.into_iter().enumerate() {
        let (id, registered) = registry
            .iter()
            .find(|(_, f)| f.definition.name.as_str() == name)
            .unwrap();
        exercised.insert(*id);
        let mut heap = Heap::default();
        let values: Vec<_> = args.iter().map(|arg| load(&mut heap, arg)).collect();
        let mut counter = WorkCounter::new(None);
        let actual = registered.native.as_ref().unwrap().call(NativeCall {
            args: &values,
            heap: &mut heap,
            counter: &mut counter,
        });
        let (outcome, result_size) = match (actual, expected) {
            (Ok(value), Ok(expected)) => {
                let actual = dump(&heap, &value);
                assert_eq!(actual, expected, "{name}");
                (serde_json::json!({"returned":actual}), deep(&actual))
            }
            (Err(NativeError::Raised(error)), Err(kind)) => {
                assert_eq!(error.kind, kind, "{name}");
                let error = ErrorDatum {
                    kind: error.kind,
                    message: error.message,
                    data: dump(&heap, &error.data),
                };
                (serde_json::json!({"raised":error}), 0)
            }
            (actual, expected) => panic!("{name}: {actual:?} != {expected:?}"),
        };
        let charged = registered
            .definition
            .charge
            .evaluate(&mut |operand, _| match operand {
                Operand::Result => result_size,
                Operand::Param(name) => deep(
                    &args[registered
                        .definition
                        .signature
                        .params
                        .iter()
                        .position(|p| p.name == *name)
                        .unwrap()],
                ),
            });
        assert_eq!(
            charged,
            1 + args.iter().map(deep).sum::<u64>() + result_size
        );
        assert_eq!(counter.spent(), 0);
        // A second call on the same implementation and heap must agree, and
        // neither call may change any input object (K-LIB-006, K-LIB-007).
        let mut warm_counter = WorkCounter::new(None);
        let warm = registered.native.as_ref().unwrap().call(NativeCall {
            args: &values,
            heap: &mut heap,
            counter: &mut warm_counter,
        });
        let warm = match warm {
            Ok(value) => serde_json::json!({"returned": dump(&heap, &value)}),
            Err(NativeError::Raised(error)) => {
                serde_json::json!({"raised": ErrorDatum { kind: error.kind, message: error.message, data: dump(&heap, &error.data) }})
            }
            error => panic!("unexpected warm outcome: {error:?}"),
        };
        assert_eq!(warm, outcome);
        assert_eq!(warm_counter.spent(), 0);
        assert_eq!(
            values
                .iter()
                .map(|value| dump_input(&heap, value))
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

fn dump_input(heap: &Heap, value: &Value) -> lash_kernel_doc::Datum {
    match value {
        Value::Absent => lash_kernel_doc::Datum::Absent,
        Value::Handle(handle) => lash_kernel_doc::Datum::Handle(handle.as_ref().clone()),
        _ => dump(heap, value),
    }
}

/// What a result holds, as a native function reserves it: a text by its
/// bytes, an integer by its magnitude, a list by its values.
fn held(heap: &Heap, value: &Value) -> u64 {
    match value {
        Value::Text(text) => text.len() as u64,
        Value::Int(integer) => integer.bits().div_ceil(8),
        Value::List(id) => heap.len(*id) as u64 * Room::VALUE,
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
    type Args = fn(&mut Heap) -> Vec<Value>;
    let cases: &[(&str, Args)] = &[
        ("text.repeat", |_| vec![s("ab"), int(40_000)]),
        ("text.replace", |_| {
            vec![s(&"x".repeat(300)), s("x"), s(&"y".repeat(300))]
        }),
        ("text.join", |heap| {
            vec![list(heap, vec![s(""); 300]), s(&"-".repeat(300))]
        }),
        ("text.split", |_| vec![s(&"x".repeat(2_000)), s("")]),
        ("text.to_code_points", |_| vec![s(&"x".repeat(2_000))]),
        ("text.to_utf16_units", |_| vec![s(&"x".repeat(2_000))]),
        ("text.pad_start", |_| vec![s(""), int(40_000), s("ab")]),
        ("text.pad_end", |_| vec![s(""), int(40_000), s("ab")]),
        ("format.pad", |_| {
            vec![s(""), int(40_000), s("ab"), s("end")]
        }),
        ("format.fixed", |_| vec![int(1), int(40_000)]),
        ("format.fixed", |_| {
            vec![Value::Float(Float::new(1.5)), int(40_000)]
        }),
        ("format.scientific", |_| {
            vec![Value::Float(Float::new(1.5)), int(40_000)]
        }),
        ("json.parse", |_| {
            vec![s("1e40000"), s("int"), s("by_spelling")]
        }),
        ("json.stringify", |heap| {
            let mut shared = list(heap, vec![s("xxxxxxxx")]);
            for _ in 0..12 {
                shared = list(heap, vec![shared.clone(), shared]);
            }
            vec![shared]
        }),
    ];
    // Each function that builds a result its room refuses, or allocates
    // an object before its reservation is refused.
    let mut unreserved = Vec::new();
    for (name, args) in cases {
        let mut heap = Heap::default();
        let args = args(&mut heap);
        let objects = heap.0.len();
        heap.1 = Room::of(ROOM);
        let refused = call(&mut heap, name, &args) == Err(NativeError::Memory);
        if !refused || heap.0.len() != objects {
            unreserved.push(*name);
            continue;
        }
        heap.1 = Room::default();
        let value = call(&mut heap, name, &args).unwrap();
        assert!(
            heap.1.reserved >= held(&heap, &value),
            "{name} reserved {} for a result of {}",
            heap.1.reserved,
            held(&heap, &value)
        );
    }
    assert_eq!(unreserved, [""; 0]);
}
