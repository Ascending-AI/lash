//! The fleet-format row (ADR 0106 §1 `F`): the one durable fact that names the
//! durable-format generation every writer in the fleet emits.
//!
//! `F` is deployment state, not a build constant: the store carries one row
//! recording it, the schema-open transaction seeds it, and every durable
//! writer consults the value the store reports rather than a version it baked
//! in. Until the first format upgrade the answer is always
//! [`FLEET_FORMAT_VERSION`] — the row's presence is what matters now, because
//! `lash admin finalize-upgrade` (FIG-3800) is the only operation that ever
//! moves it, and a fleet without the row has nowhere for that flip to land.
//!
//! The read side deliberately mirrors [`super::StoreReleaseState`]: a store that has
//! never been opened carries no row, and a row that cannot be read is a
//! different finding from an absent one, so the state is a three-arm enum
//! rather than an `Option`.

use std::ops::RangeInclusive;

use super::{
    ensure_supported_record_schema_version, ensure_supported_schema_version, record_schema_version,
};
use crate::StoreError;
use crate::compat::{CompatRefusal, VersionRange};

/// The fleet format every writer this build runs emits — the only `F` this
/// build's writable range admits.
///
/// ADR 0106 gives a build a range `[min_F, max_F]`; before the first upgrade
/// the range is one version wide and this constant is it. The first format
/// upgrade introduces `min_F`/`max_F` and widens the range; until then a store
/// recording any other value is refused at open.
pub const FLEET_FORMAT_VERSION: u32 = 1;

/// The fleet epochs this build writes under: `[F_prev, F_self]` (ADR 0115
/// §2.1).
///
/// `F` is the release compatibility epoch. A compatibility release declares
/// the epoch of the release before it and its own, and its finalize moves `F`
/// to its own even when no format changed, because moving it is what fences
/// the old release's writers. 1.0 is the first release, so its range is the
/// one epoch it introduces.
pub const FLEET_WRITABLE_RANGE: VersionRange = VersionRange::exactly(FLEET_FORMAT_VERSION);

/// The fleet format a store records, as the fleet-format row reports it.
///
/// Writers never read the integer directly — they ask for their per-format
/// writer version through [`FleetFormat::writer_version`], so a superseded
/// fleet format's pin lives in exactly one place.
///
/// The value carries the pin table [`writer_version`](Self::writer_version)
/// resolves against — [`WRITER_PINS`] for every recorded or current value —
/// so a test can stand a fleet format up on a different table and watch a
/// writer emit the pinned version rather than the build's own.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct FleetFormat {
    version: u32,
    pins: &'static [WriterPin],
}

/// One row of a fleet format's pin table: while `F` records `generation`,
/// every writer for the registered surface `constant` emits `version` — the
/// format the fleet agreed to write — instead of the build-newest version it
/// knows.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct WriterPin {
    /// The name the surface registers under in `scripts/versioned-surfaces.toml`.
    pub constant: &'static str,
    /// The fleet generation this pin applies at.
    pub generation: u32,
    /// The version the surface's writers emit while `F` is `generation`.
    pub version: u32,
}

impl FleetFormat {
    /// The fleet format an unrecorded store takes at open: this build's own.
    pub const fn current() -> Self {
        Self {
            version: FLEET_FORMAT_VERSION,
            pins: WRITER_PINS,
        }
    }

    /// The fleet format a recorded version names.
    pub const fn from_version(version: u32) -> Self {
        Self {
            version,
            pins: WRITER_PINS,
        }
    }

    /// The version the fleet-format row records.
    pub const fn version(self) -> u32 {
        self.version
    }

    /// The same fleet format resolving its writer versions against `pins`
    /// instead of [`WRITER_PINS`].
    ///
    /// Testing seam: a store's writers must emit the version the recorded `F`
    /// assigns, and that claim is only provable when the assigned version is
    /// not the constant the code would have stamped anyway.
    #[cfg(any(test, feature = "testing"))]
    pub const fn with_writer_pins(self, pins: &'static [WriterPin]) -> Self {
        Self { pins, ..self }
    }

