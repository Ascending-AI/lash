//! PostgreSQL's version owner fence, acquired before graph/artifact locks.
use super::*;
use lash_core_store::checkpoint_graph::NodeId as GraphNode;
use lash_core_store::{
    artifact_referrer::{ArtifactCleanup, ArtifactReferrer, ReferrerGuard},
    environment::*,
};
use lash_durable::environment::{
    self as env, EnvironmentRecord, EnvironmentRefusal as Refusal, EnvironmentStore,
    EnvironmentWrite,
};
use lash_store_sql::{
    Dialect, artifact::cleanup_obligations::CleanupObligationStatements,
    durable::environment::EnvironmentStatements,
};
use std::collections::{BTreeMap, BTreeSet};
static ENV: LazyLock<EnvironmentStatements> =
    LazyLock::new(|| EnvironmentStatements::render(Dialect::postgres()));
static CLEANUP: LazyLock<CleanupObligationStatements> =
    LazyLock::new(|| CleanupObligationStatements::render(Dialect::postgres()));

pub(crate) async fn owner(
    tx: &mut PgConnection,
    id: &VersionOwnerId,
) -> Result<Option<EnvironmentRecord>, DurableError> {
    let row = crate::observed_sql::query(ENV.owner.sql())
        .bind(id.to_string())
        .fetch_optional(crate::observed_sql::executor(tx))
        .await
        .map_err(sqlx_failure)?;
    row.map(|r| {
        env::record(
            get(&r, 0)?,
            get(&r, 1)?,
            get(&r, 2)?,
            get(&r, 3)?,
            get(&r, 4)?,
            get(&r, 5)?,
        )
    })
    .transpose()
}
pub(crate) async fn authority(
    tx: &mut PgConnection,
    id: &VersionOwnerId,
) -> Result<Option<env::EnvironmentAuthority>, DurableError> {
    let row = crate::observed_sql::query(ENV.authority.sql())
        .bind(id.to_string())
        .fetch_optional(crate::observed_sql::executor(tx))
        .await
        .map_err(sqlx_failure)?;
    row.map(|r| {
        env::authority(
            get(&r, 0)?,
            get(&r, 1)?,
            get(&r, 2)?,
            get(&r, 3)?,
            get(&r, 4)?,
        )
    })
    .transpose()
}
async fn required(
    tx: &mut PgConnection,
    id: &VersionOwnerId,
) -> Result<env::EnvironmentAuthority, DurableError> {
    authority(tx, id)
        .await?
        .ok_or_else(|| Refusal::Missing(id.clone()).into())
}
async fn head(
    tx: &mut PgConnection,
    session: &lash_vocabulary::identity::SessionId,
) -> Result<Option<EnvironmentHead>, DurableError> {
    let text: Option<String> = crate::observed_sql::query_scalar(ENV.head.sql())
        .bind(session.as_str())
        .fetch_optional(crate::observed_sql::executor(tx))
        .await
        .map_err(sqlx_failure)?;
    text.as_deref().map(env::decode).transpose()
}
async fn enqueue(
    tx: &mut PgConnection,
    id: &VersionOwnerId,
    retired: bool,
) -> Result<(), DurableError> {
    let cleanup = if retired {
        ArtifactCleanup::ended(
            ArtifactReferrer::EnvironmentVersion(id.clone()),
            vec![],
            None,
        )
    } else {
        ArtifactCleanup::Await(ReferrerGuard::EnvironmentVersionRetired(id.clone()))
    };
    let json = cleanup.to_json().map_err(env::corrupt)?;
    if retired {
        let changed = crate::observed_sql::query(ENV.queue_retired.sql())
            .bind(id.to_string())
            .bind(json)
            .execute(crate::observed_sql::executor(tx))
            .await
            .map_err(sqlx_failure)?
            .rows_affected();
        if changed != 1 {
            return Err(env::corrupt("owner lost its cleanup obligation"));
        }
    } else {
        let obligation = lash_core_store::store::ObligationKey::ArtifactCleanup {
            referrer: cleanup.referrer(),
        }
        .id();
        crate::observed_sql::query(CLEANUP.insert_if_absent.sql())
            .bind("environment_version")
            .bind(id.to_string())
            .bind(json)
            .bind(obligation.as_str())
            .bind(0_i64)
            .execute(crate::observed_sql::executor(tx))
            .await
            .map_err(sqlx_failure)?;
    }
    Ok(())
}
async fn retire(tx: &mut PgConnection, id: &VersionOwnerId) -> Result<(), DurableError> {
    if crate::observed_sql::query(ENV.retire.sql())
        .bind(id.to_string())
        .execute(crate::observed_sql::executor(&mut *tx))
        .await
        .map_err(sqlx_failure)?
        .rows_affected()
        != 0
    {
        enqueue(tx, id, true).await?;
    }
    Ok(())
}

