use serde::de::{Deserialize, Deserializer};
use serde_json::Number;

pub fn deserialize_f64<'de, D: Deserializer<'de>>(deserializer: D) -> Result<f64, D::Error> {
    let number = Number::deserialize(deserializer)?;
    number
        .as_f64()
        .filter(|value| value.is_finite())
        .ok_or_else(|| serde::de::Error::custom("JSON number is not a finite f64"))
}

pub fn deserialize_optional_f64<'de, D: Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<f64>, D::Error> {
    Option::<Number>::deserialize(deserializer)?
        .map(|number| {
            number
                .as_f64()
                .filter(|value| value.is_finite())
                .ok_or_else(|| serde::de::Error::custom("JSON number is not a finite f64"))
        })
        .transpose()
}
