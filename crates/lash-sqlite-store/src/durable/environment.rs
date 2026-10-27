//! SQLite's environment owner transitions, inside its single writer.
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
    artifact::cleanup_obligations::CleanupObligationStatements,
    durable::environment::EnvironmentStatements,
};
use std::collections::{BTreeMap, BTreeSet};
static ENV: LazyLock<EnvironmentStatements> =
    LazyLock::new(|| EnvironmentStatements::render(crate::schema_layout::MAIN));
static CLEANUP: LazyLock<CleanupObligationStatements> =
    LazyLock::new(|| CleanupObligationStatements::render(crate::schema_layout::MAIN));

pub(crate) fn owner(tx: &Connection, id: &VersionOwnerId) -> Answer<Option<EnvironmentRecord>> {
    let row = tx
        .prepare_cached(ENV.owner.sql())?
        .query_row([id.to_string()], |r| {
            Ok((
                r.get(0)?,
                r.get(1)?,
                r.get(2)?,
                r.get(3)?,
                r.get(4)?,
                r.get(5)?,
            ))
        })
        .optional()?;
    Ok(row
        .map(|(id, version, state, actor, epoch, body)| {
            env::record(id, version, state, actor, epoch, body)
        })
        .transpose())
}
pub(crate) fn authority(
    tx: &Connection,
    id: &VersionOwnerId,
) -> Answer<Option<env::EnvironmentAuthority>> {
    let row = tx
        .prepare_cached(ENV.authority.sql())?
        .query_row([id.to_string()], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?))
        })
        .optional()?;
    Ok(row
        .map(|(id, version, state, actor, epoch)| env::authority(id, version, state, actor, epoch))
        .transpose())
}
fn required(tx: &Connection, id: &VersionOwnerId) -> Answer<env::EnvironmentAuthority> {
    Ok(authority(tx, id)?.and_then(|r| r.ok_or_else(|| Refusal::Missing(id.clone()).into())))
}
pub(super) fn head(
    tx: &Connection,
    session: &lash_kernel_value::identity::SessionId,
) -> Answer<Option<EnvironmentHead>> {
    let text: Option<String> = tx
        .prepare_cached(ENV.head.sql())?
        .query_row([session.as_str()], |r| r.get(0))
        .optional()?;
    Ok(text.as_deref().map(env::decode).transpose())
}
fn enqueue(tx: &Connection, owner: &VersionOwnerId, retired: bool) -> rusqlite::Result<()> {
    let cleanup = if retired {
        ArtifactCleanup::ended(
            ArtifactReferrer::EnvironmentVersion(owner.clone()),
            vec![],
            None,
        )
    } else {
        ArtifactCleanup::Await(ReferrerGuard::EnvironmentVersionRetired(owner.clone()))
    };
    let json = cleanup.to_json().map_err(conversion)?;
    if retired {
        let changed = tx.execute(
            ENV.queue_retired.sql(),
            rusqlite::params![owner.to_string(), json],
        )?;
        if changed != 1 {
            return Err(conversion(env::corrupt(
                "owner lost its cleanup obligation",
            )));
        }
    } else {
        let id = lash_core_store::store::ObligationKey::ArtifactCleanup {
            referrer: cleanup.referrer(),
        }
        .id();
        tx.execute(
            CLEANUP.insert_if_absent.sql(),
            rusqlite::params![
                "environment_version",
                owner.to_string(),
                json,
                id.as_str(),
                0_i64,
                cleanup.gate().map(|gate| gate.key())
            ],
        )?;
    }
    Ok(())
}
fn retire(tx: &Connection, owner: &VersionOwnerId) -> rusqlite::Result<()> {
    if tx.execute(ENV.retire.sql(), [owner.to_string()])? != 0 {
        enqueue(tx, owner, true)?;
    }
    Ok(())
}