/// All callers take actor/history before this owner lock. Publication/acquire
/// share it with retirement's SKIP LOCKED write lock; exactly one can win.
pub(crate) async fn lock_artifact_owner(
    tx: &mut PgConnection,
    id: &VersionOwnerId,
) -> Result<(), DurableError> {
    let record = authority(tx, id).await?.ok_or_else(|| {
        DurableError::Package(lash_durable::packages::PackageRefusal::ReferrerEnded(
            ArtifactReferrer::EnvironmentVersion(id.clone()),
        ))
    })?;
    let session =
        lash_vocabulary::identity::SessionId::parse(record.actor.id()).map_err(env::corrupt)?;
    PostgresDurableStore::lock_session_writes(tx, &session)
        .await
        .map_err(store_failure)?;
    crate::observed_sql::query(PG.lock_owner.sql())
        .bind(id.to_string())
        .fetch_optional(crate::observed_sql::executor(tx))
        .await
        .map_err(sqlx_failure)?;
    Ok(())
}

pub(super) async fn validate_graph(
    tx: &mut PgConnection,
    graph: &lash_durable::domain::graph_commit::CheckedGraph,
    publishing: &BTreeSet<VersionOwnerId>,
) -> Result<(), DurableError> {
    let supplied = env::supplied_versions(graph)?;
    let mut versions = supplied
        .into_iter()
        .map(|(_, r)| (r.owner.clone(), r))
        .collect::<BTreeMap<_, _>>();
    let named: Vec<_> = graph
        .external
        .iter()
        .chain(&graph.external_roots)
        .map(|n| n.0.to_vec())
        .collect();
    let ids: Vec<String> = crate::observed_sql::query_scalar(PG.owners_of.sql())
        .bind(&named)
        .fetch_all(crate::observed_sql::executor(&mut *tx))
        .await
        .map_err(sqlx_failure)?;
    for id in ids {
        let id: VersionOwnerId =
            serde_json::from_value(serde_json::Value::String(id)).map_err(env::corrupt)?;
        let record = required(tx, &id).await?;
        versions.insert(
            id.clone(),
            RoundEnvironment {
                owner: id,
                version: record.owner.version().clone(),
            },
        );
    }
    for (id, round) in versions {
        crate::observed_sql::query(PG.lock_owner.sql())
            .bind(id.to_string())
            .fetch_optional(crate::observed_sql::executor(&mut *tx))
            .await
            .map_err(sqlx_failure)?;
        let record = required(tx, &id).await?;
        if record.owner.version() != &round.version {
            return Err(Refusal::Content.into());
        }
        match record.owner.state() {
            VersionOwnerState::Published => {}
            VersionOwnerState::Preparing if publishing.contains(&id) => {}
            VersionOwnerState::Preparing => return Err(Refusal::NotPublished(id).into()),
            VersionOwnerState::Retired => return Err(Refusal::Retired(id).into()),
        }
    }
    Ok(())
}

/// Before graph locks, skip owner leaves locked by publication/acquisition.
pub(super) async fn peel_owners(
    tx: &mut PgConnection,
    candidates: &BTreeSet<String>,
) -> Result<BTreeSet<String>, sqlx::Error> {
    let names: Vec<_> = candidates
        .iter()
        .filter_map(|s| GraphNode::from_hex(s))
        .map(|n| n.0.to_vec())
        .collect();
    let rows: Vec<(String, Vec<u8>)> = crate::observed_sql::query_as(PG.owner_nodes.sql())
        .bind(names)
        .fetch_all(crate::observed_sql::executor(&mut *tx))
        .await?;
    let mut skipped = BTreeSet::new();
    for (id, node) in rows {
        let locked: Option<String> = crate::observed_sql::query_scalar(PG.peel_owner.sql())
            .bind(id)
            .fetch_optional(crate::observed_sql::executor(&mut *tx))
            .await?;
        if locked.is_none() {
            skipped.insert(node.iter().map(|b| format!("{b:02x}")).collect::<String>());
        }
    }
    Ok(skipped)
}
pub(super) async fn peeled(tx: &mut PgConnection, nodes: &[String]) -> Result<(), sqlx::Error> {
    let names: Vec<_> = nodes
        .iter()
        .filter_map(|s| GraphNode::from_hex(s))
        .map(|n| n.0.to_vec())
        .collect();
    let ids: Vec<String> = crate::observed_sql::query_scalar(PG.owners_of.sql())
        .bind(names)
        .fetch_all(crate::observed_sql::executor(&mut *tx))
        .await?;
    for id in ids {
        let id: VersionOwnerId =
            serde_json::from_value(serde_json::Value::String(id)).map_err(protocol)?;
        let record = required(tx, &id).await.map_err(protocol)?;
        if record.owner.state() != VersionOwnerState::Published {
            return Err(protocol(env::corrupt("peeled owner was not published")));
        }
        retire(tx, &id).await.map_err(protocol)?;
    }
    Ok(())
}
fn protocol(e: impl std::error::Error + Send + Sync + 'static) -> sqlx::Error {
    sqlx::Error::Decode(Box::new(e))
}

