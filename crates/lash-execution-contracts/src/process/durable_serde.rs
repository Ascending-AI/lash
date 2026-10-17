//! Explicit serialization of internal process records, retaining trust envelopes.

use serde::{Deserialize, Serialize, Serializer};

use super::events::ProcessTerminal;
use super::model::ProcessLifecycleState;
use super::observation::{ObservedProcess, ObservedProcessEvent};

pub use lash_protocol_types::Durable;
use lash_protocol_types::DurableSerialize;

macro_rules! durable_serialize {
    ($($record:ty),+ $(,)?) => {$(
        impl DurableSerialize for $record {
            fn serialize_durable<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
                <$record>::serialize(self, serializer)
            }
        }
    )+};
}

durable_serialize!(
    ObservedProcess,
    ObservedProcessEvent,
    ProcessTerminal,
    ProcessLifecycleState,
    super::observation::ObservedProcessChange,
    super::observation::ObservedProcessChangeKind,
    super::events::ProcessAwaitOutput
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

durable_deserialize!(
    ObservedProcess,
    ObservedProcessEvent,
    ProcessLifecycleState,
    super::observation::ObservedProcessChange,
    super::observation::ObservedProcessChangeKind,
    super::events::ProcessAwaitOutput
);

pub fn serialize<T, S>(value: &T, serializer: S) -> Result<S::Ok, S::Error>
where
    S: Serializer,
    for<'a> Durable<&'a T>: Serialize,
{
    Durable(value).serialize(serializer)
}

impl<Full: DurableSerialize, Lite: Serialize> DurableSerialize
    for super::ProcessEventPage<Full, Lite>
{
    fn serialize_durable<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        use super::ProcessEventPageEvents;
        let events = match &self.events {
            ProcessEventPageEvents::Full(events) => {
                ProcessEventPageEvents::Full(events.iter().map(Durable).collect::<Vec<_>>())
            }
            ProcessEventPageEvents::Lite(events) => {
                ProcessEventPageEvents::Lite(events.iter().collect::<Vec<_>>())
            }
        };
        super::ProcessEventPage {
            events,
            more: self.more.clone(),
        }
        .serialize(serializer)
    }
}

impl<Page: DurableSerialize> DurableSerialize for super::ProcessEventReadOutcome<Page> {
    fn serialize_durable<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        use super::ProcessEventReadOutcome;
        let outcome = match self {
            ProcessEventReadOutcome::Retained(page) => {
                ProcessEventReadOutcome::Retained(Durable(page))
            }
            ProcessEventReadOutcome::NoLongerRetained(retention) => {
                ProcessEventReadOutcome::NoLongerRetained(retention.clone())
            }
        };
        outcome.serialize(serializer)
    }
}
