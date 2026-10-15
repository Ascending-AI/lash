//! Explicit serialization of internal process records, retaining trust envelopes.

use serde::{Deserialize, Serialize, Serializer};

use super::events::ProcessTerminal;
use super::model::ProcessLifecycleState;
use super::observation::{ObservedProcess, ObservedProcessEvent};

/// Opt into the internal process record format for durable persistence.
///
/// Host observations and terminal outcomes intentionally do not implement
/// `Serialize`: store their `projection()` instead. Stores and replay fixtures
/// use `Durable(&record)` to retain output trust, controls and model views.
/// Decoding continues to use the record's `Deserialize` implementation.
#[derive(Clone, Copy, Debug)]
pub struct Durable<T>(pub T);

macro_rules! durable_serialize {
    ($($record:ty),+ $(,)?) => {$(
        impl Serialize for Durable<&$record> {
            fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
                <$record>::serialize(self.0, serializer)
            }
        }
    )+};
}

durable_serialize!(
    ObservedProcess,
    ObservedProcessEvent,
    ProcessTerminal,
    ProcessLifecycleState
);

// Remote derives keep the field declarations as the single definition of the
// stored shape, without giving the host-facing type a Serialize implementation.
macro_rules! durable_deserialize {
    ($($record:ty),+ $(,)?) => {$(
        impl<'de> Deserialize<'de> for $record {
            fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
                <$record>::deserialize(deserializer)
            }
        }
    )+};
}

durable_deserialize!(ObservedProcess, ObservedProcessEvent, ProcessLifecycleState);

pub(crate) fn serialize<T, S>(value: &T, serializer: S) -> Result<S::Ok, S::Error>
where
    S: Serializer,
    for<'a> Durable<&'a T>: Serialize,
{
    Durable(value).serialize(serializer)
}

impl Serialize for Durable<&Box<ObservedProcess>> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        Durable(self.0.as_ref()).serialize(serializer)
    }
}

impl Serialize for Durable<&Vec<ObservedProcess>> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeSeq;
        let mut sequence = serializer.serialize_seq(Some(self.0.len()))?;
        for value in self.0 {
            sequence.serialize_element(&Durable(value))?;
        }
        sequence.end()
    }
}
