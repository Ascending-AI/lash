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
pub use lash_sansio::json_decode::{JsonDecodeError, JsonDecodeLimits};
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
    Budget(JsonDecodeError),
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
            Self::Budget(error) => write!(formatter, "{error}"),
        }
    }
}

impl std::fmt::Display for CallDecodeError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Debug::fmt(self, formatter)
    }
}

impl std::error::Error for CallDecodeError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Unsupported(_) => None,
            Self::Json(error) => Some(error),
            Self::Budget(error) => Some(error),
        }
    }
}

impl<T: DeserializeOwned> Call<T> {
    /// Check structural allowances and the wire before constructing the body.
    /// The SDK ingress uses the same path with [`JsonDecodeLimits::default`].
    pub fn decode_json_with_limits(
        bytes: &[u8],
        limits: JsonDecodeLimits,
    ) -> Result<Self, CallDecodeError> {
        limits.check(bytes).map_err(|error| match error {
            JsonDecodeError::Json(error) => CallDecodeError::Json(error),
            error => CallDecodeError::Budget(error),
        })?;
        #[derive(serde::Deserialize)]
        struct WireProbe {
            wire: VersionRange,
        }
        let probe: WireProbe = serde_json::from_slice(bytes).map_err(CallDecodeError::Json)?;
        if RESTATE_WIRE.select(probe.wire).is_none() {
            return Err(CallDecodeError::Unsupported(probe.wire));
        }
        serde_json::from_slice(bytes).map_err(CallDecodeError::Json)
    }
}

impl<T: DeserializeOwned> restate_sdk::serde::Deserialize for Call<T> {
    type Error = CallDecodeError;

    fn deserialize(bytes: &mut Bytes) -> Result<Self, Self::Error> {
        Self::decode_json_with_limits(bytes, JsonDecodeLimits::default())
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
        let mut bytes = Bytes::from_static(br#"{"wire":{"min":3,"max":4},"body":{"shape":"new"}}"#);
        let error = Call::<u64>::deserialize(&mut bytes).expect_err("a disjoint range refuses");
        let message = format!("Cannot decode input payload: {error:?}");
        assert_eq!(
            restate_compat_error_in(&message),
            Some(RestateCompatError::WireUnsupported {
                local: RESTATE_WIRE,
                peer: VersionRange::new(3, 4).expect("range"),
            })
        );

        let mut bytes = Bytes::from_static(br#"{"wire":{"min":1,"max":2},"body":7}"#);
        let call = Call::<u64>::deserialize(&mut bytes).expect("an overlapping range decodes");
        assert_eq!(call.open().expect("selected"), (RESTATE_WIRE.max(), 7));
    }

    #[test]
    fn wide_call_refuses_before_dto_decode() {
        #[derive(Debug)]
        struct Dto;
        impl<'de> serde::Deserialize<'de> for Dto {
            fn deserialize<D: serde::Deserializer<'de>>(_: D) -> Result<Self, D::Error> {
                Err(serde::de::Error::custom("DTO decoder was entered"))
            }
        }
        let body = "0,".repeat(1_000_000) + "0";
        let mut bytes = Bytes::from(format!(r#"{{"wire":{{"min":1,"max":1}},"body":[{body}]}}"#));
        let error = Call::<Dto>::deserialize(&mut bytes).expect_err("wide call refuses");
        assert!(
            error.to_string().contains("JSON decode nodes limit"),
            "{error}"
        );
    }

    #[test]
    fn unsupported_call_never_materializes_numbers_in_its_body() {
        let mut bytes = Bytes::from_static(br#"{"body":[1e999],"wire":{"min":3,"max":4}}"#);
        let error = Call::<u64>::deserialize(&mut bytes).expect_err("unsupported wire");
        assert!(matches!(error, CallDecodeError::Unsupported(_)), "{error}");
    }

    #[test]
    fn call_accepts_exact_limits_and_refuses_each_overrun() {
        let bytes = br#"{"wire":{"min":1,"max":1},"body":[1,2]}"#;
        let usage = JsonDecodeLimits::default().check(bytes).unwrap();
        let limits = JsonDecodeLimits {
            max_bytes: usage.bytes,
            max_nodes: usage.nodes,
            max_depth: usage.depth,
            max_estimated_allocation_bytes: usage.estimated_allocation_bytes,
        };
        assert_eq!(
            Call::<Vec<u64>>::decode_json_with_limits(bytes, limits)
                .unwrap()
                .body,
            vec![1, 2]
        );
        for tight in [
            JsonDecodeLimits {
                max_bytes: limits.max_bytes - 1,
                ..limits
            },
            JsonDecodeLimits {
                max_nodes: limits.max_nodes - 1,
                ..limits
            },
            JsonDecodeLimits {
                max_depth: limits.max_depth - 1,
                ..limits
            },
            JsonDecodeLimits {
                max_estimated_allocation_bytes: limits.max_estimated_allocation_bytes - 1,
                ..limits
            },
        ] {
            assert!(matches!(
                Call::<Vec<u64>>::decode_json_with_limits(bytes, tight),
                Err(CallDecodeError::Budget(
                    JsonDecodeError::LimitExceeded { .. }
                ))
            ));
        }
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
