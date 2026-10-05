//! A durable authorization for SQLite's three independent epoch commits.
//!
//! Retirement is checked before this intent is sealed. The intent is the
//! durable decision to finalize, not a request to authorize it again on open.
//! Recovery replays only that decision, under the same store ownership and
//! database locks as finalize, before migration or ordinary set admission.

use std::collections::BTreeMap;
use std::io::{self, Write as _};
use std::path::Path;
use std::time::Duration;

use lash_core_execution::compat::{CompatStamp, VersionRange};
use lash_core_execution::engine::BuildGeneration;
use lash_core_execution::store::fleet_finalize::{FinalizeError, FinalizeRefusal, FleetEpochFlip};
use lash_core_execution::store::generation_drain::GenerationDrainStatus;
use lash_core_execution::store::plugin_writers::PluginWriterRegistration;
use lash_core_execution::{FleetFormat, StoreError};
use serde::{Deserialize, Serialize};

use crate::compat::{AdvanceStep, advance_set_observed};
use crate::{SqliteDatabase, SqliteLocation};

const INTENT: &str = "lash-finalize.json";
const STAGING: &str = "lash-finalize.json.staging";

/// The durable authorization file consumed before SQLite store admission.
/// version_surface = "migrate"
/// version_unguarded = "backend-private recovery file decoded before the store catalog can admit its FleetFormat; exact bootstrap reader until the release cut"
/// format_outside_manifest = "backend-private recovery intent read before the store set opens"
/// version_guard(roots(AuthorizedFinalize))
pub const SQLITE_FINALIZE_INTENT_VERSION: u32 = 1;

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct AuthorizedFinalize {
    format: u32,
    store: String,
    retired: DrainedGeneration,
    from: u32,
    target: u32,
    /// In `SqliteDatabase::ALL` order, read under all three exclusive locks.
    stamps: [CompatStamp; 3],
    /// Each plugin's source and target are one authorized move.
    moves: BTreeMap<String, WriterMove>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct DrainedGeneration {
    generation: BuildGeneration,
    draining_since_ms: u64,
}

impl DrainedGeneration {
    fn from_status(status: GenerationDrainStatus) -> rusqlite::Result<Self> {
        if !status.drained() {
            return Err(invalid("the retiring generation has not drained"));
        }
        Ok(Self {
            generation: status.generation,
            draining_since_ms: status
                .draining_since_ms
                .ok_or_else(|| invalid("the retiring generation is not marked draining"))?,
        })
    }
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(try_from = "WriterMoveWire")]
struct WriterMove {
    from: Option<VersionRange>,
    to: VersionRange,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WriterMoveWire {
    from: Option<VersionRange>,
    to: VersionRange,
}

impl TryFrom<WriterMoveWire> for WriterMove {
    type Error = &'static str;
    fn try_from(wire: WriterMoveWire) -> Result<Self, Self::Error> {
        Self::new(wire.from, wire.to)
    }
}

impl WriterMove {
    fn new(from: Option<VersionRange>, to: VersionRange) -> Result<Self, &'static str> {
        if from == Some(to) {
            return Err("a plugin writer move must change its range");
        }
        Ok(Self { from, to })
    }
}

/// The durable core's side of a finalize (FIG-4746).
#[derive(Default)]
struct PluginWriterMove {
    moves: BTreeMap<String, WriterMove>,
    changed: Vec<String>,
}

impl PluginWriterMove {
    fn fresh(
        tx: &rusqlite::Transaction<'_>,
        registrations: &[PluginWriterRegistration],
    ) -> rusqlite::Result<Self> {
        let recorded = crate::compat::read_plugin_writers(tx)?;
        let finalized = recorded.finalized(registrations);
        let changed = recorded.changed_in(&finalized);
        let mut moves = BTreeMap::new();
        for (plugin, range) in finalized.iter() {
            let before = recorded.permitted_writer(plugin).ok();
            if before != Some(range) {
                moves.insert(
                    plugin.to_owned(),
                    WriterMove::new(before, range).map_err(invalid)?,
                );
            }
        }
        Ok(Self { moves, changed })
    }

    /// Replays the sealed move only while every range still names its source or target.
    fn authorized(
        tx: &rusqlite::Transaction<'_>,
        intent: &AuthorizedFinalize,
    ) -> rusqlite::Result<Self> {
        let recorded = crate::compat::read_plugin_writers(tx)?;
        for (plugin, transition) in &intent.moves {
            let found = recorded.permitted_writer(plugin).ok();
            if found != transition.from && found != Some(transition.to) {
                return Err(invalid(format!(
                    "the writer range of plugin `{plugin}` changed outside the authorized transition"
                )));
            }
        }
        Ok(Self {
            moves: intent.moves.clone(),
            changed: Vec::new(),
        })
    }

    fn record(&self, tx: &rusqlite::Transaction<'_>) -> rusqlite::Result<()> {
        let entries = self
            .moves
            .iter()
            .map(|(plugin, transition)| (plugin.to_owned(), transition.to))
            .collect();
        crate::compat::record_plugin_writers(tx, &entries)
    }
}

/// A finalize's typed outcome behind a SQLite error: a precondition the
/// databases themselves refuse is a [`FinalizeRefusal`], not a store failure.
pub(crate) fn finalize_error(error: rusqlite::Error) -> FinalizeError {
    match error {
        rusqlite::Error::ToSqlConversionFailure(error) => {
            match error.downcast::<FinalizeRefusal>() {
                Ok(refusal) => FinalizeError::Refused(*refusal),
                Err(error) => FinalizeError::Store(crate::sqlite_error(
                    rusqlite::Error::ToSqlConversionFailure(error),
                )),
            }
        }
        error => FinalizeError::Store(crate::sqlite_error(error)),
    }
}

fn invalid(detail: impl Into<String>) -> rusqlite::Error {
    crate::compat::malformed(
        SqliteDatabase::DurableCore,
        format!("invalid SQLite finalize intent: {}", detail.into()),
    )
}

fn io_error(error: io::Error) -> rusqlite::Error {
    crate::sqlite_conversion_error(StoreError::StorageFailure {
        backend: crate::SQLITE_BACKEND,
        message: format!("SQLite finalize intent: {error}"),
    })
}

impl AuthorizedFinalize {
    fn validate(&self, location: &SqliteLocation, writable: VersionRange) -> rusqlite::Result<()> {
        if self.store != location.identity()
            || self.from >= self.target
            || !crate::compat::stamps_agree(&self.stamps)?
        {
            return Err(invalid(
                "the recorded store, retirement or transition does not match",
            ));
        }
        FleetFormat::fence(self.target, writable).map_err(crate::sqlite_conversion_error)?;
        Ok(())
    }
}

#[expect(
    clippy::disallowed_methods,
    reason = "finalize recovery reads its durable intent from the host-supplied store root"
)]
fn read(location: &SqliteLocation) -> rusqlite::Result<Option<AuthorizedFinalize>> {
    let SqliteLocation::File { root } = location else {
        return Ok(None);
    };
    match std::fs::read(root.join(INTENT)) {
        Ok(bytes) => {
            #[derive(Deserialize)]
            struct Stamp {
                format: u32,
            }
            let stamp: Stamp =
                serde_json::from_slice(&bytes).map_err(|error| invalid(error.to_string()))?;
            if stamp.format != SQLITE_FINALIZE_INTENT_VERSION {
                return Err(crate::sqlite_conversion_error(StoreError::Incompatible {
                    refusal: lash_core_execution::compat::CompatRefusal::UnknownVocabulary {
                        surface: "SQLite finalize intent format".into(),
                        label: stamp.format.to_string(),
                    },
                }));
            }
            serde_json::from_slice(&bytes)
                .map(Some)
                .map_err(|error| invalid(error.to_string()))
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(io_error(error)),
    }
}

/// Publish the authorization durably before any epoch transaction commits.
#[expect(
    clippy::disallowed_methods,
    reason = "finalize syncs and atomically publishes its intent under the host-supplied store root"
)]
fn seal(location: &SqliteLocation, intent: &AuthorizedFinalize) -> rusqlite::Result<()> {
    let SqliteLocation::File { root } = location else {
        return Ok(());
    };
    let bytes = serde_json::to_vec(intent).map_err(|error| invalid(error.to_string()))?;
    let mut file = std::fs::File::create(root.join(STAGING)).map_err(io_error)?;
    file.write_all(&bytes).map_err(io_error)?;
    file.sync_all().map_err(io_error)?;
    std::fs::rename(root.join(STAGING), root.join(INTENT)).map_err(io_error)?;
    sync_root(root)
}

