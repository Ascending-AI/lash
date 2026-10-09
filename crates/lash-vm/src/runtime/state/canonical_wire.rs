use super::*;

pub(super) fn take_canonical_integer(
    bytes: &[u8],
    cursor: &mut usize,
    location: &str,
    marker: u8,
) -> Result<i128, SnapshotDecodeError> {
    let value = match marker {
        0x00..=0x7f => i128::from(marker),
        0xe0..=0xff => i128::from(i8::from_be_bytes([marker])),
        0xcc => {
            let value = take_byte(bytes, cursor)?;
            if value <= 127 {
                return Err(non_canonical(location, "integer width is not minimal"));
            }
            i128::from(value)
        }
        0xcd => {
            let value = take_u16(bytes, cursor)?;
            if value <= u16::from(u8::MAX) {
                return Err(non_canonical(location, "integer width is not minimal"));
            }
            i128::from(value)
        }
        0xce => {
            let value = take_u32(bytes, cursor)?;
            if value <= u32::from(u16::MAX) {
                return Err(non_canonical(location, "integer width is not minimal"));
            }
            i128::from(value)
        }
        0xcf => {
            let value = take_u64(bytes, cursor)?;
            if value <= u64::from(u32::MAX) {
                return Err(non_canonical(location, "integer width is not minimal"));
            }
            i128::from(value)
        }
        0xd0 => {
            let value = i8::from_be_bytes([take_byte(bytes, cursor)?]);
            if value >= -32 {
                return Err(non_canonical(location, "integer width is not minimal"));
            }
            i128::from(value)
        }
        0xd1 => {
            let value = i16::from_be_bytes(take_array::<2>(bytes, cursor)?);
            if value >= i16::from(i8::MIN) {
                return Err(non_canonical(location, "integer width is not minimal"));
            }
            i128::from(value)
        }
        0xd2 => {
            let value = i32::from_be_bytes(take_array::<4>(bytes, cursor)?);
            if value >= i32::from(i16::MIN) {
                return Err(non_canonical(location, "integer width is not minimal"));
            }
            i128::from(value)
        }
        0xd3 => {
            let value = i64::from_be_bytes(take_array::<8>(bytes, cursor)?);
            if value >= i64::from(i32::MIN) {
                return Err(non_canonical(location, "integer width is not minimal"));
            }
            i128::from(value)
        }
        _ => return Err(unexpected_marker(location, "an integer", marker)),
    };
    Ok(value)
}

fn is_integer_marker(marker: u8) -> bool {
    matches!(marker, 0x00..=0x7f | 0xcc..=0xcf | 0xd0..=0xd3 | 0xe0..=0xff)
}

pub(super) fn take_byte(bytes: &[u8], cursor: &mut usize) -> Result<u8, SnapshotDecodeError> {
    let byte = bytes
        .get(*cursor)
        .copied()
        .ok_or_else(|| invalid_messagepack("unexpected end of input"))?;
    *cursor += 1;
    Ok(byte)
}

pub(super) fn take_u16(bytes: &[u8], cursor: &mut usize) -> Result<u16, SnapshotDecodeError> {
    let value = take_array::<2>(bytes, cursor)?;
    Ok(u16::from_be_bytes(value))
}

pub(super) fn take_u32(bytes: &[u8], cursor: &mut usize) -> Result<u32, SnapshotDecodeError> {
    let value = take_array::<4>(bytes, cursor)?;
    Ok(u32::from_be_bytes(value))
}

fn take_u64(bytes: &[u8], cursor: &mut usize) -> Result<u64, SnapshotDecodeError> {
    let value = take_array::<8>(bytes, cursor)?;
    Ok(u64::from_be_bytes(value))
}

