//! Execution resolution and live-node claim capabilities:
//! the PostgreSQL side of `lash_store_sql::durable::packages`.

use std::sync::LazyLock;

use lash_durable::packages::NodeCapability;
use lash_durable::{DurableError, DurableInstant};
use lash_store_sql::Dialect;
use lash_store_sql::durable::packages::PackageStatements;
use sqlx::PgConnection;

use super::sqlx_failure;

static SQL: LazyLock<PackageStatements> =
    LazyLock::new(|| PackageStatements::render(Dialect::postgres()));

/// Every node live at `now`, by node id.
pub(super) async fn live_nodes(
    tx: &mut PgConnection,
    now: DurableInstant,
) -> Result<Vec<NodeCapability>, DurableError> {
    let rows: Vec<lash_durable::packages::NodeRow> =
        crate::observed_sql::query_as(SQL.live_nodes.sql())
            .bind(now.0)
            .fetch_all(crate::observed_sql::executor(&mut *tx))
            .await
            .map_err(sqlx_failure)?;
    lash_durable::packages::nodes_of(rows)
}

/// Up to `limit` actors that have not ended and carry claim requirements,
/// by key after `after`.
pub(super) async fn actor_requirements(
    tx: &mut PgConnection,
    after: Option<&lash_durable::ActorKey>,
    limit: usize,
) -> Result<Vec<(lash_durable::ActorKey, Vec<String>)>, DurableError> {
    let rows: Vec<(String, String)> = crate::observed_sql::query_as(SQL.actor_requirements.sql())
        .bind(after.map_or("", lash_durable::ActorKey::as_str))
        .bind(i64::try_from(limit).unwrap_or(i64::MAX))
        .fetch_all(crate::observed_sql::executor(&mut *tx))
        .await
        .map_err(sqlx_failure)?;
    lash_durable::packages::requirement_rows(rows)
}

use super::{CommitCapacity, PostgresDurableStore, finish};
use lash_durable::resolutions::{
    CommitResolution, ExecutionRegistration, ExecutionResolutions, ResolutionKey, ResolutionOwner,
    ResolutionRecord,
};

pub(super) async fn register(
    tx: &mut PgConnection,
    actor: &lash_durable::ActorKey,
    record: &ExecutionRegistration,
) -> Result<(), DurableError> {
    crate::artifact_store::lock_referrer_tx(tx, &record.referrer)
        .await
        .map_err(sqlx_failure)?;
    if crate::artifact_store::is_fenced_tx(tx, &record.referrer).await? {
        return Err(
            lash_durable::packages::PackageRefusal::ReferrerEnded(record.referrer.clone()).into(),
        );
    }
    let json = serde_json::to_string(record)
        .map_err(|error| super::corrupt("execution registration", &error.to_string()))?;
    crate::observed_sql::query(SQL.register_execution.sql())
        .bind(&record.execution.0)
        .bind(actor.as_str())
        .bind(&json)
        .execute(crate::observed_sql::executor(&mut *tx))
        .await
        .map_err(sqlx_failure)?;
    let found: Option<String> = crate::observed_sql::query_scalar(SQL.registration_matches.sql())
        .bind(&record.execution.0)
        .bind(actor.as_str())
        .bind(&json)
        .fetch_optional(crate::observed_sql::executor(&mut *tx))
        .await
        .map_err(sqlx_failure)?;
    found
        .map(|_| ())
        .ok_or_else(|| lash_durable::packages::PackageRefusal::ExecutionUse.into())
}

