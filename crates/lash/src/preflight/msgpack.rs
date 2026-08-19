//! A MessagePack *header* reader: enough to find a version integer, and
//! deliberately not enough to decode anything.
//!
//! Two of the durable formats a preflight has to report — the session
//! checkpoint manifest and the RLM snapshot envelope — are stored as
//! MessagePack. A probe cannot decode them into their real types: those types
//! belong to builds that fail closed, so a typed decode of state written by
//! another build is exactly the refusal the probe exists to predict, and a
//! probe that produced it would have crashed on the data it was asked to
//! describe. A probe also cannot decode them into a generic value tree:
//! `serde_json::Value` has no binary type, and these envelopes carry binary
//! bodies.
//!
//! So this walks the encoding's framing instead. It reads map keys, follows the
//! one it was asked for, and skips every other value without interpreting it —
//! which means an envelope full of shapes this build has never seen still
//! yields its version, and an envelope of pure garbage yields `None` rather
//! than a panic. Nothing here allocates a value, recurses without a bound, or
//! indexes without a checked slice.
//!
//! The one thing it deliberately does not do is validate. A reader that
//! insisted on canonical encoding would reject stored bytes for reasons that
//! have nothing to do with the question being asked.

/// How deep a value this reader will walk before giving up.
///
/// A malicious or corrupt envelope can claim nesting forever; a bounded reader
/// answers `None` instead of exhausting the stack. The bound is far above any
/// real envelope's nesting, so it never turns a healthy store into an
/// undecodable one.
const MAX_DEPTH: usize = 64;

/// Read one byte-length prefix of `len` bytes as a big-endian integer.
fn take<'a>(data: &'a [u8], offset: &mut usize, len: usize) -> Option<&'a [u8]> {
    let end = offset.checked_add(len)?;
    let value = data.get(*offset..end)?;
    *offset = end;
    Some(value)
}

fn take_u8(data: &[u8], offset: &mut usize) -> Option<u8> {
    take(data, offset, 1).map(|bytes| bytes[0])
}

fn take_be(data: &[u8], offset: &mut usize, len: usize) -> Option<u64> {
    let bytes = take(data, offset, len)?;
    Some(
        bytes
            .iter()
            .fold(0u64, |acc, byte| (acc << 8) | u64::from(*byte)),
    )
}

/// How many elements or bytes the value starting at `offset` holds, and what
/// kind of value it is.
enum Header {
    /// A value whose body is `len` raw bytes: strings, binaries, floats, ints.
    Bytes(usize),
    /// An array of `len` values.
    Array(usize),
    /// A map of `len` key/value pairs.
    Map(usize),
    /// A value fully described by its marker: nil, booleans, fixints.
    Empty,
}

/// Read one value's header, leaving `offset` positioned at its body.
fn header(data: &[u8], offset: &mut usize) -> Option<Header> {
    let marker = take_u8(data, offset)?;
    Some(match marker {
        // Positive and negative fixints carry their value in the marker.
        0x00..=0x7f | 0xe0..=0xff => Header::Empty,
        0x80..=0x8f => Header::Map(usize::from(marker & 0x0f)),
        0x90..=0x9f => Header::Array(usize::from(marker & 0x0f)),
        0xa0..=0xbf => Header::Bytes(usize::from(marker & 0x1f)),
        0xc0 | 0xc2 | 0xc3 => Header::Empty,
        // 0xc1 is never a valid marker; treating it as unreadable is the whole
        // contract of this module.
        0xc1 => return None,
        0xc4 | 0xd9 => Header::Bytes(usize::try_from(take_be(data, offset, 1)?).ok()?),
        0xc5 | 0xda => Header::Bytes(usize::try_from(take_be(data, offset, 2)?).ok()?),
        0xc6 | 0xdb => Header::Bytes(usize::try_from(take_be(data, offset, 4)?).ok()?),
        // Extension types carry a one-byte type tag after their length.
        0xc7 => Header::Bytes(
            usize::try_from(take_be(data, offset, 1)?)
                .ok()?
                .checked_add(1)?,
        ),
        0xc8 => Header::Bytes(
            usize::try_from(take_be(data, offset, 2)?)
                .ok()?
                .checked_add(1)?,
        ),
        0xc9 => Header::Bytes(
            usize::try_from(take_be(data, offset, 4)?)
                .ok()?
                .checked_add(1)?,
        ),
        0xca => Header::Bytes(4),
        0xcb => Header::Bytes(8),
        0xcc | 0xd0 => Header::Bytes(1),
        0xcd | 0xd1 => Header::Bytes(2),
        0xce | 0xd2 => Header::Bytes(4),
        0xcf | 0xd3 => Header::Bytes(8),
        0xd4 => Header::Bytes(2),
        0xd5 => Header::Bytes(3),
        0xd6 => Header::Bytes(5),
        0xd7 => Header::Bytes(9),
        0xd8 => Header::Bytes(17),
        0xdc => Header::Array(usize::try_from(take_be(data, offset, 2)?).ok()?),
        0xdd => Header::Array(usize::try_from(take_be(data, offset, 4)?).ok()?),
        0xde => Header::Map(usize::try_from(take_be(data, offset, 2)?).ok()?),
        0xdf => Header::Map(usize::try_from(take_be(data, offset, 4)?).ok()?),
    })
}

