//! How the frozen [`Call`] and [`Reply`] shapes travel through the Restate
//! SDK (ADR 0115 §3.1), and the two typed refusals a handler answers before
//! it reads or writes any state.
//!
//! Every lash handler takes a `Call<T>` and answers a `Reply<T>`, so the SDK
//! decodes and encodes them itself: a request's `wire` is read, and the
//! version selected, before its body is decoded. A caller whose range does
//! not meet this build's is refused with `lash.wire_unsupported`, carrying
//! both ranges, however its body is shaped. The discovery manifest names
//! both shapes ([`CALL_SCHEMA_TITLE`], [`REPLY_SCHEMA_TITLE`]), so a test
//! reads from what the binder binds that every handler takes one and
//! answers the other.

use bytes::Bytes;
use lash_core_store::compat::CompatRefusal;
use restate_sdk::errors::TerminalError;
use restate_sdk::serde::PayloadMetadata;
use serde::de::DeserializeOwned;

use crate::compat::{Call, RESTATE_WIRE, Reply, VersionRange};

/// The title a [`Call`]'s discovery schema carries.
pub(crate) const CALL_SCHEMA_TITLE: &str = "lash.restate.Call";
/// The title a [`Reply`]'s discovery schema carries.
pub(crate) const REPLY_SCHEMA_TITLE: &str = "lash.restate.Reply";

/// A lash handler's typed refusal of a call, before any state changed. It
/// travels as a terminal error's message, as the JSON of this value, so a
/// caller past the service boundary reads it back with
/// [`restate_compat_error_in`].
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "error")]
pub(crate) enum RestateCompatError {
    /// The caller reads no wire version this build answers.
    #[serde(rename = "lash.wire_unsupported")]
    WireUnsupported {
        local: VersionRange,
        peer: VersionRange,
    },
    /// The object's `_compat` record (or its absence) refuses this build.
    #[serde(rename = "lash.incompatible")]
    Incompatible { refusal: CompatRefusal },
}

impl RestateCompatError {
    fn encode(&self) -> String {
        serde_json::to_string(self).unwrap_or_else(|_| format!("{self:?}"))
    }

    pub(crate) fn terminal(&self) -> TerminalError {
        TerminalError::new(self.encode())
    }
}

/// The typed refusal a handler's terminal error carries, if that is what
/// `message` holds. The SDK prefixes an input it could not decode with its
/// own words, so the refusal is found where it starts.
pub(crate) fn restate_compat_error_in(message: &str) -> Option<RestateCompatError> {
    let refusal = message.get(message.find(r#"{"error":"lash."#)?..)?;
    serde_json::Deserializer::from_str(refusal)
        .into_iter::<RestateCompatError>()
        .next()?
        .ok()
}

/// The terminal refusal of a call whose range `peer` holds no version this
/// build answers.
pub(crate) fn wire_unsupported(peer: VersionRange) -> TerminalError {
    RestateCompatError::WireUnsupported {
        local: RESTATE_WIRE,
        peer,
    }
    .terminal()
}

/// The terminal refusal of an object whose `_compat` refuses this build.
pub(crate) fn incompatible(refusal: CompatRefusal) -> TerminalError {
    RestateCompatError::Incompatible { refusal }.terminal()
}

impl<T> Call<T> {
    /// The version this handler answers at, and the body: the wire
    /// selection every handler makes before it reads or writes any state.
    pub(crate) fn open(self) -> Result<(u32, T), TerminalError> {
        match self.select() {
            Some(wire) => Ok((wire, self.body)),
            None => Err(wire_unsupported(self.wire)),
        }
    }
}

impl<T> Reply<T> {
    /// The body of a reply a lash handler answered.
    pub(crate) fn into_body(self) -> T {
        self.body
    }
}

/// Why a [`Call`] did not decode. Its `Debug` is what the SDK's terminal
/// error carries, so an unsupported range reads as the typed refusal.
pub enum CallDecodeError {
    Unsupported(VersionRange),
    Json(serde_json::Error),
}

impl std::fmt::Debug for CallDecodeError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unsupported(peer) => formatter.write_str(
                &RestateCompatError::WireUnsupported {
                    local: RESTATE_WIRE,
                    peer: *peer,
                }
                .encode(),
            ),
            Self::Json(error) => write!(formatter, "{error}"),
        }
    }
}

impl std::fmt::Display for CallDecodeError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Debug::fmt(self, formatter)
    }
}

impl std::error::Error for CallDecodeError {}

