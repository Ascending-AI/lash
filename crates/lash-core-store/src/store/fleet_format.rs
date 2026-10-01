//! The fleet-format row (ADR 0106 §1 `F`): the one durable fact that names the
//! durable-format generation every writer in the fleet emits.
//!
//! `F` is deployment state, not a build constant: the store carries one row
//! recording it, the installer seeds it, and every durable
//! writer consults the value the store reports rather than a version it baked
//! in. Until the first format upgrade the answer is always
//! [`FLEET_FORMAT_VERSION`] — the row's presence is what matters now, because
//! `lashctl finalize` (FIG-3800) is the only operation that ever
//! moves it, and a fleet without the row has nowhere for that flip to land.
//!
//! The read side deliberately mirrors [`super::StoreReleaseState`]: a store that has
//! never been opened carries no row, and a row that cannot be read is a
//! different finding from an absent one, so the state is a three-arm enum
//! rather than an `Option`.

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
#[cfg(not(feature = "synthetic-next"))]
pub const FLEET_FORMAT_VERSION: u32 = 1;

/// Phase A's next release owns epoch 2 while it still writes under epoch 1.
#[cfg(feature = "synthetic-next")]
pub const FLEET_FORMAT_VERSION: u32 = 2;

/// The fleet epochs this build writes under: `[F_prev, F_self]` (ADR 0115
/// §2.1).
///
/// `F` is the release compatibility epoch. A compatibility release declares
/// the epoch of the release before it and its own, and its finalize moves `F`
/// to its own even when no format changed, because moving it is what fences
/// the old release's writers. 1.0 is the first release, so its range is the
/// one epoch it introduces.
#[cfg(not(feature = "synthetic-next"))]
pub const FLEET_WRITABLE_RANGE: VersionRange = VersionRange::exactly(FLEET_FORMAT_VERSION);

/// The synthetic compatibility release opens fleets at either epoch.
#[cfg(feature = "synthetic-next")]
pub const FLEET_WRITABLE_RANGE: VersionRange = VersionRange::between(1, FLEET_FORMAT_VERSION);

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
    /// This build's own epoch, `F_self`: what a store with no fleet-format
    /// row to read (an in-memory fake, a test store) reports. A durable store
    /// never takes it at open; its installer seeds [`Self::seed`].
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

    /// The epoch an installer records in a store that has none, for a build
    /// writing under `writable`: the range's floor, `F_prev` (ADR 0115 §2.1).
    ///
    /// Only finalize moves `F` to `F_self`, so a store provisioned by a
    /// compatibility release still starts inside the rollback window: the
    /// release before it reads everything written there. For 1.0 the floor is
    /// the one epoch it introduces. `lashctl migrate` seeds PostgreSQL with
    /// it; each SQLite database records it when its open-time migration
    /// provisions the database.
    pub const fn seed(writable: VersionRange) -> Self {
        Self::from_version(writable.min())
    }

    /// The epoch a store records, required present and admitted at open
    /// against `writable` (ADR 0115 §2.1).
    ///
    /// An open never records `F`: the installer seeds it. A store `component`
    /// with no recorded epoch refuses with [`CompatRefusal::FleetUnrecorded`]
    /// rather than deciding the fleet's epoch from whichever build opened it
    /// first; a recorded one goes through [`Self::admit`].
    pub fn admit_recorded(
        component: &str,
        recorded: Option<u32>,
        writable: VersionRange,
    ) -> Result<Self, StoreError> {
        match recorded {
            Some(recorded) => Self::admit(recorded, writable),
            None => Err(StoreError::Incompatible {
                refusal: CompatRefusal::FleetUnrecorded {
                    component: component.to_owned(),
                    writing_release: None,
                },
            }),
        }
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
                refusal: CompatRefusal::FleetOutsideWritable {
                    recorded,
                    writable,
                    writing_release: None,
                },
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

    /// The version a durable writer emits for `surface` — the per-format
    /// mapping the fleet-format row stands for (FIG-3796).
    ///
    /// While `F` records a generation [`WRITER_PINS`] names for the surface,
    /// the fleet has agreed the surface writes the pinned version, and a
    /// build's newer format knowledge stays unwritten until
    /// `lashctl finalize` moves the row (FIG-3800). A surface the fleet's
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

    /// Every pin this fleet format's table holds for `surface`, whatever
    /// generation `F` records: the static half of a reader's admission, which
    /// the recorded `F` selects one pin from (FIG-4454).
    pub fn writer_pins(self, surface: SurfaceFormat) -> Vec<WriterPin> {
        let constant = surface.constant_name();
        self.pins
            .iter()
            .filter(|pin| pin.constant == constant)
            .copied()
            .collect()
    }

    /// The recorded versions a reader of `surface` admits (ADR 0106 §2,
    /// ADR 0115 §5): the supported range of a [`GUARDED_SURFACES`] row, plus
    /// the version `F` pins the surface's writers to.
    ///
    /// A guarded surface admits every version from the oldest its
    /// [`RecordUpcaster`] chain lifts to the newest, up to this build's
    /// newest: `[oldest, newest]`. The range is fail-closed — it reaches down
    /// only as far as the chain is unbroken, so no reader admits a version it
    /// cannot transform — and `F` does not narrow it. That is what keeps a
    /// finalized fleet reading the rows and objects its predecessor wrote
    /// while backfills and object sweeps still run, and what keeps immutable
    /// history readable forever: a [`SurfaceReads::History`] surface reaches
    /// down to its permanent floor.
    ///
    /// A payload at an older admitted version climbs to the newest through
    /// the surface's [`RecordUpcaster`] rows before it decodes; anything
    /// outside the window is refused exactly as an exact-version decoder
    /// refuses it. A surface no row guards (a wire, a cursor) admits its
    /// newest version and `F`'s pin.
    pub fn read_window(self, surface: SurfaceFormat) -> ReadWindow {
        let newest = surface.build_newest();
        ReadWindow {
            newest,
            recorded: self.writer_version(surface),
            oldest: guarded_surface(surface)
                .and_then(|guarded| guarded.reads.floor())
                .map_or(newest, |floor| oldest_upcastable(surface, floor, newest)),
        }
    }
}