/// Advance `offset` past exactly one value.
fn skip(data: &[u8], offset: &mut usize, depth: usize) -> Option<()> {
    if depth > MAX_DEPTH {
        return None;
    }
    match header(data, offset)? {
        Header::Empty => Some(()),
        Header::Bytes(len) => take(data, offset, len).map(|_| ()),
        Header::Array(len) => {
            for _ in 0..len {
                skip(data, offset, depth + 1)?;
            }
            Some(())
        }
        Header::Map(len) => {
            for _ in 0..len {
                skip(data, offset, depth + 1)?;
                skip(data, offset, depth + 1)?;
            }
            Some(())
        }
    }
}

/// Read a string value's bytes, or `None` when the value is not a string.
fn read_str<'a>(data: &'a [u8], offset: &mut usize) -> Option<&'a str> {
    let marker = *data.get(*offset)?;
    if !matches!(marker, 0xa0..=0xbf | 0xd9 | 0xda | 0xdb) {
        return None;
    }
    let Header::Bytes(len) = header(data, offset)? else {
        return None;
    };
    std::str::from_utf8(take(data, offset, len)?).ok()
}

/// A cursor into one value inside an envelope.
#[derive(Clone, Copy, Debug)]
pub(super) struct Value<'a> {
    data: &'a [u8],
    at: usize,
}

impl<'a> Value<'a> {
    /// The whole envelope, as one value.
    pub(super) fn root(data: &'a [u8]) -> Self {
        Self { data, at: 0 }
    }