/// The outer shape of a call, read before its body.
#[derive(serde::Deserialize)]
struct RawCall {
    wire: VersionRange,
    #[serde(default)]
    body: serde_json::Value,
}

impl<T: DeserializeOwned> restate_sdk::serde::Deserialize for Call<T> {
    type Error = CallDecodeError;

    fn deserialize(bytes: &mut Bytes) -> Result<Self, Self::Error> {
        let raw: RawCall = serde_json::from_slice(bytes).map_err(CallDecodeError::Json)?;
        if RESTATE_WIRE.select(raw.wire).is_none() {
            return Err(CallDecodeError::Unsupported(raw.wire));
        }
        Ok(Call {
            wire: raw.wire,
            body: serde_json::from_value(raw.body).map_err(CallDecodeError::Json)?,
        })
    }
}

impl<T: serde::Serialize> restate_sdk::serde::Serialize for Call<T> {
    type Error = serde_json::Error;

    fn serialize(&self) -> Result<Bytes, Self::Error> {
        serde_json::to_vec(self).map(Bytes::from)
    }
}

impl<T> PayloadMetadata for Call<T> {
    fn json_schema() -> Option<serde_json::Value> {
        Some(serde_json::json!({
            "title": CALL_SCHEMA_TITLE,
            "type": "object",
            "required": ["wire", "body"],
            "properties": {
                "wire": {
                    "type": "object",
                    "required": ["min", "max"],
                    "properties": {
                        "min": {"type": "integer", "minimum": 1},
                        "max": {"type": "integer", "minimum": 1}
                    }
                },
                "body": {}
            }
        }))
    }
}

impl<T: DeserializeOwned> restate_sdk::serde::Deserialize for Reply<T> {
    type Error = serde_json::Error;

    fn deserialize(bytes: &mut Bytes) -> Result<Self, Self::Error> {
        serde_json::from_slice(bytes)
    }
}

impl<T: serde::Serialize> restate_sdk::serde::Serialize for Reply<T> {
    type Error = serde_json::Error;

    fn serialize(&self) -> Result<Bytes, Self::Error> {
        serde_json::to_vec(self).map(Bytes::from)
    }
}

impl<T> PayloadMetadata for Reply<T> {
    fn json_schema() -> Option<serde_json::Value> {
        Some(serde_json::json!({
            "title": REPLY_SCHEMA_TITLE,
            "type": "object",
            "required": ["wire", "body"],
            "properties": {
                "wire": {"type": "integer", "minimum": 1},
                "body": {}
            }
        }))
    }
}

/// A scripted ingress answer: `body` inside the Reply envelope a lash
/// handler answers on this build's wire.
#[cfg(test)]
pub(crate) fn reply_json<T: serde::Serialize + ?Sized>(body: &T) -> String {
    serde_json::json!({"wire": crate::compat::RESTATE_WIRE_VERSION, "body": body}).to_string()
}

#[cfg(test)]
mod tests {
    use restate_sdk::serde::Deserialize as _;

    use super::*;

    #[test]
    fn a_disjoint_call_is_refused_before_its_body_decodes() {
        // A newer caller's body this build cannot type: the range refuses
        // first, and the refusal carries both ranges.
        let mut bytes = Bytes::from_static(br#"{"wire":{"min":2,"max":3},"body":{"shape":"new"}}"#);
        let error = Call::<u64>::deserialize(&mut bytes).expect_err("a disjoint range refuses");
        let message = format!("Cannot decode input payload: {error:?}");
        assert_eq!(
            restate_compat_error_in(&message),
            Some(RestateCompatError::WireUnsupported {
                local: RESTATE_WIRE,
                peer: VersionRange::new(2, 3).expect("range"),
            })
        );

        let mut bytes = Bytes::from_static(br#"{"wire":{"min":1,"max":2},"body":7}"#);
        let call = Call::<u64>::deserialize(&mut bytes).expect("an overlapping range decodes");
        assert_eq!(call.open().expect("selected"), (1, 7));
    }

    #[test]
    fn an_incompatible_refusal_reads_back_typed() {
        let refusal = CompatRefusal::Unstamped {
            component: "restate-effect-group-state".to_owned(),
            writing_release: None,
        };
        let terminal = incompatible(refusal.clone());
        assert_eq!(
            restate_compat_error_in(terminal.message()),
            Some(RestateCompatError::Incompatible { refusal })
        );
        assert!(
            terminal
                .message()
                .contains(r#""error":"lash.incompatible""#)
        );
    }
}
