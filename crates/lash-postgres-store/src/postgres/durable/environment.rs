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
    session: &lash_kernel_value::identity::SessionId,
) -> Result<Option<EnvironmentHead>, DurableError> {
    let root: Option<crate::keys::Key> = crate::observed_sql::query_scalar(ENV.head_root.sql())
        .bind(session.as_str())
        .fetch_optional(crate::observed_sql::executor(&mut *tx))
        .await
        .map_err(sqlx_failure)?;
    let Some(root) = root else {
        return Ok(None);
    };
    let root = super::node_ref(&root)?;
    let limits = lash_durable::domain::graph::OWNER_READ_LIMITS;
    let nodes = super::graph::walk_within(tx, &[root], &limits, None).await?;
    let graph = lash_durable::domain::RunFacts {
        nodes,
        read_limits: limits,
        ..Default::default()
    };
    let mut decoder = lash_core_store::checkpoint_graph::GraphDecoder::new(&graph, limits);
    env::decode_head(&mut decoder, root).map(Some)
}
/// Queue retired owner `id`'s cleanup: its obligation is due now.
async fn enqueue_retired(tx: &mut PgConnection, id: &VersionOwnerId) -> Result<(), DurableError> {
    let cleanup = ArtifactCleanup::ended(
        ArtifactReferrer::EnvironmentVersion(id.clone()),
        vec![],
        None,
    );
    let json = cleanup.to_json().map_err(env::corrupt)?;
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
        enqueue_retired(tx, id).await?;
    }
    Ok(())
}

/// The session whose write authority environment owner `id`'s artifact
/// share is taken under; refused when the owner is gone.
pub(crate) async fn artifact_owner_session(
    tx: &mut PgConnection,
    id: &VersionOwnerId,
) -> Result<lash_kernel_value::identity::SessionId, DurableError> {
    let record = authority(tx, id).await?.ok_or_else(|| {
        DurableError::Package(lash_durable::packages::PackageRefusal::ReferrerEnded(
            ArtifactReferrer::EnvironmentVersion(id.clone()),
        ))
    })?;
    lash_kernel_value::identity::SessionId::parse(record.actor.id()).map_err(env::corrupt)
}