    /// The fleet epochs this build writes under, [`FLEET_WRITABLE_RANGE`]
    /// (ADR 0115 §2.1).
    pub const fn writable() -> VersionRange {
        FLEET_WRITABLE_RANGE
    }

    /// The epoch a store records, admitted at open against the writable
    /// range `writable` of the build doing the opening (ADR 0115 §2.1).
    ///
    /// Below the range a compatibility release was skipped; above it the
    /// fleet is newer. Both refuse with
    /// [`CompatRefusal::FleetOutsideWritable`], before the store takes
    /// traffic.
    pub fn admit(recorded: u32, writable: VersionRange) -> Result<Self, StoreError> {
        if writable.contains(recorded) {
            Ok(Self::from_version(recorded))
        } else {
            Err(StoreError::Incompatible {
                refusal: CompatRefusal::FleetOutsideWritable { recorded, writable },
            })
        }
    }

    /// The epoch the writer fence read inside a mutating transaction,
    /// checked against the writable range (ADR 0115 §2.4).
    ///
    /// Outside the range a newer release has finalized: the answer is the
    /// terminal [`StoreError::WriterFenced`], and the transaction must roll
    /// back having written nothing. Whether an epoch inside the range differs
    /// from the one a commit's payloads were encoded under is the caller's
    /// comparison, because only it knows the encoding epoch.
    pub fn fence(recorded: u32, writable: VersionRange) -> Result<Self, StoreError> {
        if writable.contains(recorded) {
            Ok(Self::from_version(recorded))
        } else {
            Err(StoreError::WriterFenced { recorded, writable })
        }
    }

    /// The fleet-format versions this build can write under — the
    /// `[min_F, max_F]` writable range of ADR 0106 §1.
    ///
    /// Superseded by [`FleetFormat::writable`]; it stays until the store
    /// lanes move their opens onto [`FleetFormat::admit`].
    ///
    /// Before the first format upgrade the range is one version wide. The
    /// build that introduces the next durable format widens it here, and an
    /// open hands `admit_recorded` this range so a store carrying a
    /// generation outside it is refused rather than silently wound back.
    pub fn writable_range() -> RangeInclusive<u32> {
        FLEET_FORMAT_VERSION..=FLEET_FORMAT_VERSION
    }

    /// The fleet format a recorded `version` names, admitted against the
    /// writable range `writable` of the build doing the opening.
    ///
    /// `F` is fail-closed (ADR 0106 §7): a recorded value `writable` does not
    /// contain means the fleet writes a generation this build cannot emit, and
    /// the open is refused with the typed error an operator can route rather
    /// than allowed to stamp retired formats.
    ///
    /// Superseded by [`FleetFormat::admit`]; it stays until the store lanes
    /// move their opens onto it.
    pub fn admit_recorded(
        version: u32,
        writable: RangeInclusive<u32>,
    ) -> Result<Self, crate::StoreError> {
        if writable.contains(&version) {
            Ok(Self::from_version(version))
        } else {
            Err(crate::StoreError::FleetFormatOutsideWritableRange {
                recorded: version,
                current: FLEET_FORMAT_VERSION,
            })
        }
    }

    /// The version a durable writer emits for `surface` — the per-format
    /// mapping the fleet-format row stands for (FIG-3796).
    ///
    /// While `F` records a generation [`WRITER_PINS`] names for the surface,
    /// the fleet has agreed the surface writes the pinned version, and a
    /// build's newer format knowledge stays unwritten until
    /// `finalize-upgrade` moves the row (FIG-3800). A surface the fleet's
    /// generation does not pin writes its build-newest version — the identity
    /// the 1.0 fleet answers for every registered surface.
    pub fn writer_version(self, surface: SurfaceFormat) -> u32 {
        let constant = surface.constant_name();
        for pin in self.pins {
            if pin.constant == constant && pin.generation == self.version {
                return pin.version;
            }
        }
        surface.build_newest()
    }