#[expect(
    clippy::disallowed_methods,
    reason = "completed finalize removes its intent from the host-supplied store root"
)]
fn clear(location: &SqliteLocation) -> rusqlite::Result<()> {
    let SqliteLocation::File { root } = location else {
        return Ok(());
    };
    match std::fs::remove_file(root.join(INTENT)) {
        Ok(()) => sync_root(root),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(io_error(error)),
    }
}

#[expect(
    clippy::disallowed_methods,
    reason = "finalize syncs directory entries in the host-supplied store root"
)]
fn sync_root(root: &Path) -> rusqlite::Result<()> {
    std::fs::File::open(root)
        .and_then(|directory| directory.sync_all())
        .map_err(io_error)
}

/// The caller holds store ownership and has checked drain and retirement.
pub(crate) fn finalize(
    location: &SqliteLocation,
    busy_timeout: Duration,
    writable: VersionRange,
    retired: GenerationDrainStatus,
    registrations: &[PluginWriterRegistration],
    observe: impl FnMut(AdvanceStep) -> rusqlite::Result<()>,
) -> rusqlite::Result<FleetEpochFlip> {
    if !retired.drained() {
        return Err(invalid("the retiring generation has not drained"));
    }
    let pending = read(location)?;
    advance(
        location,
        busy_timeout,
        writable,
        pending,
        Some(retired),
        registrations,
        observe,
    )
}

