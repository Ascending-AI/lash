//! SQLite finalize (ADR 0106 §2, ADR 0115 §2.2): one exclusive transaction
//! on the deployment's one database moves `F` and the plugin writer ranges
//! together, so a crash leaves the store finalized or as it was. Retirement
//! is checked before the transaction starts.

use std::collections::BTreeMap;
use std::time::Duration;

use lash_core_execution::compat::VersionRange;
use lash_core_execution::store::fleet_finalize::{FinalizeError, FinalizeRefusal, FleetEpochFlip};
use lash_core_execution::store::generation_drain::GenerationDrainStatus;
use lash_core_execution::store::plugin_writers::PluginWriterRegistration;

use crate::SqliteLocation;
use crate::compat::{AdvanceStep, advance_observed};

/// The plugin writer ranges a finalize records beside `F` (FIG-4746).
struct PluginWriterMove {
    /// Each plugin whose range the finalize moves, and its new range.
    entries: BTreeMap<String, VersionRange>,
    /// The plugins whose recorded range changes.
    changed: Vec<String>,
}

impl PluginWriterMove {
    fn of(
        tx: &rusqlite::Transaction<'_>,
        registrations: &[PluginWriterRegistration],
    ) -> rusqlite::Result<Self> {
        let recorded = crate::compat::read_plugin_writers(tx)?;
        let finalized = recorded.finalized(registrations);
        let changed = recorded.changed_in(&finalized);
        let entries = finalized
            .iter()
            .filter(|(plugin, range)| recorded.permitted_writer(plugin).ok() != Some(*range))
            .map(|(plugin, range)| (plugin.to_owned(), range))
            .collect();
        Ok(Self { entries, changed })
    }
}

/// A finalize's typed outcome behind a SQLite error: a precondition the
/// database itself refuses is a [`FinalizeRefusal`], not a store failure.
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
    crate::compat::malformed(format!("invalid SQLite finalize: {}", detail.into()))
}

/// Move `F` to the top of `writable`, with the plugin writer ranges
/// `registrations` finalize, in one exclusive transaction. The caller holds
/// store ownership and has checked that no deployment serves `retired`.
pub(crate) fn finalize(
    location: &SqliteLocation,
    busy_timeout: Duration,
    writable: VersionRange,
    retired: GenerationDrainStatus,
    registrations: &[PluginWriterRegistration],
    observe: impl FnMut(AdvanceStep) -> rusqlite::Result<()>,
) -> rusqlite::Result<FleetEpochFlip> {
    if retired.draining_since_ms.is_none() {
        return Err(invalid("the retiring generation is not marked draining"));
    }
    if !retired.drained() {
        return Err(invalid("the retiring generation has not drained"));
    }
    let target = writable.max();
    advance_observed(
        location,
        busy_timeout,
        |tx| {
            let recorded = crate::compat::fence(tx, writable)?.version();
            let plugin_writers = PluginWriterMove::of(tx, registrations)?;
            // A recorded range changes only with `F`: the move of `F` is what
            // fences the builds that cannot read the new format.
            if recorded == target && !plugin_writers.changed.is_empty() {
                return Err(rusqlite::Error::ToSqlConversionFailure(Box::new(
                    FinalizeRefusal::PluginRangesNeedEpochMove {
                        fleet: target,
                        plugins: plugin_writers.changed,
                    },
                )));
            }
            if recorded != target {
                tx.execute(
                    "UPDATE lash_compat SET fleet_format = ?1 WHERE singleton = 1",
                    [i64::from(target)],
                )?;
            }
            crate::compat::record_plugin_writers(tx, &plugin_writers.entries)?;
            Ok(if recorded == target {
                FleetEpochFlip::AlreadyFinalized { fleet: target }
            } else {
                FleetEpochFlip::Finalized {
                    from: recorded,
                    to: target,
                }
            })
        },
        observe,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use lash_core_execution::engine::BuildGeneration;

    #[test]
    fn finalize_requires_a_marked_and_drained_generation() {
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
        let location = SqliteLocation::fresh_memory();
        for (marked, in_flight) in [(false, 0), (true, 1)] {
            let refused = finalize(
                &location,
                Duration::from_millis(10),
                VersionRange::exactly(1),
                status(marked, in_flight),
                &[],
                |_| Ok(()),
            );
            assert!(refused.is_err(), "marked {marked}, in flight {in_flight}");
        }
    }
}