    /// The recorded versions a reader of `surface` admits — ADR 0106 §2's
    /// `[N-1, N]` window expressed concretely: this build's newest version,
    /// plus the version `F` records for the surface, which is what the
    /// fleet's writers still emit while a finalize is pending (FIG-3796).
    ///
    /// A reader meets [`ReadWindow::newest`] payloads verbatim; a payload at
    /// `F`'s older version climbs to the newest through the surface's
    /// [`RecordUpcaster`] hooks before it decodes. Anything outside the pair
    /// is refused exactly as an exact-version decoder refuses it.
    ///
    /// Immutable history is read through a permanent floor instead (ADR 0115
    /// §5): a surface [`HISTORY_FLOORS`] names admits every version from its
    /// floor to the newest, whatever `F` says, so history written before a
    /// finalize stays readable after it. The floor is fail-closed: it reaches
    /// down only as far as the surface's [`RecordUpcaster`] chain lifts a
    /// payload to the newest, so no reader admits a version it cannot
    /// transform.
    pub fn read_window(self, surface: SurfaceFormat) -> ReadWindow {
        let newest = surface.build_newest();
        ReadWindow {
            newest,
            recorded: self.writer_version(surface),
            oldest: history_floor(surface)
                .map_or(newest, |floor| oldest_upcastable(surface, floor, newest)),
        }
    }
}

/// The versions a reader admits for one surface: what the fleet writes now,
/// what this build decodes natively, and, for immutable history, every
/// version from the surface's permanent floor up.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ReadWindow {
    newest: u32,
    recorded: u32,
    /// The oldest version admitted below `newest`: `newest` itself for a
    /// surface without a history floor.
    oldest: u32,
}

impl ReadWindow {
    /// The newest version this build knows for the surface.
    pub const fn newest(self) -> u32 {
        self.newest
    }

    /// The version `F` records for the surface — the version the fleet's
    /// writers emit while a finalize is pending.
    pub const fn recorded(self) -> u32 {
        self.recorded
    }

    /// The oldest version the window admits through a history floor; the
    /// newest version when the surface has none.
    pub const fn oldest(self) -> u32 {
        self.oldest
    }

    /// Whether a reader admits `version`: `F`'s recorded version for the
    /// surface (FIG-3796), or any version in `[oldest, newest]`, which is the
    /// build's newest alone unless the surface has a history floor.
    pub fn admits(self, version: u32) -> bool {
        version == self.recorded || (self.oldest <= version && version <= self.newest)
    }
}

/// One immutable-history surface and its permanent read floor (ADR 0115 §5):
/// the oldest version of the surface any build of this line still reads.
/// `F` never moves it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HistoryFloor {
    /// The name the surface registers under in `scripts/versioned-surfaces.toml`.
    pub constant: &'static str,
    /// The oldest version readers admit, lifted to the newest through the
    /// surface's [`RecordUpcaster`] chain.
    pub floor: u32,
}

/// The surfaces whose records are immutable history: preserved byte for
/// byte, never rewritten, and read through a permanent floor rather than
/// `F`'s `{recorded, newest}` pair (ADR 0115 §5).
///
/// Every floor is 1, the first version at the cut; the upcasters that let a
/// later build read below its newest are FIG-3802's.
pub const HISTORY_FLOORS: &[HistoryFloor] = &[
    HistoryFloor {
        constant: "SESSION_NODE_BODY_SCHEMA_VERSION",
        floor: 1,
    },
    HistoryFloor {
        constant: "SESSION_CHECKPOINT_SCHEMA_VERSION",
        floor: 1,
    },
    HistoryFloor {
        constant: "CHECKPOINT_COMPONENT_ENCODING_VERSION",
        floor: 1,
    },
    HistoryFloor {
        constant: "RUNTIME_COMMIT_RECEIPT_SCHEMA_VERSION",
        floor: 1,
    },
    HistoryFloor {
        constant: "RLM_SNAPSHOT_VERSION",
        floor: 1,
    },
    HistoryFloor {
        constant: "LASHLANG_SNAPSHOT_VERSION",
        floor: 1,
    },
    HistoryFloor {
        constant: "HEAP_SIZE_SCHEDULE_VERSION",
        floor: 1,
    },
    HistoryFloor {
        constant: "NATIVE_TRANSPORT_VERSION",
        floor: 1,
    },
    HistoryFloor {
        constant: "PROCESS_EVENT_VOCABULARY_VERSION",
        floor: 1,
    },
];

