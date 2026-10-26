use serde::{Deserialize, Deserializer, Serializer};

use crate::QualifiedOperation;

/// # Errors
///
/// The serializer's.
pub fn serialize<S: Serializer>(
    operation: &QualifiedOperation,
    serializer: S,
) -> Result<S::Ok, S::Error> {
    serializer.serialize_str(operation.effect_name().as_str())
}

/// # Errors
///
/// A text that is no canonical operation.
pub fn deserialize<'de, D: Deserializer<'de>>(
    deserializer: D,
) -> Result<QualifiedOperation, D::Error> {
    parse(&String::deserialize(deserializer)?).map_err(serde::de::Error::custom)
}

/// The operation `text` names.
///
/// # Errors
///
/// Why `text` is no canonical operation.
pub fn parse(text: &str) -> Result<QualifiedOperation, String> {
    let effect = crate::names::EffectName::new(text).map_err(|error| error.to_string())?;
    QualifiedOperation::from_effect(&effect).map_err(|error| error.to_string())
}
