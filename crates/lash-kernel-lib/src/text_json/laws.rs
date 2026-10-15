//! Named text/JSON rules exercised through the real paged heap and census.
use crate::testsupport::{datum, decode, heap, heap_with_source, hold_with_extra};
use lash_kernel_doc::heap::{HeapFault, NativeError, NativeHeap};
use lash_kernel_doc::{Datum, FunctionRegistry, Integer, NativeCall, Value, WorkCounter};
use lash_kernel_state::paged::PagedHeap;

fn s(value: &str) -> Datum {
    Datum::Text(value.into())
}
fn int(value: i64) -> Datum {
    Datum::Int(Integer::from(value))
}
fn args(heap: &mut PagedHeap, values: Vec<Datum>) -> Vec<Value> {
    values.into_iter().map(|v| decode(heap, v)).collect()
}
fn call(heap: &mut PagedHeap, name: &str, args: &[Value]) -> Result<Value, NativeError> {
    let mut registry = FunctionRegistry::new();
    super::register_text_json(&mut registry).unwrap();
    let (_, native) = registry
        .iter()
        .find(|(_, f)| f.definition().name.as_str() == name)
        .unwrap();
    native.native().unwrap().call(NativeCall {
        args,
        heap,
        counter: &mut WorkCounter::new(None),
    })
}

/// K-VAL-007/K-LTXT-001: scalar and UTF-16 indexing have distinct coordinates.
#[test]
fn astral_text_keeps_scalar_and_utf16_coordinates() {
    let mut heap = heap();
    for (name, index, expected) in [
        ("text.len", None, int(3)),
        ("text.utf16_len", None, int(4)),
        ("text.get", Some(1), s("😀")),
        ("text.get", Some(-1), s("z")),
        ("text.utf16_get", Some(1), int(0xd83d)),
        ("text.utf16_get", Some(2), int(0xde00)),
    ] {
        let mut input = vec![s("a😀z")];
        if let Some(index) = index {
            input.push(int(index));
        }
        let input = args(&mut heap, input);
        let output = call(&mut heap, name, &input).unwrap();
        assert_eq!(datum(&heap, &output), expected, "{name}");
    }
}

/// K-LTXT-011: the supplied scalar set and chosen ends determine trimming.
#[test]
fn trimming_uses_the_supplied_scalar_set_and_ends() {
    let mut heap = heap();
    for (source, set, leading, trailing, expected) in [
        ("aa😀aaa", "a", true, false, "😀aaa"),
        ("aa😀aaa", "a", false, true, "aa😀"),
        ("aa😀aaa", "a😀", true, true, ""),
        ("😀a😀", "😀", true, true, "a"),
        ("x\u{85}x", "x", true, true, "\u{85}"),
        ("\u{feff}x\u{feff}", "\u{feff}", false, true, "\u{feff}x"),
    ] {
        let input = args(
            &mut heap,
            vec![
                s(source),
                s(set),
                Datum::Bool(leading),
                Datum::Bool(trailing),
            ],
        );
        let output = call(&mut heap, "text.trim_set", &input).unwrap();
        assert_eq!(datum(&heap, &output), s(expected));
    }
}

/// K-LJSON-005/006: syntax and depth refusal precede exhausted result space.
#[test]
fn a_refused_json_tree_preserves_its_syntax_and_depth_errors() {
    for (input, expected) in [
        (format!("[{}0", "0,".repeat(1_999)), "json_syntax"),
        (
            format!("[{}{}]", "0,".repeat(1_999), "[".repeat(100)),
            "json_depth",
        ),
    ] {
        let mut heap = heap();
        let input = args(&mut heap, vec![s(&input), s("int"), s("by_spelling")]);
        hold_with_extra(&mut heap, &input, 1 << 10);
        let Err(NativeError::Raised(error)) = call(&mut heap, "json.parse", &input) else {
            panic!("JSON error")
        };
        assert_eq!(crate::testsupport::text(&heap, &error.kind), expected);
    }
}

/// S1: a native payload read preserves infrastructure causes unchanged.
#[test]
fn a_native_text_read_preserves_its_page_transport_failure() {
    let (mut heap, source) = heap_with_source();
    let input = args(&mut heap, vec![s(&"x".repeat(1 << 20))]);
    let fault = HeapFault::Transport("native text read".into());
    source.fail_reads(fault.clone());
    assert!(
        matches!(call(&mut heap,&format!("text.upper_u{}_{}_{}",char::UNICODE_VERSION.0,char::UNICODE_VERSION.1,char::UNICODE_VERSION.2),&input), Err(NativeError::Heap(actual)) if actual==fault)
    );
}