/// The permanent floor of `surface`, when it is immutable history.
pub fn history_floor(surface: SurfaceFormat) -> Option<u32> {
    let constant = surface.constant_name();
    HISTORY_FLOORS
        .iter()
        .find(|entry| entry.constant == constant)
        .map(|entry| entry.floor)
}

/// The oldest version at or above `floor` from which the surface's upcaster
/// chain reaches `newest` unbroken.
fn oldest_upcastable(surface: SurfaceFormat, floor: u32, newest: u32) -> u32 {
    let mut oldest = newest;
    while oldest > floor && upcast_chain_covers(surface, oldest - 1, newest) {
        oldest -= 1;
    }
    oldest
}

/// One registered transform lifting a registered surface's recorded payload
/// from `from_version` to `from_version + 1` — a reader's upcaster hook
/// (FIG-3796).
///
/// A build that bumps a surface's format registers the row here, and every
/// reader that admits the older half of its window lifts the payload a
/// generation at a time until it reaches the build's newest shape. The hook
/// owns the whole lift for its step: the payload tree it leaves is the next
/// generation's, including the record's `schema_version` field.
pub struct RecordUpcaster {
    /// The name the surface registers under in `scripts/versioned-surfaces.toml`.
    pub constant: &'static str,
    /// The recorded version the row lifts from.
    pub from_version: u32,
    /// The transform applied to the record's decoded JSON tree.
    pub upcast: fn(&mut serde_json::Value) -> Result<(), crate::StoreError>,
}

/// The upcaster hooks readers consult when a recorded version is older than
/// the build's newest — the transform half of the `[N-1, N]` window
/// (FIG-3796).
///
/// 1.0 registers none: every registered surface is at its first shipped
/// generation, so no `N-1` payload can exist and no transform is owed yet.
/// The slot exists so the first format bump hangs its transform in one
/// visible place rather than teaching each decoder a new rule.
pub const RECORD_UPCASTERS: &[RecordUpcaster] = &[];

/// The identity of one registered durable surface — what a writer or reader
/// asks `F` about.
///
/// `constant` is the name the surface registers under in
/// `scripts/versioned-surfaces.toml`; `build_newest` is that constant's own
/// value: the newest format version this build knows for the surface. The
/// pair travels together so a lookup can never name one surface while
/// pricing another's version.
#[derive(Clone, Copy, Debug)]
pub struct SurfaceFormat {
    constant: &'static str,
    build_newest: u32,
}

impl SurfaceFormat {
    /// The registered surface `constant` — the name keys the fleet's
    /// per-format pins, and `build_newest` is the constant's own value.
    ///
    /// Call sites spell both halves as one constant; the registry check
    /// refuses a pair whose string and value name different constants.
    pub const fn of(constant: &'static str, build_newest: u32) -> Self {
        Self {
            constant,
            build_newest,
        }
    }

    /// The name the surface registers under in `scripts/versioned-surfaces.toml`.
    ///
    /// A spelled path normalizes to its last segment: the registry names
    /// surfaces by their constant, not by the path a call site wrote.
    pub fn constant_name(self) -> &'static str {
        self.constant.rsplit("::").next().unwrap_or(self.constant)
    }

    /// The newest format version this build knows for the surface.
    pub const fn build_newest(self) -> u32 {
        self.build_newest
    }
}

/// The writer versions every recorded `F` pins registered surfaces to — the
/// build's own table of "while the fleet writes generation `generation`,
/// surface `constant` emits `version`".
///
/// The table is empty while the fleet writes its only generation: an
/// unpinned surface writes build-newest, and the first format upgrade adds
/// the rows that hold a superseded surface's writers at the old version until
/// `finalize-upgrade` moves `F`.
const WRITER_PINS: &[WriterPin] = &[];