pub(super) async fn apply(
    tx: &mut PgConnection,
    commit: &Committing<'_>,
    write: &EnvironmentWrite,
) -> Result<(), DurableError> {
    if commit.actor.kind() != lash_durable::ActorKind::Session {
        return Err(Refusal::SessionActor.into());
    }
    match write {
        EnvironmentWrite::Prepare { owner, body_json } => {
            let body: EnvironmentBody = env::decode(body_json)?;
            if body.validate().is_err()
                || body.id() != owner.version()
                || owner.state() != VersionOwnerState::Preparing
            {
                return Err(Refusal::Content.into());
            }
            let root = env::version_node(&RoundEnvironment {
                owner: owner.id().clone(),
                version: owner.version().clone(),
            })?;
            if crate::observed_sql::query(ENV.prepare.sql())
                .bind(owner.id().to_string())
                .bind(owner.version().to_string())
                .bind(commit.actor.as_str())
                .bind(commit.epoch.0)
                .bind(body_json)
                .bind(&root.0[..])
                .execute(crate::observed_sql::executor(&mut *tx))
                .await
                .map_err(sqlx_failure)?
                .rows_affected()
                != 1
            {
                return Err(Refusal::Preparation(owner.id().clone()).into());
            }
            enqueue(tx, owner.id(), false).await?;
        }
        EnvironmentWrite::Publish { owner } | EnvironmentWrite::Abort { owner } => {
            crate::observed_sql::query(PG.lock_owner.sql())
                .bind(owner.to_string())
                .fetch_optional(crate::observed_sql::executor(&mut *tx))
                .await
                .map_err(sqlx_failure)?;
            let record = required(tx, owner).await?;
            if record.actor != *commit.actor
                || record.preparing_epoch != commit.epoch
                || record.owner.state() != VersionOwnerState::Preparing
            {
                return Err(Refusal::Preparation(owner.clone()).into());
            }
            if matches!(write, EnvironmentWrite::Publish { .. }) {
                let loaded = owner_body(tx, owner).await?;
                let body = loaded
                    .body
                    .as_ref()
                    .ok_or_else(|| env::corrupt("preparing owner has no body"))?;
                for fragment in body.fragments().values() {
                    for accepted in &fragment.fragment().package_roots {
                        let reference = lash_durable::packages::publication_reference(
                            &accepted.publication.key(),
                        );
                        let held: bool =
                            crate::observed_sql::query_scalar(ENV.publication_held.sql())
                                .bind(owner.to_string())
                                .bind(reference)
                                .fetch_one(crate::observed_sql::executor(&mut *tx))
                                .await
                                .map_err(sqlx_failure)?;
                        if !held {
                            return Err(Refusal::Content.into());
                        }
                    }
                }
                crate::observed_sql::query(ENV.publish.sql())
                    .bind(owner.to_string())
                    .bind(commit.actor.as_str())
                    .bind(commit.epoch.0)
                    .execute(crate::observed_sql::executor(tx))
                    .await
                    .map_err(sqlx_failure)?;
            } else {
                retire(tx, owner).await?;
            }
        }
        EnvironmentWrite::Head {
            expected,
            head: next,
        } => {
            let session = lash_vocabulary::identity::SessionId::parse(commit.actor.id())
                .map_err(env::corrupt)?;
            if head(tx, &session).await? != *expected {
                return Err(Refusal::HeadConflict.into());
            }
            if let Some(next) = next {
                let record = required(tx, &next.owner).await?;
                if record.owner.state() != VersionOwnerState::Published {
                    return Err(Refusal::NotPublished(next.owner.clone()).into());
                }
                if record.owner.version() != &next.version {
                    return Err(Refusal::Content.into());
                }
            }
            let old: Option<Vec<u8>> = crate::observed_sql::query_scalar(ENV.head_root.sql())
                .bind(session.as_str())
                .fetch_optional(crate::observed_sql::executor(&mut *tx))
                .await
                .map_err(sqlx_failure)?;
            if let Some(next) = next {
                let root = env::stage_head(&mut env::staging(), next)?;
                crate::observed_sql::query(ENV.head_put.sql())
                    .bind(session.as_str())
                    .bind(env::json(next)?)
                    .bind(&root.0[..])
                    .bind(commit.epoch.0)
                    .execute(crate::observed_sql::executor(&mut *tx))
                    .await
                    .map_err(sqlx_failure)?;
            } else {
                crate::observed_sql::query(ENV.head_remove.sql())
                    .bind(session.as_str())
                    .execute(crate::observed_sql::executor(&mut *tx))
                    .await
                    .map_err(sqlx_failure)?;
            }
            if let Some(old) = old {
                commit
                    .retired
                    .stored(old.iter().map(|b| format!("{b:02x}")).collect::<String>());
            }
        }
    }
    Ok(())
}
pub(super) async fn finish(
    tx: &mut PgConnection,
    writes: &[DomainWrite],
) -> Result<(), DurableError> {
    for write in writes {
        if let DomainWrite::Environment(EnvironmentWrite::Publish { owner }) = write {
            let record = required(tx, owner).await?;
            let root = env::version_node(&RoundEnvironment {
                owner: owner.clone(),
                version: record.owner.version().clone(),
            })?;
            let retained: bool = crate::observed_sql::query_scalar(ENV.has_reader.sql())
                .bind(&root.0[..])
                .fetch_one(crate::observed_sql::executor(&mut *tx))
                .await
                .map_err(sqlx_failure)?;
            if !retained {
                return Err(Refusal::NoReader.into());
            }
        }
    }
    Ok(())
}
pub(super) async fn remove_head(
    tx: &mut PgConnection,
    session: &lash_vocabulary::identity::SessionId,
) -> Result<Vec<String>, sqlx::Error> {
    crate::keys::query_scalar(ENV.head_remove.sql())
        .bind(session.as_str())
        .fetch_all(crate::observed_sql::executor(tx))
        .await
}
#[async_trait::async_trait]
impl EnvironmentStore for PostgresDurableStore {
    async fn environment_owner(
        &self,
        id: &VersionOwnerId,
    ) -> Result<Option<EnvironmentRecord>, DurableError> {
        self.read(|tx| Box::pin(owner(tx, id))).await
    }
    async fn environment_head(
        &self,
        session: &lash_vocabulary::identity::SessionId,
    ) -> Result<Option<EnvironmentHead>, DurableError> {
        self.read(|tx| Box::pin(head(tx, session))).await
    }
    async fn retire_abandoned_environment(
        &self,
        id: &VersionOwnerId,
    ) -> Result<bool, DurableError> {
        let (mut tx, _) = self.open(CommitLabel::new("environment.abandon")).await?;
        let outcome = async {
            let record = required(&mut tx, id).await?;
            let session = lash_vocabulary::identity::SessionId::parse(record.actor.id())
                .map_err(env::corrupt)?;
            PostgresDurableStore::lock_session_writes(&mut tx, &session)
                .await
                .map_err(store_failure)?;
            crate::observed_sql::query(PG.lock_owner.sql())
                .bind(id.to_string())
                .fetch_optional(crate::observed_sql::executor(&mut **tx))
                .await
                .map_err(sqlx_failure)?;
            let record = required(&mut tx, id).await?;
            match record.owner.state() {
                VersionOwnerState::Published => Ok(false),
                VersionOwnerState::Retired => Ok(true),
                VersionOwnerState::Preparing => {
                    let epoch: Option<i64> =
                        crate::observed_sql::query_scalar(ENV.actor_epoch.sql())
                            .bind(record.actor.as_str())
                            .fetch_optional(crate::observed_sql::executor(&mut **tx))
                            .await
                            .map_err(sqlx_failure)?;
                    if epoch == Some(record.preparing_epoch.0) {
                        return Ok(false);
                    }
                    retire(&mut tx, id).await?;
                    Ok(true)
                }
            }
        }
        .await;
        super::finish(tx, outcome).await
    }
}
lash_store_sql::statements! {
    pub(crate) struct PostgresEnvironmentStatements @ "durable_environment_postgres" {
        lock_owner = "SELECT owner_id FROM environment_owners WHERE owner_id = ?1 FOR UPDATE";
        peel_owner = "SELECT owner_id FROM environment_owners WHERE owner_id = ?1 FOR UPDATE SKIP LOCKED";
        owners_of = "SELECT owner_id FROM environment_owners WHERE manifest_ref = ANY(?1) ORDER BY owner_id";
        owner_nodes = "SELECT owner_id, manifest_ref FROM environment_owners WHERE manifest_ref = ANY(?1) ORDER BY owner_id";
    }
}
static PG: LazyLock<PostgresEnvironmentStatements> =
    LazyLock::new(|| PostgresEnvironmentStatements::render(Dialect::postgres()));

async fn owner_body(
    tx: &mut PgConnection,
    id: &VersionOwnerId,
) -> Result<EnvironmentRecord, DurableError> {
    owner(tx, id)
        .await?
        .ok_or_else(|| Refusal::Missing(id.clone()).into())
}