/// Same transaction as each deletion, including inline peel and orphan audit.
pub(super) fn peeled(tx: &Connection, nodes: &BTreeSet<String>) -> rusqlite::Result<()> {
    let rows = tx
        .prepare_cached(SQLITE.peeled.sql())?
        .query_map([graph::json_array(nodes)], |r| r.get::<_, String>(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    for id in rows {
        let id: VersionOwnerId =
            serde_json::from_value(serde_json::Value::String(id)).map_err(conversion)?;
        let record = required(tx, &id)?.map_err(conversion)?;
        if record.owner.state() != VersionOwnerState::Published {
            return Err(conversion(env::corrupt("peeled owner was not published")));
        }
        retire(tx, &id)?;
    }
    Ok(())
}

pub(super) type SuppliedVersions = Result<Vec<(GraphNode, RoundEnvironment)>, DurableError>;

pub(super) fn validate_graph(
    tx: &Connection,
    graph: &lash_durable::domain::graph_commit::CheckedGraph,
    supplied: &SuppliedVersions,
    publishing: &BTreeSet<VersionOwnerId>,
) -> Answer<()> {
    let supplied = match supplied {
        Ok(s) => s,
        Err(e) => return Ok(Err(e.clone())),
    };
    let mut versions = supplied
        .iter()
        .map(|(_, r)| (r.owner.clone(), (r.clone(), None)))
        .collect::<BTreeMap<_, _>>();
    let named: BTreeSet<_> = graph
        .external
        .iter()
        .chain(&graph.external_roots)
        .map(GraphNode::to_hex)
        .collect();
    let ids: Vec<_> = versions.keys().map(ToString::to_string).collect();
    let mut statement = tx.prepare_cached(SQLITE.validation_authorities.sql())?;
    let mut rows = statement.query(rusqlite::params![
        graph::json_array(&named),
        graph::json_array(&ids)
    ])?;
    while let Some(row) = rows.next()? {
        let record = match env::authority(
            row.get(0)?,
            row.get(1)?,
            row.get(2)?,
            row.get(3)?,
            row.get(4)?,
        ) {
            Ok(record) => record,
            Err(error) => return Ok(Err(error)),
        };
        let id = record.owner.id().clone();
        if row.get::<_, bool>(5)? {
            versions.insert(
                id.clone(),
                (
                    RoundEnvironment {
                        owner: id,
                        version: record.owner.version().clone(),
                    },
                    Some(record),
                ),
            );
        } else if let Some((_, observed)) = versions.get_mut(&id) {
            *observed = Some(record);
        }
    }
    for (_, (round, observed)) in versions {
        let record = match observed {
            Some(record) => record,
            None => return Ok(Err(Refusal::Missing(round.owner).into())),
        };
        if record.owner.version() != &round.version {
            return Ok(Err(Refusal::Content.into()));
        }
        match record.owner.state() {
            VersionOwnerState::Published => {}
            VersionOwnerState::Preparing if publishing.contains(&round.owner) => {}
            VersionOwnerState::Preparing => {
                return Ok(Err(Refusal::NotPublished(round.owner).into()));
            }
            VersionOwnerState::Retired => return Ok(Err(Refusal::Retired(round.owner).into())),
        }
    }
    Ok(Ok(()))
}

pub(super) fn apply(
    tx: &Connection,
    commit: &Committing<'_>,
    write: &EnvironmentWrite,
) -> Answer<()> {
    macro_rules! take {
        ($e:expr) => {
            match $e? {
                Ok(v) => v,
                Err(e) => return Ok(Err(e)),
            }
        };
    }
    if commit.actor.kind() != lash_durable::ActorKind::Session {
        return Ok(Err(Refusal::SessionActor.into()));
    }
    match write {
        EnvironmentWrite::Prepare { owner, body_json } => {
            let body = match env::decoded_body(owner.version(), body_json) {
                Ok(b) => b,
                Err(e) => return Ok(Err(e)),
            };
            if body.validate().is_err()
                || body.id() != owner.version()
                || owner.state() != VersionOwnerState::Preparing
            {
                return Ok(Err(Refusal::Content.into()));
            }
            let root = match env::version_node(&RoundEnvironment {
                owner: owner.id().clone(),
                version: owner.version().clone(),
            }) {
                Ok(r) => r,
                Err(e) => return Ok(Err(e)),
            };
            if tx.execute(
                ENV.prepare.sql(),
                rusqlite::params![
                    owner.id().to_string(),
                    owner.version().to_string(),
                    commit.actor.as_str(),
                    commit.epoch.0,
                    body_json,
                    &root.0[..]
                ],
            )? != 1
            {
                return Ok(Err(Refusal::Preparation(owner.id().clone()).into()));
            }
            enqueue(tx, owner.id(), false)?;
        }
        EnvironmentWrite::Publish { owner } | EnvironmentWrite::Abort { owner } => {
            let record = take!(required(tx, owner));
            if record.actor != *commit.actor
                || record.preparing_epoch != commit.epoch
                || record.owner.state() != VersionOwnerState::Preparing
            {
                return Ok(Err(Refusal::Preparation(owner.clone()).into()));
            }
            if matches!(write, EnvironmentWrite::Publish { .. }) {
                let loaded = take!(owner_body(tx, owner));
                let Some(body) = loaded.body.as_ref() else {
                    return Ok(Err(env::corrupt("preparing owner has no body")));
                };
                for fragment in body.fragments().values() {
                    for accepted in &fragment.fragment().package_roots {
                        let reference = lash_durable::packages::publication_reference(
                            &accepted.publication.key(),
                        );
                        let held: bool = tx
                            .prepare_cached(ENV.publication_held.sql())?
                            .query_row(rusqlite::params![owner.to_string(), reference], |r| {
                                r.get(0)
                            })?;
                        if !held {
                            return Ok(Err(Refusal::Content.into()));
                        }
                    }
                }
                tx.execute(
                    ENV.publish.sql(),
                    rusqlite::params![owner.to_string(), commit.actor.as_str(), commit.epoch.0],
                )?;
            } else {
                retire(tx, owner)?;
            }
        }
        EnvironmentWrite::Head {
            expected,
            head: next,
        } => {
            let session = lash_kernel_value::identity::SessionId::parse(commit.actor.id())
                .map_err(env::corrupt)
                .map_err(conversion)?;
            if take!(head(tx, &session)) != *expected {
                return Ok(Err(Refusal::HeadConflict.into()));
            }
            if let Some(next) = next {
                let record = take!(required(tx, &next.owner));
                if record.owner.state() != VersionOwnerState::Published {
                    return Ok(Err(Refusal::NotPublished(next.owner.clone()).into()));
                }
                if record.owner.version() != &next.version {
                    return Ok(Err(Refusal::Content.into()));
                }
            }
            let old: Option<String> = tx
                .prepare_cached(ENV.head_root.sql())?
                .query_row([session.as_str()], |r| crate::codec::hash_column(r, 0))
                .optional()?;
            if let Some(next) = next {
                let mut staging = env::staging();
                let root = match env::stage_head(&mut staging, next) {
                    Ok(r) => r,
                    Err(e) => return Ok(Err(e)),
                };
                let json = match env::json(next) {
                    Ok(j) => j,
                    Err(e) => return Ok(Err(e)),
                };
                tx.prepare_cached(ENV.head_put.sql())?
                    .execute(rusqlite::params![
                        session.as_str(),
                        json,
                        &root.0[..],
                        commit.epoch.0
                    ])?;
            } else {
                remove_head(tx, &session)?;
            }
            if let Some(old) = old {
                commit.retired.stored(old);
            }
        }
    }
    Ok(Ok(()))
}

pub(super) fn finish(tx: &Connection, writes: &[DomainWrite]) -> Answer<()> {
    for write in writes {
        if let DomainWrite::Environment(EnvironmentWrite::Publish { owner }) = write {
            let record = match required(tx, owner)? {
                Ok(r) => r,
                Err(e) => return Ok(Err(e)),
            };
            let root = match env::version_node(&RoundEnvironment {
                owner: owner.clone(),
                version: record.owner.version().clone(),
            }) {
                Ok(r) => r,
                Err(e) => return Ok(Err(e)),
            };
            let retained: bool = tx
                .prepare_cached(ENV.has_reader.sql())?
                .query_row([&root.0[..]], |r| r.get(0))?;
            if !retained {
                return Ok(Err(Refusal::NoReader.into()));
            }
        }
    }
    Ok(Ok(()))
}

#[async_trait::async_trait]
impl EnvironmentStore for SqliteDurableStore {
    async fn environment_owner(
        &self,
        id: &VersionOwnerId,
    ) -> Result<Option<EnvironmentRecord>, DurableError> {
        let id = id.clone();
        self.query(move |tx| owner(tx, &id)).await
    }
    async fn environment_head(
        &self,
        session: &lash_kernel_value::identity::SessionId,
    ) -> Result<Option<EnvironmentHead>, DurableError> {
        let session = session.clone();
        self.query(move |tx| head(tx, &session)).await
    }
    async fn retire_abandoned_environment(
        &self,
        id: &VersionOwnerId,
    ) -> Result<bool, DurableError> {
        let id = id.clone();
        self.write(CommitLabel::new("environment.abandon"), move |tx, _| {
            let record = match required(tx, &id)? {
                Ok(r) => r,
                Err(e) => return refuse(e),
            };
            match record.owner.state() {
                VersionOwnerState::Published => commit(false),
                VersionOwnerState::Retired => commit(true),
                VersionOwnerState::Preparing => {
                    let epoch: Option<i64> = tx
                        .prepare_cached(ENV.actor_epoch.sql())?
                        .query_row([record.actor.as_str()], |r| r.get(0))
                        .optional()?;
                    if epoch == Some(record.preparing_epoch.0) {
                        return commit(false);
                    }
                    retire(tx, &id)?;
                    commit(true)
                }
            }
        })
        .await
    }
}

lash_store_sql::statements! {
    pub(crate) struct SqliteEnvironmentStatements @ "durable_environment_sqlite" {
        /// The commit's complete authority set, without owner bodies or
        /// one lookup per owner. SQLite's writer lock fixes the snapshot.
        validation_authorities = "SELECT owner_id, version_id, state, actor_key, preparing_epoch,
            manifest_ref IN (SELECT unhex(value) FROM json_each(?1))
            FROM environment_owners
            WHERE manifest_ref IN (SELECT unhex(value) FROM json_each(?1))
               OR owner_id IN (SELECT value FROM json_each(?2))
            ORDER BY owner_id";
        peeled = "SELECT owner_id FROM environment_owners WHERE manifest_ref IN
            (SELECT unhex(value) FROM json_each(?1)) ORDER BY owner_id";
    }
}
static SQLITE: LazyLock<SqliteEnvironmentStatements> =
    LazyLock::new(|| SqliteEnvironmentStatements::render(crate::schema_layout::MAIN));

pub(super) fn remove_head(
    tx: &Connection,
    session: &lash_kernel_value::identity::SessionId,
) -> rusqlite::Result<Vec<String>> {
    tx.prepare_cached(ENV.head_remove.sql())?
        .query_map([session.as_str()], |r| crate::codec::hash_column(r, 0))?
        .collect()
}

fn conversion(error: impl std::fmt::Display) -> rusqlite::Error {
    crate::sqlite_conversion_error(crate::StoreError::StoredDataCorrupt {
        record_kind: "environment ownership",
        message: error.to_string(),
    })
}

fn owner_body(tx: &Connection, id: &VersionOwnerId) -> Answer<EnvironmentRecord> {
    Ok(owner(tx, id)?.and_then(|r| r.ok_or_else(|| Refusal::Missing(id.clone()).into())))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// ENV-AUTHORITY-READ-ONCE: validation carries the authority it already
    /// read under this writer instead of querying the same fact again.
    #[tokio::test]
    async fn each_external_version_reads_its_authority_once() {
        let set = crate::SqliteStoreSet::memory().await.unwrap();
        let identity = lash_core_execution::testing::sql_work::owner_commit(&set).await;
        let store = set.session_store_factory();
        let (verdict, runs) = store
            .conn
            .call(move |connection| {
                let body = assemble(None, []).unwrap();
                let owner = VersionOwner::preparing(body.id().clone());
                let round = RoundEnvironment {
                    owner: owner.id().clone(),
                    version: body.id().clone(),
                };
                let mut staging = env::staging();
                let root = env::stage_version(&mut staging, &round).unwrap();
                let node = staging.nodes().get(&root).unwrap();
                connection.execute(
                    "INSERT INTO blobs(hash, content, encoding) VALUES (?1, ?2, 1)",
                    rusqlite::params![root.0.as_slice(), node.bytes.as_ref()],
                )?;
                let actor = lash_durable::ActorKey::session("sql-work-owner").unwrap();
                connection.execute(
                    ENV.prepare.sql(),
                    rusqlite::params![
                        owner.id().to_string(),
                        body.id().to_string(),
                        actor.as_str(),
                        identity.epoch,
                        env::json(&body).unwrap(),
                        root.0.as_slice(),
                    ],
                )?;
                connection.execute(
                    ENV.publish.sql(),
                    rusqlite::params![owner.id().to_string(), actor.as_str(), identity.epoch],
                )?;
                let graph = lash_durable::domain::graph_commit::check_graph(
                    &BTreeMap::new(),
                    &BTreeSet::from([root]),
                    &[],
                )
                .unwrap();
                let before = connection
                    .prepare_cached(SQLITE.validation_authorities.sql())?
                    .get_status(rusqlite::StatementStatus::Run);
                let supplied = env::supplied_versions(&graph);
                let verdict = validate_graph(connection, &graph, &supplied, &BTreeSet::new())?;
                let after = connection
                    .prepare_cached(SQLITE.validation_authorities.sql())?
                    .get_status(rusqlite::StatementStatus::Run);
                Ok((verdict, after - before))
            })
            .await
            .unwrap();
        assert!(verdict.is_ok(), "{verdict:?}");
        assert_eq!(
            runs, 1,
            "ENV-AUTHORITY-READ-ONCE: repeated authority lookup"
        );
    }
}