/// The versions a reader admits for one surface: what the fleet writes now,
/// what this build decodes natively, and every version the surface's
/// upcaster chain lifts from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ReadWindow {
    newest: u32,
    recorded: u32,
    /// The oldest version admitted below `newest`: `newest` itself for a
    /// surface without an upcaster chain.
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

    /// The oldest version the window admits through the surface's upcaster
    /// chain; the newest version when the surface has none.
    pub const fn oldest(self) -> u32 {
        self.oldest
    }

    /// The contiguous range the surface's decoder reads, `[oldest, newest]`:
    /// the surface's supported range, whatever `F` records.
    pub fn supported(self) -> VersionRange {
        VersionRange::between(self.oldest, self.newest)
    }

    /// Whether a reader admits `version`: `F`'s recorded version for the
    /// surface (FIG-3796), or any version of [`Self::supported`].
    pub fn admits(self, version: u32) -> bool {
        version == self.recorded || (self.oldest <= version && version <= self.newest)
    }

    /// Whether `version` reaches the newest through a lift: an admitted
    /// version below the newest. The decoder hands such a payload to
    /// [`upcast_json_record`], or to its own predecessor decoder for a
    /// [`Lift::Decoder`] surface.
    pub fn lifts(self, version: u32) -> bool {
        self.admits(version) && version != self.newest
    }
}

/// How a guarded surface's stored records live, which decides how far down
/// its readers must reach (ADR 0115 §5).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SurfaceReads {
    /// Immutable history: preserved byte for byte, hashed, and never
    /// rewritten. Its readers admit every version from `floor` up, forever:
    /// no backfill or sweep ever moves a record off an old version, so the
    /// upcaster of every step from the floor is permanent and `F` never
    /// moves the floor.
    History {
        /// The oldest version any build of this line still reads.
        floor: u32,
    },
    /// Mutable state: rows a writer rewrites in place, and Restate objects an
    /// `upgrade` handler rewrites. Its readers admit whatever the upcaster
    /// chain lifts; after the release that finalized it, backfills and
    /// object sweeps move every record to the newest version, and the next
    /// release may drop the lift.
    Mutable,
    /// A projection derived from a source lash keeps (a workflow graph from
    /// its module). Its readers admit only the newest version and regenerate
    /// an older record from the source instead of lifting it, so it never
    /// registers a lift.
    Derived,
}

impl SurfaceReads {
    /// The lowest version the window may reach through the upcaster chain;
    /// `None` for a derived projection, which admits only its newest.
    pub const fn floor(self) -> Option<u32> {
        match self {
            Self::History { floor } => Some(floor),
            Self::Mutable => Some(1),
            Self::Derived => None,
        }
    }
}

/// One guarded surface: a stamped stored record whose readers go through
/// `F`'s read window and the [`RECORD_UPCASTERS`] chain, never an
/// exact-version check (FIG-3802).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct GuardedSurface {
    /// The name the surface registers under in `scripts/versioned-surfaces.toml`.
    pub constant: &'static str,
    /// The crate that owns the surface's constant, decoder and law probe.
    pub owner: &'static str,
    /// How far down its readers reach.
    pub reads: SurfaceReads,
}

