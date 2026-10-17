//! PC-EXACT, codec half: the kernel's values, forms and definitions read
//! back exactly from their one binary spelling.

use num_bigint::BigInt;

use super::*;
use crate::graph_tokens::{CanonicalFault, DecodeDimension, decode_body, encode_body};
use crate::parse_document;
use crate::tests::{catalog, every_form_corpus};

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn round_trip<T: TokenCodec + PartialEq + std::fmt::Debug>(value: &T) -> Vec<u8> {
    let bytes = encode_body(value);
    let back: T = decode_body(&bytes).unwrap_or_else(|error| panic!("{error}: {value:?}"));
    assert_eq!(&back, value);
    assert_eq!(encode_body(&back), bytes, "re-encoding changed the bytes");
    bytes
}

/// An integer is always the arbitrary-integer token, minimal at every width:
/// the i64 edges and the first values past them read back as the same
/// integer whichever representation holds it.
#[test]
fn integers_take_one_spelling_at_every_width_boundary() {
    let cases: Vec<(Integer, &str)> = vec![
        (Integer::from(0), "050000"),
        (Integer::from(-1), "05010101"),
        (Integer::from(255), "050001ff"),
        (Integer::from(256), "0500020001"),
        (Integer::from(i64::MAX), "050008ffffffffffffff7f"),
        (Integer::from(i64::MIN), "0501080000000000000080"),
        (
            Integer::new(BigInt::from(i64::MAX) + 1),
            "0500080000000000000080",
        ),
        (
            Integer::new(BigInt::from(i64::MIN) - 1),
            "0501080100000000000080",
        ),
        (
            Integer::new(BigInt::from(u64::MAX) + 1),
            "050009000000000000000001",
        ),
    ];
    for (integer, spelling) in cases {
        assert_eq!(hex(&round_trip(&integer)), spelling, "{integer}");
    }
    // A big decode of an i64-range magnitude is the small integer.
    let decoded: Integer = decode_body(&[0x05, 0x01, 0x08, 0, 0, 0, 0, 0, 0, 0, 0x80]).unwrap();
    assert_eq!(decoded, Integer::from(i64::MIN));
    assert_eq!(decoded.to_string(), i64::MIN.to_string());
}

/// A float is its bits: signed zero survives, every NaN is the canonical
/// one, and another NaN's bits are refused rather than repaired.
#[test]
fn floats_keep_signed_zero_and_refuse_a_second_nan() {
    let positive = round_trip(&Datum::Float(Float::new(0.0)));
    let negative = round_trip(&Datum::Float(Float::new(-0.0)));
    assert_ne!(positive, negative);
    let nan = round_trip(&Datum::Float(Float::new(f64::from_bits(
        0x7ff0_0000_0000_0001,
    ))));
    assert_eq!(hex(&nan), "0c040106000000000000f87f");
    for infinity in [f64::INFINITY, f64::NEG_INFINITY, f64::MIN_POSITIVE, 1e300] {
        round_trip(&Datum::Float(Float::new(infinity)));
    }
    let other_nan = hex(&nan).replace("f87f", "f8ff");
    let bytes: Vec<u8> = (0..other_nan.len())
        .step_by(2)
        .map(|index| u8::from_str_radix(&other_nan[index..index + 2], 16).unwrap())
        .collect();
    assert_eq!(
        decode_body::<Datum>(&bytes),
        Err(TokenError::NonCanonical(CanonicalFault::InvalidNaN))
    );
}