#[expect(
    clippy::expect_used,
    reason = "the slice length was checked by endian-fixed take_length arithmetic above, per the message"
)]
fn take_array<const N: usize>(
    bytes: &[u8],
    cursor: &mut usize,
) -> Result<[u8; N], SnapshotDecodeError> {
    let end = cursor
        .checked_add(N)
        .ok_or_else(|| invalid_messagepack("length overflow"))?;
    let value = bytes
        .get(*cursor..end)
        .ok_or_else(|| invalid_messagepack("unexpected end of input"))?
        .try_into()
        .expect("slice length was checked");
    *cursor = end;
    Ok(value)
}

pub(super) fn skip_bytes(
    bytes: &[u8],
    cursor: &mut usize,
    length: usize,
) -> Result<(), SnapshotDecodeError> {
    let end = cursor
        .checked_add(length)
        .ok_or_else(|| invalid_messagepack("length overflow"))?;
    if end > bytes.len() {
        return Err(invalid_messagepack("unexpected end of input"));
    }
    *cursor = end;
    Ok(())
}

fn take_slice<'a>(
    bytes: &'a [u8],
    cursor: &mut usize,
    length: usize,
) -> Result<&'a [u8], SnapshotDecodeError> {
    let start = *cursor;
    skip_bytes(bytes, cursor, length)?;
    Ok(&bytes[start..*cursor])
}

pub(super) fn skip_messagepack_value(
    bytes: &[u8],
    cursor: &mut usize,
) -> Result<(), SnapshotDecodeError> {
    let marker = take_byte(bytes, cursor)?;
    match marker {
        0x00..=0x7f | 0xe0..=0xff | 0xc0 | 0xc2 | 0xc3 => Ok(()),
        0xcc | 0xd0 => skip_bytes(bytes, cursor, 1),
        0xcd | 0xd1 => skip_bytes(bytes, cursor, 2),
        0xce | 0xd2 | 0xca => skip_bytes(bytes, cursor, 4),
        0xcf | 0xd3 | 0xcb => skip_bytes(bytes, cursor, 8),
        0xa0..=0xbf => skip_bytes(bytes, cursor, usize::from(marker & 0x1f)),
        0xd9 | 0xc4 => {
            let length = usize::from(take_byte(bytes, cursor)?);
            skip_bytes(bytes, cursor, length)
        }
        0xda | 0xc5 => {
            let length = usize::from(take_u16(bytes, cursor)?);
            skip_bytes(bytes, cursor, length)
        }
        0xdb | 0xc6 => {
            let length = usize_from_u32(take_u32(bytes, cursor)?)?;
            skip_bytes(bytes, cursor, length)
        }
        0x90..=0x9f => {
            for _ in 0..usize::from(marker & 0x0f) {
                skip_messagepack_value(bytes, cursor)?;
            }
            Ok(())
        }
        0xdc => {
            let length = usize::from(take_u16(bytes, cursor)?);
            for _ in 0..length {
                skip_messagepack_value(bytes, cursor)?;
            }
            Ok(())
        }
        0xdd => {
            let length = usize_from_u32(take_u32(bytes, cursor)?)?;
            for _ in 0..length {
                skip_messagepack_value(bytes, cursor)?;
            }
            Ok(())
        }
        0x80..=0x8f => {
            for _ in 0..usize::from(marker & 0x0f) {
                skip_messagepack_value(bytes, cursor)?;
                skip_messagepack_value(bytes, cursor)?;
            }
            Ok(())
        }
        0xde => {
            let length = usize::from(take_u16(bytes, cursor)?);
            for _ in 0..length {
                skip_messagepack_value(bytes, cursor)?;
                skip_messagepack_value(bytes, cursor)?;
            }
            Ok(())
        }
        0xdf => {
            let length = usize_from_u32(take_u32(bytes, cursor)?)?;
            for _ in 0..length {
                skip_messagepack_value(bytes, cursor)?;
                skip_messagepack_value(bytes, cursor)?;
            }
            Ok(())
        }
        _ => Err(invalid_messagepack(&format!(
            "unsupported MessagePack marker 0x{marker:02x}"
        ))),
    }
}