/// Every guarded surface: each stamped record lash stores and reads back,
/// with the crate that decodes it and how far down its readers reach
/// (FIG-3802, ADR 0115 §5).
///
/// The registry gate (`scripts/check_format_registry.py`) holds this table
/// equal to the `upgrade = "migrate"` rows of
/// `scripts/versioned-surfaces.toml` that no compatibility descriptor or
/// exclusion covers, and each owner's `every_guarded_surface_decodes_its_supported_range`
/// law refuses a row it has no probe for. Every history floor is 1, the
/// first version at the cut.
pub const GUARDED_SURFACES: &[GuardedSurface] = &[
    GuardedSurface {
        constant: "SESSION_NODE_BODY_SCHEMA_VERSION",
        owner: "lash-core-store",
        reads: SurfaceReads::History { floor: 1 },
    },
    GuardedSurface {
        constant: "SESSION_CHECKPOINT_SCHEMA_VERSION",
        owner: "lash-core-store",
        reads: SurfaceReads::History { floor: 1 },
    },
    GuardedSurface {
        constant: "CHECKPOINT_COMPONENT_ENCODING_VERSION",
        owner: "lash-core-store",
        reads: SurfaceReads::History { floor: 1 },
    },
    GuardedSurface {
        constant: "RUNTIME_COMMIT_RECEIPT_SCHEMA_VERSION",
        owner: "lash-core-store",
        reads: SurfaceReads::History { floor: 1 },
    },
    GuardedSurface {
        constant: "SESSION_HEAD_META_SCHEMA_VERSION",
        owner: "lash-core-store",
        reads: SurfaceReads::Mutable,
    },
    GuardedSurface {
        constant: "PROCESS_WAKE_DELIVERY_FORMAT_VERSION",
        owner: "lash-core-execution",
        reads: SurfaceReads::Mutable,
    },
    GuardedSurface {
        constant: "CURRENT_SESSION_STATE_VERSION",
        owner: "lash-core-store",
        reads: SurfaceReads::Mutable,
    },
    GuardedSurface {
        constant: "OBLIGATION_LEDGER_VOCABULARY_VERSION",
        owner: "lash-core-store",
        reads: SurfaceReads::Mutable,
    },
    GuardedSurface {
        constant: "PROCESS_EVENT_VOCABULARY_VERSION",
        owner: "lash-core-execution",
        reads: SurfaceReads::History { floor: 1 },
    },
    GuardedSurface {
        constant: "SCOPE_STORAGE_PAYLOAD_VERSION",
        owner: "lash-core-execution",
        reads: SurfaceReads::Mutable,
    },
    GuardedSurface {
        constant: "LASHLANG_SNAPSHOT_VERSION",
        owner: "lashlang",
        reads: SurfaceReads::History { floor: 1 },
    },
    GuardedSurface {
        constant: "HEAP_SIZE_SCHEDULE_VERSION",
        owner: "lashlang",
        reads: SurfaceReads::History { floor: 1 },
    },
    GuardedSurface {
        constant: "WORKFLOW_GRAPH_SCHEMA_VERSION",
        owner: "lashlang",
        reads: SurfaceReads::Derived,
    },
    GuardedSurface {
        constant: "RLM_SNAPSHOT_VERSION",
        owner: "lash-protocol-rlm",
        reads: SurfaceReads::History { floor: 1 },
    },
    GuardedSurface {
        constant: "NATIVE_TRANSPORT_VERSION",
        owner: "lash-protocol-rlm",
        reads: SurfaceReads::History { floor: 1 },
    },
    GuardedSurface {
        constant: "NATIVE_DRIVER_STATE_VERSION",
        owner: "lash-protocol-rlm",
        reads: SurfaceReads::Mutable,
    },
    GuardedSurface {
        constant: "SQLITE_BLOB_ENVELOPE_VERSION",
        owner: "lash-sqlite-store",
        reads: SurfaceReads::Mutable,
    },
    GuardedSurface {
        constant: "EFFECT_GROUP_STATE_FORMAT_VERSION",
        owner: "lash-restate",
        reads: SurfaceReads::Mutable,
    },
    GuardedSurface {
        constant: "EFFECT_GROUP_PAYLOAD_FORMAT_VERSION",
        owner: "lash-restate",
        reads: SurfaceReads::Mutable,
    },
    GuardedSurface {
        constant: "DURABLE_WAIT_REGISTRY_FORMAT_VERSION",
        owner: "lash-restate",
        reads: SurfaceReads::Mutable,
    },
    // A `LashTurn` workflow's outcome is written once by its `run` and no
    // handler may rewrite a finished workflow's state, so it is history.
    GuardedSurface {
        constant: "LASH_TURN_OUTCOME_FORMAT_VERSION",
        owner: "lash-restate",
        reads: SurfaceReads::History { floor: 1 },
    },
];

