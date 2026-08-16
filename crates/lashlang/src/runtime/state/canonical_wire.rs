fn take_canonical_integer(
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

fn take_byte(bytes: &[u8], cursor: &mut usize) -> Result<u8, SnapshotDecodeError> {
    let byte = bytes
        .get(*cursor)
        .copied()
        .ok_or_else(|| invalid_messagepack("unexpected end of input"))?;
    *cursor += 1;
    Ok(byte)
}

fn take_u16(bytes: &[u8], cursor: &mut usize) -> Result<u16, SnapshotDecodeError> {
    let value = take_array::<2>(bytes, cursor)?;
    Ok(u16::from_be_bytes(value))
}

fn take_u32(bytes: &[u8], cursor: &mut usize) -> Result<u32, SnapshotDecodeError> {
    let value = take_array::<4>(bytes, cursor)?;
    Ok(u32::from_be_bytes(value))
}

fn take_u64(bytes: &[u8], cursor: &mut usize) -> Result<u64, SnapshotDecodeError> {
    let value = take_array::<8>(bytes, cursor)?;
    Ok(u64::from_be_bytes(value))
}

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

fn skip_bytes(bytes: &[u8], cursor: &mut usize, length: usize) -> Result<(), SnapshotDecodeError> {
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

fn usize_from_u32(value: u32) -> Result<usize, SnapshotDecodeError> {
    usize::try_from(value).map_err(|_| invalid_messagepack("length does not fit usize"))
}

fn invalid_messagepack(message: &str) -> SnapshotDecodeError {
    SnapshotDecodeError::InvalidEncoding(message.to_string())
}

fn invalid_at(location: &str, message: &str) -> SnapshotDecodeError {
    invalid_messagepack(&format!("at `{location}`: {message}"))
}

fn unexpected_marker(location: &str, expected: &str, marker: u8) -> SnapshotDecodeError {
    invalid_at(
        location,
        &format!("expected {expected}, found marker 0x{marker:02x}"),
    )
}

fn non_canonical(location: &str, reason: &str) -> SnapshotDecodeError {
    SnapshotDecodeError::NonCanonicalEncoding {
        location: location.to_string(),
        reason: reason.to_string(),
    }
}
