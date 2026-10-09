//! What a parent and its worker say to each other about one kernel run, in
//! the kernel's own JSON encoding.
//!
//! The protocol carries these as opaque payloads; the machine's interface
//! types are not serialisable themselves, so each crosses as the mirror
//! here. Numbers keep their tokens and objects their identities, so nothing
//! is reduced on the way.

use std::collections::BTreeMap;
use std::time::Duration;

use lash_kernel_doc::{
    Datum, EffectIdentity, EffectName, ErrorDatum, Handle, Name, Object, ObjectId, Type, Value,
};
use lash_kernel_vm::{
    Bindings, EffectRequest, Outcome, Park, Request, SleepRequest, Start, Target, WaitId,
};
use lash_vm_protocol::{EncodedPayload, PayloadKind};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

use crate::PoolError;

/// JSON text as a payload: one MessagePack binary, so the transport bounds
/// it as it bounds every payload, without reading the JSON.
///
/// # Errors
///
/// A payload breach when the bytes cannot be framed.
pub fn wrap(kind: PayloadKind, json: &[u8]) -> Result<EncodedPayload, PoolError> {
    rmp_serde::to_vec(serde_bytes::Bytes::new(json))
        .map(EncodedPayload)
        .map_err(|error| PoolError::payload(kind, error))
}

/// The JSON text a payload of `kind` carries.
///
/// # Errors
///
/// A payload breach when the payload is not one binary.
pub fn unwrap(kind: PayloadKind, payload: &EncodedPayload) -> Result<Vec<u8>, PoolError> {
    rmp_serde::from_slice::<serde_bytes::ByteBuf>(&payload.0)
        .map(serde_bytes::ByteBuf::into_vec)
        .map_err(|error| PoolError::payload(kind, error))
}

/// `value` as a payload of `kind`.
///
/// # Errors
///
/// A payload breach when the value has no JSON encoding.
pub fn encode<T: Serialize>(kind: PayloadKind, value: &T) -> Result<EncodedPayload, PoolError> {
    let json = serde_json::to_vec(value).map_err(|error| PoolError::payload(kind, error))?;
    wrap(kind, &json)
}

/// The value a payload of `kind` carries.
///
/// # Errors
///
/// A payload breach when the payload is not that value.
pub fn decode<T: DeserializeOwned>(
    kind: PayloadKind,
    payload: &EncodedPayload,
) -> Result<T, PoolError> {
    from_json(kind, &unwrap(kind, payload)?)
}

/// The value `json` encodes. Kernel values bound their own nesting, so the
/// decoder's fixed recursion limit is lifted.
///
/// # Errors
///
/// A payload breach when the text is not that value.
pub fn from_json<T: DeserializeOwned>(kind: PayloadKind, json: &[u8]) -> Result<T, PoolError> {
    let mut decoder = serde_json::Deserializer::from_slice(json);
    decoder.disable_recursion_limit();
    let value = T::deserialize(&mut decoder).map_err(|error| PoolError::payload(kind, error))?;
    decoder
        .end()
        .map_err(|error| PoolError::payload(kind, error))?;
    Ok(value)
}

/// How a new run starts ([`Start`]).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StartWire {
    /// The entry to run; `None` runs the document's `main`.
    pub entry: Option<Name>,
    pub args: Vec<Datum>,
    pub variables: BTreeMap<Name, Value>,
    pub objects: Vec<(ObjectId, Object)>,
}

impl From<Start> for StartWire {
    fn from(start: Start) -> Self {
        Self {
            entry: match start.target {
                Target::Main => None,
                Target::Entry(name) => Some(name),
            },
            args: start.args,
            variables: start.bindings.variables,
            objects: start.bindings.objects.into_iter().collect(),
        }
    }
}

impl From<StartWire> for Start {
    fn from(wire: StartWire) -> Self {
        Self {
            target: wire.entry.map_or(Target::Main, Target::Entry),
            args: wire.args,
            bindings: Bindings {
                variables: wire.variables,
                objects: wire.objects.into_iter().collect(),
            },
        }
    }
}

/// One wait a park requests ([`Request`]).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum RequestWire {
    Effect {
        wait: WaitId,
        identity: EffectIdentity,
        effect: EffectName,
        args: Vec<Datum>,
        result: Type,
    },
    Sleep {
        wait: WaitId,
        identity: EffectIdentity,
        seconds: u64,
        nanoseconds: u32,
    },
}

/// What a park hands the parent ([`Park`]).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ParkWire {
    pub requests: Vec<RequestWire>,
    pub withdrawn: Vec<WaitId>,
}

impl From<Park> for ParkWire {
    fn from(park: Park) -> Self {
        Self {
            requests: park
                .requests
                .into_iter()
                .map(|request| match request {
                    Request::Effect(request) => RequestWire::Effect {
                        wait: request.wait,
                        identity: request.identity,
                        effect: request.effect,
                        args: request.args,
                        result: request.result,
                    },
                    Request::Sleep(sleep) => RequestWire::Sleep {
                        wait: sleep.wait,
                        identity: sleep.identity,
                        seconds: sleep.duration.as_secs(),
                        nanoseconds: sleep.duration.subsec_nanos(),
                    },
                })
                .collect(),
            withdrawn: park.withdrawn,
        }
    }
}

impl From<ParkWire> for Park {
    fn from(wire: ParkWire) -> Self {
        Self {
            requests: wire
                .requests
                .into_iter()
                .map(|request| match request {
                    RequestWire::Effect {
                        wait,
                        identity,
                        effect,
                        args,
                        result,
                    } => Request::Effect(EffectRequest {
                        wait,
                        identity,
                        effect,
                        args,
                        result,
                    }),
                    RequestWire::Sleep {
                        wait,
                        identity,
                        seconds,
                        nanoseconds,
                    } => Request::Sleep(SleepRequest {
                        wait,
                        identity,
                        duration: Duration::new(seconds, nanoseconds),
                    }),
                })
                .collect(),
            withdrawn: wire.withdrawn,
        }
    }
}

/// How a wait ended ([`Outcome`]).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum OutcomeWire {
    Completed(Datum),
    Failed(ErrorDatum),
    Elapsed,
}

impl From<Outcome> for OutcomeWire {
    fn from(outcome: Outcome) -> Self {
        match outcome {
            Outcome::Completed(value) => Self::Completed(value),
            Outcome::Failed(error) => Self::Failed(error),
            Outcome::Elapsed => Self::Elapsed,
        }
    }
}

impl From<OutcomeWire> for Outcome {
    fn from(wire: OutcomeWire) -> Self {
        match wire {
            OutcomeWire::Completed(value) => Self::Completed(value),
            OutcomeWire::Failed(error) => Self::Failed(error),
            OutcomeWire::Elapsed => Self::Elapsed,
        }
    }
}

/// How a run ended: the end a checkpoint would record, or `None` for a
/// cancelled run, which records none.
pub type EndWire = Option<RecordedEnd>;

pub use lash_vm_broker::kernel::RecordedEnd;

/// A read through a projection handle. The answer is
/// [`ProjectionAnswer`].
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProjectionRead {
    pub handle: Handle,
    pub request: Datum,
}

/// Kernel data, or the error the read raises in the guest.
pub type ProjectionAnswer = Result<Datum, ErrorDatum>;