/// Resume without an assembled store, before migrations and set admission.
pub(crate) async fn recover_on_open(
    location: &SqliteLocation,
    busy_timeout: Duration,
) -> Result<(), StoreError> {
    if read(location).map_err(crate::sqlite_error)?.is_none() {
        return Ok(());
    }
    let ownership = crate::store_ownership::exclusive(location, busy_timeout).await?;
    let location = location.clone();
    tokio::task::spawn_blocking(move || {
        let _ownership = ownership;
        if let Some(pending) = read(&location)? {
            advance(
                &location,
                busy_timeout,
                FleetFormat::writable(),
                Some(pending),
                None,
                &[],
                |_| Ok(()),
            )?;
        }
        Ok(())
    })
    .await
    .map_err(|error| {
        StoreError::Backend(format!("the SQLite finalize recovery task ended: {error}"))
    })?
    .map_err(crate::sqlite_error)
}

fn advance(
    location: &SqliteLocation,
    busy_timeout: Duration,
    writable: VersionRange,
    pending: Option<AuthorizedFinalize>,
    mut retired: Option<GenerationDrainStatus>,
    registrations: &[PluginWriterRegistration],
    observe: impl FnMut(AdvanceStep) -> rusqlite::Result<()>,
) -> rusqlite::Result<FleetEpochFlip> {
    if let Some(intent) = &pending {
        intent.validate(location, writable)?;
    }
    let target = pending
        .as_ref()
        .map_or(writable.max(), |intent| intent.target);
    let mut lowest = target;
    let mut stamps = Vec::with_capacity(3);
    let mut epochs = Vec::with_capacity(3);
    let mut plugin_writers = PluginWriterMove::default();
    advance_set_observed(
        location,
        busy_timeout,
        |database, tx| {
            let recorded = crate::compat::fence(tx, database, writable)?.version();
            let (stamp, _) = crate::compat::read(tx, database)?
                .ok_or_else(|| invalid("a database has no stamp"))?;
            if let Some(intent) = &pending
                && (stamp != intent.stamps[stamps.len()]
                    || (recorded != intent.from && recorded != intent.target))
            {
                return Err(invalid(format!(
                    "the {} changed outside the authorized transition",
                    database.name()
                )));
            }
            stamps.push(stamp);
            epochs.push(recorded);
            lowest = lowest.min(recorded);
            if recorded != target {
                tx.execute(
                    "UPDATE lash_compat SET fleet_format = ?1 WHERE singleton = 1",
                    [i64::from(target)],
                )?;
            }
            // The durable core carries the plugin writer ranges beside `F`,
            // and its one transaction moves both.
            if database == SqliteDatabase::DurableCore {
                plugin_writers = match &pending {
                    Some(intent) => PluginWriterMove::authorized(tx, intent)?,
                    None => PluginWriterMove::fresh(tx, registrations)?,
                };
                plugin_writers.record(tx)?;
            }
            if database == SqliteDatabase::Triggers && pending.is_none() {
                if !crate::compat::stamps_agree(&stamps)?
                    || epochs.windows(2).any(|pair| pair[0] != pair[1])
                {
                    return Err(invalid("a fresh finalize requires a consistent store set"));
                }
                // A recorded range changes only with `F`: the move of `F` is
                // what fences the builds that cannot read the new format.
                if lowest == target && !plugin_writers.changed.is_empty() {
                    return Err(rusqlite::Error::ToSqlConversionFailure(Box::new(
                        FinalizeRefusal::PluginRangesNeedEpochMove {
                            fleet: target,
                            plugins: std::mem::take(&mut plugin_writers.changed),
                        },
                    )));
                }
                if lowest != target {
                    let intent = AuthorizedFinalize {
                        format: lash_core_store::store::FleetFormat::from_version(lowest)
                            .writer_version(lash_core_store::surface_format!(
                                SQLITE_FINALIZE_INTENT_VERSION
                            )),
                        store: location.identity(),
                        retired: DrainedGeneration::from_status(
                            retired
                                .take()
                                .ok_or_else(|| invalid("no retirement authorization"))?,
                        )?,
                        from: lowest,
                        target,
                        stamps: stamps
                            .as_slice()
                            .try_into()
                            .map_err(|_| invalid("incomplete stamp set"))?,
                        moves: plugin_writers.moves.clone(),
                    };
                    intent.validate(location, writable)?;
                    seal(location, &intent)?;
                }
            }
            Ok(())
        },
        observe,
    )?;
    clear(location)?;
    Ok(if lowest == target {
        FleetEpochFlip::AlreadyFinalized { fleet: target }
    } else {
        FleetEpochFlip::Finalized {
            from: lowest,
            to: target,
        }
    })
}