/// The [`GUARDED_SURFACES`] row of `surface`, when one guards it.
pub fn guarded_surface(surface: SurfaceFormat) -> Option<&'static GuardedSurface> {
    let constant = surface.constant_name();
    GUARDED_SURFACES
        .iter()
        .find(|guarded| guarded.constant == constant)
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

/// How one [`RecordUpcaster`] row lifts a payload a generation.
#[derive(Clone, Copy)]
pub enum Lift {
    /// A transform of the record's decoded JSON tree. The tree it leaves is
    /// the next generation's, including the record's own version field; a
    /// Restate object family's lift rewrites the stamped value's body.
    Tree(fn(&mut serde_json::Value) -> Result<(), crate::StoreError>),
    /// The surface's bytes are not a JSON tree — a canonical binary encoding,
    /// a label vocabulary, a byte envelope — so its own decoder reads
    /// `from_version` natively, beside the newest. The row is what admits the
    /// version: the decoder asks [`ReadWindow::lifts`] and dispatches on it,
    /// and its owner's law proves it reads what the row admits.
    Decoder,
}

impl std::fmt::Debug for Lift {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::Tree(_) => "Tree",
            Self::Decoder => "Decoder",
        })
    }
}

/// One registered lift of a guarded surface's recorded payload from
/// `from_version` to `from_version + 1` — a reader's upcaster hook
/// (FIG-3796, FIG-3802).
///
/// A build that bumps a surface's format registers the row here, and every
/// reader that admits the older version lifts the payload a generation at a
/// time until it reaches the build's newest shape.
#[derive(Clone, Copy, Debug)]
pub struct RecordUpcaster {
    /// The name the surface registers under in `scripts/versioned-surfaces.toml`.
    pub constant: &'static str,
    /// The recorded version the row lifts from.
    pub from_version: u32,
    /// How the row lifts it.
    pub lift: Lift,
}

/// The upcaster hooks readers consult when a recorded version is older than
/// the build's newest — the transform half of every guarded surface's read
/// window (FIG-3796, FIG-3802). This is the one place a lift is registered:
/// an object family's, a SQL row's, and history's alike.
///
/// 1.0 registers none: every guarded surface is at its first shipped
/// generation, so no older payload can exist and no transform is owed yet.
/// The first real successor hangs its rows here, as the synthetic one does.
#[cfg(not(feature = "synthetic-next"))]
pub const RECORD_UPCASTERS: &[RecordUpcaster] = &[];

/// Phase A's synthetic N+1 (ADR 0115 §6) moves every guarded surface one
/// version on without changing its shape, and registers each step's lift:
/// the version N wrote is admitted, lifted and decoded, before and after
/// finalize, and history N wrote stays readable through its floor.
#[cfg(feature = "synthetic-next")]
pub const RECORD_UPCASTERS: &[RecordUpcaster] = super::synthetic_next::RECORD_UPCASTERS;

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
/// `lashctl finalize` moves `F`.
#[cfg(not(feature = "synthetic-next"))]
const WRITER_PINS: &[WriterPin] = &[];

/// Phase A's synthetic N+1 (ADR 0115 §6) pins, while `F` is N's epoch, each
/// stored surface it bumps to the version N reads: before finalize N+1 writes
/// only what N reads.
#[cfg(feature = "synthetic-next")]
const WRITER_PINS: &[WriterPin] = super::synthetic_next::WRITER_PINS;

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
/// the registered [`RecordUpcaster`] tree lifts, one generation per row
/// (FIG-3796).
///
/// The walk is fail-closed: a step with no row refuses the record rather
/// than decoding a half-lifted payload, so a reader that admits an older
/// version without a transform reports the same unsupported-version error
/// the exact-version path reports. A [`Lift::Decoder`] step is the
/// surface's own decoder's to take, never a tree walk's, and is refused the
/// same way.
pub fn upcast_json_record(
    record_kind: &'static str,
    surface: SurfaceFormat,
    from_version: u32,
    to_version: u32,
    value: &mut serde_json::Value,
) -> Result<(), StoreError> {
    let mut version = from_version;
    while version != to_version {
        let Some(Lift::Tree(lift)) = upcaster(surface, version).map(|row| row.lift) else {
            return Err(StoreError::UnsupportedRecordSchemaVersion {
                record_kind,
                actual: from_version,
                expected: to_version,
            });
        };
        lift(value)?;
        version += 1;
    }
    Ok(())
}

