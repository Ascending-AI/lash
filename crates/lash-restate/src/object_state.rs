//! Versioned values in Restate object state (ADR 0106 §3, FIG-3814; ADR 0115
//! §3.2–3.3).
//!
//! Every value a Lash Restate object retains — the durable-wait index's
//! metadata, wait, resolution, marker and membership rows, the effect-group
//! index record, and the effect-group payload's bytes and retirement fence —
//! is stored under an explicit [`StampedValue`] envelope. Readers dispatch on
//! the `format` stamp: the newest format decodes directly, a stamp one
//! format behind goes through the family's N-1 upcaster hook, and any other
//! stamp is refused before the handler acts, with a typed terminal error
//! Restate does not retry. Writers stamp the format the fleet selects
//! ([`StoredValueFormats::writer`]), never simply the newest one this build
//! knows: before a finalize, a newer build writes what the older one reads.
//!
//! The stamp lives in the value, not in the key: object keys and service
//! names stay stable across format changes, and a format move is an in-place
//! lazy upcast (a sweep is the operator's lever, not the type system's).
//!
//! Beside its values every object keeps one [`ObjectCompat`] record under
//! [`COMPAT_KEY`], which every handler reads first ([`admit_exclusive`],
//! [`admit_shared`]): it names the oldest family formats a build must read
//! to read the object, or write to mutate it. The record is never
//! enveloped, and clearing an object keeps it.

use std::sync::Arc;

use lash_core::{
    FleetFormat, FleetFormatStore, RuntimeEffectControllerError, RuntimeErrorCode, SurfaceFormat,
};
use lash_core_store::compat::{CompatRefusal, ComponentId, descriptor};
use restate_sdk::context::{
    ContextReadState, ContextWriteState, ObjectContext, SharedObjectContext,
};
use restate_sdk::errors::TerminalError;
use restate_sdk::serde::Json;
use serde::{Deserialize, Serialize, de::DeserializeOwned};

use crate::compat::{COMPAT_KEY, ObjectCompat};

/// The field a stamped object-state value carries its format under.
const FORMAT_FIELD: &str = "format";
/// The field a stamped object-state value carries the value itself under.
const BODY_FIELD: &str = "body";

/// The on-state envelope every value written to a Lash Restate object wears:
/// `{ "format": F, "body": <the value> }`.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct StampedValue<T> {
    pub(crate) format: u32,
    pub(crate) body: T,
}

/// An N-1 upcaster: the stored `body` of a value written under a registered
/// previous format goes in, and the body the newest format expects comes
/// out. No upcaster is registered anywhere yet — the first stamped layout is
/// the 1.0 baseline — but the slot is where each lands when a format moves.
pub(crate) type Upcast = fn(serde_json::Value) -> Result<serde_json::Value, TerminalError>;

/// A family's `(format, upcaster)` table.
pub(crate) type UpcastTable = [(u32, Upcast)];

/// The stored formats one family of stamped values admits: the registered
/// `surface` whose constant is the newest format this build knows, plus
/// `upcast_n1`, the N-1 upcaster hooks for values one format behind. `what`
/// names the family in refusals an operator reads.
pub(crate) struct StoredValueFormats {
    pub what: &'static str,
    pub surface: SurfaceFormat,
    pub upcast_n1: &'static UpcastTable,
}

/// One Restate object family: the component its `_compat` record is
/// admitted against, and the formats its values are stamped with.
pub(crate) struct ObjectFamily {
    pub component: ComponentId,
    pub formats: &'static StoredValueFormats,
}

impl StoredValueFormats {
    /// The newest format this build reads natively.
    pub(crate) fn newest(&self) -> u32 {
        self.surface.build_newest()
    }

    /// The writer the fleet selects for this family: the format `fleet`
    /// pins the family's surface to, with the family's down-converters (none
    /// at 1.0).
    pub(crate) fn writer(&self, fleet: FleetFormat) -> StoredValueWriter {
        StoredValueWriter {
            format: fleet.writer_version(self.surface),
        }
    }
}

/// The format one handler invocation writes a family's values at, as the
/// fleet selected it ([`StoredValueFormats::writer`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct StoredValueWriter {
    format: u32,
}

/// Where a deployment's object handlers read the fleet epoch `F` from: the
/// deployment store's last observed `F` (ADR 0115 §2.3, §3.3). A view over
/// no store — a handler a test binds by itself — answers this build's own
/// epoch, the only one a store without a row could record.
#[derive(Clone, Default)]
pub(crate) struct FleetView(Option<Arc<dyn FleetFormatStore>>);

impl FleetView {
    pub(crate) fn of(store: Arc<dyn FleetFormatStore>) -> Self {
        Self(Some(store))
    }

