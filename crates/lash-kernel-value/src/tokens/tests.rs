//! PC-CODEC, token half: every tag's one spelling, and every refusal of a
//! spelling the grammar does not admit.

use super::*;

fn hex(text: &str) -> Vec<u8> {
    (0..text.len())
        .step_by(2)
        .map(|index| u8::from_str_radix(&text[index..index + 2], 16).unwrap())
        .collect()
}

fn read_all(bytes: &[u8]) -> Result<Vec<TokenEvent<'_>>, TokenError> {
    let mut reader = TokenReader::new(bytes);
    let mut events = Vec::new();
    while !reader.is_empty() {
        events.push(reader.next_event()?);
    }
    Ok(events)
}

/// Each token's one canonical spelling: writing it gives these bytes, and
/// reading these bytes gives the token back and writes them again.
#[test]
fn every_tag_has_one_pinned_spelling_that_round_trips() {
    let child = ChildRef([0xab; 32]);
    let cases: Vec<(TokenEvent<'_>, String)> = vec![
        (TokenEvent::Unit, "00".into()),
        (TokenEvent::Bool(false), "01".into()),
        (TokenEvent::Bool(true), "02".into()),
        (TokenEvent::Unsigned(0), "0300".into()),
        (TokenEvent::Unsigned(127), "037f".into()),
        (TokenEvent::Unsigned(128), "038001".into()),
        (
            TokenEvent::Unsigned(u64::MAX),
            "03ffffffffffffffffff01".into(),
        ),
        (TokenEvent::Signed(0), "0400".into()),
        (TokenEvent::Signed(-1), "0401".into()),
        (TokenEvent::Signed(1), "0402".into()),
        (
            TokenEvent::Signed(i64::MIN),
            "04ffffffffffffffffff01".into(),
        ),
        (
            TokenEvent::Signed(i64::MAX),
            "04feffffffffffffffff01".into(),
        ),
        (
            TokenEvent::BigInteger {
                negative: false,
                magnitude: &[],
            },
            "050000".into(),
        ),
        (
            TokenEvent::BigInteger {
                negative: true,
                magnitude: &[0x01, 0x02],
            },
            "0501020102".into(),
        ),
        (TokenEvent::Float(0.0), "060000000000000000".into()),
        (TokenEvent::Float(-0.0), "060000000000000080".into()),
        (TokenEvent::Float(1.5), "06000000000000f83f".into()),
        (TokenEvent::Float(f64::NAN), "06000000000000f87f".into()),
        (TokenEvent::Text(""), "0700".into()),
        (TokenEvent::Text("é"), "0702c3a9".into()),
        (TokenEvent::Bytes(&[0, 255]), "080200ff".into()),
        (TokenEvent::Seq(2), "0902".into()),
        (TokenEvent::Map(1), "0a01".into()),
        (TokenEvent::Child(child), format!("0b{}", "ab".repeat(32))),
        (
            TokenEvent::Sum {
                variant: 18,
                fields: 1,
            },
            "0c1201".into(),
        ),
        (TokenEvent::None, "0d".into()),
        (TokenEvent::Some, "0e".into()),
    ];
    for (event, spelling) in cases {
        let mut writer = TokenWriter::new();
        writer.event(&event);
        assert_eq!(
            writer.as_bytes(),
            hex(&spelling),
            "{event:?} writes {spelling}"
        );
        let bytes = hex(&spelling);
        let read = read_all(&bytes).unwrap();
        assert_eq!(read.len(), 1, "{spelling} is one token");
        let mut again = TokenWriter::new();
        again.event(&read[0]);
        assert_eq!(again.as_bytes(), bytes, "{spelling} reads and writes back");
    }
}

/// The writer normalizes what has two spellings in Rust: any NaN, and an
/// arbitrary integer with high zero bytes or a negative zero.
#[test]
fn the_writer_writes_only_canonical_spellings() {
    let mut writer = TokenWriter::new();
    writer.float(f64::from_bits(0x7ff0_0000_0000_0001));
    writer.big_integer(true, &[0, 0]);
    writer.big_integer(false, &[5, 0, 0]);
    assert_eq!(
        writer.as_bytes(),
        hex("06000000000000f87f" /* NaN */)
            .into_iter()
            .chain(hex("050000"))
            .chain(hex("05000105"))
            .collect::<Vec<_>>()
    );
}