    /// The value stored under `key`, when this value is a map that has one.
    ///
    /// Keys are compared as strings; a map keyed by anything else is simply a
    /// map without the key, which is the right answer for a probe that is
    /// looking for a named version field.
    pub(super) fn field(self, key: &str) -> Option<Value<'a>> {
        let mut offset = self.at;
        let Header::Map(len) = header(self.data, &mut offset)? else {
            return None;
        };
        for _ in 0..len {
            let found = read_str(self.data, &mut offset);
            if found.is_none() {
                // A non-string key still has to be stepped over, or the walk
                // would desynchronise and start reading a value as a key.
                skip(self.data, &mut offset, 0)?;
            }
            if found == Some(key) {
                return Some(Value {
                    data: self.data,
                    at: offset,
                });
            }
            skip(self.data, &mut offset, 0)?;
        }
        None
    }

    /// Every key/value pair, when this value is a map with string keys.
    ///
    /// Pairs whose key is not a string are skipped rather than failing the
    /// whole read: an envelope this build does not understand should still
    /// yield the entries it does.
    pub(super) fn entries(self) -> Option<Vec<(&'a str, Value<'a>)>> {
        let mut offset = self.at;
        let Header::Map(len) = header(self.data, &mut offset)? else {
            return None;
        };
        let mut entries = Vec::new();
        for _ in 0..len {
            let key = read_str(self.data, &mut offset);
            if key.is_none() {
                skip(self.data, &mut offset, 0)?;
            }
            let value = Value {
                data: self.data,
                at: offset,
            };
            if let Some(key) = key {
                entries.push((key, value));
            }
            skip(self.data, &mut offset, 0)?;
        }
        Some(entries)
    }

    /// This value as an unsigned 32-bit integer, when it is one.
    pub(super) fn as_u32(self) -> Option<u32> {
        let mut offset = self.at;
        let marker = take_u8(self.data, &mut offset)?;
        let value = match marker {
            0x00..=0x7f => u64::from(marker),
            0xcc => take_be(self.data, &mut offset, 1)?,
            0xcd => take_be(self.data, &mut offset, 2)?,
            0xce => take_be(self.data, &mut offset, 4)?,
            0xcf => take_be(self.data, &mut offset, 8)?,
            _ => return None,
        };
        u32::try_from(value).ok()
    }

    /// This value as a string, when it is one.
    ///
    /// Test-only: no format this build probes carries its version as a string,
    /// but the walker still has to *skip* strings correctly to reach the fields
    /// that follow one, and reading one back is how that is proven.
    #[cfg(test)]
    pub(super) fn as_str(self) -> Option<&'a str> {
        let mut offset = self.at;
        read_str(self.data, &mut offset)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The shape a checkpoint manifest has: a named map whose `components`
    /// entry is a map of descriptors.
    fn manifest() -> Vec<u8> {
        let value = serde_json::json!({
            "schema_version": 2u32,
            "turn_state": {"nested": [1, 2, {"deep": true}], "text": "x"},
            "components": {
                "execution_state": {"blob_ref": "sha256:abc", "encoding_version": 2u32},
                "tool_state": {"blob_ref": "sha256:def", "encoding_version": 2u32},
            },
            "plugin_snapshot_revision": 7u32,
        });
        rmp_serde::to_vec_named(&value).expect("the fixture encodes")
    }

    #[test]
    fn a_version_is_read_past_every_shape_that_precedes_it() {
        // The property that matters: the version field is found without
        // decoding, or even understanding, the values around it.
        let bytes = manifest();
        assert_eq!(
            Value::root(&bytes)
                .field("schema_version")
                .and_then(Value::as_u32),
            Some(2)
        );
        assert_eq!(
            Value::root(&bytes)
                .field("plugin_snapshot_revision")
                .and_then(Value::as_u32),
            Some(7),
            "a field after a deeply nested one is still reachable"
        );
    }

    #[test]
    fn every_component_descriptor_yields_its_encoding_version() {
        let bytes = manifest();
        let components = Value::root(&bytes)
            .field("components")
            .and_then(Value::entries)
            .expect("the manifest carries components");
        let mut versions: Vec<(&str, Option<u32>)> = components
            .into_iter()
            .map(|(key, value)| (key, value.field("encoding_version").and_then(Value::as_u32)))
            .collect();
        versions.sort();
        assert_eq!(
            versions,
            vec![("execution_state", Some(2)), ("tool_state", Some(2))]
        );
    }

    #[test]
    fn a_named_blob_reference_is_readable_as_a_string() {
        let bytes = manifest();
        let blob_ref = Value::root(&bytes)
            .field("components")
            .and_then(|components| components.field("execution_state"))
            .and_then(|component| component.field("blob_ref"))
            .and_then(Value::as_str);
        assert_eq!(blob_ref, Some("sha256:abc"));
    }

    #[test]
    fn binary_bodies_do_not_stop_the_walk() {
        // The reason a generic value tree is not an option: these envelopes
        // carry binary, which `serde_json::Value` cannot hold.
        #[derive(serde::Serialize)]
        struct Envelope {
            version: u32,
            #[serde(with = "serde_bytes")]
            body: Vec<u8>,
            engine: String,
        }
        let bytes = rmp_serde::to_vec_named(&Envelope {
            version: 13,
            body: vec![0xff; 300],
            engine: "rlm".to_string(),
        })
        .expect("the fixture encodes");
        assert_eq!(
            Value::root(&bytes).field("version").and_then(Value::as_u32),
            Some(13)
        );
        assert_eq!(
            Value::root(&bytes).field("engine").and_then(Value::as_str),
            Some("rlm")
        );
    }

    #[test]
    fn garbage_reads_as_absent_rather_than_panicking() {
        // The probe's central promise: it never panics on the data it warns
        // about. Truncation is the case a real corrupt store produces.
        let bytes = manifest();
        for cut in 1..bytes.len() {
            let truncated = &bytes[..cut];
            let _ = Value::root(truncated)
                .field("schema_version")
                .and_then(Value::as_u32);
            let _ = Value::root(truncated)
                .field("components")
                .map(Value::entries);
        }
        for junk in [
            b"not messagepack at all".as_slice(),
            &[0xc1],
            &[0xdf, 0xff, 0xff, 0xff, 0xff],
            &[],
        ] {
            assert_eq!(
                Value::root(junk)
                    .field("schema_version")
                    .and_then(Value::as_u32),
                None
            );
        }
    }

    #[test]
    fn a_deeply_nested_envelope_is_bounded_rather_than_unbounded() {
        // The fixture has to be a *map*, and the bound has to be what stops the
        // lookup. A non-map root would return `None` from `field` for the
        // ordinary reason, and the test would pass with the depth bound
        // deleted. Here the nested value sits in front of the field being read,
        // so reaching `version` means skipping past the bound.
        let mut bytes = vec![0x82u8]; // two-entry map
        bytes.push(0xa4); // "deep"
        bytes.extend_from_slice(b"deep");
        bytes.extend(std::iter::repeat_n(0x91u8, MAX_DEPTH * 2)); // nested arrays
        bytes.push(0xc0); // the innermost nil
        bytes.push(0xa7); // "version"
        bytes.extend_from_slice(b"version");
        bytes.push(0x07);

        // No stack exhaustion, no panic: an envelope past the bound is simply
        // unreadable.
        assert_eq!(
            Value::root(&bytes).field("version").and_then(Value::as_u32),
            None
        );

        // The same bytes with a shallow value are read, so the bound is what
        // decided the case above and not a malformed fixture.
        let mut shallow = vec![0x82u8];
        shallow.push(0xa4);
        shallow.extend_from_slice(b"deep");
        shallow.push(0xc0);
        shallow.push(0xa7);
        shallow.extend_from_slice(b"version");
        shallow.push(0x07);
        assert_eq!(
            Value::root(&shallow)
                .field("version")
                .and_then(Value::as_u32),
            Some(7)
        );
    }

    #[test]
    fn a_non_map_root_has_no_fields() {
        let bytes = rmp_serde::to_vec_named(&vec![1u32, 2, 3]).expect("the fixture encodes");
        assert!(Value::root(&bytes).field("version").is_none());
        assert!(Value::root(&bytes).entries().is_none());
    }
}