    /// The epoch this invocation writes under.
    pub(crate) fn fleet_format(&self) -> FleetFormat {
        self.0
            .as_ref()
            .map_or_else(FleetFormat::current, |store| store.fleet_format())
    }
}

impl std::fmt::Debug for FleetView {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_tuple("FleetView")
            .field(&self.fleet_format())
            .finish()
    }
}

/// An object an exclusive handler admitted: the writer its values are
/// stamped with, and the `_compat` record it keeps.
#[derive(Clone, Copy, Debug)]
pub(crate) struct AdmittedObject {
    pub(crate) writer: StoredValueWriter,
    compat: ObjectCompat,
}

impl AdmittedObject {
    /// Clear every value the object holds and keep its `_compat` record: a
    /// cleared or retired object's record is what fences a stale handler
    /// from recreating its state.
    pub(crate) fn clear_all(&self, ctx: &ObjectContext<'_>) {
        ctx.clear_all();
        ctx.set(COMPAT_KEY, Json(self.compat));
    }
}

/// Whether `key` names a value of the object's family, not its `_compat`
/// record.
pub(crate) fn is_value_key(key: &str) -> bool {
    key != COMPAT_KEY
}

/// The `_compat` gate of an exclusive handler (ADR 0115 §3.2), run after the
/// wire selection and before any other state is read.
///
/// The record must admit this build as a reader and a writer. An object with
/// no record and no other keys is fresh: the handler stamps it at the format
/// the fleet selects and proceeds. A populated object without one is
/// `Unstamped`. A refusal is terminal and changes nothing.
///
/// The record and the key listing are two reads, and they agree: the
/// handler holds the object's lock across every attempt, so no other
/// invocation writes between them, whichever attempt made each.
pub(crate) async fn admit_exclusive(
    ctx: &ObjectContext<'_>,
    family: &ObjectFamily,
    fleet: FleetFormat,
) -> Result<AdmittedObject, TerminalError> {
    let writer = family.formats.writer(fleet);
    let compat = match read_compat(ctx.get::<Vec<u8>>(COMPAT_KEY).await?, family)? {
        Some(compat) => {
            check_compat(compat, family, Access::Write).map_err(crate::wire::incompatible)?;
            compat
        }
        None => {
            if !ctx.get_keys().await?.is_empty() {
                return Err(unstamped(family));
            }
            let compat = ObjectCompat::fresh(writer.format);
            ctx.set(COMPAT_KEY, Json(compat));
            compat
        }
    };
    Ok(AdmittedObject { writer, compat })
}

/// The `_compat` gate of an exclusive handler that writes nothing: it runs
/// exclusive only to order its read against the object's writers. The
/// record must admit this build as a reader, as on a shared handler, and a
/// fresh object stays unstamped, so the read changes no state. Its two
/// reads agree as [`admit_exclusive`]'s do.
pub(crate) async fn admit_exclusive_read(
    ctx: &ObjectContext<'_>,
    family: &ObjectFamily,
) -> Result<(), TerminalError> {
    match read_compat(ctx.get::<Vec<u8>>(COMPAT_KEY).await?, family)? {
        Some(compat) => {
            check_compat(compat, family, Access::Read).map_err(crate::wire::incompatible)
        }
        None if ctx.get_keys().await?.is_empty() => Ok(()),
        None => Err(unstamped(family)),
    }
}

