//! Explicit encoding of internal records, retaining trust envelopes.

use serde::{Serialize, Serializer};

/// Opt into the internal record format for persistence or a trusted wire.
///
/// Tool values, tool outputs and host process observations deliberately have
/// no automatic `Serialize`. Public views use their projection methods instead.
/// `Durable(&record)` retains trust, controls and model views; decoding uses
/// the record's `Deserialize` implementation.
#[derive(Clone, Copy, Debug)]
pub struct Durable<T>(pub T);

/// A record's explicit internal encoding, used through [`Durable`].
/// Hosts may implement this for their own containers of durable records.
pub trait DurableSerialize {
    fn serialize_durable<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error>;
}

impl<T: DurableSerialize + ?Sized> Serialize for Durable<&T> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        self.0.serialize_durable(serializer)
    }
}

/// Field adapter for records that explicitly retain a nested durable value.
pub fn serialize<T: DurableSerialize + ?Sized, S: Serializer>(
    value: &T,
    serializer: S,
) -> Result<S::Ok, S::Error> {
    Durable(value).serialize(serializer)
}

impl<T: DurableSerialize + ?Sized> DurableSerialize for Box<T> {
    fn serialize_durable<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serialize(self.as_ref(), serializer)
    }
}

impl<T: DurableSerialize> DurableSerialize for Option<T> {
    fn serialize_durable<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self {
            Some(value) => serializer.serialize_some(&Durable(value)),
            None => serializer.serialize_none(),
        }
    }
}

impl<T: DurableSerialize> DurableSerialize for Vec<T> {
    fn serialize_durable<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeSeq;
        let mut sequence = serializer.serialize_seq(Some(self.len()))?;
        for value in self {
            sequence.serialize_element(&Durable(value))?;
        }
        sequence.end()
    }
}

impl<T: DurableSerialize> DurableSerialize for std::collections::BTreeMap<String, T> {
    fn serialize_durable<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeMap;
        let mut map = serializer.serialize_map(Some(self.len()))?;
        for (key, value) in self {
            map.serialize_entry(key, &Durable(value))?;
        }
        map.end()
    }
}

// Remote derives keep each shape beside its decoder without implementing Serialize.
macro_rules! durable_records {
    ($($record:ty),+ $(,)?) => {$(
        impl DurableSerialize for $record {
            fn serialize_durable<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
                <$record>::serialize(self, serializer)
            }
        }
        impl<'de> serde::Deserialize<'de> for $record {
            fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
                <$record>::deserialize(deserializer)
            }
        }
    )+};
}

durable_records!(
    crate::ToolCallOutput,
    crate::ToolCallRecord,
    crate::ToolCallOutcome,
    crate::ToolFailure,
    crate::ToolCancellation,
    crate::TurnControl,
    crate::ToolControl,
);
