use super::*;

/// Field-order policy for the shared canonical MessagePack pre-pass.
///
/// This is public only so the RLM persistence envelope can use the same raw
/// parser as Lashlang snapshots; it is not a general serialization API.
#[doc(hidden)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CanonicalMapOrder {
    /// Require only canonical key encodings and unique string keys.
    Unordered,
    /// Permit declared fields in any order while rejecting unknown fields.
    Fields(&'static [&'static str]),
    /// Require lexicographically sorted, strictly unique string keys.
    Sorted,
    /// Require keys to follow their declaration order. Optional omitted fields
    /// are permitted, but unknown or reordered fields are rejected.
    Declared(&'static [&'static str]),
}

/// Validate arbitrary MessagePack with the canonical scalar/length rules and
/// Lash-owned nesting guard used by snapshot decoding.
///
/// `map_order` classifies maps after their marker is seen. `map_required`
/// identifies serde struct/map locations where sequence-form input must be
/// rejected before deserialization.
#[doc(hidden)]
pub fn validate_canonical_messagepack_structure(
    bytes: &[u8],
    root_location: &str,
    max_depth: usize,
    map_order: impl Fn(&str) -> CanonicalMapOrder,
    map_required: impl Fn(&str) -> bool,
) -> Result<(), SnapshotDecodeError> {
    let mut cursor = 0;
    validate_arbitrary_messagepack_value(
        bytes,
        &mut cursor,
        root_location,
        1,
        max_depth,
        &map_order,
        &map_required,
    )?;
    if cursor != bytes.len() {
        return Err(invalid_messagepack("trailing bytes"));
    }
    Ok(())
}

fn validate_arbitrary_messagepack_value(
    bytes: &[u8],
    cursor: &mut usize,
    location: &str,
    depth: usize,
    max_depth: usize,
    map_order: &impl Fn(&str) -> CanonicalMapOrder,
    map_required: &impl Fn(&str) -> bool,
) -> Result<(), SnapshotDecodeError> {
    if depth > max_depth {
        return Err(SnapshotDecodeError::DepthLimitExceeded { limit: max_depth });
    }
    let marker = *bytes
        .get(*cursor)
        .ok_or_else(|| invalid_messagepack("unexpected end of input"))?;
    let is_map = matches!(marker, 0x80..=0x8f | 0xde | 0xdf);
    if map_required(location) && !is_map {
        return Err(non_canonical(
            location,
            "structs and dynamic maps must use map form",
        ));
    }

    match marker {
        0x00..=0x7f | 0xe0..=0xff | 0xc0 | 0xc2 | 0xc3 => {
            *cursor += 1;
        }
        0xcc..=0xd3 => {
            *cursor += 1;
            take_canonical_integer(bytes, cursor, location, marker)?;
        }
        0xca => {
            return Err(non_canonical(
                location,
                "floating-point values must use the canonical 64-bit width",
            ));
        }
        0xcb => validate_f64(bytes, cursor, location)?,
        0xa0..=0xbf | 0xd9..=0xdb => {
            take_canonical_string(bytes, cursor, location)?;
        }
        0xc4..=0xc6 => validate_canonical_binary(bytes, cursor, location)?,
        0x90..=0x9f | 0xdc | 0xdd => {
            let length = take_array_length(bytes, cursor, location)?;
            for index in 0..length {
                validate_arbitrary_messagepack_value(
                    bytes,
                    cursor,
                    &format!("{location}[{index}]"),
                    depth + 1,
                    max_depth,
                    map_order,
                    map_required,
                )?;
            }
        }
        0x80..=0x8f | 0xde | 0xdf => {
            let length = take_map_length(bytes, cursor, location, "map")?;
            let order = map_order(location);
            let mut previous_key: Option<String> = None;
            let mut previous_declaration = None;
            let mut keys = std::collections::BTreeSet::new();
            for _ in 0..length {
                let key = take_canonical_string(bytes, cursor, location)?.to_string();
                if !keys.insert(key.clone()) {
                    return Err(non_canonical(
                        location,
                        &format!("map contains duplicate key `{key}`"),
                    ));
                }
                match order {
                    CanonicalMapOrder::Unordered => {}
                    CanonicalMapOrder::Fields(fields) => {
                        if !fields.contains(&key.as_str()) {
                            return Err(non_canonical(
                                location,
                                &format!("map contains unknown field `{key}`"),
                            ));
                        }
                    }
                    CanonicalMapOrder::Sorted => {
                        if let Some(previous) = previous_key.as_deref()
                            && previous >= key.as_str()
                        {
                            return Err(non_canonical(
                                location,
                                &format!(
                                    "map key `{key}` is not strictly greater than `{previous}`"
                                ),
                            ));
                        }
                    }
                    CanonicalMapOrder::Declared(fields) => {
                        let declaration = fields
                            .iter()
                            .position(|field| *field == key)
                            .ok_or_else(|| {
                                non_canonical(
                                    location,
                                    &format!("map contains unknown field `{key}`"),
                                )
                            })?;
                        if previous_declaration.is_some_and(|previous| previous >= declaration) {
                            return Err(non_canonical(
                                location,
                                &format!("field `{key}` is not in canonical declaration order"),
                            ));
                        }
                        previous_declaration = Some(declaration);
                    }
                }
                let child = child_location(location, &key);
                validate_arbitrary_messagepack_value(
                    bytes,
                    cursor,
                    &child,
                    depth + 1,
                    max_depth,
                    map_order,
                    map_required,
                )?;
                previous_key = Some(key);
            }
        }
        _ => {
            return Err(unexpected_marker(
                location,
                "a canonical scalar, array, or map",
                marker,
            ));
        }
    }
    Ok(())
}

fn validate_canonical_binary(
    bytes: &[u8],
    cursor: &mut usize,
    location: &str,
) -> Result<(), SnapshotDecodeError> {
    let marker = take_byte(bytes, cursor)?;
    let length = match marker {
        0xc4 => usize::from(take_byte(bytes, cursor)?),
        0xc5 => {
            let length = usize::from(take_u16(bytes, cursor)?);
            if length <= usize::from(u8::MAX) {
                return Err(non_canonical(location, "binary length is not minimal"));
            }
            length
        }
        0xc6 => {
            let length = usize_from_u32(take_u32(bytes, cursor)?)?;
            if length <= usize::from(u16::MAX) {
                return Err(non_canonical(location, "binary length is not minimal"));
            }
            length
        }
        _ => unreachable!("caller checked binary marker"),
    };
    let end = cursor
        .checked_add(length)
        .ok_or_else(|| invalid_messagepack("binary length overflow"))?;
    if end > bytes.len() {
        return Err(invalid_messagepack("unexpected end of binary body"));
    }
    *cursor = end;
    Ok(())
}
