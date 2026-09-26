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

/// The fleet format every writer this build runs emits — the only `F` this
/// build's writable range admits.
///
/// ADR 0106 gives a build a range `[min_F, max_F]`; before the first upgrade
/// the range is one version wide and this constant is it. The first format
/// upgrade introduces `min_F`/`max_F` and widens the range; until then a store
/// recording any other value is refused at open.
pub const FLEET_FORMAT_VERSION: u32 = 1;

/// The fleet format a store records, as the fleet-format row reports it.
///
/// Writers never read the integer directly — they ask for their per-format
/// writer version through [`FleetFormat::writer_version`], so a superseded
/// fleet format's pin lives in exactly one place.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct FleetFormat(u32);

impl FleetFormat {
    /// The fleet format an unrecorded store takes at open: this build's own.
    pub const fn current() -> Self {
        Self(FLEET_FORMAT_VERSION)
    }

    /// The fleet format a recorded version names.
    pub const fn from_version(version: u32) -> Self {
        Self(version)
    }

    /// The version the fleet-format row records.
    pub const fn version(self) -> u32 {
        self.0
    }

    /// The fleet-format versions this build can write under — the
    /// `[min_F, max_F]` writable range of ADR 0106 §1.
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

    /// The version a durable writer emits for the durable format whose
    /// build-newest version is `current` — the per-format mapping the
    /// fleet-format row stands for (FIG-3796).
    ///
    /// While the fleet runs its only format the mapping is the identity, so a
    /// writer that asks stamps exactly the version it stamped before this hook
    /// existed. When `finalize-upgrade` moves the row (FIG-3800), the
    /// superseded fleet format's pin lives here and every writer that already
    /// consults keeps emitting the generation the fleet agreed on.
    pub fn writer_version(self, current: u32) -> u32 {
        let _ = self;
        current
    }
}

impl std::fmt::Display for FleetFormat {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
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
