//! Versioned values in Restate object state (ADR 0106 §3, FIG-3814; ADR 0115
//! §3.2–3.3; FIG-4041).
//!
//! Every value a Lash Restate object retains — the durable-wait index's
//! metadata, indexed wait, marker and membership rows, the effect-group
//! index record, and the effect-group payload's bytes and retirement fence —
//! is stored under an explicit [`StampedValue`] envelope. Readers dispatch on
//! the `format` stamp: the newest format decodes directly, an older stamp the
//! family's read window admits climbs to the newest through the family's
//! [`RecordUpcaster`](lash_core::store::RecordUpcaster) rows — the one lift
//! registry every guarded surface shares — and any other stamp is refused
//! before the handler acts, with a typed terminal error Restate does not
//! retry. Writers stamp the format the fleet selects
//! ([`StoredValueFormats::writer`]), never simply the newest one this build
//! knows: before a finalize, a newer build writes what the older one reads.
//!
//! The stamp lives in the value, not in the key: object keys and service
//! names stay stable across format changes. After a finalize every object
//! family's `upgrade` handler ([`upgrade_object`]) rewrites one object's
//! values at the newest format and raises its `_compat` record, and the
//! object sweep (`crate::object_upgrade`) calls it on every object the
//! preflight still lists at an older format.
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
pub(crate) const FORMAT_FIELD: &str = "format";
/// The field a stamped object-state value carries the value itself under.
const BODY_FIELD: &str = "body";

/// The on-state envelope every value written to a Lash Restate object wears:
/// `{ "format": F, "body": <the value> }`.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct StampedValue<T> {
    pub(crate) format: u32,
    pub(crate) body: T,
}

/// The stored formats one family of stamped values admits: the registered
/// `surface` whose constant is the newest format this build knows. Older
/// formats are admitted by the surface's read window and lifted by its
/// `RECORD_UPCASTERS` rows. `what` names the family in refusals an operator
/// reads.
pub(crate) struct StoredValueFormats {
    pub what: &'static str,
    pub surface: SurfaceFormat,
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

impl StoredValueWriter {
    /// The format the writer stamps.
    #[cfg(test)]
    pub(crate) fn format(self) -> u32 {
        self.format
    }
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