async fn load(
    tx: &mut PgConnection,
    key: &ResolutionKey,
) -> Result<Option<ResolutionRecord>, DurableError> {
    let json: Option<String> = crate::observed_sql::query_scalar(SQL.load_resolution.sql())
        .bind(&key.execution.0)
        .bind(key.use_.link.to_string())
        .bind(i64::from(key.use_.slot.0))
        .fetch_optional(crate::observed_sql::executor(&mut *tx))
        .await
        .map_err(sqlx_failure)?;
    json.map(|json| {
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
    .transpose()
}

lash_store_sql::statements! {
    /// Lock ownership before classifying a resolution or reading its winner.
    struct ResolutionPostgresStatements @ "durable_resolutions_postgres" {
        owner = "SELECT epoch, state, state_revision, base_requires FROM actors WHERE actor_key = ?1 FOR UPDATE";
    }
}
static POSTGRES: LazyLock<ResolutionPostgresStatements> =
    LazyLock::new(|| ResolutionPostgresStatements::render(Dialect::postgres()));

#[async_trait::async_trait]
impl ExecutionResolutions for PostgresDurableStore {
    async fn requirements(
        &self,
        actor: &lash_durable::ActorKey,
    ) -> Result<lash_durable::resolutions::ClaimRequirements, DurableError> {
        self.within(CommitCapacity::Work, async {
            let mut connection = self.pools.work.acquire().await.map_err(sqlx_failure)?;
            requirements(&mut connection, actor).await
        })
        .await
    }
    async fn load(&self, key: &ResolutionKey) -> Result<Option<ResolutionRecord>, DurableError> {
        self.within(CommitCapacity::Work, async {
            let mut connection = self.pools.work.acquire().await.map_err(sqlx_failure)?;
            load(&mut connection, key).await
        })
        .await
    }
    async fn commit(
        &self,
        owner: &ResolutionOwner,
        candidate: ResolutionRecord,
    ) -> Result<CommitResolution, DurableError> {
        self.within(CommitCapacity::Work, async {
            let (mut tx, _) = self
                .open(lash_durable::CommitLabel::RESOLUTION_COMMIT)
                .await?;
            let outcome = commit_resolution(&mut tx, owner, candidate).await;
            finish(tx, outcome).await
        })
        .await
    }
}

async fn commit_resolution(
    tx: &mut PgConnection,
    owner: &ResolutionOwner,
    candidate: ResolutionRecord,
) -> Result<CommitResolution, DurableError> {
    let row: Option<(i64, String, i64, Option<String>)> =
        crate::observed_sql::query_as(POSTGRES.owner.sql())
            .bind(owner.actor.as_str())
            .fetch_optional(crate::observed_sql::executor(&mut *tx))
            .await
            .map_err(sqlx_failure)?;
    let Some((epoch, state, revision, _requires)) = row else {
        return Err(DurableError::OwnershipLost(lash_durable::Fenced {
            actor: owner.actor.clone(),
            held: owner.epoch,
            current: None,
        }));
    };
    if epoch != owner.epoch.0 || state != "owned" {
        return Err(DurableError::OwnershipLost(lash_durable::Fenced {
            actor: owner.actor.clone(),
            held: owner.epoch,
            current: Some(lash_durable::Epoch(epoch)),
        }));
    }
    let row: Option<(String, String)> = crate::observed_sql::query_as(SQL.registration.sql())
        .bind(&candidate.key.execution.0)
        .fetch_optional(crate::observed_sql::executor(&mut *tx))
        .await
        .map_err(sqlx_failure)?;
    let Some((actor, json)) = row else {
        return Err(lash_durable::packages::PackageRefusal::ExecutionUse.into());
    };
    if actor != owner.actor.as_str() {
        return Err(lash_durable::packages::PackageRefusal::ExecutionUse.into());
    }
    let registration: ExecutionRegistration =
        serde_json::from_str(&json).map_err(|_| super::corrupt("execution registration", &json))?;
    registration.validate(&candidate)?;
    crate::artifact_store::lock_referrer_tx(tx, &registration.referrer)
        .await
        .map_err(sqlx_failure)?;
    if crate::artifact_store::is_fenced_tx(tx, &registration.referrer)
        .await
        .map_err(DurableError::from)?
    {
        return Err(
            lash_durable::packages::PackageRefusal::ReferrerEnded(registration.referrer).into(),
        );
    }
    if let Some(winner) = load(tx, &candidate.key).await? {
        return Ok(CommitResolution::Existing(winner));
    }
    if revision != owner.expected_revision.0 {
        return Err(DurableError::ResolutionRevisionConflict {
            actor: owner.actor.clone(),
            expected: owner.expected_revision,
            current: lash_durable::StateRevision(revision),
        });
    }
    crate::observed_sql::query(SQL.resolution_fence.sql())
        .bind(owner.actor.as_str())
        .bind(owner.epoch.0)
        .bind(revision)
        .fetch_one(crate::observed_sql::executor(&mut *tx))
        .await
        .map_err(sqlx_failure)?;
    let json = serde_json::to_string(&candidate)
        .map_err(|error| super::corrupt("resolution", &error.to_string()))?;
    crate::observed_sql::query(SQL.insert_resolution.sql())
        .bind(&candidate.key.execution.0)
        .bind(candidate.key.use_.link.to_string())
        .bind(i64::from(candidate.key.use_.slot.0))
        .bind(json)
        .execute(crate::observed_sql::executor(&mut *tx))
        .await
        .map_err(sqlx_failure)?;
    let requires = owner_requires(tx, &owner.actor, None).await?;
    crate::observed_sql::query(super::SQL.actor.stamp_effective_requires.sql())
        .bind(owner.actor.as_str())
        .bind(lash_durable::encode_requires(&requires))
        .execute(crate::observed_sql::executor(&mut *tx))
        .await
        .map_err(sqlx_failure)?;
    Ok(CommitResolution::Inserted(candidate))
}

pub(super) async fn end_execution(
    tx: &mut PgConnection,
    actor: &lash_durable::ActorKey,
    execution: &lash_durable::resolutions::ExecutionId,
) -> Result<(), DurableError> {
    let changed = crate::observed_sql::query(SQL.end_execution.sql())
        .bind(&execution.0)
        .bind(actor.as_str())
        .execute(crate::observed_sql::executor(&mut *tx))
        .await
        .map_err(sqlx_failure)?
        .rows_affected();
    if changed == 0 {
        return Err(lash_durable::packages::PackageRefusal::ExecutionUse.into());
    }
    Ok(())
}

async fn requirements(
    tx: &mut PgConnection,
    actor: &lash_durable::ActorKey,
) -> Result<lash_durable::resolutions::ClaimRequirements, DurableError> {
    let rows: Vec<(String, i32)> = crate::observed_sql::query_as(SQL.requirements_rows.sql())
        .bind(actor.as_str())
        .fetch_all(crate::observed_sql::executor(&mut *tx))
        .await
        .map_err(sqlx_failure)?;
    let (registrations, resolutions): (Vec<_>, Vec<_>) =
        rows.into_iter().partition(|(_, resolved)| *resolved == 0);
    lash_durable::resolutions::requirements_of(
        registrations.into_iter().map(|(json, _)| json).collect(),
        resolutions.into_iter().map(|(json, _)| json).collect(),
    )
}

pub(super) async fn owner_requires(
    tx: &mut PgConnection,
    actor: &lash_durable::ActorKey,
    requested: Option<&[String]>,
) -> Result<Vec<String>, DurableError> {
    let mut requires = match requested {
        Some(requires) => {
            crate::observed_sql::query(super::SQL.actor.stamp_requires.sql())
                .bind(actor.as_str())
                .bind(lash_durable::encode_requires(requires))
                .execute(crate::observed_sql::executor(&mut *tx))
                .await
                .map_err(sqlx_failure)?;
            requires.to_vec()
        }
        None => {
            let row: (i64, String, i64, Option<String>) =
                crate::observed_sql::query_as(SQL.owner_revision.sql())
                    .bind(actor.as_str())
                    .fetch_one(crate::observed_sql::executor(&mut *tx))
                    .await
                    .map_err(sqlx_failure)?;
            lash_durable::parse_requires(row.3.as_deref())
                .map_err(|_| super::corrupt("actor requirements", actor.as_str()))?
        }
    };
    requires.extend(requirements(tx, actor).await?.tokens());
    requires.sort();
    requires.dedup();
    Ok(requires)
}

async fn registered(
    tx: &mut PgConnection,
    actor: &lash_durable::ActorKey,
    execution: &lash_durable::resolutions::ExecutionId,
) -> Result<ExecutionRegistration, DurableError> {
    let row: Option<(String, String)> = crate::observed_sql::query_as(SQL.registration.sql())
        .bind(&execution.0)
        .fetch_optional(crate::observed_sql::executor(&mut *tx))
        .await
        .map_err(sqlx_failure)?;
    let Some((owner, json)) = row else {
        return Err(lash_durable::packages::PackageRefusal::ExecutionUse.into());
    };
    if owner != actor.as_str() {
        return Err(lash_durable::packages::PackageRefusal::ExecutionUse.into());
    }
    serde_json::from_str(&json).map_err(|_| super::corrupt("execution registration", &json))
}

pub(super) async fn adopt(
    tx: &mut PgConnection,
    actor: &lash_durable::ActorKey,
    successor: &ExecutionRegistration,
) -> Result<(), DurableError> {
    let old = registered(tx, actor, &successor.execution).await?;
    let record = old.adopt(successor)?;
    crate::artifact_store::lock_referrer_tx(tx, &record.referrer)
        .await
        .map_err(sqlx_failure)?;
    if crate::artifact_store::is_fenced_tx(tx, &record.referrer).await? {
        return Err(lash_durable::packages::PackageRefusal::ReferrerEnded(record.referrer).into());
    }
    let json = serde_json::to_string(&record)
        .map_err(|error| super::corrupt("execution registration", &error.to_string()))?;
    crate::observed_sql::query(SQL.adopt_execution.sql())
        .bind(&record.execution.0)
        .bind(actor.as_str())
        .bind(json)
        .execute(crate::observed_sql::executor(&mut *tx))
        .await
        .map_err(sqlx_failure)?;
    Ok(())
}

pub(super) async fn seed(
    tx: &mut PgConnection,
    actor: &lash_durable::ActorKey,
    record: &ResolutionRecord,
) -> Result<(), DurableError> {
    let registration = registered(tx, actor, &record.key.execution).await?;
    registration.validate(record)?;
    crate::artifact_store::lock_referrer_tx(tx, &registration.referrer)
        .await
        .map_err(sqlx_failure)?;
    if crate::artifact_store::is_fenced_tx(tx, &registration.referrer).await? {
        return Err(
            lash_durable::packages::PackageRefusal::ReferrerEnded(registration.referrer).into(),
        );
    }
    if let Some(stored) = load(tx, &record.key).await? {
        return if stored == *record {
            Ok(())
        } else {
            Err(lash_durable::packages::PackageRefusal::Immutable(format!(
                "resolution {:?}",
                record.key
            ))
            .into())
        };
    }
    let json = serde_json::to_string(record)
        .map_err(|error| super::corrupt("resolution", &error.to_string()))?;
    crate::observed_sql::query(SQL.insert_resolution.sql())
        .bind(&record.key.execution.0)
        .bind(record.key.use_.link.to_string())
        .bind(i64::from(record.key.use_.slot.0))
        .bind(json)
        .execute(crate::observed_sql::executor(&mut *tx))
        .await
        .map_err(sqlx_failure)?;
    Ok(())
}

/// Actor creation and inherited resolution debt are one registrar commit.
pub(crate) async fn register_process_execution(
    tx: &mut PgConnection,
    process: &lash_vocabulary::identity::ProcessId,
    execution: Option<&ExecutionRegistration>,
    initial: &[ResolutionRecord],
) -> Result<(), DurableError> {
    let Some(execution) = execution else {
        return if initial.is_empty() {
            Ok(())
        } else {
            Err(lash_durable::packages::PackageRefusal::ExecutionUse.into())
        };
    };
    if execution.execution != lash_durable::resolutions::ExecutionId::process(process)
        || execution.referrer
            != lash_core_store::artifact_referrer::ArtifactReferrer::ProcessRecord(process.clone())
    {
        return Err(lash_durable::packages::PackageRefusal::ExecutionUse.into());
    }
    let actor = lash_durable::ActorKey::process(process.as_str())
        .map_err(|_| lash_durable::packages::PackageRefusal::ExecutionUse)?;
    register(tx, &actor, execution).await?;
    for record in initial {
        seed(tx, &actor, record).await?;
    }
    let requires = owner_requires(tx, &actor, None).await?;
    crate::observed_sql::query(super::SQL.actor.stamp_effective_requires.sql())
        .bind(actor.as_str())
        .bind(lash_durable::encode_requires(&requires))
        .execute(crate::observed_sql::executor(&mut *tx))
        .await
        .map_err(sqlx_failure)?;
    Ok(())
}

pub(super) async fn end_actor_executions(
    tx: &mut PgConnection,
    actor: &lash_durable::ActorKey,
) -> Result<(), DurableError> {
    crate::observed_sql::query(SQL.end_actor_executions.sql())
        .bind(actor.as_str())
        .execute(crate::observed_sql::executor(tx))
        .await
        .map_err(sqlx_failure)?;
    Ok(())
}
