use std::collections::BTreeMap;

use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::QualifiedOperation;

/// # Errors
///
/// The serializer's.
pub fn serialize<V: Serialize, S: Serializer>(
    map: &BTreeMap<QualifiedOperation, V>,
    serializer: S,
) -> Result<S::Ok, S::Error> {
    serializer.collect_map(
        map.iter()
            .map(|(operation, value)| (operation.effect_name().to_string(), value)),
    )
}

/// # Errors
///
/// A key that is no canonical operation.
pub fn deserialize<'de, V: Deserialize<'de>, D: Deserializer<'de>>(
    deserializer: D,
) -> Result<BTreeMap<QualifiedOperation, V>, D::Error> {
    BTreeMap::<String, V>::deserialize(deserializer)?
        .into_iter()
        .map(|(key, value)| {
            super::operation_text::parse(&key)
                .map(|operation| (operation, value))
                .map_err(serde::de::Error::custom)
        })
        .collect()
}