/// The pinned FIG-5887 §2 discriminants: each Datum variant is the
/// sum number the design fixes, whatever the Rust declaration order.
#[test]
fn datum_variants_carry_their_pinned_numbers() {
    let data = [
        Datum::Null,
        Datum::Absent,
        Datum::Bool(false),
        Datum::Int(Integer::from(1)),
        Datum::Float(Float::new(2.0)),
        Datum::Number(NumberToken::new("1.50e3").unwrap()),
        Datum::Text("x".into()),
        Datum::Bytes(Bytes::new(vec![])),
        Datum::Timestamp(Timestamp {
            nanoseconds: Integer::from(0),
        }),
        Datum::Tuple(vec![Datum::Null]),
        Datum::List(vec![]),
        Datum::Map(vec![
            (Datum::Text("b".into()), Datum::Null),
            (Datum::Text("a".into()), Datum::Null),
        ]),
        Datum::Set(vec![
            Datum::Int(Integer::from(2)),
            Datum::Int(Integer::from(1)),
        ]),
        Datum::Record(vec![("z".into(), Datum::Null), ("a".into(), Datum::Absent)]),
        Datum::Error(Box::new(ErrorDatum {
            kind: "k".into(),
            message: "m".into(),
            data: Datum::Null,
        })),
        Datum::Function(Name::new("g")),
        Datum::Handle(Handle {
            kind: "k".into(),
            id: "i".into(),
        }),
    ];
    for (number, datum) in data.iter().enumerate() {
        let bytes = round_trip(datum);
        assert_eq!(&bytes[..2], &[0x0c, number as u8], "{datum:?}");
    }
    // A number token keeps its exact lexical spelling.
    let token = encode_body(&Datum::Number(NumberToken::new("1.50e3").unwrap()));
    assert!(token.ends_with(b"1.50e3"));
}

/// Every form, literal and type of the corpus, and every catalog definition,
/// reads back exactly; insertion-ordered guest maps keep their order.
#[test]
fn documents_and_definitions_read_back_exactly() {
    for text in every_form_corpus() {
        let document = parse_document(&text).unwrap();
        round_trip(&document);
    }
    for definition in catalog().0.values() {
        round_trip(definition);
    }
    round_trip(&serde_json::json!({"z": [1, -2, 2.5, null, true, "s"], "a": {}}));
}

/// A sorted map or set read in another order, or with a repeated key, is
/// refused: it is not the one spelling.
#[test]
fn an_unsorted_or_repeated_key_is_refused() {
    let mut out = TokenWriter::new();
    out.seq(2);
    Name::new("b").encode(&mut out);
    Name::new("a").encode(&mut out);
    assert_eq!(
        decode_body::<BTreeSet<Name>>(out.as_bytes()),
        Err(TokenError::NonCanonical(
            CanonicalFault::UnsortedOrDuplicateKey
        ))
    );
    let mut out = TokenWriter::new();
    out.map(2);
    for _ in 0..2 {
        Name::new("a").encode(&mut out);
        Datum::Null.encode(&mut out);
    }
    assert_eq!(
        decode_body::<BTreeMap<Name, Datum>>(out.as_bytes()),
        Err(TokenError::NonCanonical(
            CanonicalFault::UnsortedOrDuplicateKey
        ))
    );
}

/// Nesting past the bound is a typed limit refusal, not a stack overflow.
#[test]
fn nesting_past_the_decode_bound_is_refused() {
    let depth = MAX_DECODE_DEPTH as usize + 1;
    let mut out = TokenWriter::new();
    for _ in 0..depth {
        out.sum(9, 1);
        out.seq(1);
    }
    out.sum(0, 0);
    assert!(matches!(
        decode_body::<Datum>(out.as_bytes()),
        Err(TokenError::Limit {
            dimension: DecodeDimension::Depth,
            ..
        })
    ));
    let mut nested = Datum::Null;
    for _ in 0..depth - 2 {
        nested = Datum::Tuple(vec![nested]);
    }
    round_trip(&nested);
}