/// The [`SurfaceFormat`] a call site hands [`FleetFormat::writer_version`]
/// names a registered surface and carries the constant's own value as its
/// build-newest version, so the name and version cannot diverge.
#[macro_export]
macro_rules! surface_format {
    ($constant:expr) => {
        $crate::store::SurfaceFormat::of(::core::stringify!($constant), $constant as u32)
    };
}

impl std::fmt::Display for FleetFormat {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.version)
    }
}

/// What a store said about the fleet's write generation.
///
/// Three answers rather than an `Option`, for the reason
/// [`super::StoreReleaseState`] takes the same shape: a store that carries no
/// fleet-format row and a row that could not be read are different findings,
/// and collapsing them into "absent" would report an unreadable row as an
/// observed absence.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
#[non_exhaustive]
pub enum FleetFormatState {
    /// The store records the fleet format its writers emit.
    Recorded(FleetFormat),
    /// The store carries no fleet-format row: either nothing has opened it
    /// yet, or it was last written by a build that predates the row.
    ///
    /// This is the default because a handle that has read nothing has observed
    /// no row, and the absence is the honest starting point.
    #[default]
    Unrecorded,
    /// The row exists but could not be read. Carries the backend's own words.
    Unreadable {
        /// The backend's diagnostic, verbatim.
        reason: String,
    },
}

impl FleetFormatState {
    /// The recorded fleet format, when one was read.
    pub fn fleet_format(&self) -> Option<FleetFormat> {
        match self {
            FleetFormatState::Recorded(format) => Some(*format),
            FleetFormatState::Unrecorded | FleetFormatState::Unreadable { .. } => None,
        }
    }
}

impl std::fmt::Display for FleetFormatState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            FleetFormatState::Recorded(format) => {
                write!(f, "fleet format {format} (writers emit this generation)")
            }
            FleetFormatState::Unrecorded => {
                write!(f, "no fleet-format row (nothing has recorded this store)")
            }
            FleetFormatState::Unreadable { reason } => {
                write!(f, "fleet-format row unreadable: {reason}")
            }
        }
    }
}

/// Implements [`FleetFormatStore`](crate::store::FleetFormatStore) for a store
/// with no recorded fleet-format row — in-memory fakes and test stores —
/// answering [`FleetFormat::current`], the only generation such a store could
/// write (FIG-3796).
#[macro_export]
macro_rules! impl_current_fleet_format {
    ($ty:ty) => {
        impl $crate::store::FleetFormatStore for $ty {
            fn fleet_format(&self) -> $crate::store::FleetFormat {
                $crate::store::FleetFormat::current()
            }
        }
    };
}

/// The fleet leg of [`ensure_supported_schema_version`]: `actual` is admitted
/// when `fleet`'s read window for `surface` admits it — this build's newest
/// version, or the version `F` records for the surface (FIG-3796, ADR 0106
/// §2's `[N-1, N]` reader window). A version outside the window is refused
/// exactly as the exact-version check refuses it.
pub fn ensure_supported_schema_version_for_fleet(
    record_kind: &'static str,
    actual: u32,
    surface: SurfaceFormat,
    fleet: FleetFormat,
) -> Result<(), StoreError> {
    let window = fleet.read_window(surface);
    if window.admits(actual) {
        Ok(())
    } else {
        ensure_supported_schema_version(record_kind, actual, window.newest())
    }
}

/// The fleet leg of [`ensure_supported_record_schema_version`]: extracts the
/// persisted `schema_version` with the same missing/invalid refusals, admits
/// it when `fleet`'s read window for `surface` admits it, and hands back the
/// version so the caller can lift the payload through the surface's
/// [`RecordUpcaster`] hooks before decode (FIG-3796).
pub fn ensure_supported_record_schema_version_for_fleet(
    record_kind: &'static str,
    value: &serde_json::Value,
    surface: SurfaceFormat,
    fleet: FleetFormat,
) -> Result<u32, StoreError> {
    let window = fleet.read_window(surface);
    let actual = record_schema_version(record_kind, value, window.newest())?;
    if !window.admits(actual) {
        ensure_supported_schema_version(record_kind, actual, window.newest())?;
    }
    Ok(actual)
}

