//! Canonical primitive codec laws, independent of replaced callable transport.
use super::*;
use crate::graph_tokens::{CanonicalFault, DecodeDimension, decode_body, encode_body};
use crate::parse_document;
use lash_vocabulary::legacy_names::Name;
use lash_vocabulary::tokens::MAX_DECODE_DEPTH;
use num_bigint::BigInt;

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

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

#[test]
fn declared_loop_charges_round_trip_in_text_and_graph_tokens() {
    let document = parse_document("kernel 1\nnumbers by_spelling\nmain { for x in [1] metered 2 { while false metered 3 { continue } } }").unwrap();
    round_trip(&document);
    let text = crate::print_document(&document);
    assert_eq!(parse_document(&text).unwrap(), document);
    assert!(text.contains("metered 2"));
    assert!(text.contains("metered 3"));
}

fn round_trip<T: TokenCodec + PartialEq + std::fmt::Debug>(value: &T) -> Vec<u8> {
    let bytes = encode_body(value);
    let back: T = decode_body(&bytes).unwrap_or_else(|error| panic!("{error}: {value:?}"));
    assert_eq!(&back, value);
    assert_eq!(encode_body(&back), bytes, "re-encoding changed the bytes");
    bytes
}
