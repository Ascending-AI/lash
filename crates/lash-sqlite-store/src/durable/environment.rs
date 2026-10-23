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
    session: &lash_vocabulary::identity::SessionId,
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

pub(super) fn validate_graph(
    tx: &Connection,
    graph: &lash_durable::domain::graph_commit::CheckedGraph,
    publishing: &BTreeSet<VersionOwnerId>,
) -> Answer<()> {
    let supplied = match env::supplied_versions(graph) {
        Ok(s) => s,
        Err(e) => return Ok(Err(e)),
    };
    let mut versions = supplied
        .into_iter()
        .map(|(_, r)| (r.owner.clone(), r))
        .collect::<BTreeMap<_, _>>();
    let named: BTreeSet<_> = graph
        .external
        .iter()
        .chain(&graph.external_roots)
        .map(GraphNode::to_hex)
        .collect();
    let rows = tx
        .prepare_cached(SQLITE.peeled.sql())?
        .query_map([graph::json_array(&named)], |r| r.get::<_, String>(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    for id in rows {
        let id = match serde_json::from_value::<VersionOwnerId>(serde_json::Value::String(id)) {
            Ok(id) => id,
            Err(e) => return Ok(Err(env::corrupt(e))),
        };
        let record = match required(tx, &id)? {
            Ok(r) => r,
            Err(e) => return Ok(Err(e)),
        };
        versions.insert(
            id.clone(),
            RoundEnvironment {
                owner: id,
                version: record.owner.version().clone(),
            },
        );
    }
    for (_, round) in versions {
        let record = match required(tx, &round.owner)? {
            Ok(r) => r,
            Err(e) => return Ok(Err(e)),
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
            let body: EnvironmentBody = match env::decode(body_json) {
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
            let session = lash_vocabulary::identity::SessionId::parse(commit.actor.id())
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
                tx.execute(
                    ENV.head_put.sql(),
                    rusqlite::params![session.as_str(), json, &root.0[..], commit.epoch.0],
                )?;
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
        session: &lash_vocabulary::identity::SessionId,
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
        peeled = "SELECT owner_id FROM environment_owners WHERE manifest_ref IN
            (SELECT unhex(value) FROM json_each(?1)) ORDER BY owner_id";
    }
}
static SQLITE: LazyLock<SqliteEnvironmentStatements> =
    LazyLock::new(|| SqliteEnvironmentStatements::render(crate::schema_layout::MAIN));

pub(super) fn remove_head(
    tx: &Connection,
    session: &lash_vocabulary::identity::SessionId,
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