/// The fleet leg of [`decode_versioned_json_record`]: a persisted record
/// admits the version `fleet` records for `surface` as well as this build's
/// newest — the `[N-1, N]` window ADR 0106 §2 gives readers while a finalize
/// is pending. An admitted older payload climbs to the newest through the
/// surface's [`RecordUpcaster`] hooks before it decodes; a version outside
/// the window is refused exactly as the exact-version decoder refuses it.
pub fn decode_versioned_json_record_for_fleet<T>(
    json: &str,
    record_kind: &'static str,
    surface: SurfaceFormat,
    fleet: FleetFormat,
) -> Result<T, StoreError>
where
    T: serde::de::DeserializeOwned,
{
    let mut value: serde_json::Value = serde_json::from_str(json)
        .map_err(|err| StoreError::Backend(format!("failed to decode {record_kind}: {err}")))?;
    let actual =
        ensure_supported_record_schema_version_for_fleet(record_kind, &value, surface, fleet)?;
    let window = fleet.read_window(surface);
    if actual != window.newest() {
        upcast_json_record(record_kind, surface, actual, window.newest(), &mut value)?;
    }
    serde_json::from_value(value)
        .map_err(|err| StoreError::Backend(format!("failed to decode {record_kind}: {err}")))
}

/// Lifts `value` from `from_version` to `to_version` for `surface` through
/// the registered [`RecordUpcaster`] hooks, one generation per row
/// (FIG-3796).
///
/// The walk is fail-closed: a step with no hook refuses the record rather
/// than decoding a half-lifted payload, so a reader that admits an older
/// version without a transform reports the same unsupported-version error
/// the exact-version path reports.
pub fn upcast_json_record(
    record_kind: &'static str,
    surface: SurfaceFormat,
    from_version: u32,
    to_version: u32,
    value: &mut serde_json::Value,
) -> Result<(), StoreError> {
    let mut version = from_version;
    while version != to_version {
        let Some(hook) = RECORD_UPCASTERS
            .iter()
            .find(|hook| hook.constant == surface.constant_name() && hook.from_version == version)
        else {
            return Err(StoreError::UnsupportedRecordSchemaVersion {
                record_kind,
                actual: from_version,
                expected: to_version,
            });
        };
        (hook.upcast)(value)?;
        version += 1;
    }
    Ok(())
}

/// Whether a [`RecordUpcaster`] chain can lift `surface`'s recorded
/// `from_version` to `to_version` — the fleetless leg of the reader window
/// for decode sites (serde impls, wire parsers) that cannot consult `F`
/// directly. A surface with no registered chain admits exactly its newest
/// version, which is the whole of its window while no `N-1` exists
/// (FIG-3796).
pub fn upcast_chain_covers(surface: SurfaceFormat, from_version: u32, to_version: u32) -> bool {
    let mut version = from_version;
    while version != to_version {
        let Some(hook) = RECORD_UPCASTERS
            .iter()
            .find(|hook| hook.constant == surface.constant_name() && hook.from_version == version)
        else {
            return false;
        };
        let _ = hook;
        version += 1;
    }
    true
}

pub fn decode_versioned_json_record<T>(
    json: &str,
    record_kind: &'static str,
    expected: u32,
) -> Result<T, StoreError>
where
    T: serde::de::DeserializeOwned,
{
    let value: serde_json::Value = serde_json::from_str(json)
        .map_err(|err| StoreError::Backend(format!("failed to decode {record_kind}: {err}")))?;
    ensure_supported_record_schema_version(record_kind, &value, expected)?;
    serde_json::from_value(value)
        .map_err(|err| StoreError::Backend(format!("failed to decode {record_kind}: {err}")))
}

