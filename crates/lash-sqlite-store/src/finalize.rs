//! A durable authorization for SQLite's three independent epoch commits.
//!
//! Retirement is checked before this intent is sealed. The intent is the
//! durable decision to finalize, not a request to authorize it again on open.
//! Recovery replays only that decision, under the same store ownership and
//! database locks as finalize, before migration or ordinary set admission.

use std::io::{self, Write as _};
use std::path::Path;
use std::time::Duration;

use lash_core_execution::compat::{CompatStamp, VersionRange};
use lash_core_execution::store::fleet_finalize::FleetEpochFlip;
use lash_core_execution::store::generation_drain::GenerationDrainStatus;
use lash_core_execution::{FleetFormat, StoreError};
use serde::{Deserialize, Serialize};

use crate::compat::{AdvanceStep, advance_set_observed};
use crate::{SqliteDatabase, SqliteLocation};

const INTENT: &str = "lash-finalize.json";
const STAGING: &str = "lash-finalize.json.staging";

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct AuthorizedFinalize {
    store: String,
    retired: GenerationDrainStatus,
    from: u32,
    target: u32,
    /// In `SqliteDatabase::ALL` order, read under all three exclusive locks.
    stamps: [CompatStamp; 3],
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
            || !self.retired.drained()
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
        Ok(bytes) => serde_json::from_slice(&bytes)
            .map(Some)
            .map_err(|error| invalid(error.to_string())),
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
            if database == SqliteDatabase::Triggers && pending.is_none() {
                if !crate::compat::stamps_agree(&stamps)?
                    || epochs.windows(2).any(|pair| pair[0] != pair[1])
                {
                    return Err(invalid("a fresh finalize requires a consistent store set"));
                }
                if lowest != target {
                    let intent = AuthorizedFinalize {
                        store: location.identity(),
                        retired: retired
                            .take()
                            .ok_or_else(|| invalid("no retirement authorization"))?,
                        from: lowest,
                        target,
                        stamps: stamps
                            .as_slice()
                            .try_into()
                            .map_err(|_| invalid("incomplete stamp set"))?,
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
