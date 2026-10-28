//! CPU receipts keep their exact value and a constant binary footprint.

pub(crate) fn serialize<S: serde::Serializer>(
    value: &u64,
    serializer: S,
) -> Result<S::Ok, S::Error> {
    if serializer.is_human_readable() {
        serializer.serialize_u64(*value)
    } else {
        serializer.serialize_bytes(&value.to_be_bytes())
    }
}

pub(crate) fn deserialize<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<u64, D::Error> {
    if deserializer.is_human_readable() {
        return serde::Deserialize::deserialize(deserializer);
    }
    struct Visitor;
    impl serde::de::Visitor<'_> for Visitor {
        type Value = u64;

        fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            formatter.write_str("eight big-endian CPU counter bytes")
        }

        fn visit_bytes<E: serde::de::Error>(self, bytes: &[u8]) -> Result<u64, E> {
            let value = bytes
                .try_into()
                .map_err(|_| E::invalid_length(bytes.len(), &self))?;
            Ok(u64::from_be_bytes(value))
        }
    }
    deserializer.deserialize_bytes(Visitor)
}