#[cfg(test)]
mod nested_format_tests {
    use super::*;

    #[test]
    fn authorization_requires_a_marked_and_drained_generation() {
        let status = |marked: bool, in_flight_turns: u64| GenerationDrainStatus {
            generation: BuildGeneration::for_test("retired"),
            draining_since_ms: marked.then_some(5),
            live_processes: 0,
            parked_processes: 0,
            parked_turns: 0,
            in_flight_turns,
            closing_sessions: 0,
            unfinished_invocations: 0,
            stalled_obligations: BTreeMap::new(),
            checked_at: 9,
        };
        assert!(DrainedGeneration::from_status(status(false, 0)).is_err());
        assert!(DrainedGeneration::from_status(status(true, 1)).is_err());
        assert!(DrainedGeneration::from_status(status(true, 0)).is_ok());
    }

    #[test]
    fn a_writer_move_cannot_repeat_its_source_range() {
        let range = VersionRange::exactly(1);
        assert!(WriterMove::new(Some(range), range).is_err());
        assert!(
            serde_json::from_value::<WriterMove>(serde_json::json!({
                "from": range, "to": range,
            }))
            .is_err()
        );
    }

    #[test]
    #[expect(
        clippy::disallowed_methods,
        reason = "the law writes its own finalize intent"
    )]
    fn a_foreign_intent_is_refused_before_decoding_its_authorization() {
        let root = tempfile::tempdir().expect("root");
        std::fs::write(root.path().join(INTENT), br#"{"format":4294967295}"#).expect("intent");
        let location = SqliteLocation::File {
            root: root.path().to_path_buf(),
        };
        let error = match read(&location) {
            Err(error) => crate::sqlite_error(error),
            Ok(_) => panic!("foreign intent was admitted"),
        };
        assert!(
            matches!(
                error,
                StoreError::Incompatible {
                    refusal: lash_core_execution::compat::CompatRefusal::UnknownVocabulary { .. }
                }
            ),
            "{error}"
        );
    }
}