pub(super) fn usize_from_u32(value: u32) -> Result<usize, SnapshotDecodeError> {
    usize::try_from(value).map_err(|_| invalid_messagepack("length does not fit usize"))
}

pub(super) fn invalid_messagepack(message: &str) -> SnapshotDecodeError {
    SnapshotDecodeError::InvalidEncoding(message.to_string())
}

pub(super) fn invalid_at(location: &str, message: &str) -> SnapshotDecodeError {
    invalid_messagepack(&format!("at `{location}`: {message}"))
}

pub(super) fn unexpected_marker(location: &str, expected: &str, marker: u8) -> SnapshotDecodeError {
    invalid_at(
        location,
        &format!("expected {expected}, found marker 0x{marker:02x}"),
    )
}

pub(super) fn non_canonical(location: &str, reason: &str) -> SnapshotDecodeError {
    SnapshotDecodeError::NonCanonicalEncoding {
        location: location.to_string(),
        reason: reason.to_string(),
    }
}

pub(super) fn expect_key(
    bytes: &[u8],
    cursor: &mut usize,
    expected: &str,
    location: &str,
) -> Result<(), SnapshotDecodeError> {
    let found = take_canonical_string(bytes, cursor, location)?;
    if found != expected {
        return Err(non_canonical(
            location,
            &format!(
                "struct fields must use canonical order; expected `{expected}`, found `{found}`"
            ),
        ));
    }
    Ok(())
}

pub(super) fn take_canonical_string<'a>(
    bytes: &'a [u8],
    cursor: &mut usize,
    location: &str,
) -> Result<&'a str, SnapshotDecodeError> {
    let marker = take_byte(bytes, cursor)?;
    let length = match marker {
        0xa0..=0xbf => usize::from(marker & 0x1f),
        0xd9 => {
            let length = usize::from(take_byte(bytes, cursor)?);
            if length <= 31 {
                return Err(non_canonical(
                    location,
                    "string length is not minimally encoded",
                ));
            }
            length
        }
        0xda => {
            let length = usize::from(take_u16(bytes, cursor)?);
            if length <= usize::from(u8::MAX) {
                return Err(non_canonical(
                    location,
                    "string length is not minimally encoded",
                ));
            }
            length
        }
        0xdb => {
            let length = usize_from_u32(take_u32(bytes, cursor)?)?;
            if length <= usize::from(u16::MAX) {
                return Err(non_canonical(
                    location,
                    "string length is not minimally encoded",
                ));
            }
            length
        }
        _ => return Err(unexpected_marker(location, "a string", marker)),
    };
    let value = take_slice(bytes, cursor, length)?;
    std::str::from_utf8(value).map_err(|_| invalid_at(location, "string is not valid UTF-8"))
}

pub(super) fn validate_f64(
    bytes: &[u8],
    cursor: &mut usize,
    location: &str,
) -> Result<(), SnapshotDecodeError> {
    let marker = take_byte(bytes, cursor)?;
    if marker == 0xcb {
        let bits = u64::from_be_bytes(take_array::<8>(bytes, cursor)?);
        let value = f64::from_bits(bits);
        if value.is_nan() && bits != CANONICAL_NAN_BITS {
            return Err(non_canonical(
                location,
                "NaN must use the canonical bit pattern",
            ));
        }
        return Ok(());
    }
    if marker == 0xca || is_integer_marker(marker) {
        return Err(non_canonical(
            location,
            "runtime number must use f64 encoding",
        ));
    }
    Err(unexpected_marker(location, "an f64", marker))
}

pub(super) fn validate_json_number(
    bytes: &[u8],
    cursor: &mut usize,
    location: &str,
) -> Result<(), SnapshotDecodeError> {
    let marker = take_byte(bytes, cursor)?;
    if marker == 0xcb {
        let value = f64::from_bits(u64::from_be_bytes(take_array::<8>(bytes, cursor)?));
        if !value.is_finite() {
            return Err(invalid_at(
                location,
                "projection JSON number must be finite",
            ));
        }
        return Ok(());
    }
    if marker == 0xca {
        return Err(non_canonical(
            location,
            "floating-point number must use f64 encoding",
        ));
    }
    take_canonical_integer(bytes, cursor, location, marker).map(|_| ())
}