/// FIG-6193: a UTF-16 slice of a paged text reads the pages its range
/// addresses, as a scalar slice does, never the whole text. TypeScript's
/// `text.slice(0, 4096)` on a 1 MB session global read every page of it on
/// each turn.
#[test]
fn a_utf16_slice_of_a_paged_text_reads_only_the_pages_it_addresses() {
    // Distinct leaves: content-addressed pages of a repeated text dedupe.
    let whole: String = (0..100_000).map(|i| format!("{i:07}😀")).collect();
    let (mut heap, source) = heap_with_source();
    let input = args(&mut heap, vec![s(&whole)]);
    let Value::Text(text) = &input[0] else {
        panic!("a text")
    };
    let text = text.clone();
    // Cold: the text's pages are read through the source.
    let mut heap = crate::testsupport::reopen(&mut heap, &source);
    for (start, end) in [(0, 4096), (7, 9)] {
        let before = source.pages_read();
        let range = args(&mut heap, vec![int(start), int(end)]);
        let output = call(
            &mut heap,
            "text.utf16_slice",
            &[
                Value::Text(text.clone()),
                range[0].clone(),
                range[1].clone(),
            ],
        )
        .unwrap();
        let read = source.pages_read() - before;
        let units: Vec<u16> = whole.encode_utf16().collect();
        assert_eq!(
            datum(&heap, &output),
            s(&String::from_utf16(&units[start as usize..end as usize]).unwrap())
        );
        assert!(read <= 4, "{start}..{end} read {read} pages");
    }
}

/// K-LJSON-007: classification reads its rooted input with no guest reservation.
#[test]
fn classification_needs_no_extra_guest_space() {
    for (input, expected) in [
        (Datum::Null, 3),
        (Datum::List(vec![int(1), int(2)]), 7),
        (Datum::Record(vec![("toJSON".into(), Datum::Null)]), 16),
    ] {
        let mut heap = heap();
        let input = args(
            &mut heap,
            vec![
                input,
                int(128),
                int(8),
                Datum::Tuple(vec![s("toJSON"), s("brand")]),
                s("0"),
                s(":"),
            ],
        );
        hold_with_extra(&mut heap, &input, 0);
        let before = heap.certificate();
        let output = call(&mut heap, "json.classify", &input).unwrap();
        assert_eq!(datum(&heap, &output), int(expected));
        assert_eq!(heap.certificate(), before);
    }
}

/// S1/K-LJSON-006: only a guest memory refusal may be deferred for syntax
/// validation. A census page failure is returned immediately and unchanged.
#[test]
fn json_reservation_preserves_a_census_page_failure() {
    let (mut heap, source) = heap_with_source();
    let list = heap
        .allocate(lash_kernel_doc::Object::List(vec![Value::Null]))
        .unwrap();
    let mut heap = crate::testsupport::reopen(&mut heap, &source);
    let input = args(&mut heap, vec![s("[0"), s("int"), s("by_spelling")]);
    let mut held = input.clone();
    held.push(Value::List(list));
    hold_with_extra(&mut heap, &held, 0);
    let fault = HeapFault::Transport("JSON census page".into());
    source.fail_reads(fault.clone());
    assert!(
        matches!(call(&mut heap,"json.parse",&input),Err(NativeError::Heap(actual)) if actual==fault)
    );
}

/// K-LJSON-005: paged input preserves escaped Unicode, number tokens and
/// duplicate-key insertion order across payload-leaf boundaries.
#[test]
fn paged_json_preserves_unicode_numbers_and_first_key_positions() {
    let prefix = "x".repeat(4093);
    let source = format!(
        "{{\"first\":\"{prefix}😀\",\"first\":\"last\",\"num\":1e3,\"text\":\"{prefix}\\uD83D\\uDE00\"}}"
    );
    let mut heap = heap();
    let input = args(&mut heap, vec![s(&source), s("int"), s("by_spelling")]);
    let output = call(&mut heap, "json.parse", &input).unwrap();
    assert_eq!(
        datum(&heap, &output),
        Datum::Record(vec![
            ("first".into(), s("last")),
            ("num".into(), int(1000)),
            ("text".into(), s(&format!("{prefix}😀")))
        ])
    );
}

/// K-LFMT-004: every out-of-range radix has the same error, even when its
/// magnitude is paged and there is no room to materialize it.
#[test]
fn a_paged_radix_keeps_the_range_error_before_reading_the_number() {
    for negative in [false, true] {
        let mut heap = heap();
        let magnitude = num_bigint::BigInt::from(10_u32).pow(1000);
        let radix = Integer::new(if negative { -magnitude } else { magnitude });
        let input = args(&mut heap, vec![Datum::Absent, Datum::Int(radix)]);
        hold_with_extra(&mut heap, &input, 0);
        let Err(NativeError::Raised(error)) = call(&mut heap, "format.radix", &input) else {
            panic!("radix range error")
        };
        assert_eq!(crate::testsupport::text(&heap, &error.kind), "number_range");
        assert_eq!(
            crate::testsupport::text(&heap, &error.message),
            "radix must be between 2 and 36"
        );
    }
}