/// The `_compat` gate of a shared handler: the record must admit this build
/// as a reader. A shared handler never writes, so a fresh object stays
/// unstamped; a populated object without a record is `Unstamped`.
///
/// A shared handler holds no lock, so an exclusive writer runs beside it,
/// and its two state reads are two views: each read is journaled with the
/// value it saw, and an attempt that replays answers a recorded read from
/// the attempt that made it and a new read from its own, newer snapshot.
/// `Unstamped` is therefore decided on the one key listing: value keys
/// without `_compat` in the same view. Every writer stamps `_compat` before
/// its first value and keeps it until it clears the whole object, so no view
/// of a stamped object shows values without the record. The record is read
/// only once the listing names it; if it is gone by then, a clear emptied
/// the object in between, and an empty object is admitted.
pub(crate) async fn admit_shared(
    ctx: &SharedObjectContext<'_>,
    family: &ObjectFamily,
) -> Result<(), TerminalError> {
    let keys = ctx.get_keys().await?;
    if !keys.iter().any(|key| key == COMPAT_KEY) {
        return if keys.iter().any(|key| is_value_key(key)) {
            Err(unstamped(family))
        } else {
            Ok(())
        };
    }
    match read_compat(ctx.get::<Vec<u8>>(COMPAT_KEY).await?, family)? {
        Some(compat) => {
            check_compat(compat, family, Access::Read).map_err(crate::wire::incompatible)
        }
        None => Ok(()),
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Access {
    Read,
    Write,
}

fn unstamped(family: &ObjectFamily) -> TerminalError {
    crate::wire::incompatible(CompatRefusal::Unstamped {
        component: family.component.as_str().to_owned(),
        writing_release: None,
    })
}

fn read_compat(
    bytes: Option<Vec<u8>>,
    family: &ObjectFamily,
) -> Result<Option<ObjectCompat>, TerminalError> {
    bytes
        .map(|bytes| {
            serde_json::from_slice::<ObjectCompat>(&bytes).map_err(|error| {
                crate::wire::incompatible(CompatRefusal::MalformedStamp {
                    component: family.component.as_str().to_owned(),
                    detail: format!("`{COMPAT_KEY}` does not decode: {error}"),
                    writing_release: None,
                })
            })
        })
        .transpose()
}

/// Whether `compat` admits this build to read, or to mutate, an object of
/// `family`.
fn check_compat(
    compat: ObjectCompat,
    family: &ObjectFamily,
    access: Access,
) -> Result<(), CompatRefusal> {
    let component = || family.component.as_str().to_owned();
    if compat.format == 0 || compat.min_reader == 0 || compat.min_writer == 0 {
        return Err(CompatRefusal::MalformedStamp {
            component: component(),
            detail: format!(
                "format {}, reader floor {} and writer floor {} must each be at least 1",
                compat.format, compat.min_reader, compat.min_writer
            ),
            writing_release: None,
        });
    }
    let Some(declared) = descriptor(family.component) else {
        return Err(CompatRefusal::MalformedStamp {
            component: component(),
            detail: "this build declares no descriptor for the component".to_owned(),
            writing_release: None,
        });
    };
    if compat.min_reader > declared.reads.max() {
        return Err(CompatRefusal::ReaderFloorAbove {
            component: component(),
            found: compat.format,
            min_reader: compat.min_reader,
            reads: declared.reads,
            writing_release: None,
        });
    }
    if access == Access::Write && compat.min_writer > declared.writes.max() {
        return Err(CompatRefusal::WriterFloorAbove {
            component: component(),
            found: compat.format,
            min_writer: compat.min_writer,
            writes: declared.writes,
        });
    }
    Ok(())
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

/// Decode one raw stamped value: bytes that are not JSON predate the
/// envelope and are the typed "unstamped" refusal, never an SDK
/// deserialization trap.
pub(crate) fn decode_stamped_bytes<T: DeserializeOwned>(
    key: &str,
    bytes: &[u8],
    formats: &StoredValueFormats,
) -> Result<T, TerminalError> {
    let raw = serde_json::from_slice::<serde_json::Value>(bytes)
        .map_err(|_| stored_format_terminal(key, None, formats))?;
    decode_stamped_value(key, raw, formats)
}

/// Write one stamped value at the format the fleet selected.
pub(crate) fn set_stamped<'ctx, C, T>(ctx: &C, key: &str, writer: StoredValueWriter, body: T)
where
    C: ContextWriteState<'ctx>,
    T: Serialize + 'static,
{
    ctx.set(
        key,
        Json(StampedValue {
            format: writer.format,
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
        Some(stamp) if stamp == u64::from(formats.newest()) => body,
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
            formats.what,
            formats.newest()
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
            formats.newest()
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
/// that is what `message` is: a stored value's stamp this build does not
/// read, or an object whose `_compat` record refuses it (ADR 0115).
pub(crate) fn stored_format_error_in(message: &str) -> Option<RuntimeEffectControllerError> {
    if let Some(crate::wire::RestateCompatError::Incompatible { refusal }) =
        crate::wire::restate_compat_error_in(message)
    {
        return Some(RuntimeEffectControllerError::new(
            RuntimeErrorCode::EngineObjectStateFormatUnsupported,
            refusal.to_string(),
        ));
    }
    serde_json::from_str::<RuntimeEffectControllerError>(message)
        .ok()
        .filter(|error| error.code == RuntimeErrorCode::EngineObjectStateFormatUnsupported)
}

/// The typed stored-format refusal an index handler answered an ingress call
/// with, recovered from the terminal error's message in the response body.
pub(crate) fn ingress_stored_format_refusal(
    error: &crate::RestateHttpError,
) -> Option<RuntimeEffectControllerError> {
    let crate::RestateHttpError::Status { body, .. } = error else {
        return None;
    };
    let message = serde_json::from_str::<serde_json::Value>(body)
        .ok()?
        .get("message")?
        .as_str()?
        .to_owned();
    stored_format_error_in(&message)
}

#[cfg(test)]
mod tests {
    use super::*;

    const TEST_OBJECT_FORMAT_VERSION: u32 = 2;

    const FORMATS: StoredValueFormats = StoredValueFormats {
        what: "test-object",
        surface: lash_core::surface_format!(TEST_OBJECT_FORMAT_VERSION),
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

    fn stamped(format: u32, body: serde_json::Value) -> serde_json::Value {
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
