//! Versioned values in Restate object state (ADR 0106 §3, FIG-3814).
//!
//! Every value a Lash Restate object retains — the durable-wait index's
//! metadata, wait, resolution, marker and membership rows, the effect-group
//! index record, and the effect-group payload's bytes and retirement fence —
//! is stored under an explicit [`StampedValue`] envelope. Readers dispatch on
//! the `format` stamp: the current format decodes directly, a stamp one
//! format behind goes through the family's N-1 upcaster hook, and any other
//! stamp — including unstamped pre-format state — is refused before the
//! handler acts, with a typed terminal error Restate does not retry.
//!
//! The stamp lives in the value, not in the key: object keys and service
//! names stay stable across format changes, and a format move is an in-place
//! lazy upcast (a sweep is the operator's lever, not the type system's).

use lash_core::{RuntimeEffectControllerError, RuntimeErrorCode};
use restate_sdk::context::{
    ContextReadState, ContextWriteState, ObjectContext, SharedObjectContext,
};
use restate_sdk::errors::TerminalError;
use restate_sdk::serde::Json;
use serde::{Deserialize, Serialize, de::DeserializeOwned};

/// The field a stamped object-state value carries its format under.
const FORMAT_FIELD: &str = "format";
/// The field a stamped object-state value carries the value itself under.
const BODY_FIELD: &str = "body";

/// The on-state envelope every value written to a Lash Restate object wears:
/// `{ "format": F, "body": <the value> }`.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct StampedValue<T> {
    pub(crate) format: u16,
    pub(crate) body: T,
}

/// An N-1 upcaster: the stored `body` of a value written under a registered
/// previous format goes in, and the body the current format expects comes
/// out. No upcaster is registered anywhere yet — the first stamped layout is
/// the 1.0 baseline — but the slot is where each lands when a format moves.
pub(crate) type Upcast = fn(serde_json::Value) -> Result<serde_json::Value, TerminalError>;

/// A family's `(format, upcaster)` table.
pub(crate) type UpcastTable = [(u16, Upcast)];

/// The stored formats one object-state family admits: the `current` stamp
/// every write carries, plus `upcast_n1`, the N-1 upcaster hooks for values
/// one format behind. `what` names the family in refusals an operator reads.
pub(crate) struct StoredValueFormats {
    pub what: &'static str,
    pub current: u16,
    pub upcast_n1: &'static UpcastTable,
}

/// Read and decode one stamped object-state value, or `None` when the key
/// carries no state. The read is raw bytes: a value written before the
/// envelope — a payload's bare bytes, a bare bool, unstamped JSON — fails
/// closed as the typed "unstamped" refusal instead of trapping in the SDK's
/// deserialization and retrying.
pub(crate) async fn get_stamped<'ctx, T>(
    ctx: &ObjectContext<'ctx>,
    key: &'ctx str,
    formats: &StoredValueFormats,
) -> Result<Option<T>, TerminalError>
where
    T: DeserializeOwned + 'static,
{
    ctx.get::<Vec<u8>>(key)
        .await?
        .map(|bytes| decode_stamped_bytes(key, &bytes, formats))
        .transpose()
}

/// [`get_stamped`] for a shared handler's read-only context.
pub(crate) async fn get_stamped_shared<'ctx, T>(
    ctx: &SharedObjectContext<'ctx>,
    key: &'ctx str,
    formats: &StoredValueFormats,
) -> Result<Option<T>, TerminalError>
where
    T: DeserializeOwned + 'static,
{
    ctx.get::<Vec<u8>>(key)
        .await?
        .map(|bytes| decode_stamped_bytes(key, &bytes, formats))
        .transpose()
}