/// Lock environment owner `id`'s row. Every caller holds the owner's
/// session write authority ([`artifact_owner_session`]) first.
/// Publication/acquire share it with retirement's SKIP LOCKED write lock;
/// exactly one can win.
pub(crate) async fn lock_artifact_owner(
    tx: &mut PgConnection,
    id: &VersionOwnerId,
) -> Result<(), DurableError> {
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

use super::steps::{Entry, Shape, Step, Ty, Value};

/// A preparation's insert, with the publication references its body owes:
/// decoded once here, so its publication checks them without a decoder.
pub(super) static PREPARE: Entry = Entry::new(
    || &PG.prepare,
    &[
        Ty::Text,
        Ty::Text,
        Ty::Text,
        Ty::Int,
        Ty::Text,
        Ty::Bytes,
        Ty::Texts,
    ],
    Shape::Exec,
);
/// A preparing owner's cleanup obligation, awaiting its retirement.
pub(super) static AWAIT_CLEANUP: Entry = Entry::new(
    || &CLEANUP.insert_if_absent,
    &[Ty::Text, Ty::Text, Ty::Text, Ty::Text, Ty::Int, Ty::Text],
    Shape::Exec,
);
/// Lock owner `?1` and answer whether `?2` at epoch `?3` prepares it and,
/// when `?4`, holds every publication its body owes.
pub(super) static PREPARED: Entry = Entry::new(
    || &PG.prepared,
    &[Ty::Text, Ty::Text, Ty::Int, Ty::Bool],
    Shape::Select,
)
.flagged("ready");
pub(super) static PUBLISH: Entry =
    Entry::new(|| &ENV.publish, &[Ty::Text, Ty::Text, Ty::Int], Shape::Exec);
pub(super) static RETIRE: Entry = Entry::new(|| &ENV.retire, &[Ty::Text], Shape::Exec);
pub(super) static QUEUE_RETIRED: Entry =
    Entry::new(|| &ENV.queue_retired, &[Ty::Text, Ty::Text], Shape::Exec);
/// Whether session `?1`'s head root is `?2` (none: no head).
pub(super) static HEAD_IS: Entry =
    Entry::new(|| &PG.head_is, &[Ty::Text, Ty::Bytes], Shape::Select).flagged("same");
/// Whether owner `?1` is published at version `?2`.
pub(super) static PUBLISHED_AT: Entry =
    Entry::new(|| &PG.published_at, &[Ty::Text, Ty::Text], Shape::Select).flagged("published");
/// Session `?1`'s head becomes `?2` at epoch `?3`; the root it replaces
/// retires.
pub(super) static HEAD_PUT: Entry = Entry::new(
    || &PG.head_replace,
    &[Ty::Text, Ty::Bytes, Ty::Int],
    Shape::Returning,
)
.retiring("root_ref");
pub(super) static HEAD_REMOVE: Entry =
    Entry::new(|| &ENV.head_remove, &[Ty::Text], Shape::Returning).retiring("root_ref");
/// Whether published owner `?1`'s version node has a rooted reader.
pub(super) static HAS_READER: Entry =
    Entry::new(|| &PG.owner_has_reader, &[Ty::Text], Shape::Select).flagged("held");

/// What a refused environment step means, once the explicit form reads why.
#[derive(Clone, Copy)]
enum Check {
    Prepared,
    Preparing { publishing: bool },
    Head,
    Next,
    Obligation,
}

/// `write`'s step program for `actor` at `epoch`, each step with what its
/// refusal checks. Input that is not a version owner's refuses here.
fn program(
    actor: &lash_durable::ActorKey,
    epoch: Epoch,
    write: &EnvironmentWrite,
) -> Result<Vec<(Step, Option<Check>)>, DurableError> {
    if actor.kind() != lash_durable::ActorKind::Session {
        return Err(Refusal::SessionActor.into());
    }
    let preparing = |owner: &VersionOwnerId, publishing: bool| {
        (
            Step::one(
                &PREPARED,
                vec![
                    Value::text(owner.to_string()),
                    Value::text(actor.as_str()),
                    Value::int(epoch.0),
                    Value::Bool(publishing),
                ],
            ),
            Some(Check::Preparing { publishing }),
        )
    };
    Ok(match write {
        EnvironmentWrite::Prepare { owner, body_json } => {
            let body = env::decoded_body(owner.version(), body_json)?;
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
            let owed: BTreeSet<String> = body
                .fragments()
                .values()
                .flat_map(|fragment| &fragment.fragment().package_roots)
                .map(|accepted| {
                    lash_durable::packages::publication_reference(&accepted.publication.key())
                })
                .collect();
            let cleanup = ArtifactCleanup::Await(ReferrerGuard::EnvironmentVersionRetired(
                owner.id().clone(),
            ));
            let obligation = lash_core_store::store::ObligationKey::ArtifactCleanup {
                referrer: cleanup.referrer(),
            }
            .id();
            vec![
                (
                    Step::one(
                        &PREPARE,
                        vec![
                            Value::text(owner.id().to_string()),
                            Value::text(owner.version().to_string()),
                            Value::text(actor.as_str()),
                            Value::int(epoch.0),
                            Value::text(body_json.clone()),
                            Value::Bytes(Some(root.0.to_vec())),
                            Value::texts(owed),
                        ],
                    ),
                    Some(Check::Prepared),
                ),
                (
                    Step::any(
                        &AWAIT_CLEANUP,
                        vec![
                            Value::text("environment_version"),
                            Value::text(owner.id().to_string()),
                            Value::text(cleanup.to_json().map_err(env::corrupt)?),
                            Value::text(obligation.as_str()),
                            Value::int(0),
                            Value::Text(cleanup.gate().map(|gate| gate.key().to_owned())),
                        ],
                    ),
                    None,
                ),
            ]
        }
        EnvironmentWrite::Publish { owner } => vec![
            preparing(owner, true),
            (
                Step::one(
                    &PUBLISH,
                    vec![
                        Value::text(owner.to_string()),
                        Value::text(actor.as_str()),
                        Value::int(epoch.0),
                    ],
                ),
                None,
            ),
        ],
        EnvironmentWrite::Abort { owner } => {
            let ended = ArtifactCleanup::ended(
                ArtifactReferrer::EnvironmentVersion(owner.clone()),
                vec![],
                None,
            );
            vec![
                preparing(owner, false),
                (
                    Step::one(&RETIRE, vec![Value::text(owner.to_string())]),
                    None,
                ),
                (
                    Step::one(
                        &QUEUE_RETIRED,
                        vec![
                            Value::text(owner.to_string()),
                            Value::text(ended.to_json().map_err(env::corrupt)?),
                        ],
                    ),
                    Some(Check::Obligation),
                ),
            ]
        }
        EnvironmentWrite::Head {
            expected,
            head: next,
        } => {
            let root =
                |head: &EnvironmentHead| env::stage_head(&mut env::staging(), head).map(|n| n.0);
            let session = Value::text(actor.id());
            let mut program = vec![(
                Step::one(
                    &HEAD_IS,
                    vec![
                        session.clone(),
                        Value::Bytes(expected.as_ref().map(root).transpose()?.map(Vec::from)),
                    ],
                ),
                Some(Check::Head),
            )];
            match next {
                Some(next) => {
                    program.push((
                        Step::one(
                            &PUBLISHED_AT,
                            vec![
                                Value::text(next.owner.to_string()),
                                Value::text(next.version.to_string()),
                            ],
                        ),
                        Some(Check::Next),
                    ));
                    program.push((
                        Step::any(
                            &HEAD_PUT,
                            vec![
                                session,
                                Value::Bytes(Some(root(next)?.to_vec())),
                                Value::int(epoch.0),
                            ],
                        ),
                        None,
                    ));
                }
                None => program.push((Step::any(&HEAD_REMOVE, vec![session]), None)),
            }
            program
        }
    })
}

/// `write`'s step program for `actor` at `epoch`.
pub(super) fn steps(
    actor: &lash_durable::ActorKey,
    epoch: Epoch,
    write: &EnvironmentWrite,
) -> Result<Vec<Step>, DurableError> {
    Ok(program(actor, epoch, write)?
        .into_iter()
        .map(|(step, _)| step)
        .collect())
}

/// The checks that end a commit of `writes`, after every domain write: each
/// owner it publishes has a rooted reader.
pub(super) fn finish_steps(writes: &[DomainWrite]) -> Vec<Step> {
    writes
        .iter()
        .filter_map(|write| match write {
            DomainWrite::Environment(EnvironmentWrite::Publish { owner }) => {
                Some(Step::one(&HAS_READER, vec![Value::text(owner.to_string())]))
            }
            _ => None,
        })
        .collect()
}

/// Why `write`'s program refused at `check`, read in the refused
/// transaction.
async fn refusal(
    tx: &mut PgConnection,
    write: &EnvironmentWrite,
    check: Check,
) -> Result<DurableError, DurableError> {
    let owner = match write {
        EnvironmentWrite::Prepare { owner, .. } => owner.id(),
        EnvironmentWrite::Publish { owner } | EnvironmentWrite::Abort { owner } => owner,
        EnvironmentWrite::Head { head, .. } => match (check, head) {
            (Check::Next, Some(next)) => &next.owner,
            _ => return Ok(Refusal::HeadConflict.into()),
        },
    };
    Ok(match check {
        Check::Prepared => Refusal::Preparation(owner.clone()).into(),
        Check::Obligation => env::corrupt("owner lost its cleanup obligation"),
        Check::Head => Refusal::HeadConflict.into(),
        Check::Preparing { publishing } => {
            let record = required(tx, owner).await?;
            if publishing && record.owner.state() == VersionOwnerState::Preparing {
                Refusal::Content.into()
            } else {
                Refusal::Preparation(owner.clone()).into()
            }
        }
        Check::Next => {
            let record = required(tx, owner).await?;
            if record.owner.state() == VersionOwnerState::Published {
                Refusal::Content.into()
            } else {
                Refusal::NotPublished(owner.clone()).into()
            }
        }
    })
}

pub(super) async fn apply(
    tx: &mut PgConnection,
    commit: &Committing<'_>,
    write: &EnvironmentWrite,
) -> Result<(), DurableError> {
    let program = program(commit.actor, commit.epoch, write)?;
    let (steps, checks): (Vec<_>, Vec<_>) = program.into_iter().unzip();
    match super::steps::run(tx, &steps, &commit.steps()).await? {
        None => Ok(()),
        Some(refused) => match checks[refused.index] {
            Some(check) => Err(refusal(tx, write, check).await?),
            None => Err(env::corrupt(
                "an environment step answered another row count",
            )),
        },
    }
}

pub(super) async fn finish(
    tx: &mut PgConnection,
    context: &super::steps::Context<'_>,
    writes: &[DomainWrite],
) -> Result<(), DurableError> {
    match super::steps::run(tx, &finish_steps(writes), context).await? {
        None => Ok(()),
        Some(_) => Err(Refusal::NoReader.into()),
    }
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
        session: &lash_kernel_value::identity::SessionId,
    ) -> Result<Option<EnvironmentHead>, DurableError> {
        self.read_snapshot(|tx| Box::pin(head(tx, session))).await
    }
    async fn retire_abandoned_environment(
        &self,
        id: &VersionOwnerId,
    ) -> Result<bool, DurableError> {
        let (mut tx, _) = self.open(CommitLabel::new("environment.abandon")).await?;
        let outcome = async {
            let record = required(&mut tx, id).await?;
            let session = lash_kernel_value::identity::SessionId::parse(record.actor.id())
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
        /// Lock the whole commit's version-owner set before taking graph
        /// locks. Supplied versions are [owner, version] pairs; named graph
        /// references discover their owners through the canonical manifest.
        /// Skip contention: a dropped autocommit future cannot roll back a
        /// server still waiting on an owner. The held-set guard declines
        /// that call, then the explicit transaction owns the cancellable wait.
        commit_owners = "SELECT ARRAY(SELECT owner_id FROM environment_owners
            WHERE manifest_ref = ANY(?1::bytea[])
               OR owner_id IN (SELECT value->>0 FROM jsonb_array_elements(?2::jsonb))
            ORDER BY owner_id FOR UPDATE SKIP LOCKED)";
        /// Read after the locks, so retirement that won the lock is visible.
        /// Every supplied owner must exist with its exact published version;
        /// every named version must remain published. An owner among `?4`,
        /// which the commit publishes, may still be preparing: its
        /// publication step checks its preparation.
        commit_versions_valid = "SELECT NOT EXISTS (
            SELECT 1 FROM environment_owners
            WHERE manifest_ref = ANY(?1::bytea[]) AND (NOT (state = 'published'
                    OR (state = 'preparing' AND owner_id = ANY(?4::text[])))
                OR NOT (owner_id = ANY(?3::text[])))
            ) AND NOT EXISTS (
            SELECT 1 FROM jsonb_array_elements(?2::jsonb) AS supplied(value)
            LEFT JOIN environment_owners AS owner ON owner.owner_id = supplied.value->>0
            WHERE owner.owner_id IS NULL OR NOT (owner.owner_id = ANY(?3::text[]))
               OR NOT (owner.state = 'published'
                    OR (owner.state = 'preparing' AND owner.owner_id = ANY(?4::text[])))
               OR (NOT (owner.manifest_ref = ANY(?1::bytea[])) AND owner.version_id <> supplied.value->>1))
            AND NOT EXISTS (SELECT 1 FROM environment_owners WHERE owner_id = ANY(?3::text[])
                AND (owner_id !~ '^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$'
                  OR version_id !~ '^[0-9a-f]{64}$' OR preparing_epoch < 0
                  OR left(actor_key, 2) <> 's/' OR length(actor_key) <= 2))";
        lock_owner = "SELECT owner_id FROM environment_owners WHERE owner_id = ?1 FOR UPDATE";
        /// Prepare owner `?1` at version `?2` for actor `?3` at epoch `?4`
        /// with body `?5`, version node `?6` and owed publications `?7`.
        prepare = "INSERT INTO environment_owners
            (owner_id, version_id, state, actor_key, preparing_epoch, body_json, manifest_ref,
             publication_refs)
            VALUES (?1, ?2, 'preparing', ?3, ?4, ?5, ?6, ?7::text[])
            ON CONFLICT (owner_id) DO NOTHING";
        prepared = "SELECT actor_key = ?2 AND preparing_epoch = ?3 AND state = 'preparing'
                AND (NOT ?4::boolean OR NOT EXISTS (
                    SELECT 1 FROM unnest(publication_refs) AS owed(ref)
                    WHERE NOT EXISTS (SELECT 1 FROM artifact_referrer_edges
                        WHERE namespace = 'kernel_document' AND artifact_ref = owed.ref
                          AND referrer_kind = 'environment_version' AND referrer_id = ?1))) AS ready
            FROM environment_owners WHERE owner_id = ?1 FOR UPDATE";
        head_is = "SELECT (SELECT root.hash FROM environment_heads AS head
                JOIN blobs AS root ON root.id = head.root_ref WHERE head.session_id = ?1)
            IS NOT DISTINCT FROM ?2::bytea AS same";
        published_at = "SELECT state = 'published' AND version_id = ?2 AS published
            FROM environment_owners WHERE owner_id = ?1";
        head_replace = "WITH old AS (SELECT root_ref FROM environment_heads WHERE session_id = ?1),
            put AS (INSERT INTO environment_heads (session_id, root_ref, written_epoch)
                VALUES (?1, COALESCE((SELECT id FROM blobs WHERE hash = ?2), CASE WHEN ?2 IS NOT NULL THEN -1 END), ?3)
                ON CONFLICT (session_id) DO UPDATE
                SET root_ref = excluded.root_ref, written_epoch = excluded.written_epoch)
            SELECT (SELECT hash FROM blobs WHERE id = old.root_ref) AS root_ref FROM old";
        owner_has_reader = "WITH RECURSIVE reachable AS (
            SELECT id FROM blobs
            WHERE hash = (SELECT manifest_ref FROM environment_owners WHERE owner_id = ?1)
            UNION SELECT e.parent_id FROM blob_edges e JOIN reachable r ON e.child_id = r.id
            ) SELECT EXISTS(SELECT 1 FROM reachable
                JOIN graph_root_catalog roots ON roots.root_ref = reachable.id) AS held";
        peel_owner = "SELECT owner_id FROM environment_owners WHERE owner_id = ?1 FOR UPDATE SKIP LOCKED";
        owners_of = "SELECT owner_id FROM environment_owners WHERE manifest_ref = ANY(?1) ORDER BY owner_id";
        owner_nodes = "SELECT owner_id, manifest_ref FROM environment_owners WHERE manifest_ref = ANY(?1) ORDER BY owner_id";
    }
}
static PG: LazyLock<PostgresEnvironmentStatements> =
    LazyLock::new(|| PostgresEnvironmentStatements::render(Dialect::postgres()));

/// Static statements shared with the generated one-call owner commit.
pub(super) fn commit_owners() -> &'static lash_store_sql::Rendered {
    &PG.commit_owners
}

/// Version-owner boundaries a generated graph peel must leave to the explicit owner bridge.
pub(super) fn retirement_owners() -> &'static lash_store_sql::Rendered {
    &PG.owner_nodes
}

pub(super) fn retirement_owner_lock() -> &'static lash_store_sql::Rendered {
    &PG.peel_owner
}

pub(super) fn commit_versions_valid() -> &'static lash_store_sql::Rendered {
    &PG.commit_versions_valid
}
