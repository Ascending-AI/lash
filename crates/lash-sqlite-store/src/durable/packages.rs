//! Execution resolution and live-node claim capabilities:
//! the SQLite side of `lash_store_sql::durable::packages`.

use std::sync::LazyLock;

use lash_durable::packages::NodeCapability;
use lash_durable::{DurableError, DurableInstant};
use lash_store_sql::durable::packages::PackageStatements;
use rusqlite::{Connection, OptionalExtension};

static SQL: LazyLock<PackageStatements> =
    LazyLock::new(|| PackageStatements::render(crate::schema_layout::MAIN));

/// Every node live at `now`, by node id.
pub(super) fn live_nodes(
    tx: &Connection,
    now: DurableInstant,
) -> rusqlite::Result<Result<Vec<NodeCapability>, DurableError>> {
    let rows = tx
        .prepare_cached(SQL.live_nodes.sql())?
        .query_map([now.0], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(lash_durable::packages::nodes_of(rows))
}

type RequirementRows = Vec<(lash_durable::ActorKey, Vec<String>)>;

/// Up to `limit` actors that have not ended and carry claim requirements,
/// by key after `after`.
pub(super) fn actor_requirements(
    tx: &Connection,
    after: Option<&str>,
    limit: i64,
) -> rusqlite::Result<Result<RequirementRows, DurableError>> {
    let rows = tx
        .prepare_cached(SQL.actor_requirements.sql())?
        .query_map(rusqlite::params![after.unwrap_or(""), limit], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(lash_durable::packages::requirement_rows(rows))
}

use super::{Answer, SqliteDurableStore, commit, refuse};
use lash_durable::resolutions::{
    CommitResolution, ExecutionRegistration, ExecutionResolutions, ResolutionKey, ResolutionOwner,
    ResolutionRecord,
};

pub(super) fn register(
    tx: &Connection,
    actor: &lash_durable::ActorKey,
    record: &ExecutionRegistration,
) -> Answer<()> {
    if crate::artifact_store::artifact_fenced_tx(tx, &record.referrer)? {
        return Ok(Err(lash_durable::packages::PackageRefusal::ReferrerEnded(
            record.referrer.clone(),
        )
        .into()));
    }
    let json = serde_json::to_string(record)
        .map_err(|error| rusqlite::Error::ToSqlConversionFailure(Box::new(error)))?;
    tx.prepare_cached(SQL.register_execution.sql())?
        .execute(rusqlite::params![record.execution.0, actor.as_str(), json])?;
    let matches = tx
        .prepare_cached(SQL.registration_matches.sql())?
        .query_row(
            rusqlite::params![record.execution.0, actor.as_str(), json],
            |_| Ok(()),
        )
        .optional()?;
    Ok(matches.ok_or_else(|| lash_durable::packages::PackageRefusal::ExecutionUse.into()))
}

fn load(tx: &Connection, key: &ResolutionKey) -> Answer<Option<ResolutionRecord>> {
    let json: Option<String> = tx
        .prepare_cached(SQL.load_resolution.sql())?
        .query_row(
            rusqlite::params![
                key.execution.0,
                key.use_.link.to_string(),
                i64::from(key.use_.slot.0)
            ],
            |row| row.get(0),
        )
        .optional()?;
    Ok(json
        .map(|json| {
            serde_json::from_str::<ResolutionRecord>(&json)
                .map_err(|_| super::corrupt("resolution", &json))
                .and_then(|record| {
                    if record.key == *key {
                        lash_durable::resolutions::validate_resolution(&record)?;
                        Ok(record)
                    } else {
                        Err(super::corrupt("resolution key", &json))
                    }
                })
        })
        .transpose())
}

#[async_trait::async_trait]
impl ExecutionResolutions for SqliteDurableStore {
    async fn requirements(
        &self,
        actor: &lash_durable::ActorKey,
    ) -> Result<lash_durable::resolutions::ClaimRequirements, DurableError> {
        let actor = actor.clone();
        self.read(move |tx| requirements(tx, &actor)).await
    }
    async fn load(&self, key: &ResolutionKey) -> Result<Option<ResolutionRecord>, DurableError> {
        let key = key.clone();
        self.read(move |tx| load(tx, &key)).await
    }

    async fn commit(
        &self,
        owner: &ResolutionOwner,
        candidate: ResolutionRecord,
    ) -> Result<CommitResolution, DurableError> {
        let owner = owner.clone();
        self.write(
            lash_durable::CommitLabel::RESOLUTION_COMMIT,
            move |tx, _| {
                let row = tx
                    .prepare_cached(SQL.owner_revision.sql())?
                    .query_row([owner.actor.as_str()], |row| {
                        Ok((
                            row.get::<_, i64>(0)?,
                            row.get::<_, String>(1)?,
                            row.get::<_, i64>(2)?,
                            row.get::<_, Option<String>>(3)?,
                        ))
                    })
                    .optional()?;
                let Some((epoch, state, revision, requires)) = row else {
                    return refuse(super::fenced(tx, &owner.actor, owner.epoch)?);
                };
                if epoch != owner.epoch.0 || state != "owned" {
                    return refuse(super::fenced(tx, &owner.actor, owner.epoch)?);
                }
                let registration = tx
                    .prepare_cached(SQL.registration.sql())?
                    .query_row([&candidate.key.execution.0], |row| {
                        Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
                    })
                    .optional()?;
                let Some((actor, json)) = registration else {
                    return refuse(lash_durable::packages::PackageRefusal::ExecutionUse.into());
                };
                if actor != owner.actor.as_str() {
                    return refuse(lash_durable::packages::PackageRefusal::ExecutionUse.into());
                }
                let registration: ExecutionRegistration = match serde_json::from_str(&json) {
                    Ok(record) => record,
                    Err(_) => return refuse(super::corrupt("execution registration", &json)),
                };
                if let Err(error) = registration.validate(&candidate) {
                    return refuse(error);
                }
                if crate::artifact_store::artifact_fenced_tx(tx, &registration.referrer)? {
                    return refuse(
                        lash_durable::packages::PackageRefusal::ReferrerEnded(
                            registration.referrer,
                        )
                        .into(),
                    );
                }
                match load(tx, &candidate.key)? {
                    Err(error) => return refuse(error),
                    Ok(Some(winner)) => return commit(CommitResolution::Existing(winner)),
                    Ok(None) => {}
                }
                if revision != owner.expected_revision.0 {
                    return refuse(DurableError::ResolutionRevisionConflict {
                        actor: owner.actor,
                        expected: owner.expected_revision,
                        current: lash_durable::StateRevision(revision),
                    });
                }
                tx.prepare_cached(SQL.resolution_fence.sql())?.query_row(
                    rusqlite::params![owner.actor.as_str(), owner.epoch.0, revision],
                    |_| Ok(()),
                )?;
                let mut requires = match lash_durable::parse_requires(requires.as_deref()) {
                    Ok(requires) => requires,
                    Err(_) => {
                        return refuse(super::corrupt("actor requirements", owner.actor.as_str()));
                    }
                };
                requires.extend(registration.requirements.tokens());
                if let lash_durable::resolutions::ResolutionEntry::Resolved {
                    requirements, ..
                } = &candidate.entry
                {
                    requires.extend(requirements.tokens());
                }
                super::processes::stamp_requires(tx, owner.actor.as_str(), &requires)?;
                let json = serde_json::to_string(&candidate)
                    .map_err(|error| rusqlite::Error::ToSqlConversionFailure(Box::new(error)))?;
                tx.prepare_cached(SQL.insert_resolution.sql())?
                    .execute(rusqlite::params![
                        candidate.key.execution.0,
                        candidate.key.use_.link.to_string(),
                        i64::from(candidate.key.use_.slot.0),
                        json
                    ])?;
                commit(CommitResolution::Inserted(candidate))
            },
        )
        .await
    }
}

pub(super) fn end_execution(
    tx: &Connection,
    actor: &lash_durable::ActorKey,
    execution: &lash_durable::resolutions::ExecutionId,
) -> Answer<()> {
    let changed = tx
        .prepare_cached(SQL.end_execution.sql())?
        .execute([&execution.0, actor.as_str()])?;
    Ok(if changed == 0 {
        Err(lash_durable::packages::PackageRefusal::ExecutionUse.into())
    } else {
        Ok(())
    })
}

fn requirements(
    tx: &Connection,
    actor: &lash_durable::ActorKey,
) -> Answer<lash_durable::resolutions::ClaimRequirements> {
    let rows: Vec<(String, i64)> = tx
        .prepare_cached(SQL.requirements_rows.sql())?
        .query_map([actor.as_str()], |row| Ok((row.get(0)?, row.get(1)?)))?
        .collect::<rusqlite::Result<_>>()?;
    let (registrations, resolutions): (Vec<_>, Vec<_>) =
        rows.into_iter().partition(|(_, resolved)| *resolved == 0);
    Ok(lash_durable::resolutions::requirements_of(
        registrations.into_iter().map(|(json, _)| json).collect(),
        resolutions.into_iter().map(|(json, _)| json).collect(),
    ))
}

pub(super) fn owner_requires(
    tx: &Connection,
    actor: &lash_durable::ActorKey,
    requested: Option<&[String]>,
) -> Answer<Vec<String>> {
    let mut requires = match requested {
        Some(requires) => requires.to_vec(),
        None => {
            let stored: Option<String> = tx
                .prepare_cached(SQL.owner_revision.sql())?
                .query_row([actor.as_str()], |row| row.get(3))?;
            match lash_durable::parse_requires(stored.as_deref()) {
                Ok(requires) => requires,
                Err(_) => return Ok(Err(super::corrupt("actor requirements", actor.as_str()))),
            }
        }
    };
    match requirements(tx, actor)? {
        Ok(debt) => {
            requires.extend(debt.tokens());
            requires.sort();
            requires.dedup();
            Ok(Ok(requires))
        }
        Err(error) => Ok(Err(error)),
    }
}

fn registered(
    tx: &Connection,
    actor: &lash_durable::ActorKey,
    execution: &lash_durable::resolutions::ExecutionId,
) -> Answer<ExecutionRegistration> {
    let row = tx
        .prepare_cached(SQL.registration.sql())?
        .query_row([&execution.0], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })
        .optional()?;
    let Some((owner, json)) = row else {
        return Ok(Err(
            lash_durable::packages::PackageRefusal::ExecutionUse.into()
        ));
    };
    if owner != actor.as_str() {
        return Ok(Err(
            lash_durable::packages::PackageRefusal::ExecutionUse.into()
        ));
    }
    Ok(serde_json::from_str(&json).map_err(|_| super::corrupt("execution registration", &json)))
}

pub(super) fn adopt(
    tx: &Connection,
    actor: &lash_durable::ActorKey,
    successor: &ExecutionRegistration,
) -> Answer<()> {
    let old = match registered(tx, actor, &successor.execution)? {
        Ok(old) => old,
        Err(error) => return Ok(Err(error)),
    };
    let record = match old.adopt(successor) {
        Ok(record) => record,
        Err(error) => return Ok(Err(error)),
    };
    if crate::artifact_store::artifact_fenced_tx(tx, &record.referrer)? {
        return Ok(Err(lash_durable::packages::PackageRefusal::ReferrerEnded(
            record.referrer,
        )
        .into()));
    }
    let json = serde_json::to_string(&record)
        .map_err(|error| rusqlite::Error::ToSqlConversionFailure(Box::new(error)))?;
    tx.prepare_cached(SQL.adopt_execution.sql())?
        .execute(rusqlite::params![record.execution.0, actor.as_str(), json])?;
    Ok(Ok(()))
}

pub(super) fn seed(
    tx: &Connection,
    actor: &lash_durable::ActorKey,
    record: &ResolutionRecord,
) -> Answer<()> {
    let registration = match registered(tx, actor, &record.key.execution)? {
        Ok(registration) => registration,
        Err(error) => return Ok(Err(error)),
    };
    if let Err(error) = registration.validate(record) {
        return Ok(Err(error));
    }
    if crate::artifact_store::artifact_fenced_tx(tx, &registration.referrer)? {
        return Ok(Err(lash_durable::packages::PackageRefusal::ReferrerEnded(
            registration.referrer,
        )
        .into()));
    }
    match load(tx, &record.key)? {
        Err(error) => return Ok(Err(error)),
        Ok(Some(stored)) => {
            return Ok(if stored == *record {
                Ok(())
            } else {
                Err(lash_durable::packages::PackageRefusal::Immutable(format!(
                    "resolution {:?}",
                    record.key
                ))
                .into())
            });
        }
        Ok(None) => {}
    }
    let json = serde_json::to_string(record)
        .map_err(|error| rusqlite::Error::ToSqlConversionFailure(Box::new(error)))?;
    tx.prepare_cached(SQL.insert_resolution.sql())?
        .execute(rusqlite::params![
            record.key.execution.0,
            record.key.use_.link.to_string(),
            i64::from(record.key.use_.slot.0),
            json
        ])?;
    Ok(Ok(()))
}

/// New-child facts commit with actor creation under the registrar's existing
/// transaction, before the ready actor can be claimed by another node.
pub(crate) fn register_process_execution(
    tx: &Connection,
    process: &lash_protocol_types::ProcessId,
    execution: Option<&ExecutionRegistration>,
    initial: &[ResolutionRecord],
) -> Answer<()> {
    let Some(execution) = execution else {
        return Ok(if initial.is_empty() {
            Ok(())
        } else {
            Err(lash_durable::packages::PackageRefusal::ExecutionUse.into())
        });
    };
    if execution.execution != lash_durable::resolutions::ExecutionId::process(process)
        || execution.referrer
            != lash_core_store::artifact_referrer::ArtifactReferrer::ProcessRecord(process.clone())
    {
        return Ok(Err(
            lash_durable::packages::PackageRefusal::ExecutionUse.into()
        ));
    }
    let actor = match lash_durable::ActorKey::process(process.as_str()) {
        Ok(actor) => actor,
        Err(_) => {
            return Ok(Err(
                lash_durable::packages::PackageRefusal::ExecutionUse.into()
            ));
        }
    };
    if let Err(error) = register(tx, &actor, execution)? {
        return Ok(Err(error));
    }
    for record in initial {
        if let Err(error) = seed(tx, &actor, record)? {
            return Ok(Err(error));
        }
    }
    let requires = match owner_requires(tx, &actor, None)? {
        Ok(requires) => requires,
        Err(error) => return Ok(Err(error)),
    };
    super::processes::stamp_requires(tx, actor.as_str(), &requires)?;
    Ok(Ok(()))
}
