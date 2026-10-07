//! Durations as whole milliseconds, the host configuration's one wire unit.
//!
//! A value is never rounded: deserializing reads an integer count of
//! milliseconds, and serializing a duration that does not fit refuses
//! instead of saturating. Validation refuses sub-millisecond values before
//! anything serializes them.

use std::time::Duration;

use serde::{Deserialize as _, Deserializer, Serializer};

fn whole_millis<E: serde::ser::Error>(value: &Duration) -> Result<u64, E> {
    if !value.subsec_nanos().is_multiple_of(1_000_000) {
        return Err(E::custom("duration is not a whole number of milliseconds"));
    }
    u64::try_from(value.as_millis()).map_err(|_| E::custom("duration does not fit u64 ms"))
}

pub(crate) fn serialize<S: Serializer>(value: &Duration, serializer: S) -> Result<S::Ok, S::Error> {
    serializer.serialize_u64(whole_millis(value)?)
}

pub(crate) fn deserialize<'de, D: Deserializer<'de>>(
    deserializer: D,
) -> Result<Duration, D::Error> {
    u64::deserialize(deserializer).map(Duration::from_millis)
}

/// `Option<Duration>` as milliseconds, `null` for `None`.
pub(crate) mod option {
    use std::time::Duration;

    use serde::{Deserialize as _, Deserializer, Serializer};

    pub(crate) fn serialize<S: Serializer>(
        value: &Option<Duration>,
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        match value {
            Some(value) => serializer.serialize_some(&super::whole_millis::<S::Error>(value)?),
            None => serializer.serialize_none(),
        }
    }

    pub(crate) fn deserialize<'de, D: Deserializer<'de>>(
        deserializer: D,
    ) -> Result<Option<Duration>, D::Error> {
        Option::<u64>::deserialize(deserializer).map(|value| value.map(Duration::from_millis))
    }
}