/// Refuse an object that carries any value without the stamped envelope — the
/// object-level gate for a family that must never write beside pre-stamp
/// state (an old identity marker, or a row written before the envelope
/// existed). `retired_keys` name markers a pre-stamp deployment wrote that
/// must refuse even when their bytes happen to look stamped. A fresh object
/// holds no keys and passes; the per-key reads that follow decide whether
/// each stamp is one this build reads.
pub(crate) async fn gate_stamped_object_state(
    ctx: &ObjectContext<'_>,
    formats: &StoredValueFormats,
    retired_keys: &[&'static str],
) -> Result<(), TerminalError> {
    for key in ctx.get_keys().await? {
        if retired_keys.contains(&key.as_str()) {
            return Err(stored_format_terminal(&key, None, formats));
        }
        if let Some(bytes) = ctx.get::<Vec<u8>>(&key).await?
            && !carries_format_stamp(&bytes)
        {
            return Err(stored_format_terminal(&key, None, formats));
        }
    }
    Ok(())
}

/// Whether `bytes` carry a format stamp at all. The version dispatch that
/// follows decides whether the stamp is one this build reads.
fn carries_format_stamp(bytes: &[u8]) -> bool {
    serde_json::from_slice::<serde_json::Value>(bytes)
        .ok()
        .is_some_and(|raw| {
            raw.get(FORMAT_FIELD)
                .and_then(serde_json::Value::as_u64)
                .is_some()
        })
}

/// Decode one raw object-state value: bytes that are not JSON predate the
/// envelope and are the typed "unstamped" refusal, never an SDK
/// deserialization trap.
fn decode_stamped_bytes<T: DeserializeOwned>(
    key: &str,
    bytes: &[u8],
    formats: &StoredValueFormats,
) -> Result<T, TerminalError> {
    let raw = serde_json::from_slice::<serde_json::Value>(bytes)
        .map_err(|_| stored_format_terminal(key, None, formats))?;
    decode_stamped_value(key, raw, formats)
}

/// Write one stamped object-state value under the family's current format.
pub(crate) fn set_stamped<T>(
    ctx: &ObjectContext<'_>,
    key: &str,
    formats: &StoredValueFormats,
    body: T,
) where
    T: Serialize + 'static,
{
    ctx.set(
        key,
        Json(StampedValue {
            format: formats.current,
            body,
        }),
    );
}

/// Decode one raw object-state value: read its `format` stamp before its
/// body, dispatch on it, and refuse stamps this build does not read — so a
/// value whose shape has moved is refused by stamp, never by an accident of
/// decoding.
pub(crate) fn decode_stamped_value<T: DeserializeOwned>(
    key: &str,
    raw: serde_json::Value,
    formats: &StoredValueFormats,
) -> Result<T, TerminalError> {
    let stamp = raw.get(FORMAT_FIELD).and_then(serde_json::Value::as_u64);
    let body = raw
        .get(BODY_FIELD)
        .cloned()
        .unwrap_or(serde_json::Value::Null);
    let body = match stamp {
        Some(stamp) if stamp == u64::from(formats.current) => body,
        Some(stamp) => match formats
            .upcast_n1
            .iter()
            .find(|(format, _)| u64::from(*format) == stamp)
        {
            Some((_, upcast)) => upcast(body)?,
            None => return Err(stored_format_terminal(key, Some(stamp), formats)),
        },
        None => return Err(stored_format_terminal(key, None, formats)),
    };
    serde_json::from_value(body).map_err(|error| {
        TerminalError::new(format!(
            "{} state {key} does not decode under stored format {}: {error}",
            formats.what, formats.current
        ))
    })
}

/// The typed refusal of object state carrying a stamp this build does not
/// read. It travels as the terminal error's serialized message so callers
/// past a service boundary can recover it with [`stored_format_error_in`].
pub(crate) fn stored_format_error(
    what: &str,
    key: &str,
    stamped: Option<u64>,
    formats: &StoredValueFormats,
) -> RuntimeEffectControllerError {
    let found = stamped.map_or_else(
        || "no format stamp".to_string(),
        |stamp| format!("format stamp {stamp}"),
    );
    RuntimeEffectControllerError::new(
        RuntimeErrorCode::EngineObjectStateFormatUnsupported,
        format!(
            "{what} state {key} carries {found}; this deployment reads stored \
             format {} and refuses the value before any effect",
            formats.current
        ),
    )
}

pub(crate) fn stored_format_terminal(
    key: &str,
    stamped: Option<u64>,
    formats: &StoredValueFormats,
) -> TerminalError {
    let error = stored_format_error(formats.what, key, stamped, formats);
    TerminalError::new(serde_json::to_string(&error).unwrap_or_else(|_| error.message.clone()))
}

/// The typed stored-format refusal a handler's terminal error carries, if
/// that is what `message` is.
pub(crate) fn stored_format_error_in(message: &str) -> Option<RuntimeEffectControllerError> {
    serde_json::from_str::<RuntimeEffectControllerError>(message)
        .ok()
        .filter(|error| error.code == RuntimeErrorCode::EngineObjectStateFormatUnsupported)
}

#[cfg(test)]
mod tests {
    use super::*;

    const FORMATS: StoredValueFormats = StoredValueFormats {
        what: "test-object",
        current: 2,
        upcast_n1: &[(1, upcast_v1_body)],
    };

    fn upcast_v1_body(body: serde_json::Value) -> Result<serde_json::Value, TerminalError> {
        let mut object = body.as_object().cloned().unwrap_or_default();
        // The test's N-1 format kept the answer under `value`, not `body`.
        if let Some(value) = object.remove("value") {
            object.insert("body".to_string(), value);
        }
        Ok(serde_json::Value::Object(object))
    }

    fn stamped(format: u16, body: serde_json::Value) -> serde_json::Value {
        serde_json::to_value(StampedValue { format, body }).expect("stamp a value")
    }

    #[test]
    fn the_current_format_round_trips() {
        let raw = stamped(2, serde_json::json!({"name": "alpha", "count": 3}));
        let decoded: serde_json::Value =
            decode_stamped_value("state-key", raw, &FORMATS).expect("current format decodes");
        assert_eq!(decoded["name"], "alpha");
        assert_eq!(decoded["count"], 3);
    }

    #[test]
    fn a_stamped_scalar_round_trips() {
        let raw = stamped(2, serde_json::json!("resolved"));
        let decoded: String =
            decode_stamped_value("state-key", raw, &FORMATS).expect("scalar body decodes");
        assert_eq!(decoded, "resolved");
    }

    #[test]
    fn stamped_bytes_round_trip() {
        let bytes = vec![0u8, 1, 2, 250, 255];
        let raw = stamped(2, serde_json::to_value(&bytes).expect("bytes to value"));
        let decoded: Vec<u8> =
            decode_stamped_value("state-key", raw, &FORMATS).expect("bytes decode");
        assert_eq!(decoded, bytes);
    }

    #[test]
    fn an_n_minus_one_value_upcasts() {
        let raw = stamped(1, serde_json::json!({"value": {"name": "beta"}}));
        let decoded: serde_json::Value = decode_stamped_value("state-key", raw, &FORMATS)
            .expect("the N-1 upcaster answers a current body");
        assert_eq!(decoded["body"]["name"], "beta");
    }

    #[test]
    fn unstamped_and_foreign_stamps_are_typed_refusals() {
        for raw in [
            serde_json::json!({"body": {"name": "legacy"}}),
            stamped(0, serde_json::json!({"name": "older"})),
            stamped(7, serde_json::json!({"name": "newer"})),
        ] {
            let error = decode_stamped_value::<serde_json::Value>("state-key", raw, &FORMATS)
                .expect_err("a value this build does not read is refused");
            let typed = stored_format_error_in(error.message())
                .unwrap_or_else(|| panic!("the refusal is typed: {error}"));
            assert_eq!(
                typed.code,
                RuntimeErrorCode::EngineObjectStateFormatUnsupported
            );
            // The refusal names what it met, so an operator sees which side
            // of the boundary the deployment is on.
            assert!(
                typed.message.contains("test-object")
                    && typed.message.contains("state-key")
                    && typed.message.contains("format 2"),
                "{typed:?}"
            );
        }
    }

    #[test]
    fn a_current_body_that_does_not_decode_is_terminal() {
        let raw = stamped(2, serde_json::json!("a string body"));
        let error = decode_stamped_value::<serde_json::Map<String, serde_json::Value>>(
            "state-key",
            raw,
            &FORMATS,
        )
        .expect_err("a corrupt current-format body is terminal");
        assert!(
            stored_format_error_in(error.message()).is_none(),
            "a decode fault is corruption, not a format refusal: {error}"
        );
        assert!(error.message().contains("does not decode"));
    }
}