/// Every non-canonical or malformed spelling is refused with its own cause.
#[test]
fn every_noncanonical_spelling_is_refused() {
    use CanonicalFault::*;
    let cases: &[(&str, TokenError)] = &[
        ("03", TokenError::Truncated),
        ("0380", TokenError::Truncated),
        ("038000", TokenError::NonCanonical(NonMinimalVarint)),
        ("03ff00", TokenError::NonCanonical(NonMinimalVarint)),
        ("03ffffffffffffffffff02", TokenError::LengthOverflow),
        ("03ffffffffffffffffff8100", TokenError::LengthOverflow),
        ("050100", TokenError::NonCanonical(NonMinimalInteger)),
        ("0500020100", TokenError::NonCanonical(NonMinimalInteger)),
        ("05020101", TokenError::NonCanonical(NonMinimalInteger)),
        ("06010000000000f87f", TokenError::NonCanonical(InvalidNaN)),
        ("06000000000000f8", TokenError::Truncated),
        ("0702c3", TokenError::Truncated),
        ("0701ff", TokenError::InvalidUtf8),
        ("0702c328", TokenError::InvalidUtf8),
        ("0b00", TokenError::Truncated),
        ("0c8080808010", TokenError::LengthOverflow),
        ("0f", TokenError::NonCanonical(UnknownTag(0x0f))),
        ("ff", TokenError::NonCanonical(UnknownTag(0xff))),
    ];
    for (spelling, refusal) in cases {
        let bytes = hex(spelling);
        assert_eq!(read_all(&bytes), Err(refusal.clone()), "{spelling}");
    }
}

#[test]
fn a_typed_decode_refuses_trailing_bytes_wrong_tags_and_field_counts() {
    assert_eq!(
        decode_body::<u64>(&hex("030100")),
        Err(TokenError::TrailingBytes)
    );
    assert_eq!(
        decode_body::<u64>(&hex("0401")),
        Err(TokenError::NonCanonical(CanonicalFault::UnexpectedTag {
            expected: 0x03,
            found: 0x04
        }))
    );
    let mut reader = TokenReader::new(&[0x09, 0x02]);
    assert_eq!(
        reader.expect_struct(3),
        Err(TokenError::NonCanonical(CanonicalFault::FieldCount {
            expected: 3,
            found: 2
        }))
    );
    // A sequence count beyond the unread bytes is refused before it sizes an
    // allocation.
    assert_eq!(
        decode_body::<Vec<u64>>(&hex("09ffffffffffffffffff01")),
        Err(TokenError::Truncated)
    );
    assert_eq!(
        decode_body::<Option<u64>>(&hex("0e")),
        Err(TokenError::Truncated)
    );
}

/// Exact numeric round trips: every integer width, signed zero and the
/// canonical NaN come back bit for bit.
#[test]
fn numbers_round_trip_exactly() {
    for value in [0, 1, 127, 128, 16_383, 16_384, u64::MAX >> 1, u64::MAX] {
        assert_eq!(decode_body::<u64>(&encode_body(&value)), Ok(value));
    }
    for value in [0, -1, 1, -64, 63, -65, 64, i64::MIN, i64::MAX] {
        assert_eq!(decode_body::<i64>(&encode_body(&value)), Ok(value));
    }
    for value in [
        0.0_f64,
        -0.0,
        1.0,
        -1.5,
        f64::INFINITY,
        f64::NEG_INFINITY,
        f64::MIN_POSITIVE,
    ] {
        let back = decode_body::<f64>(&encode_body(&value)).unwrap();
        assert_eq!(back.to_bits(), value.to_bits());
    }
    let nan = decode_body::<f64>(&encode_body(&f64::NAN)).unwrap();
    assert_eq!(nan.to_bits(), CANONICAL_NAN_BITS);
    let nested: Vec<Option<String>> = vec![None, Some("x".into()), Some(String::new())];
    assert_eq!(
        decode_body::<Vec<Option<String>>>(&encode_body(&nested)),
        Ok(nested)
    );
}

#[test]
fn a_physical_reader_reads_slots_and_skips_whole_trees() {
    let mut writer = TokenWriter::new();
    writer.seq(2);
    writer.slot(300);
    writer.sum(1, 1);
    writer.text("a");
    writer.unit();
    let bytes = writer.into_bytes();
    let mut reader = TokenReader::with_form(&bytes, RefForm::Physical);
    assert_eq!(reader.skip_tree(), Ok(4));
    assert_eq!(reader.next_event(), Ok(TokenEvent::Unit));
    assert!(reader.is_empty());
    let mut reader = TokenReader::with_form(&bytes, RefForm::Physical);
    reader.seq().unwrap();
    assert_eq!(reader.next_event(), Ok(TokenEvent::Slot(300)));
}

#[test]
fn varint_lengths_match_their_encodings() {
    for value in [
        0,
        1,
        127,
        128,
        16_383,
        16_384,
        u64::from(u32::MAX),
        u64::MAX,
    ] {
        let mut out = Vec::new();
        put_varint(&mut out, value);
        assert_eq!(out.len(), varint_len(value));
        let mut pos = 0;
        assert_eq!(get_varint(&out, &mut pos), Ok(value));
        assert_eq!(pos, out.len());
    }
}
