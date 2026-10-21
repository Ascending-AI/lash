//! Wire codec for canonical maps whose keys are records rather than strings.
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::collections::BTreeMap;
pub fn serialize<K: Serialize + Ord, V: Serialize, S: Serializer>(
    map: &BTreeMap<K, V>,
    serializer: S,
) -> Result<S::Ok, S::Error> {
    serializer.collect_seq(map.iter())
}
pub fn deserialize<'de, K: Deserialize<'de> + Ord, V: Deserialize<'de>, D: Deserializer<'de>>(
    deserializer: D,
) -> Result<BTreeMap<K, V>, D::Error> {
    let rows = Vec::<(K, V)>::deserialize(deserializer)?;
    let mut map = BTreeMap::new();
    for (key, value) in rows {
        if map.last_key_value().is_some_and(|(last, _)| last >= &key) {
            return Err(serde::de::Error::custom(
                "map keys must be strictly ascending",
            ));
        }
        map.insert(key, value);
    }
    Ok(map)
}