pub(super) fn validate_unsigned(
    bytes: &[u8],
    cursor: &mut usize,
    location: &str,
    maximum: u64,
) -> Result<(), SnapshotDecodeError> {
    let marker = take_byte(bytes, cursor)?;
    let value = take_canonical_integer(bytes, cursor, location, marker)?;
    if value < 0 || value > i128::from(maximum) {
        return Err(invalid_at(location, "unsigned integer is out of range"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn canonical_integer_widths_preserve_signed_values_and_refuse_wider_forms() {
        for value in [
            -2_147_483_649i64,
            -2_147_483_648,
            -32_769,
            -32_768,
            -129,
            -128,
            -33,
            -32,
            -1,
            0,
            127,
            128,
        ] {
            let bytes = rmp_serde::to_vec(&value).expect("encode integer");
            let mut cursor = 1;
            assert_eq!(
                take_canonical_integer(&bytes, &mut cursor, "value", bytes[0])
                    .expect("minimal integer"),
                i128::from(value)
            );
            assert_eq!(cursor, bytes.len());
        }
        for bytes in [
            vec![0xd0, 0xe0],
            vec![0xd1, 0xff, 0x80],
            vec![0xd2, 0xff, 0xff, 0x80, 0x00],
            vec![0xd3, 0xff, 0xff, 0xff, 0xff, 0x80, 0x00, 0x00, 0x00],
        ] {
            assert!(matches!(
                take_canonical_integer(&bytes, &mut 1, "value", bytes[0]),
                Err(SnapshotDecodeError::NonCanonicalEncoding { .. })
            ));
        }
    }

    #[test]
    fn canonical_strings_accept_minimal_large_widths_and_refuse_small_wide_strings() {
        for length in [0, 31, 32, 255, 256, 65_535, 65_536] {
            let text = "x".repeat(length);
            let bytes = rmp_serde::to_vec(&text).expect("encode string");
            let mut cursor = 0;
            assert_eq!(
                take_canonical_string(&bytes, &mut cursor, "text").expect("minimal string"),
                text
            );
            assert_eq!(cursor, bytes.len());
        }
        for bytes in [vec![0xda, 0, 1, b'x'], vec![0xdb, 0, 0, 0, 1, b'x']] {
            assert!(matches!(
                take_canonical_string(&bytes, &mut 0, "text"),
                Err(SnapshotDecodeError::NonCanonicalEncoding { .. })
            ));
        }
        assert_eq!(usize_from_u32(65_536).expect("length fits"), 65_536);
    }

    #[test]
    fn numeric_validation_preserves_f64_and_unsigned_range_boundaries() {
        for value in [0.0, -1.5, f64::INFINITY, f64::from_bits(CANONICAL_NAN_BITS)] {
            let bytes = rmp_serde::to_vec(&value).expect("encode f64");
            assert_eq!(validate_f64(&bytes, &mut 0, "number"), Ok(()));
            assert_eq!(
                validate_json_number(&bytes, &mut 0, "number").is_ok(),
                value.is_finite()
            );
        }
        assert!(matches!(
            validate_f64(&[0xc0], &mut 0, "number"),
            Err(SnapshotDecodeError::InvalidEncoding(_))
        ));
        for (value, valid) in [
            (-1i64, false),
            (0, true),
            (1, true),
            (255, true),
            (256, false),
        ] {
            let bytes = rmp_serde::to_vec(&value).expect("encode integer");
            assert_eq!(
                validate_unsigned(&bytes, &mut 0, "byte", 255).is_ok(),
                valid,
                "{value}"
            );
        }
    }
}
