//! The stored consumption record keeps every reading exact and fixed-width.

use lash_kernel_value::tokens::{TokenCodec, TokenError, TokenReader, TokenWriter};
use lash_protocol_types::{CellConsumption, CellResource};
use serde::{Deserializer, Serializer};

struct Reading(u64);

impl TokenCodec for Reading {
    fn encode(&self, out: &mut TokenWriter) {
        out.bytes(&self.0.to_le_bytes());
    }

    fn decode(input: &mut TokenReader<'_>) -> Result<Self, TokenError> {
        let bytes = input
            .bytes()?
            .try_into()
            .map_err(|_| TokenError::Invalid("a consumption reading has eight bytes".into()))?;
        Ok(Self(u64::from_le_bytes(bytes)))
    }
}

fn tag(resource: CellResource) -> u64 {
    match resource {
        CellResource::Charges => 0,
        CellResource::MemoryBytes => 1,
        CellResource::LiveTasks => 2,
        CellResource::CallDepth => 3,
        CellResource::RequestsPerPark => 4,
        CellResource::JoinMembers => 5,
        CellResource::WorkerCpuNanos => 6,
        CellResource::HeldWallNanos => 7,
        CellResource::ComputeNanos => 8,
        CellResource::SerializationNanos => 9,
        CellResource::LibraryWork => 10,
        CellResource::HostReadNanos => 11,
        CellResource::WorkerMemoryBytes => 12,
        CellResource::Effects => 13,
        CellResource::CheckpointBytes => 14,
        CellResource::ToolCalls => 15,
        CellResource::EffectValueBytes => 16,
        CellResource::VmStateBytes => 17,
        CellResource::FrameBytes => 18,
    }
}

fn resource(tag: u64) -> Result<CellResource, TokenError> {
    Ok(match tag {
        0 => CellResource::Charges,
        1 => CellResource::MemoryBytes,
        2 => CellResource::LiveTasks,
        3 => CellResource::CallDepth,
        4 => CellResource::RequestsPerPark,
        5 => CellResource::JoinMembers,
        6 => CellResource::WorkerCpuNanos,
        7 => CellResource::HeldWallNanos,
        8 => CellResource::ComputeNanos,
        9 => CellResource::SerializationNanos,
        10 => CellResource::LibraryWork,
        11 => CellResource::HostReadNanos,
        12 => CellResource::WorkerMemoryBytes,
        13 => CellResource::Effects,
        14 => CellResource::CheckpointBytes,
        15 => CellResource::ToolCalls,
        16 => CellResource::EffectValueBytes,
        17 => CellResource::VmStateBytes,
        18 => CellResource::FrameBytes,
        _ => return Err(TokenError::Invalid("unknown consumption resource".into())),
    })
}

pub(crate) fn encode(values: &[CellConsumption], out: &mut TokenWriter) {
    out.seq(values.len() as u64);
    for value in values {
        out.seq(4);
        out.unsigned(tag(value.resource));
        value.consumed.map(Reading).encode(out);
        out.unsigned(value.limit);
        value.observed.map(Reading).encode(out);
    }
}

pub(crate) fn decode(input: &mut TokenReader<'_>) -> Result<Vec<CellConsumption>, TokenError> {
    let count = input.seq()?;
    let mut values = Vec::new();
    for _ in 0..count {
        input.expect_struct(4)?;
        values.push(CellConsumption {
            resource: resource(input.unsigned()?)?,
            consumed: Option::<Reading>::decode(input)?.map(|value| value.0),
            limit: input.unsigned()?,
            observed: Option::<Reading>::decode(input)?.map(|value| value.0),
        });
    }
    Ok(values)
}

pub(crate) fn serialize<S: Serializer>(
    values: &[CellConsumption],
    serializer: S,
) -> Result<S::Ok, S::Error> {
    let mut out = TokenWriter::new();
    encode(values, &mut out);
    let bytes = out.into_bytes();
    let mut text = String::with_capacity(bytes.len().saturating_mul(2));
    lash_kernel_value::names::write_hex(&bytes, &mut text);
    serializer.serialize_str(&text)
}

pub(crate) fn deserialize<'de, D: Deserializer<'de>>(
    deserializer: D,
) -> Result<Vec<CellConsumption>, D::Error> {
    struct Visitor;
    impl serde::de::Visitor<'_> for Visitor {
        type Value = Vec<CellConsumption>;
        fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str("a lowercase hexadecimal consumption record")
        }
        fn visit_str<E: serde::de::Error>(self, text: &str) -> Result<Self::Value, E> {
            let bytes = lash_kernel_value::names::parse_hex(text)
                .ok_or_else(|| E::custom("invalid consumption record hex"))?;
            let mut input = TokenReader::new(&bytes);
            let values = decode(&mut input).map_err(E::custom)?;
            input.finish().map_err(E::custom)?;
            Ok(values)
        }
    }
    deserializer.deserialize_str(Visitor)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[derive(serde::Serialize, serde::Deserialize)]
    struct Stored(#[serde(with = "super")] Vec<CellConsumption>);

    /// FIG-7515: real measured values cannot change the allocation law's encoded width.
    #[test]
    fn measured_readings_keep_the_same_encoded_width_and_exact_value() {
        let mut widths = Vec::new();
        for actual in [0, 99_999_999, 100_000_000, u64::MAX] {
            let values = vec![
                CellConsumption {
                    resource: CellResource::WorkerCpuNanos,
                    consumed: Some(actual),
                    limit: u64::MAX,
                    observed: Some(actual),
                },
                CellConsumption {
                    resource: CellResource::CallDepth,
                    consumed: None,
                    limit: 1024,
                    observed: None,
                },
            ];
            let encoded = serde_json::to_string(&Stored(values.clone())).expect("record encodes");
            let decoded: Stored = serde_json::from_str(&encoded).expect("record decodes");
            assert_eq!(decoded.0, values);
            widths.push(encoded.len());
        }
        assert!(widths.iter().all(|width| *width == widths[0]), "{widths:?}");
    }
}