/// The MessagePack counterpart of [`decode_versioned_json_record_for_fleet`]
/// (FIG-3796): the record admits this build's newest and the version `fleet`
/// records for `surface`; an admitted older payload climbs through the
/// surface's [`RecordUpcaster`] hooks before decode; anything else is refused
/// exactly as the exact-version decode refuses it.
pub fn decode_versioned_msgpack_record_for_fleet<T>(
    bytes: &[u8],
    record_kind: &'static str,
    surface: SurfaceFormat,
    fleet: FleetFormat,
) -> Result<T, StoreError>
where
    T: serde::de::DeserializeOwned,
{
    let corrupt = |err: rmp_serde::decode::Error| StoreError::StoredDataCorrupt {
        record_kind,
        message: format!("failed to decode {record_kind}: {err}"),
    };
    let mut value: serde_json::Value = rmp_serde::from_slice(bytes).map_err(corrupt)?;
    let actual =
        ensure_supported_record_schema_version_for_fleet(record_kind, &value, surface, fleet)?;
    let window = fleet.read_window(surface);
    if actual == window.newest() {
        return rmp_serde::from_slice(bytes).map_err(corrupt);
    }
    upcast_json_record(record_kind, surface, actual, window.newest(), &mut value)?;
    serde_json::from_value(value).map_err(|err| StoreError::StoredDataCorrupt {
        record_kind,
        message: format!("failed to decode {record_kind}: {err}"),
    })
}

#[cfg(test)]
mod tests {
    use super::{FLEET_WRITABLE_RANGE, FleetFormat, SurfaceFormat, history_floor};
    use crate::StoreError;
    use crate::compat::{CompatRefusal, VersionRange};

    #[test]
    fn fleet_epoch_is_admitted_at_open_and_fenced_in_a_transaction() {
        assert_eq!(FleetFormat::writable(), FLEET_WRITABLE_RANGE);
        assert_eq!(FLEET_WRITABLE_RANGE, VersionRange::exactly(1));
        let writable = VersionRange::new(2, 3).expect("range");

        assert_eq!(
            FleetFormat::admit(3, writable).expect("inside").version(),
            3
        );
        for recorded in [1, 4] {
            let Err(StoreError::Incompatible {
                refusal:
                    CompatRefusal::FleetOutsideWritable {
                        recorded: r,
                        writable: w,
                    },
            }) = FleetFormat::admit(recorded, writable)
            else {
                panic!("F {recorded} outside {writable} must refuse at open");
            };
            assert_eq!((r, w), (recorded, writable));
        }

        assert_eq!(
            FleetFormat::fence(2, writable).expect("inside").version(),
            2
        );
        let Err(StoreError::WriterFenced {
            recorded,
            writable: w,
        }) = FleetFormat::fence(4, writable)
        else {
            panic!("F 4 outside {writable} must fence the writer");
        };
        assert_eq!((recorded, w), (4, writable));
    }

    #[test]
    fn history_is_read_through_its_permanent_floor() {
        let history = SurfaceFormat::of("SESSION_NODE_BODY_SCHEMA_VERSION", 1);
        assert_eq!(history_floor(history), Some(1));
        let window = FleetFormat::current().read_window(history);
        assert_eq!((window.oldest(), window.newest()), (1, 1));
        assert!(window.admits(1) && !window.admits(2));

        // Fail-closed: without an upcaster chain the floor reaches no lower
        // than the newest version.
        let unlifted = SurfaceFormat::of("crate::SESSION_CHECKPOINT_SCHEMA_VERSION", 4);
        let window = FleetFormat::current().read_window(unlifted);
        assert_eq!(window.oldest(), 4);
        assert!(window.admits(4) && !window.admits(1));

        // A surface that is not history keeps `F`'s `{recorded, newest}` pair.
        let mutable = SurfaceFormat::of("SESSION_HEAD_META_SCHEMA_VERSION", 3);
        assert_eq!(history_floor(mutable), None);
        let window = FleetFormat::current().read_window(mutable);
        assert_eq!(window.oldest(), 3);
        assert!(window.admits(3) && !window.admits(2));
    }
}