/// The [`RecordUpcaster`] row lifting `surface` from `from_version`.
pub fn upcaster(surface: SurfaceFormat, from_version: u32) -> Option<&'static RecordUpcaster> {
    let constant = surface.constant_name();
    RECORD_UPCASTERS
        .iter()
        .find(|row| row.constant == constant && row.from_version == from_version)
}

/// Whether a [`RecordUpcaster`] chain can lift `surface`'s recorded
/// `from_version` to `to_version` — the fleetless leg of the reader window
/// for decode sites (serde impls, wire parsers) that cannot consult `F`
/// directly. A surface with no registered chain admits exactly its newest
/// version (FIG-3796).
pub fn upcast_chain_covers(surface: SurfaceFormat, from_version: u32, to_version: u32) -> bool {
    from_version <= to_version
        && (from_version..to_version).all(|version| upcaster(surface, version).is_some())
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
    use super::{FLEET_WRITABLE_RANGE, FleetFormat, SurfaceFormat, SurfaceReads, guarded_surface};
    use crate::StoreError;
    use crate::compat::{CompatRefusal, VersionRange};

    #[test]
    fn fleet_epoch_is_admitted_at_open_and_fenced_in_a_transaction() {
        assert_eq!(FleetFormat::writable(), FLEET_WRITABLE_RANGE);
        #[cfg(not(feature = "synthetic-next"))]
        assert_eq!(FLEET_WRITABLE_RANGE, VersionRange::exactly(1));
        #[cfg(feature = "synthetic-next")]
        assert_eq!(FLEET_WRITABLE_RANGE, VersionRange::between(1, 2));
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
                        writing_release: None,
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
    fn an_installer_seeds_the_writable_floor_and_an_open_requires_it() {
        // The seed is `F_prev`, so a compatibility release provisioning a
        // store leaves it inside the rollback window.
        assert_eq!(FleetFormat::seed(FLEET_WRITABLE_RANGE).version(), 1);
        let compatibility = VersionRange::between(1, 2);
        assert_eq!(FleetFormat::seed(compatibility).version(), 1);

        assert_eq!(
            FleetFormat::admit_recorded("postgres", Some(1), compatibility)
                .expect("recorded")
                .version(),
            1
        );
        let Err(StoreError::Incompatible {
            refusal:
                CompatRefusal::FleetUnrecorded {
                    component,
                    writing_release: None,
                },
        }) = FleetFormat::admit_recorded("postgres", None, compatibility)
        else {
            panic!("a store with no recorded F must refuse at open");
        };
        assert_eq!(component, "postgres");
        assert!(matches!(
            FleetFormat::admit_recorded("postgres", Some(3), compatibility),
            Err(StoreError::Incompatible {
                refusal: CompatRefusal::FleetOutsideWritable { recorded: 3, .. }
            })
        ));
    }

    #[test]
    fn history_is_read_through_its_permanent_floor() {
        let history = SurfaceFormat::of("SESSION_NODE_BODY_SCHEMA_VERSION", 1);
        assert_eq!(
            guarded_surface(history).map(|guarded| guarded.reads),
            Some(SurfaceReads::History { floor: 1 })
        );
        let window = FleetFormat::current().read_window(history);
        assert_eq!((window.oldest(), window.newest()), (1, 1));
        assert!(window.admits(1) && !window.admits(2));

        // Fail-closed: without an upcaster chain the window reaches no lower
        // than the newest version.
        let unlifted = SurfaceFormat::of("crate::SESSION_CHECKPOINT_SCHEMA_VERSION", 40);
        let window = FleetFormat::current().read_window(unlifted);
        assert_eq!(window.oldest(), 40);
        assert!(window.admits(40) && !window.admits(1));

        // A mutable surface reaches as far down as its chain, whatever `F`
        // records; a surface no row guards keeps `F`'s `{recorded, newest}`.
        let mutable = SurfaceFormat::of("SESSION_HEAD_META_SCHEMA_VERSION", 30);
        assert_eq!(
            guarded_surface(mutable).map(|guarded| guarded.reads),
            Some(SurfaceReads::Mutable)
        );
        let window = FleetFormat::current().read_window(mutable);
        assert_eq!(window.oldest(), 30);
        assert!(window.admits(30) && !window.admits(29));
        let wire = SurfaceFormat::of("RESTATE_WIRE_VERSION", 30);
        assert_eq!(guarded_surface(wire), None);
        assert_eq!(FleetFormat::current().read_window(wire).oldest(), 30);
    }
}