    pub(crate) fn weak_store(&self) -> Option<std::sync::Weak<dyn FleetFormatStore>> {
        self.0.as_ref().map(Arc::downgrade)
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
    let body = lift_stamped_value(key, raw, formats)?;
    serde_json::from_value(body).map_err(|error| {
        TerminalError::new(format!(
            "{} state {key} does not decode under stored format {}: {error}",
            formats.what,
            formats.newest()
        ))
    })
}

/// A raw object-state value's body, lifted to the newest format: a stamp in
/// the family's supported range climbs through the family's upcaster rows;
/// any other stamp, or none, is the typed refusal. Object readers consult no
/// `F`: a stamp is admitted exactly when the lift chain reaches the newest.
fn lift_stamped_value(
    key: &str,
    raw: serde_json::Value,
    formats: &StoredValueFormats,
) -> Result<serde_json::Value, TerminalError> {
    let stamp = raw.get(FORMAT_FIELD).and_then(serde_json::Value::as_u64);
    let mut body = raw
        .get(BODY_FIELD)
        .cloned()
        .unwrap_or(serde_json::Value::Null);
    let supported = FleetFormat::current()
        .read_window(formats.surface)
        .supported();
    let Some(stamp) = stamp else {
        return Err(stored_format_terminal(key, None, formats));
    };
    let Some(format) = u32::try_from(stamp)
        .ok()
        .filter(|format| supported.contains(*format))
    else {
        return Err(stored_format_terminal(key, Some(stamp), formats));
    };
    if format != formats.newest() {
        lash_core::store::upcast_json_record(
            formats.what,
            formats.surface,
            format,
            formats.newest(),
            &mut body,
        )
        .map_err(|error| {
            TerminalError::new(format!(
                "{} state {key} does not lift from stored format {format}: {error}",
                formats.what
            ))
        })?;
    }
    Ok(body)
}

/// What an object's `upgrade` handler did (ADR 0115 §3.2, FIG-4041). JSON is
/// tagged by `upgrade`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "upgrade", rename_all = "snake_case")]
pub enum ObjectUpgradeResponse {
    /// The object holds no state: nothing to convert, and nothing written.
    Absent,
    /// The fleet still writes format `writes`: finalize has not moved `F`,
    /// so nothing is rewritten while rollback is still promised.
    NotFinalized { writes: u32 },
    /// The object's `_compat` already names the newest format.
    Current { format: u32 },
    /// Every value was rewritten at `format` and `_compat` raised to it, in
    /// this one exclusive invocation.
    Upgraded { from: u32, format: u32 },
}

/// What one `upgrade` invocation does to an object, decided from what the
/// object holds before anything is written.
#[derive(Debug, PartialEq)]
pub(crate) enum ObjectUpgradePlan {
    /// Nothing is written.
    Unchanged(ObjectUpgradeResponse),
    /// Every value is written again at the newest format, lifted, and the
    /// `_compat` record is raised to `compat`, in one exclusive invocation.
    Rewrite {
        values: Vec<(String, serde_json::Value)>,
        compat: ObjectCompat,
        response: ObjectUpgradeResponse,
    },
}

/// The upgrade of one object of `family` under `fleet`, from the object's
/// `_compat` record and every other value it holds, as raw bytes.
///
/// The object must admit this build as a writer, exactly as any exclusive
/// handler requires. While the fleet still writes an older format the
/// answer is [`ObjectUpgradeResponse::NotFinalized`]: nothing is rewritten
/// while rollback is still promised. An object already at the newest
/// format is [`ObjectUpgradeResponse::Current`]. Otherwise each value is
/// lifted through the family's upcaster rows — a value that does not lift is
/// the typed refusal, and the object is left as it was — and the plan
/// rewrites them all and raises `_compat` to the newest format, so a
/// leftover older handler is refused by the record from then on.
pub(crate) fn plan_object_upgrade(
    family: &ObjectFamily,
    fleet: FleetFormat,
    compat: Option<ObjectCompat>,
    values: Vec<(String, Vec<u8>)>,
) -> Result<ObjectUpgradePlan, TerminalError> {
    let Some(compat) = compat else {
        return if values.is_empty() {
            Ok(ObjectUpgradePlan::Unchanged(ObjectUpgradeResponse::Absent))
        } else {
            Err(unstamped(family))
        };
    };
    check_compat(compat, family, Access::Write).map_err(crate::wire::incompatible)?;
    let formats = family.formats;
    let newest = formats.newest();
    let writes = formats.writer(fleet).format;
    if writes != newest {
        return Ok(ObjectUpgradePlan::Unchanged(
            ObjectUpgradeResponse::NotFinalized { writes },
        ));
    }
    if compat.format >= newest {
        return Ok(ObjectUpgradePlan::Unchanged(
            ObjectUpgradeResponse::Current {
                format: compat.format,
            },
        ));
    }
    let values = values
        .into_iter()
        .map(|(key, bytes)| {
            let raw = serde_json::from_slice::<serde_json::Value>(&bytes)
                .map_err(|_| stored_format_terminal(&key, None, formats))?;
            let body = lift_stamped_value(&key, raw, formats)?;
            Ok((key, body))
        })
        .collect::<Result<Vec<_>, TerminalError>>()?;
    Ok(ObjectUpgradePlan::Rewrite {
        values,
        compat: ObjectCompat::fresh(newest),
        response: ObjectUpgradeResponse::Upgraded {
            from: compat.format,
            format: newest,
        },
    })
}

/// The `upgrade` handler body every object family binds (ADR 0115 §3.2,
/// FIG-4041): read the object's `_compat` record and values, plan the
/// upgrade with [`plan_object_upgrade`], and apply it. It runs exclusive, so
/// the reads and the writes see no other invocation between them, and a
/// retried attempt plans from the same state.
pub(crate) async fn upgrade_object(
    ctx: &ObjectContext<'_>,
    family: &ObjectFamily,
    fleet: FleetFormat,
) -> Result<ObjectUpgradeResponse, TerminalError> {
    let compat = read_compat(ctx.get::<Vec<u8>>(COMPAT_KEY).await?, family)?;
    let mut values = Vec::new();
    for key in ctx.get_keys().await? {
        if !is_value_key(&key) {
            continue;
        }
        if let Some(bytes) = ctx.get::<Vec<u8>>(&key).await? {
            values.push((key, bytes));
        }
    }
    match plan_object_upgrade(family, fleet, compat, values)? {
        ObjectUpgradePlan::Unchanged(response) => Ok(response),
        ObjectUpgradePlan::Rewrite {
            values,
            compat,
            response,
        } => {
            let writer = family.formats.writer(fleet);
            for (key, body) in values {
                set_stamped(ctx, &key, writer, body);
            }
            ctx.set(COMPAT_KEY, Json(compat));
            Ok(response)
        }
    }
}

/// The typed refusal of object state carrying a stamp this build does not
/// read. It travels as the terminal error's message, as the error's record,
/// so callers past a service boundary recover it with
/// [`typed_terminal`](crate::wire::typed_terminal).
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
    TerminalError::new(stored_format_error(formats.what, key, stamped, formats).to_record())
}

/// The typed stored-format refusal a handler's terminal error carries, if
/// that is what `message` is: a stored value's stamp this build does not
/// read, or an object whose `_compat` record refuses it (ADR 0115).
pub(crate) fn stored_format_error_in(message: &str) -> Option<RuntimeEffectControllerError> {
    crate::wire::typed_terminal(message)
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
    use crate::effect_group::EFFECT_GROUP_PAYLOAD_FAMILY;

    const FORMATS: &StoredValueFormats = EFFECT_GROUP_PAYLOAD_FAMILY.formats;

    fn newest() -> u32 {
        FORMATS.newest()
    }

    fn stamped(format: u32, body: serde_json::Value) -> serde_json::Value {
        serde_json::to_value(StampedValue { format, body }).expect("stamp a value")
    }

    fn stamped_bytes(format: u32, body: serde_json::Value) -> Vec<u8> {
        serde_json::to_vec(&stamped(format, body)).expect("encode a stamped value")
    }

    #[test]
    fn the_current_format_round_trips() {
        let raw = stamped(newest(), serde_json::json!({"name": "alpha", "count": 3}));
        let decoded: serde_json::Value =
            decode_stamped_value("state-key", raw, FORMATS).expect("current format decodes");
        assert_eq!(decoded["name"], "alpha");
        assert_eq!(decoded["count"], 3);
    }

    #[test]
    fn a_stamped_scalar_round_trips() {
        let raw = stamped(newest(), serde_json::json!("resolved"));
        let decoded: String =
            decode_stamped_value("state-key", raw, FORMATS).expect("scalar body decodes");
        assert_eq!(decoded, "resolved");
    }

    #[test]
    fn stamped_bytes_round_trip() {
        let bytes = vec![0u8, 1, 2, 250, 255];
        let raw = stamped(
            newest(),
            serde_json::to_value(&bytes).expect("bytes to value"),
        );
        let decoded: Vec<u8> =
            decode_stamped_value("state-key", raw, FORMATS).expect("bytes decode");
        assert_eq!(decoded, bytes);
    }

    /// The synthetic N+1 reads the payload family's format-1 values through
    /// the family's registered lift.
    #[cfg(feature = "synthetic-next")]
    #[test]
    fn a_predecessor_value_lifts_through_the_registry() {
        assert_eq!(newest(), 2);
        let raw = stamped(1, serde_json::json!([1, 2, 3]));
        let decoded: Vec<u8> = decode_stamped_value("state-key", raw, FORMATS)
            .expect("the family's lift answers a current body");
        assert_eq!(decoded, vec![1, 2, 3]);
    }

    fn values(format: u32) -> Vec<(String, Vec<u8>)> {
        vec![
            (
                "effect-group/v1/payload".to_owned(),
                stamped_bytes(format, serde_json::json!([7, 8])),
            ),
            (
                "effect-group/v1/retired".to_owned(),
                stamped_bytes(format, serde_json::json!(false)),
            ),
        ]
    }

    #[test]
    fn an_object_without_state_or_at_the_newest_format_is_left_alone() {
        let fleet = FleetFormat::current();
        assert_eq!(
            plan_object_upgrade(&EFFECT_GROUP_PAYLOAD_FAMILY, fleet, None, Vec::new())
                .expect("an empty object"),
            ObjectUpgradePlan::Unchanged(ObjectUpgradeResponse::Absent)
        );
        assert_eq!(
            plan_object_upgrade(
                &EFFECT_GROUP_PAYLOAD_FAMILY,
                fleet,
                Some(ObjectCompat::fresh(newest())),
                values(newest()),
            )
            .expect("a current object"),
            ObjectUpgradePlan::Unchanged(ObjectUpgradeResponse::Current { format: newest() })
        );
        // Values without a `_compat` record predate the stamp: refused, typed.
        let error = plan_object_upgrade(&EFFECT_GROUP_PAYLOAD_FAMILY, fleet, None, values(1))
            .expect_err("an unstamped object");
        assert!(stored_format_error_in(error.message()).is_some(), "{error}");
    }

    /// After finalize the plan lifts every value and raises `_compat`; before
    /// it, the plan writes nothing.
    #[cfg(feature = "synthetic-next")]
    #[test]
    fn an_object_at_the_predecessor_format_is_rewritten_only_after_finalize() {
        let before = FleetFormat::from_version(1);
        assert_eq!(
            plan_object_upgrade(
                &EFFECT_GROUP_PAYLOAD_FAMILY,
                before,
                Some(ObjectCompat::fresh(1)),
                values(1),
            )
            .expect("before finalize"),
            ObjectUpgradePlan::Unchanged(ObjectUpgradeResponse::NotFinalized { writes: 1 })
        );
        let after = FleetFormat::from_version(2);
        let ObjectUpgradePlan::Rewrite {
            values: rewritten,
            compat,
            response,
        } = plan_object_upgrade(
            &EFFECT_GROUP_PAYLOAD_FAMILY,
            after,
            Some(ObjectCompat::fresh(1)),
            values(1),
        )
        .expect("after finalize")
        else {
            panic!("a format-1 object is rewritten after finalize");
        };
        assert_eq!(compat, ObjectCompat::fresh(2));
        assert_eq!(
            response,
            ObjectUpgradeResponse::Upgraded { from: 1, format: 2 }
        );
        assert_eq!(
            rewritten,
            vec![
                (
                    "effect-group/v1/payload".to_owned(),
                    serde_json::json!([7, 8])
                ),
                (
                    "effect-group/v1/retired".to_owned(),
                    serde_json::json!(false)
                ),
            ]
        );
        // A value the family cannot lift leaves the whole object as it was.
        let mut foreign = values(1);
        foreign.push((
            "effect-group/v1/payload-next".to_owned(),
            stamped_bytes(3, serde_json::json!([])),
        ));
        let error = plan_object_upgrade(
            &EFFECT_GROUP_PAYLOAD_FAMILY,
            after,
            Some(ObjectCompat::fresh(1)),
            foreign,
        )
        .expect_err("an unliftable value refuses the upgrade");
        assert!(stored_format_error_in(error.message()).is_some(), "{error}");
    }

    #[test]
    fn unstamped_and_foreign_stamps_are_typed_refusals() {
        for raw in [
            serde_json::json!({"body": {"name": "legacy"}}),
            stamped(0, serde_json::json!({"name": "older"})),
            stamped(7, serde_json::json!({"name": "newer"})),
        ] {
            let error = decode_stamped_value::<serde_json::Value>("state-key", raw, FORMATS)
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
                typed.message.contains("effect-group payload")
                    && typed.message.contains("state-key")
                    && typed
                        .message
                        .contains(&format!("stored format {}", newest())),
                "{typed:?}"
            );
        }
    }

    #[test]
    fn a_current_body_that_does_not_decode_is_terminal() {
        let raw = stamped(newest(), serde_json::json!("a string body"));
        let error = decode_stamped_value::<serde_json::Map<String, serde_json::Value>>(
            "state-key",
            raw,
            FORMATS,
        )
        .expect_err("a corrupt current-format body is terminal");
        assert!(
            stored_format_error_in(error.message()).is_none(),
            "a decode fault is corruption, not a format refusal: {error}"
        );
        assert!(error.message().contains("does not decode"));
    }
}
