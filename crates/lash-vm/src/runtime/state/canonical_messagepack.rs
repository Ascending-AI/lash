use super::*;

/// Field-order policy for the shared canonical MessagePack pre-pass.
///
/// This is public only so the RLM persistence envelope can use the same raw
/// parser as Lash VM snapshots; it is not a general serialization API.
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

/// A structural step from the root of a canonical MessagePack value.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CanonicalPathSegment {
    /// The value of a map entry, independent of how its key is displayed.
    Key(String),
    /// An array element.
    Index(usize),
}

impl CanonicalPathSegment {
    pub fn key(&self) -> Option<&str> {
        match self {
            Self::Key(key) => Some(key),
            Self::Index(_) => None,
        }
    }
}

/// Validate arbitrary MessagePack with the canonical scalar/length rules and
/// Lash-owned nesting guard used by snapshot decoding.
///
/// `map_order` classifies maps after their marker is seen. `map_required`
/// identifies serde struct/map locations where sequence-form input must be
/// rejected before deserialization.
pub fn validate_canonical_messagepack_structure(
    bytes: &[u8],
    root_location: &str,
    max_depth: usize,
    map_order: impl Fn(&[CanonicalPathSegment]) -> CanonicalMapOrder,
    map_required: impl Fn(&[CanonicalPathSegment]) -> bool,
) -> Result<(), SnapshotDecodeError> {
    let mut cursor = 0;
    validate_arbitrary_messagepack_value(
        bytes,
        &mut cursor,
        (root_location, &mut Vec::new()),
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
    position: (&str, &mut Vec<CanonicalPathSegment>),
    depth: usize,
    max_depth: usize,
    map_order: &impl Fn(&[CanonicalPathSegment]) -> CanonicalMapOrder,
    map_required: &impl Fn(&[CanonicalPathSegment]) -> bool,
) -> Result<(), SnapshotDecodeError> {
    let (location, path) = position;
    if depth > max_depth {
        return Err(SnapshotDecodeError::DepthLimitExceeded { limit: max_depth });
    }
    let marker = *bytes
        .get(*cursor)
        .ok_or_else(|| invalid_messagepack("unexpected end of input"))?;
    let is_map = matches!(marker, 0x80..=0x8f | 0xde | 0xdf);
    if map_required(path) && !is_map {
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
                path.push(CanonicalPathSegment::Index(index));
                validate_arbitrary_messagepack_value(
                    bytes,
                    cursor,
                    (&format!("{location}[{index}]"), path),
                    depth + 1,
                    max_depth,
                    map_order,
                    map_required,
                )?;
                path.pop();
            }
        }
        0x80..=0x8f | 0xde | 0xdf => {
            let length = take_map_length(bytes, cursor, location, "map")?;
            let order = map_order(path);
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
                path.push(CanonicalPathSegment::Key(key.clone()));
                validate_arbitrary_messagepack_value(
                    bytes,
                    cursor,
                    (&child, path),
                    depth + 1,
                    max_depth,
                    map_order,
                    map_required,
                )?;
                path.pop();
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

#[cfg(test)]
mod tests {
    use super::*;

    fn validate(bytes: &[u8], depth: usize) -> Result<(), SnapshotDecodeError> {
        validate_canonical_messagepack_structure(
            bytes,
            "root",
            depth,
            |_| CanonicalMapOrder::Sorted,
            |_| false,
        )
    }

    #[test]
    fn canonical_structure_checks_depth_map_order_and_field_paths() {
        assert_eq!(validate(&[0x91, 0xc0], 2), Ok(()));
        assert!(matches!(
            validate(&[0x91, 0x91, 0xc0], 2),
            Err(SnapshotDecodeError::DepthLimitExceeded { limit: 2 })
        ));
        let ordered = [0x82, 0xa1, b'a', 0xc0, 0xa1, b'b', 0xc0];
        assert_eq!(validate(&ordered, 2), Ok(()));
        let reversed = [0x82, 0xa1, b'b', 0xc0, 0xa1, b'a', 0xc0];
        assert!(matches!(
            validate(&reversed, 2),
            Err(SnapshotDecodeError::NonCanonicalEncoding { .. })
        ));
        assert_eq!(
            CanonicalPathSegment::Key("field".into()).key(),
            Some("field")
        );
        assert_eq!(CanonicalPathSegment::Index(1).key(), None);
        assert!(
            validate_canonical_messagepack_structure(
                &[0x91, 0xc0],
                "root",
                2,
                |_| CanonicalMapOrder::Unordered,
                |path| path.is_empty()
            )
            .is_err()
        );
        let map = [0x81, 0xa1, b'a', 0x91, 0xc0];
        assert!(matches!(
            validate(&map, 2),
            Err(SnapshotDecodeError::DepthLimitExceeded { limit: 2 })
        ));
        let seen = std::cell::RefCell::new(Vec::new());
        assert_eq!(
            validate_canonical_messagepack_structure(
                &map,
                "root",
                3,
                |path| {
                    seen.borrow_mut().push(path.to_vec());
                    CanonicalMapOrder::Fields(&["a"])
                },
                |_| false
            ),
            Ok(())
        );
        assert_eq!(*seen.borrow(), vec![Vec::<CanonicalPathSegment>::new()]);
        assert!(matches!(
            validate(&[0xca, 0, 0, 0, 0], 1),
            Err(SnapshotDecodeError::NonCanonicalEncoding { .. })
        ));

        assert_eq!(
            validate(&[0x92, 0xc4, 1, 0, 0xc4, 1, 1], 2),
            Ok(()),
            "binary bodies can end before the enclosing array ends"
        );
        for length in [0usize, 1, 255, 256, 65_535, 65_536] {
            let mut bytes = if length <= 255 {
                vec![0xc4, length as u8]
            } else if length <= 65_535 {
                let mut bytes = vec![0xc5];
                bytes.extend_from_slice(&(length as u16).to_be_bytes());
                bytes
            } else {
                let mut bytes = vec![0xc6];
                bytes.extend_from_slice(&(length as u32).to_be_bytes());
                bytes
            };
            bytes.resize(bytes.len() + length, 0);
            assert_eq!(validate(&bytes, 1), Ok(()), "length {length}");
            if length > 0 {
                bytes.pop();
                assert!(
                    matches!(
                        validate(&bytes, 1),
                        Err(SnapshotDecodeError::InvalidEncoding(_))
                    ),
                    "length {length}"
                );
            }
        }
        for bytes in [vec![0xc5, 0, 1, 0], vec![0xc6, 0, 0, 0, 1, 0]] {
            assert!(matches!(
                validate(&bytes, 1),
                Err(SnapshotDecodeError::NonCanonicalEncoding { .. })
            ));
        }
    }
}