/// KERNEL-DATA-POLICY: recursive safety guards can be narrowed at the host's reader.
#[test]
fn a_host_kernel_data_policy_controls_recursive_decoding() {
    let value = Datum::Tuple(vec![Datum::Tuple(vec![Datum::Null])]);
    let bytes = encode_body(&value);
    let limits = crate::KernelDataLimits::new(1, 1, 1, 1, 1).unwrap();
    assert!(matches!(
        crate::graph_tokens::decode_body_with_limits::<Datum>(&bytes, limits),
        Err(TokenError::Limit {
            dimension: DecodeDimension::Depth,
            limit: 1,
            ..
        })
    ));
    let document = parse_document("kernel 1\nnumbers by_spelling\nmain { return null }\n").unwrap();
    assert!(
        crate::text::parse_document_with_limits(
            "kernel 1\nnumbers by_spelling\nmain { return ((null)) }\n",
            limits
        )
        .is_err()
    );
    let json = document.to_json().unwrap();
    assert!(matches!(
        crate::Document::from_json_with_limits(&json, limits),
        Err(crate::DecodeError::TooDeep { limit: 1, .. })
    ));
}

/// ADR 0123: a decoder's guest nesting budget belongs to that decoder.
#[test]
fn independent_decoders_do_not_share_guest_depth() {
    let bytes = encode_body(&TaskIdentity::Main);
    let limits = crate::KernelDataLimits::new(64, 352, 1, 512, 128).unwrap();
    let mut outer = TokenReader::new(&[]).with_data_limits(limits);
    let result = outer.nested(|_| {
        let mut inner = TokenReader::new(&bytes).with_data_limits(limits);
        TaskIdentity::decode(&mut inner)
    });
    assert_eq!(result.unwrap(), TaskIdentity::Main);
}

/// FN-BOUND: a function value whose code and captures take exactly the
/// standard bound reads back from its datum spelling, and checks; one byte
/// more is refused before anything holds it.
#[test]
fn a_function_value_at_its_bound_round_trips_and_one_past_it_is_refused() {
    let (catalog, _) = catalog();
    let value = |length: usize| {
        let mut document = crate::Document::new(NumberPolicy::Float, Vec::new());
        let run = Name::new(crate::FUNCTION_VALUE_ENTRY);
        document.functions.insert(
            run.clone(),
            crate::Function {
                params: Vec::new(),
                body: vec![crate::Stmt::Return {
                    value: crate::Expr::Literal(crate::Literal::Text("x".repeat(length))),
                }],
                start: None,
            },
        );
        document.entries.insert(
            run.clone(),
            crate::Entry {
                signature: crate::Signature {
                    params: Vec::new(),
                    result: crate::Type::Any,
                },
                convention: crate::StartConvention::Direct,
            },
        );
        crate::FunctionValue {
            document,
            entry: run,
        }
    };
    let ceiling = crate::FunctionValueLimits::CEILING;
    let base = value(0).counted_bytes().expect("the value's bytes");
    let length = usize::try_from(ceiling - base).expect("a length");
    let at = value(length);
    assert_eq!(at.counted_bytes().expect("the value's bytes"), ceiling);
    let limits = crate::FunctionValueLimits::standard();
    let data = crate::KernelDataLimits::standard();
    at.check(&catalog, limits, data)
        .expect("a value at the bound checks");
    round_trip(&Datum::FunctionValue(Box::new(at)));
    assert_eq!(
        value(length + 1).check(&catalog, limits, data),
        Err(crate::FunctionValueRefusal::TooLarge {
            bytes: ceiling + 1,
            limit: ceiling,
        })
    );
}

/// K-CHG-007/K-ID-002: declared loop events are semantic definition data,
/// preserved by both canonical graph tokens and the kernel text notation.
#[test]
fn declared_loop_charges_round_trip_in_text_and_graph_tokens() {
    let document = parse_document("kernel 1\nnumbers by_spelling\nmain { for x in [1] metered 2 { while false metered 3 { continue } } }").unwrap();
    round_trip(&document);
    let text = crate::print_document(&document);
    assert_eq!(parse_document(&text).unwrap(), document);
    assert!(text.contains("metered 2"));
    assert!(text.contains("metered 3"));
}
