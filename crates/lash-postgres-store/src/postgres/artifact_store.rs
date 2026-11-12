use std::collections::BTreeSet;
use std::sync::LazyLock;

use lash_core_store::artifact_referrer::ArtifactReferrer;
use lash_core_store::artifact_referrer::ReferrerClaim;
use lash_core_store::artifact_referrer::ResolvedArtifactCleanup;
use lash_core_store::store::StoreError;
use lash_execution_contracts::module_artifacts::ArtifactStoreError;
use lash_execution_contracts::module_nodes::stage_module_node;
use lash_store_sql::Dialect;
use lash_store_sql::artifact::dependency_edges::DependencyEdgeStatements;
use lash_store_sql::artifact::referrer_edges::ReferrerEdgeStatements;
use lash_store_sql::artifact::referrer_fences::ReferrerFenceStatements;
use sqlx::postgres::PgRow;
use sqlx::{Postgres, Row, Transaction};

use crate::*;

lash_store_sql::statements! {
    pub(crate) struct ReferrerPostgresStatements @ "artifact_referrer_edge" {
        end_local = "SELECT lash_artifact_end_local(?1, ?2, ?3, ?4, ?5)";
        /// Lock referrer `?1`/`?2`, then whether it ended (FIG-7701 R3).
        lock_referrer = "SELECT lash_lock_referrer(?1, ?2)";
        /// Lock keys `?1` in order, then the first of `?2`/`?3` not stored,
        /// else edges from referrer `?4`/`?5` to each of `?6`/`?7`.
        acquire_holds = "SELECT lash_acquire_holds(?1::TEXT[], ?2::TEXT[], ?3::TEXT[], ?4, ?5, ?6::TEXT[], ?7::TEXT[])";
        /// Publish environment `?1` as head `?2` over body `?3`/`?5` at
        /// `?4` if that body is stored: its first missing module of `?6`,
        /// else its edges and referrer `?7`/`?8`'s (FIG-7701 R5).
        publish_over_stored_body = "SELECT stored, missing FROM lash_publish_env_over_stored_body(?1, ?2, ?3, ?4, ?5, ?6::TEXT[], ?7, ?8)";
        /// Environment `?1`'s body reference, read unlocked, then its
        /// closure's lock and existence, then its head's lock; with `?4`,
        /// the head's existence and referrer `?2`/`?3`'s edges to head and
        /// closure (FIG-7700). Answers `{state, body}`.
        acquire_env = "SELECT lash_acquire_env(?1, ?2, ?3, ?4)";
        /// Each stored head of `?2` in namespace `?1` and the body it
        /// splices back (FIG-7700).
        select_body_refs = "SELECT artifact_ref, body_ref FROM lash_vm_artifacts
             WHERE namespace = ?1 AND artifact_ref = ANY(?2::TEXT[]) AND body_ref IS NOT NULL";
        /// Each of `?2` stored in namespace `?1`.
        select_stored_refs = "SELECT artifact_ref FROM lash_vm_artifacts
             WHERE namespace = ?1 AND artifact_ref = ANY(?2::TEXT[])";
        /// Delete each artifact of `?2` in namespace `?1` that no referrer
        /// edge and no dependency edge names. Each deleted row answers the
        /// graph node it rooted, if bridged, once per artifact it held: its
        /// dependency edges go with it, and the caller reclaims what that
        /// frees (FIG-7700).
        delete_unreferenced = "WITH gone AS (
                 DELETE FROM lash_vm_artifacts AS artifact
                 WHERE artifact.namespace = ?1 AND artifact.artifact_ref = ANY(?2::TEXT[])
                   AND NOT EXISTS (SELECT 1 FROM artifact_referrer_edges AS edge
                       WHERE edge.namespace = artifact.namespace AND edge.artifact_ref = artifact.artifact_ref)
                   AND NOT EXISTS (SELECT 1 FROM artifact_dependency_edges AS held
                       WHERE held.namespace = artifact.namespace AND held.artifact_ref = artifact.artifact_ref)
                 RETURNING artifact.namespace, artifact.artifact_ref, artifact.graph_ref
             ) SELECT (SELECT hash FROM blobs WHERE id = gone.graph_ref) AS graph_ref,
                 freed.namespace, freed.artifact_ref
             FROM gone LEFT JOIN artifact_dependency_edges AS freed
                 ON freed.holder_namespace = gone.namespace AND freed.holder_ref = gone.artifact_ref";
        /// Everything the artifacts of `?1`/`?2` hold, transitively. Edges
        /// are immutable while their holder lives, so the answer read before
        /// the holders' locks is the set their deletion can free.
        select_held_closure = "WITH RECURSIVE reach AS (
                 SELECT held.namespace, held.artifact_ref
                 FROM unnest(?1::TEXT[], ?2::TEXT[]) AS named(namespace, artifact_ref)
                 JOIN artifact_dependency_edges AS held
                     ON held.holder_namespace = named.namespace AND held.holder_ref = named.artifact_ref
                 UNION
                 SELECT held.namespace, held.artifact_ref FROM artifact_dependency_edges AS held
                 JOIN reach ON held.holder_namespace = reach.namespace AND held.holder_ref = reach.artifact_ref
             ) SELECT namespace, artifact_ref FROM reach";
        /// Holder `?3`/`?4` keeps each artifact of `?2` in namespace `?1`, in
        /// one statement.
        insert_dependency_edges = "INSERT INTO artifact_dependency_edges
             (namespace, artifact_ref, holder_namespace, holder_ref)
             SELECT ?1, wanted.artifact_ref, ?3, ?4 FROM unnest(?2::TEXT[]) AS wanted(artifact_ref)
             ON CONFLICT DO NOTHING";
        /// Each reference of `?2` in namespace `?1` that referrer `?3`/`?4`
        /// does not reach: through its own edges, or through what those
        /// artifacts hold.
        select_unreached = "WITH RECURSIVE reach AS (
                 SELECT namespace, artifact_ref FROM artifact_referrer_edges
                 WHERE referrer_kind = ?3 AND referrer_id = ?4
                 UNION
                 SELECT held.namespace, held.artifact_ref FROM artifact_dependency_edges AS held
                 JOIN reach ON held.holder_namespace = reach.namespace AND held.holder_ref = reach.artifact_ref
             ) SELECT wanted.artifact_ref FROM unnest(?2::TEXT[]) AS wanted(artifact_ref)
             WHERE NOT EXISTS (SELECT 1 FROM reach
                 WHERE reach.namespace = ?1 AND reach.artifact_ref = wanted.artifact_ref)";
        /// Every referrer of each artifact of `?2` in namespace `?1`, in
        /// artifact then stored-pair order.
        select_artifact_set_edges = "SELECT artifact_ref, referrer_kind, referrer_id
             FROM artifact_referrer_edges
             WHERE namespace = ?1 AND artifact_ref = ANY(?2::TEXT[])
             ORDER BY artifact_ref, referrer_kind, referrer_id";
    }
}

lash_store_sql::statements! {
    pub(crate) struct LashVmArtifactStatements @ "lash_vm_artifact" {
        /// First absent member, preserving the manifest's refusal order.
        select_missing_manifest = "SELECT wanted.artifact_ref
             FROM unnest(?1::TEXT[], ?2::TEXT[]) WITH ORDINALITY AS wanted(namespace, artifact_ref, ordinal)
             WHERE NOT EXISTS (SELECT 1 FROM lash_vm_artifacts AS stored
                 WHERE stored.namespace = wanted.namespace AND stored.artifact_ref = wanted.artifact_ref)
             ORDER BY wanted.ordinal LIMIT 1";
        /// The whole closure's exact retention edges, in one statement.
        insert_manifest_edges = "INSERT INTO artifact_referrer_edges
             (namespace, artifact_ref, referrer_kind, referrer_id)
             SELECT namespace, artifact_ref, ?3, ?4
             FROM unnest(?1::TEXT[], ?2::TEXT[]) AS wanted(namespace, artifact_ref)
             ON CONFLICT DO NOTHING";
        insert_bytes = "INSERT INTO lash_vm_artifacts (namespace, artifact_ref, storage_kind, artifact_bytes)
             VALUES (?1, ?2, 0, ?3) ON CONFLICT (namespace, artifact_ref) DO NOTHING";
        /// The bytes of `?2` in namespace `?1`; an environment head's with
        /// its body spliced back, in the same snapshot (FIG-7700).
        select_bytes = "SELECT CASE WHEN artifact.storage_kind = 2
                 THEN substr(artifact.artifact_bytes, 1, artifact.body_at) || body.artifact_bytes
                     || substr(artifact.artifact_bytes, artifact.body_at + 1)
                 ELSE artifact.artifact_bytes END
             FROM lash_vm_artifacts AS artifact
             LEFT JOIN lash_vm_artifacts AS body ON artifact.storage_kind = 2
                 AND body.namespace = 'process_env_body' AND body.artifact_ref = artifact.body_ref
             WHERE artifact.namespace = ?1 AND artifact.artifact_ref = ?2";
        /// An environment head: its bytes without the body span `?4`, which
        /// splices back at `?5`.
        insert_head = "INSERT INTO lash_vm_artifacts (namespace, artifact_ref, storage_kind, artifact_bytes, body_ref, body_at)
             VALUES (?1, ?2, 2, ?3, ?4, ?5) ON CONFLICT (namespace, artifact_ref) DO NOTHING";
        /// A node-bodied artifact's row: its graph node, which the row roots
        /// through the bridge (FIG-6469).
        insert_node = "INSERT INTO lash_vm_artifacts (namespace, artifact_ref, storage_kind, graph_ref)
             VALUES (?1, ?2, 1, COALESCE((SELECT id FROM blobs WHERE hash = ?3), CASE WHEN ?3 IS NOT NULL THEN -1 END)) ON CONFLICT (namespace, artifact_ref) DO NOTHING";
        /// What artifact `?2` of namespace `?1` stores: its bytes, as
        /// [`Self::select_bytes`] reads them, or the graph node its row roots.
        select_stored = "SELECT CASE WHEN artifact.storage_kind = 2
                 THEN substr(artifact.artifact_bytes, 1, artifact.body_at) || body.artifact_bytes
                     || substr(artifact.artifact_bytes, artifact.body_at + 1)
                 ELSE artifact.artifact_bytes END,
                 (SELECT hash FROM blobs WHERE id = artifact.graph_ref) AS graph_ref
             FROM lash_vm_artifacts AS artifact
             LEFT JOIN lash_vm_artifacts AS body ON artifact.storage_kind = 2
                 AND body.namespace = 'process_env_body' AND body.artifact_ref = artifact.body_ref
             WHERE artifact.namespace = ?1 AND artifact.artifact_ref = ?2";
        /// What each stored artifact of `?2` in namespace `?1` stores, as
        /// [`Self::select_stored`]; an absent one has no row.
        select_stored_set = "SELECT artifact_ref, artifact_bytes, (SELECT hash FROM blobs WHERE id = graph_ref) AS graph_ref
             FROM lash_vm_artifacts
             WHERE namespace = ?1 AND artifact_ref = ANY(?2::TEXT[])";
        /// Each stored byte artifact of `?2` in namespace `?1`: its bytes,
        /// and no referrer edge, so the read does not grow with the
        /// artifact's referrers (FIG-7701).
        select_records = "SELECT artifact_ref, artifact_bytes
             FROM lash_vm_artifacts
             WHERE namespace = ?1 AND artifact_ref = ANY(?2::TEXT[])
                 AND storage_kind = 0 AND artifact_bytes IS NOT NULL";
        /// Each reference of `?2` in namespace `?1` that referrer `?3`/`?4`
        /// holds no edge to.
        select_unheld = "SELECT wanted.artifact_ref FROM unnest(?2::TEXT[]) AS wanted(artifact_ref)
             WHERE NOT EXISTS (SELECT 1 FROM artifact_referrer_edges AS edge
                 WHERE edge.namespace = ?1 AND edge.artifact_ref = wanted.artifact_ref
                   AND edge.referrer_kind = ?3 AND edge.referrer_id = ?4)";
        exists = "SELECT EXISTS (SELECT 1 FROM lash_vm_artifacts
             WHERE namespace = ?1 AND artifact_ref = ?2)";
        /// One page of namespace `?1` after `?2`: each artifact's bytes, or
        /// the encoded graph node its row roots.
        list_namespace_page = "SELECT artifact.artifact_ref, artifact.artifact_bytes, node.content
             FROM lash_vm_artifacts AS artifact
             LEFT JOIN blobs AS node ON node.id = artifact.graph_ref
             WHERE artifact.namespace = ?1 AND (?2::text IS NULL OR artifact.artifact_ref > ?2::text)
             ORDER BY artifact.artifact_ref LIMIT ?3";
    }
}

pub(crate) struct ArtifactSql {
    pub(crate) dependencies: DependencyEdgeStatements,
    pub(crate) edges: ReferrerEdgeStatements,
    pub(crate) fences: ReferrerFenceStatements,
    pub(crate) postgres: ReferrerPostgresStatements,
    pub(crate) lash_vm_artifacts: LashVmArtifactStatements,
}

static ARTIFACT_SQL: LazyLock<ArtifactSql> = LazyLock::new(|| {
    let dialect = Dialect::postgres();
    ArtifactSql {
        dependencies: DependencyEdgeStatements::render(dialect),
        edges: ReferrerEdgeStatements::render(dialect),
        fences: ReferrerFenceStatements::render(dialect),
        postgres: ReferrerPostgresStatements::render(dialect),
        lash_vm_artifacts: LashVmArtifactStatements::render(dialect),
    }
});

pub(crate) fn artifact_sql() -> &'static ArtifactSql {
    &ARTIFACT_SQL
}

pub(crate) const MODULE_ARTIFACT_NAMESPACE: &str = "kernel_document";
pub(crate) const PROCESS_ENV_NAMESPACE: &str = "process_execution_env";
/// Where environment bodies live, beside the heads at their references.
pub(crate) const PROCESS_ENV_BODY_NAMESPACE: &str =
    lash_core_store::process_env::stored::BODY_NAMESPACE;
/// Where each body's module closure lives, at the body's reference: a row
/// with no bytes whose dependency edges hold the modules. A referrer's
/// module share includes its edge here (FIG-7700).
pub(crate) const PROCESS_ENV_CLOSURE_NAMESPACE: &str =
    lash_core_store::process_env::stored::CLOSURE_NAMESPACE;
pub(crate) const PROCESS_DEFINITION_NAMESPACE: &str = "process_definition";
pub(crate) const TOOL_MATERIAL_NAMESPACE: &str = "tool_material";
pub(crate) const TURN_PRELUDE_NAMESPACE: &str = "turn_prelude";

#[path = "artifact_store/local_end.rs"]
mod local_end;
pub(crate) use local_end::{
    EndArguments as LocalEndArguments, LockPlan, Planned, ScopeLock, failure as end_local_failure,
    lock_plan_tx, validate as validate_local_end,
};

#[cfg(any(test, feature = "testing"))]
pub(crate) use local_end::function_ddl as local_end_function_ddl;

#[path = "artifact_store/holds.rs"]
mod holds;
use holds::{Holds, lock_live_referrer_tx};

#[cfg(any(test, feature = "testing"))]
pub(crate) use holds::function_ddl as holds_function_ddl;

#[cfg(any(test, feature = "testing"))]
pub(crate) use env_storage::function_ddl as env_over_body_function_ddl;

#[path = "artifact_store/env_storage.rs"]
mod env_storage;
use env_storage::{CarriedClosures, EnvPublication, acquire_env_tx, held_closure_tx};

#[path = "artifact_store/tool_material.rs"]
mod tool_material;

/// The namespace of a store-set artifact store; an engine's own store has
/// none here.
pub(crate) fn store_namespace(
    store: &lash_core_store::artifact_referrer::ArtifactStoreId,
) -> Option<&'static str> {
    use lash_core_store::artifact_referrer::ArtifactStoreId;
    match store {
        ArtifactStoreId::KernelDocument => Some(MODULE_ARTIFACT_NAMESPACE),
        ArtifactStoreId::ProcessEnv => Some(PROCESS_ENV_NAMESPACE),
        ArtifactStoreId::ProcessDefinition => Some(PROCESS_DEFINITION_NAMESPACE),
        ArtifactStoreId::ToolMaterial => Some(TOOL_MATERIAL_NAMESPACE),
        ArtifactStoreId::TurnPrelude => Some(TURN_PRELUDE_NAMESPACE),
        ArtifactStoreId::Engine(_) => None,
    }
}

/// A manifest's store-set share as `(namespace, reference)` pairs. Only the
/// module and environment stores share a definition's transaction.
fn definition_manifest(
    manifest: &[lash_core_store::artifact_referrer::ArtifactName],
) -> Result<Vec<(&'static str, String)>, ArtifactStoreError> {
    use lash_core_store::artifact_referrer::ArtifactStoreId;
    manifest
        .iter()
        .map(|artifact| {
            let namespace = match &artifact.store {
                ArtifactStoreId::KernelDocument => MODULE_ARTIFACT_NAMESPACE,
                ArtifactStoreId::ProcessEnv => PROCESS_ENV_NAMESPACE,
                other => {
                    return Err(ArtifactStoreError::Backend(format!(
                        "a definition manifest held in the descriptor's transaction names \
                         store {other:?}"
                    )));
                }
            };
            if !crate::namespace::is_valid_opaque_key(&artifact.artifact_ref) {
                return Err(ArtifactStoreError::Encode(
                    "invalid definition manifest reference".into(),
                ));
            }
            Ok((namespace, artifact.artifact_ref.clone()))
        })
        .collect()
}

/// A failed artifact statement: a lock race PostgreSQL aborted is
/// contention, as every store writer reports it (FIG-7700), which the
/// artifact writers retry; anything else is the backend's.
fn backend(error: sqlx::Error) -> ArtifactStoreError {
    if crate::support::is_contention_error(&error) {
        ArtifactStoreError::StoreFault(lash_core_store::store::StoreFault::Contended)
    } else {
        ArtifactStoreError::Backend(error.to_string())
    }
}

/// The advisory lock key of `referrer`.
pub(crate) fn referrer_lock_key(referrer: &ArtifactReferrer) -> String {
    format!(
        "lash-artifact-referrer:{}:{}",
        referrer.kind().as_str(),
        referrer.canonical_id()
    )
}

pub(crate) async fn lock_referrer_tx(
    conn: &mut sqlx::PgConnection,
    referrer: &ArtifactReferrer,
) -> Result<(), sqlx::Error> {
    let key = referrer_lock_key(referrer);
    crate::observed_sql::query(
        crate::connection_sql::connection_sql()
            .lock_xact_by_text
            .sql(),
    )
    .bind(key)
    .execute(crate::observed_sql::executor(conn))
    .await
    .map(|_| ())
}

/// Lock every `(namespace, reference)` of `artifacts` in one statement, in
/// key order: no namespace prefixes another, so the pairs' order is the
/// keys' order, the one every writer and the server-side end follow. The
/// set seals the transaction: a single publisher's set is its artifact
/// locks, last ([`crate::advisory_lock`]).
async fn lock_artifact_set_tx(
    tx: &mut sqlx::PgConnection,
    artifacts: &BTreeSet<(String, String)>,
) -> Result<(), ArtifactStoreError> {
    if artifacts.is_empty() {
        return Ok(());
    }
    let lock_keys: Vec<String> = artifacts
        .iter()
        .map(|(namespace, reference)| artifact_lock_key(namespace, reference))
        .collect();
    crate::observed_sql::query(
        crate::connection_sql::connection_sql()
            .lock_xact_artifacts
            .sql(),
    )
    .bind(&lock_keys)
    .execute(crate::observed_sql::executor(&mut *tx))
    .await
    .map_err(backend)?;
    crate::replayable::note_sealed();
    Ok(())
}

/// Validate and retain a start's environment in its admission transaction.
/// The artifact lock orders this acquisition against source cleanup.
pub(crate) async fn acquire_process_env_tx(
    tx: &mut crate::guarded_tx::GuardedTx<'_>,
    env: &lash_core_store::process_identity::ProcessEnvRef,
    process: &lash_vocabulary::identity::ProcessId,
) -> Result<(), lash_execution_contracts::plugin_error::PluginError> {
    let referrer = ArtifactReferrer::ProcessRecord(process.clone());
    lock_live_referrer_tx(tx, &referrer).await?;
    // Both shares: the head, which holds its body, and the body's closure.
    acquire_env_tx(tx, env.as_str(), &referrer, None).await?;
    Ok(())
}

/// The advisory lock key of artifact `artifact_ref` in `namespace`.
fn artifact_lock_key(namespace: &str, artifact_ref: &str) -> String {
    format!("lash-artifact:{namespace}:{artifact_ref}")
}

/// Lock one artifact: a set of one, which seals the transaction.
async fn lock_artifact_tx(
    tx: &mut sqlx::PgConnection,
    namespace: &str,
    artifact_ref: &str,
) -> Result<(), ArtifactStoreError> {
    lock_artifact_set_tx(
        tx,
        &BTreeSet::from([(namespace.to_owned(), artifact_ref.to_owned())]),
    )
    .await
}

pub(crate) async fn is_fenced_tx(
    tx: &mut sqlx::PgConnection,
    referrer: &ArtifactReferrer,
) -> Result<bool, ArtifactStoreError> {
    crate::observed_sql::query_scalar(artifact_sql().fences.select_is_fenced.sql())
        .bind(referrer.kind().as_str())
        .bind(referrer.canonical_id())
        .fetch_one(crate::observed_sql::executor(&mut *tx))
        .await
        .map_err(backend)
}

/// Take `referrer`'s owner and referrer locks, admit its owner, and refuse
/// it if it has ended, in that order. A referrer whose owner admission
/// reads nothing has its lock and fence read in one call.
async fn lock_admitted_referrer_tx(
    tx: &mut sqlx::PgConnection,
    referrer: &ArtifactReferrer,
) -> Result<(), ArtifactStoreError> {
    lock_session_artifact_owners_tx(tx, std::slice::from_ref(referrer)).await?;
    if matches!(
        referrer,
        ArtifactReferrer::EnvironmentVersion(_) | ArtifactReferrer::Session(_)
    ) {
        lock_referrer_tx(tx, referrer).await.map_err(backend)?;
        admit_artifact_owner_tx(tx, referrer).await?;
        if is_fenced_tx(tx, referrer).await? {
            return Err(ArtifactStoreError::ReferrerEnded {
                referrer: referrer.clone(),
            });
        }
        return Ok(());
    }
    admit_artifact_owner_tx(tx, referrer).await?;
    lock_live_referrer_tx(tx, referrer).await
}

/// Take the session write authority of every session that owns one of
/// `referrers`' artifact shares (a session referrer's own, an environment
/// owner's stored session), as one set in session order, then each
/// environment owner's row in the referrers' order: one owner at a time
/// would let two ends whose owners map to the same sessions in opposite
/// owner order wait on each other (FIG-7700). Every referrer lock comes
/// after, matching session deletion's order.
async fn lock_session_artifact_owners_tx(
    tx: &mut sqlx::PgConnection,
    referrers: &[ArtifactReferrer],
) -> Result<(), ArtifactStoreError> {
    let mut sessions = BTreeSet::new();
    let mut owners = Vec::new();
    for referrer in referrers {
        match referrer {
            ArtifactReferrer::EnvironmentVersion(id) => {
                sessions.insert(
                    crate::durable::environment::artifact_owner_session(tx, id)
                        .await
                        .map_err(ArtifactStoreError::from)?,
                );
                owners.push(id);
            }
            ArtifactReferrer::Session(session) => {
                sessions.insert(session.clone());
            }
            _ => {}
        }
    }
    for session in &sessions {
        crate::runtime_persistence::lock_session_history_mutation_tx(tx, session)
            .await
            .map_err(ArtifactStoreError::from)?;
    }
    for id in owners {
        crate::durable::environment::lock_artifact_owner(tx, id)
            .await
            .map_err(ArtifactStoreError::from)?;
    }
    Ok(())
}

async fn session_artifact_owner_live_tx(
    tx: &mut sqlx::PgConnection,
    referrer: &ArtifactReferrer,
) -> Result<bool, ArtifactStoreError> {
    let ArtifactReferrer::Session(session) = referrer else {
        return Ok(true);
    };
    let (metadata, deleted, _): (bool, bool, bool) = crate::observed_sql::query_as(
        crate::attachments::attachment_sql()
            .postgres
            .session_state
            .sql(),
    )
    .bind(session.as_str())
    .fetch_one(crate::observed_sql::executor(tx))
    .await
    .map_err(backend)?;
    Ok(metadata && !deleted)
}

async fn admit_artifact_owner_tx(
    tx: &mut sqlx::PgConnection,
    referrer: &ArtifactReferrer,
) -> Result<(), ArtifactStoreError> {
    if let ArtifactReferrer::EnvironmentVersion(id) = referrer {
        let record = crate::durable::environment::authority(tx, id)
            .await
            .map_err(ArtifactStoreError::from)?;
        if record.is_none_or(|record| {
            record.owner.state() == lash_core_store::environment::VersionOwnerState::Retired
        }) {
            return Err(ArtifactStoreError::ReferrerEnded {
                referrer: referrer.clone(),
            });
        }
    }
    if !referrer.kind().holds_artifacts() {
        return Err(ArtifactStoreError::ReferrerKindRefused {
            kind: referrer.kind(),
        });
    }
    if let ArtifactReferrer::Session(session) = referrer
        && !session_artifact_owner_live_tx(tx, referrer).await?
    {
        return Err(lash_durable::packages::PackageRefusal::SessionNotLive(session.clone()).into());
    }
    Ok(())
}

/// Carry each of `live` to its destination: the first carried artifact
/// missing, in carry order, refuses the whole end; otherwise one edge write
/// per live destination. The caller holds every artifact and referrer lock.
async fn carry_edges_tx(
    tx: &mut sqlx::PgConnection,
    namespace: &str,
    live: &[&lash_core_store::artifact_referrer::ArtifactCarry],
    destinations: &[(&ArtifactReferrer, bool)],
) -> Result<(), ArtifactStoreError> {
    if live.is_empty() {
        return Ok(());
    }
    let references: Vec<&str> = live
        .iter()
        .map(|carry| carry.artifact.artifact_ref.as_str())
        .collect();
    let namespaces = vec![namespace; references.len()];
    let missing: Option<String> = crate::observed_sql::query_scalar(
        artifact_sql()
            .lash_vm_artifacts
            .select_missing_manifest
            .sql(),
    )
    .bind(&namespaces)
    .bind(&references)
    .fetch_optional(crate::observed_sql::executor(&mut *tx))
    .await
    .map_err(backend)?;
    if let Some(artifact_ref) = missing {
        let to = live
            .iter()
            .find(|carry| carry.artifact.artifact_ref == artifact_ref)
            .map(|carry| carry.to.clone())
            .ok_or_else(|| ArtifactStoreError::Encode("carry refusal names no carry".into()))?;
        return Err(ArtifactStoreError::CarryArtifactMissing { artifact_ref, to });
    }
    for (to, _) in destinations.iter().filter(|(_, live)| *live) {
        let references: Vec<&str> = live
            .iter()
            .filter(|carry| &carry.to == *to)
            .map(|carry| carry.artifact.artifact_ref.as_str())
            .collect();
        let namespaces = vec![namespace; references.len()];
        crate::observed_sql::query(artifact_sql().lash_vm_artifacts.insert_manifest_edges.sql())
            .bind(&namespaces)
            .bind(&references)
            .bind(to.kind().as_str())
            .bind(to.canonical_id())
            .execute(crate::observed_sql::executor(&mut *tx))
            .await
            .map_err(backend)?;
    }
    Ok(())
}

fn decode_edge_referrer(row: &PgRow) -> Result<ArtifactReferrer, ArtifactStoreError> {
    let kind: String = row.try_get("referrer_kind").map_err(backend)?;
    let id: String = row.try_get("referrer_id").map_err(backend)?;
    decode_referrer(&kind, &id)
}

/// A stored referrer edge's referrer: an unknown kind is vocabulary this
/// build cannot read, anything else unreadable is corrupt.
fn decode_referrer(kind: &str, id: &str) -> Result<ArtifactReferrer, ArtifactStoreError> {
    ArtifactReferrer::decode(kind, id).map_err(|error| match error {
        lash_core_store::artifact_referrer::ArtifactReferrerError::UnknownKind(label) => {
            StoreError::Incompatible {
                refusal: lash_core_store::compat::CompatRefusal::UnknownVocabulary {
                    surface: "artifact referrer edge kind".to_owned(),
                    label,
                },
            }
            .into()
        }
        other => StoreError::StoredDataCorrupt {
            record_kind: "artifact_referrer_edge",
            message: other.to_string(),
        }
        .into(),
    })
}

impl PostgresLashVmArtifactStore {
    /// Publish `bytes` as `artifact_ref` of `namespace` under `claim`, or,
    /// without bytes, acquire it. A transaction PostgreSQL aborts in a lock
    /// race is run again within the writer's retry policy (FIG-7700).
    async fn write_namespaced(
        &self,
        namespace: &str,
        artifact_ref: &str,
        bytes: Option<&[u8]>,
        claim: &ReferrerClaim,
    ) -> Result<(), ArtifactStoreError> {
        crate::guarded_tx::retry_contended(&self.fence, || {
            self.write_namespaced_once(namespace, artifact_ref, bytes, claim)
        })
        .await
    }

    async fn write_namespaced_once(
        &self,
        namespace: &str,
        artifact_ref: &str,
        bytes: Option<&[u8]>,
        claim: &ReferrerClaim,
    ) -> Result<(), ArtifactStoreError> {
        if !claim.referrer().kind().holds_artifacts() {
            return Err(ArtifactStoreError::ReferrerKindRefused {
                kind: claim.referrer().kind(),
            });
        }
        // A published process execution environment carries plugin config
        // namespaces: they are admitted against the fleet record's writer
        // ranges before the artifact is written (FIG-4746).
        let plugin_publication = match bytes {
            Some(bytes) if namespace == PROCESS_ENV_NAMESPACE => {
                lash_core_store::store::plugin_writers::PluginPublication::of_process_env(bytes)
                    .map_err(|error| ArtifactStoreError::Encode(error.to_string()))?
            }
            _ => Default::default(),
        };
        let mut tx = begin_guarded(&self.pool, &self.fence)
            .await
            .map_err(ArtifactStoreError::from)?;
        tx.admit_plugin_writers(&plugin_publication)
            .await
            .map_err(ArtifactStoreError::from)?;
        lock_admitted_referrer_tx(&mut tx, &claim.referrer()).await?;
        let Some(bytes) = bytes else {
            let cleanup = claim.guard_cleanup();
            if namespace == PROCESS_ENV_NAMESPACE {
                let now_ms = self.clock.timestamp_ms();
                acquire_env_tx(
                    &mut tx,
                    artifact_ref,
                    &claim.referrer(),
                    cleanup.as_ref().map(|cleanup| (cleanup, now_ms)),
                )
                .await?;
                return tx.commit().await.map_err(backend);
            }
            // An acquisition: the artifact's lock, the claim's guard, then
            // its existence and the edge.
            let artifact = [(namespace, artifact_ref)];
            Holds {
                locks: &artifact,
                checks: if cleanup.is_none() { &artifact } else { &[] },
                edges: if cleanup.is_none() { &artifact } else { &[] },
            }
            .run(&mut tx, &claim.referrer())
            .await?;
            if let Some(cleanup) = cleanup {
                crate::obligation_ledger::arm_cleanup_tx(
                    &mut tx,
                    &cleanup,
                    self.clock.timestamp_ms(),
                )
                .await
                .map_err(ArtifactStoreError::from)?;
                Holds {
                    locks: &[],
                    checks: &artifact,
                    edges: &artifact,
                }
                .run(&mut tx, &claim.referrer())
                .await?;
            }
            return tx.commit().await.map_err(backend);
        };
        // A published environment's head, body and closure join one sorted
        // lock acquisition (FIG-7700).
        let env_publication = if namespace == PROCESS_ENV_NAMESPACE {
            let publication = EnvPublication::read(&mut tx, bytes).await?;
            lock_artifact_set_tx(&mut tx, &publication.lock_set(artifact_ref)).await?;
            Some(publication)
        } else {
            lock_artifact_tx(&mut tx, namespace, artifact_ref).await?;
            None
        };
        if let Some(cleanup) = claim.guard_cleanup() {
            crate::obligation_ledger::arm_cleanup_tx(&mut tx, &cleanup, self.clock.timestamp_ms())
                .await
                .map_err(ArtifactStoreError::from)?;
        }
        if let Some(publication) = env_publication {
            publication
                .store(&mut tx, artifact_ref, bytes, &claim.referrer())
                .await?;
            return tx.commit().await.map_err(backend);
        }
        crate::observed_sql::query(artifact_sql().lash_vm_artifacts.insert_bytes.sql())
            .bind(namespace)
            .bind(artifact_ref)
            .bind(bytes)
            .execute(crate::observed_sql::executor(&mut **tx))
            .await
            .map_err(backend)?;
        let stored: Vec<u8> =
            crate::observed_sql::query_scalar(artifact_sql().lash_vm_artifacts.select_bytes.sql())
                .bind(namespace)
                .bind(artifact_ref)
                .fetch_one(crate::observed_sql::executor(&mut **tx))
                .await
                .map_err(backend)?;
        if stored != bytes {
            return Err(ArtifactStoreError::Immutable {
                artifact_ref: artifact_ref.to_owned(),
            });
        }
        crate::observed_sql::query(artifact_sql().edges.insert_edge.sql())
            .bind(namespace)
            .bind(artifact_ref)
            .bind(claim.referrer().kind().as_str())
            .bind(claim.referrer().canonical_id())
            .execute(crate::observed_sql::executor(&mut **tx))
            .await
            .map_err(backend)?;
        tx.commit().await.map_err(backend)
    }

    /// Hold every module artifact under the claim in one transaction: the
    /// referrer's lock, fence and guard once, every artifact's lock in key
    /// order, each artifact stored as its hold says, then every edge. An
    /// acquisition the referrer already holds writes nothing: the referrer's
    /// lock keeps its edge, and the edge keeps the stored bytes.
    async fn hold_module_artifact_batch(
        &self,
        claim: &ReferrerClaim,
        artifacts: &[lash_execution_contracts::module_artifacts::ModuleArtifactHold<'_>],
    ) -> Result<(), ArtifactStoreError> {
        crate::guarded_tx::retry_contended(&self.fence, || {
            self.hold_module_artifact_batch_once(claim, artifacts)
        })
        .await
    }

    async fn hold_module_artifact_batch_once(
        &self,
        claim: &ReferrerClaim,
        artifacts: &[lash_execution_contracts::module_artifacts::ModuleArtifactHold<'_>],
    ) -> Result<(), ArtifactStoreError> {
        if !claim.referrer().kind().holds_artifacts() {
            return Err(ArtifactStoreError::ReferrerKindRefused {
                kind: claim.referrer().kind(),
            });
        }
        let mut tx = begin_guarded(&self.pool, &self.fence)
            .await
            .map_err(ArtifactStoreError::from)?;
        lock_admitted_referrer_tx(&mut tx, &claim.referrer()).await?;
        let acquired: Vec<&str> = artifacts
            .iter()
            .filter(|artifact| {
                matches!(
                    artifact,
                    lash_execution_contracts::module_artifacts::ModuleArtifactHold::Acquire { .. }
                )
            })
            .map(lash_execution_contracts::module_artifacts::ModuleArtifactHold::module_ref)
            .collect();
        let unheld: BTreeSet<String> = if acquired.is_empty() {
            BTreeSet::new()
        } else {
            crate::observed_sql::query_scalar::<_, String>(
                artifact_sql().lash_vm_artifacts.select_unheld.sql(),
            )
            .bind(MODULE_ARTIFACT_NAMESPACE)
            .bind(&acquired)
            .bind(claim.referrer().kind().as_str())
            .bind(claim.referrer().canonical_id())
            .fetch_all(crate::observed_sql::executor(&mut **tx))
            .await
            .map_err(backend)?
            .into_iter()
            .collect()
        };
        let artifacts: Vec<&lash_execution_contracts::module_artifacts::ModuleArtifactHold<'_>> =
            artifacts
                .iter()
                .filter(|artifact| match artifact {
                    lash_execution_contracts::module_artifacts::ModuleArtifactHold::Publish {
                        ..
                    } => true,
                    lash_execution_contracts::module_artifacts::ModuleArtifactHold::Retain {
                        ..
                    } => true,
                    lash_execution_contracts::module_artifacts::ModuleArtifactHold::Acquire {
                        module_ref,
                        ..
                    } => unheld.contains(*module_ref),
                })
                .collect();
        // Every module the batch holds, in one sorted acquisition that seals
        // the transaction (FIG-7700).
        let locked: BTreeSet<(String, String)> = artifacts
            .iter()
            .map(|artifact| {
                (
                    MODULE_ARTIFACT_NAMESPACE.to_owned(),
                    artifact.module_ref().to_owned(),
                )
            })
            .collect();
        lock_artifact_set_tx(&mut tx, &locked).await?;
        if let Some(cleanup) = claim.guard_cleanup() {
            crate::obligation_ledger::arm_cleanup_tx(&mut tx, &cleanup, self.clock.timestamp_ms())
                .await
                .map_err(ArtifactStoreError::from)?;
        }
        for artifact in artifacts {
            let artifact_ref = artifact.module_ref();
            let verified = matches!(
                artifact,
                lash_execution_contracts::module_artifacts::ModuleArtifactHold::Publish { .. }
            );
            if let Some(body) = artifact.body() {
                match body.node_kind() {
                    None => {
                        Self::hold_module_bytes_tx(&mut tx, artifact_ref, body.bytes(), verified)
                            .await?;
                    }
                    Some(kind) => {
                        Self::hold_module_node_tx(
                            &mut tx,
                            artifact_ref,
                            kind,
                            body.bytes(),
                            verified,
                        )
                        .await?;
                    }
                }
            } else {
                let exists: bool = crate::observed_sql::query_scalar(
                    artifact_sql().lash_vm_artifacts.exists.sql(),
                )
                .bind(MODULE_ARTIFACT_NAMESPACE)
                .bind(artifact_ref)
                .fetch_one(crate::observed_sql::executor(&mut **tx))
                .await
                .map_err(backend)?;
                if !exists {
                    return Err(ArtifactStoreError::ArtifactMissing {
                        artifact_ref: artifact_ref.to_owned(),
                    });
                }
            }
            crate::observed_sql::query(artifact_sql().edges.insert_edge.sql())
                .bind(MODULE_ARTIFACT_NAMESPACE)
                .bind(artifact_ref)
                .bind(claim.referrer().kind().as_str())
                .bind(claim.referrer().canonical_id())
                .execute(crate::observed_sql::executor(&mut **tx))
                .await
                .map_err(backend)?;
        }
        tx.commit().await.map_err(backend)
    }

    /// Store module bytes under `artifact_ref` if absent, and verify they
    /// equal the stored bytes when `verified`.
    async fn hold_module_bytes_tx(
        tx: &mut sqlx::PgConnection,
        artifact_ref: &str,
        bytes: &[u8],
        verified: bool,
    ) -> Result<(), ArtifactStoreError> {
        crate::observed_sql::query(artifact_sql().lash_vm_artifacts.insert_bytes.sql())
            .bind(MODULE_ARTIFACT_NAMESPACE)
            .bind(artifact_ref)
            .bind(bytes)
            .execute(crate::observed_sql::executor(&mut *tx))
            .await
            .map_err(backend)?;
        if verified {
            let stored: Option<Vec<u8>> = crate::observed_sql::query_scalar(
                artifact_sql().lash_vm_artifacts.select_bytes.sql(),
            )
            .bind(MODULE_ARTIFACT_NAMESPACE)
            .bind(artifact_ref)
            .fetch_one(crate::observed_sql::executor(&mut *tx))
            .await
            .map_err(backend)?;
            if stored.as_deref() != Some(bytes) {
                return Err(ArtifactStoreError::Immutable {
                    artifact_ref: artifact_ref.to_owned(),
                });
            }
        }
        Ok(())
    }

    /// Store a node body under `artifact_ref` if absent, once, as its graph
    /// node, which the row roots; verify a stored row roots that node when
    /// `verified`. The caller holds the artifact's lock.
    async fn hold_module_node_tx(
        tx: &mut sqlx::PgConnection,
        artifact_ref: &str,
        kind: lash_core_store::checkpoint_graph::NodeKind,
        logical: &[u8],
        verified: bool,
    ) -> Result<(), ArtifactStoreError> {
        let stored: Option<(Option<Vec<u8>>, Option<crate::keys::Key>)> =
            crate::observed_sql::query_as(artifact_sql().lash_vm_artifacts.select_stored.sql())
                .bind(MODULE_ARTIFACT_NAMESPACE)
                .bind(artifact_ref)
                .fetch_optional(crate::observed_sql::executor(&mut *tx))
                .await
                .map_err(backend)?;
        match stored {
            Some(_) if !verified => Ok(()),
            Some((_, root)) => {
                let staged = stage_module_node(kind, logical)?;
                if root.as_deref() == Some(staged.root.to_hex().as_str()) {
                    Ok(())
                } else {
                    Err(ArtifactStoreError::Immutable {
                        artifact_ref: artifact_ref.to_owned(),
                    })
                }
            }
            None => {
                let staged = stage_module_node(kind, logical)?;
                crate::durable::install_staged(&mut *tx, &staged.nodes)
                    .await
                    .map_err(module_node_refused)?;
                crate::observed_sql::query(artifact_sql().lash_vm_artifacts.insert_node.sql())
                    .bind(MODULE_ARTIFACT_NAMESPACE)
                    .bind(artifact_ref)
                    .bind(crate::keys::key(staged.root.to_hex()))
                    .execute(crate::observed_sql::executor(&mut *tx))
                    .await
                    .map_err(backend)?;
                Ok(())
            }
        }
    }

    /// Hold one definition closure under the claim in one transaction: the
    /// referrer's lock and fence, then every artifact's lock in key order,
    /// every manifest artifact stored, the descriptor published (verified
    /// byte for byte against a stored one) or stored, the claim's guard, then
    /// every edge (ADR 0113 §3.6).
    async fn hold_definition_closure(
        &self,
        claim: &ReferrerClaim,
        id: &str,
        descriptor: Option<&[u8]>,
        manifest: &[(&'static str, String)],
    ) -> Result<(), ArtifactStoreError> {
        crate::guarded_tx::retry_contended(&self.fence, || {
            self.hold_definition_closure_once(claim, id, descriptor, manifest)
        })
        .await
    }

    async fn hold_definition_closure_once(
        &self,
        claim: &ReferrerClaim,
        id: &str,
        descriptor: Option<&[u8]>,
        manifest: &[(&'static str, String)],
    ) -> Result<(), ArtifactStoreError> {
        if !claim.referrer().kind().holds_artifacts() {
            return Err(ArtifactStoreError::ReferrerKindRefused {
                kind: claim.referrer().kind(),
            });
        }
        let mut tx = begin_guarded(&self.pool, &self.fence)
            .await
            .map_err(ArtifactStoreError::from)?;
        lock_admitted_referrer_tx(&mut tx, &claim.referrer()).await?;
        let mut locked: Vec<(&str, &str)> = manifest
            .iter()
            .map(|(namespace, artifact_ref)| (*namespace, artifact_ref.as_str()))
            .chain(std::iter::once((PROCESS_DEFINITION_NAMESPACE, id)))
            .collect();
        locked.sort_by_key(|(namespace, artifact_ref)| artifact_lock_key(namespace, artifact_ref));
        locked.dedup();
        // Every lock, then the first manifest member missing in manifest
        // order, then, for an acquisition, the descriptor itself: the
        // edges join when no guard is armed between.
        let mut checks: Vec<(&str, &str)> = manifest
            .iter()
            .map(|(namespace, reference)| (*namespace, reference.as_str()))
            .collect();
        if descriptor.is_none() {
            checks.push((PROCESS_DEFINITION_NAMESPACE, id));
        }
        let joined = descriptor.is_none() && claim.guard_cleanup().is_none();
        Holds {
            locks: &locked,
            checks: &checks,
            edges: if joined { &locked } else { &[] },
        }
        .run(&mut tx, &claim.referrer())
        .await?;
        if joined {
            return tx.commit().await.map_err(backend);
        }
        // An acquisition's descriptor was checked with the manifest.
        if let Some(bytes) = descriptor {
            crate::observed_sql::query(artifact_sql().lash_vm_artifacts.insert_bytes.sql())
                .bind(PROCESS_DEFINITION_NAMESPACE)
                .bind(id)
                .bind(bytes)
                .execute(crate::observed_sql::executor(&mut **tx))
                .await
                .map_err(backend)?;
            let stored: Vec<u8> = crate::observed_sql::query_scalar(
                artifact_sql().lash_vm_artifacts.select_bytes.sql(),
            )
            .bind(PROCESS_DEFINITION_NAMESPACE)
            .bind(id)
            .fetch_one(crate::observed_sql::executor(&mut **tx))
            .await
            .map_err(backend)?;
            if stored != bytes {
                return Err(ArtifactStoreError::Immutable {
                    artifact_ref: id.to_owned(),
                });
            }
        }
        if let Some(cleanup) = claim.guard_cleanup() {
            crate::obligation_ledger::arm_cleanup_tx(&mut tx, &cleanup, self.clock.timestamp_ms())
                .await
                .map_err(ArtifactStoreError::from)?;
        }
        let (namespaces, references): (Vec<_>, Vec<_>) = locked.into_iter().unzip();
        crate::observed_sql::query(artifact_sql().lash_vm_artifacts.insert_manifest_edges.sql())
            .bind(&namespaces)
            .bind(&references)
            .bind(claim.referrer().kind().as_str())
            .bind(claim.referrer().canonical_id())
            .execute(crate::observed_sql::executor(&mut **tx))
            .await
            .map_err(backend)?;
        tx.commit().await.map_err(backend)
    }

    async fn end_namespaced(
        &self,
        namespace: &str,
        cleanup: &ResolvedArtifactCleanup,
    ) -> Result<(), ArtifactStoreError> {
        crate::guarded_tx::retry_contended(&self.fence, || {
            self.end_namespaced_once(namespace, cleanup)
        })
        .await
    }

    async fn end_namespaced_once(
        &self,
        namespace: &str,
        cleanup: &ResolvedArtifactCleanup,
    ) -> Result<(), ArtifactStoreError> {
        let mut tx = begin_guarded(&self.pool, &self.fence)
            .await
            .map_err(ArtifactStoreError::from)?;
        if cleanup.carries.is_empty() {
            // A module share includes the referrer's environment closures.
            let shares: &[&str] = if namespace == MODULE_ARTIFACT_NAMESPACE {
                &[MODULE_ARTIFACT_NAMESPACE, PROCESS_ENV_CLOSURE_NAMESPACE]
            } else {
                std::slice::from_ref(&namespace)
            };
            local_end::end(
                &mut tx,
                &cleanup.referrer,
                None,
                self.clock.timestamp_ms(),
                shares,
            )
            .await
            .map_err(ArtifactStoreError::from)?;
        } else {
            Self::end_namespaced_tx(&mut tx, namespace, cleanup, self.clock.timestamp_ms()).await?;
        }
        tx.commit().await.map_err(backend)
    }

    /// Fence, carry, sever and reclaim in the owner's transaction. A module
    /// share includes the referrer's environment closures; an environment
    /// carried to a referrer gives it both shares, storing the closure when
    /// it is not (FIG-7700).
    pub(crate) async fn end_namespaced_tx(
        tx: &mut sqlx::PgConnection,
        namespace: &str,
        cleanup: &ResolvedArtifactCleanup,
        now_ms: u64,
    ) -> Result<(), ArtifactStoreError> {
        for carry in &cleanup.carries {
            if !carry.to.kind().holds_artifacts() {
                return Err(ArtifactStoreError::ReferrerKindRefused {
                    kind: carry.to.kind(),
                });
            }
        }
        let mut referrers: Vec<ArtifactReferrer> = cleanup
            .carries
            .iter()
            .map(|carry| carry.to.clone())
            .collect();
        referrers.push(cleanup.referrer.clone());
        referrers.sort_by_key(referrer_lock_key);
        referrers.dedup();
        // Lock every owning session's history before any referrer, as one
        // set in session order.
        lock_session_artifact_owners_tx(tx, &referrers).await?;
        for referrer in &referrers {
            lock_referrer_tx(tx, referrer).await.map_err(backend)?;
        }
        let shares: &[&str] = if namespace == MODULE_ARTIFACT_NAMESPACE {
            &[MODULE_ARTIFACT_NAMESPACE, PROCESS_ENV_CLOSURE_NAMESPACE]
        } else {
            std::slice::from_ref(&namespace)
        };
        let mut sources: Vec<(&str, BTreeSet<String>)> = Vec::with_capacity(shares.len());
        for share in shares {
            let edges = crate::observed_sql::query(
                artifact_sql()
                    .edges
                    .select_referrer_edges_in_namespace
                    .sql(),
            )
            .bind(*share)
            .bind(cleanup.referrer.kind().as_str())
            .bind(cleanup.referrer.canonical_id())
            .fetch_all(crate::observed_sql::executor(&mut *tx))
            .await
            .map_err(backend)?;
            let refs: BTreeSet<String> = edges
                .iter()
                .map(|row| {
                    decode_edge_referrer(row)?;
                    row.try_get::<String, _>(1).map_err(backend)
                })
                .collect::<Result<_, _>>()?;
            sources.push((share, refs));
        }
        // Every destination's referrer lock is held, so whether it still
        // takes carries cannot change below: read it once per destination.
        // The ending referrer is fenced below, so it takes none.
        let mut destinations: Vec<(&ArtifactReferrer, bool)> = Vec::new();
        for carry in &cleanup.carries {
            if destinations.iter().all(|(to, _)| *to != &carry.to) {
                let live = carry.to != cleanup.referrer
                    && session_artifact_owner_live_tx(tx, &carry.to).await?
                    && !is_fenced_tx(tx, &carry.to).await?;
                destinations.push((&carry.to, live));
            }
        }
        let live: Vec<&lash_core_store::artifact_referrer::ArtifactCarry> = cleanup
            .carries
            .iter()
            .filter(|carry| destinations.contains(&(&carry.to, true)))
            .collect();
        // A carried environment's closure, and the modules to store it with
        // when it is not stored, join the one sorted acquisition below with
        // the sources: no artifact lock comes before it (FIG-7700).
        // Only an environment carry has a closure.
        let carried_envs: &[&lash_core_store::artifact_referrer::ArtifactCarry] =
            if namespace == PROCESS_ENV_NAMESPACE {
                &live
            } else {
                &[]
            };
        let closures = CarriedClosures::plan(tx, carried_envs).await?;
        // What the released artifacts hold, transitively, joins their one
        // sorted lock acquisition: deleting them can free it (FIG-7700).
        let mut locked: BTreeSet<(String, String)> = cleanup
            .carries
            .iter()
            .map(|carry| (namespace.to_owned(), carry.artifact.artifact_ref.clone()))
            .collect();
        for (share, refs) in &sources {
            locked.extend(
                refs.iter()
                    .map(|reference| ((*share).to_owned(), reference.clone())),
            );
            locked.extend(held_closure_tx(tx, share, refs).await?);
        }
        locked.extend(closures.locks());
        lock_artifact_set_tx(tx, &locked).await?;
        let closures = closures.settle(tx, carried_envs, &locked).await?;
        crate::observed_sql::query(artifact_sql().fences.insert_fence.sql())
            .bind(cleanup.referrer.kind().as_str())
            .bind(cleanup.referrer.canonical_id())
            .bind(crate::support::clamp_epoch_ms(now_ms))
            .execute(crate::observed_sql::executor(&mut *tx))
            .await
            .map_err(backend)?;
        carry_edges_tx(tx, namespace, &live, &destinations).await?;
        closures.carry(tx, carried_envs, &destinations).await?;
        for (share, refs) in &sources {
            crate::observed_sql::query(
                artifact_sql()
                    .edges
                    .delete_referrer_edges_in_namespace
                    .sql(),
            )
            .bind(*share)
            .bind(cleanup.referrer.kind().as_str())
            .bind(cleanup.referrer.canonical_id())
            .execute(crate::observed_sql::executor(&mut *tx))
            .await
            .map_err(backend)?;
            Self::reclaim_sources_tx(tx, share, refs).await?;
        }
        Ok(())
    }

    /// Reclaim each of `source_refs` in `namespace` that no referrer names
    /// and nothing holds, and what that frees.
    async fn reclaim_sources_tx(
        tx: &mut sqlx::PgConnection,
        namespace: &str,
        source_refs: &BTreeSet<String>,
    ) -> Result<(), ArtifactStoreError> {
        if source_refs.is_empty() {
            return Ok(());
        }
        let wanted: Vec<&str> = source_refs.iter().map(String::as_str).collect();
        let rows =
            crate::observed_sql::query(artifact_sql().postgres.select_artifact_set_edges.sql())
                .bind(namespace)
                .bind(&wanted)
                .fetch_all(crate::observed_sql::executor(&mut *tx))
                .await
                .map_err(backend)?;
        let mut held: std::collections::BTreeMap<
            String,
            Vec<(
                lash_core_store::artifact_referrer::ArtifactReferrerKind,
                String,
            )>,
        > = std::collections::BTreeMap::new();
        for row in &rows {
            let referrer = decode_edge_referrer(row)?;
            held.entry(row.try_get::<String, _>(0).map_err(backend)?)
                .or_default()
                .push((referrer.kind(), referrer.canonical_id()));
        }
        let mut witnesses = Vec::with_capacity(source_refs.len());
        for artifact_ref in source_refs {
            use lash_core_store::store::EnumerationProgress;
            use lash_core_store::store::ReclamationEnumeration;
            let referrers = held.remove(artifact_ref).unwrap_or_default();
            let mut enumeration = ReclamationEnumeration::<
                lash_core_store::artifact_referrer::ArtifactReferrerKind,
                (
                    lash_core_store::artifact_referrer::ArtifactReferrerKind,
                    String,
                ),
            >::new();
            for kind in lash_core_store::artifact_referrer::ArtifactReferrerKind::ALL {
                enumeration
                    .page(
                        kind,
                        0,
                        referrers
                            .iter()
                            .filter(|(source, _)| *source == kind)
                            .cloned(),
                        EnumerationProgress::Exhausted,
                    )
                    .map_err(ArtifactStoreError::from)?;
            }
            let witness = enumeration.finish().map_err(ArtifactStoreError::from)?;
            witnesses.push((artifact_ref.as_str(), witness));
        }
        Self::delete_unreferenced_artifacts_tx(tx, namespace, &witnesses).await
    }

    /// Retire the database-owned share in the owner's transaction. The
    /// ledger still owes acknowledgements from independent stores.
    pub(crate) async fn end_local_referrer_tx(
        tx: &mut sqlx::PgConnection,
        referrer: &ArtifactReferrer,
        now_ms: u64,
    ) -> Result<(), StoreError> {
        local_end::end(tx, referrer, None, now_ms, &local_end::NAMESPACES).await
    }

    pub(crate) async fn end_local_execution_tx(
        tx: &mut sqlx::PgConnection,
        journal: &lash_protocol_types::EffectJournalIdentity,
        now_ms: u64,
    ) -> Result<(), StoreError> {
        local_end::end(
            tx,
            &ArtifactReferrer::Execution(journal.clone()),
            Some(journal),
            now_ms,
            &local_end::NAMESPACES,
        )
        .await
    }

    /// Delete each artifact whose complete referrer enumeration is empty
    /// and that no artifact holds, then, round by round, each artifact a
    /// deleted one held that nothing keeps now; peel the graph nodes bridged
    /// ones rooted. The caller locked the closure the deletions can free.
    async fn delete_unreferenced_artifacts_tx(
        tx: &mut sqlx::PgConnection,
        namespace: &str,
        artifacts: &[(&str, lash_core_store::store::CompleteArtifactReferrers)],
    ) -> Result<(), ArtifactStoreError> {
        let unreferenced: BTreeSet<String> = artifacts
            .iter()
            .filter(|(_, referrers)| referrers.is_empty())
            .map(|(artifact_ref, _)| (*artifact_ref).to_owned())
            .collect();
        let mut round = std::collections::BTreeMap::new();
        if !unreferenced.is_empty() {
            round.insert(namespace.to_owned(), unreferenced);
        }
        let mut bridged = BTreeSet::new();
        while !round.is_empty() {
            let mut freed: std::collections::BTreeMap<String, BTreeSet<String>> =
                std::collections::BTreeMap::new();
            for (namespace, references) in &round {
                let wanted: Vec<&str> = references.iter().map(String::as_str).collect();
                let rows: Vec<(Option<crate::keys::Key>, Option<String>, Option<String>)> =
                    crate::observed_sql::query_as(
                        artifact_sql().postgres.delete_unreferenced.sql(),
                    )
                    .bind(namespace)
                    .bind(&wanted)
                    .fetch_all(crate::observed_sql::executor(&mut *tx))
                    .await
                    .map_err(backend)?;
                for (graph_ref, freed_namespace, freed_ref) in rows {
                    // A bridged artifact's row was its graph node's root.
                    bridged.extend(graph_ref.map(|key| key.0));
                    if let (Some(freed_namespace), Some(freed_ref)) = (freed_namespace, freed_ref) {
                        freed.entry(freed_namespace).or_default().insert(freed_ref);
                    }
                }
            }
            round = freed;
        }
        crate::durable::peel(tx, bridged)
            .await
            .map_err(|error| ArtifactStoreError::Backend(error.to_string()))?;
        Ok(())
    }

    async fn get_namespaced(
        &self,
        namespace: &str,
        artifact_ref: &str,
    ) -> Result<Option<Vec<u8>>, ArtifactStoreError> {
        // Only the artifact's own row: a bytes read neither fetches nor
        // decodes its referrer edges, so its cost does not grow with how
        // many referrers hold it. Paths that act on edges decode them
        // (FIG-7701).
        let stored: Option<(Option<Vec<u8>>, Option<crate::keys::Key>)> =
            crate::observed_sql::query_as(artifact_sql().lash_vm_artifacts.select_stored.sql())
                .bind(namespace)
                .bind(artifact_ref)
                .fetch_optional(crate::observed_sql::executor(&self.pool))
                .await
                .map_err(backend)?;
        let Some((bytes, root)) = stored else {
            return Ok(None);
        };
        match bytes {
            Some(bytes) => Ok(Some(bytes)),
            None => self.module_node_body(root.as_deref()).await,
        }
    }

    /// The logical body of the node a bridged artifact's row roots, its
    /// closure read in one statement: none when a reclaim took the row's
    /// node after the row was read.
    async fn module_node_body(
        &self,
        root: Option<&str>,
    ) -> Result<Option<Vec<u8>>, ArtifactStoreError> {
        use lash_core_store::checkpoint_graph::{GraphRefusal, NodeId};
        let corrupt = |message: String| ArtifactStoreError::StoredDataCorrupt {
            source:
                lash_protocol_types::module_artifact_refusal::ModuleArtifactCorruption::Storage {
                    record_kind: "module artifact node".into(),
                    message,
                },
        };
        let root = root
            .and_then(NodeId::from_hex)
            .ok_or_else(|| corrupt("a bridged row names no node".into()))?;
        let mut connection = self.pool.acquire().await.map_err(backend)?;
        let closure = match crate::durable::graph_closure(
            &mut connection,
            root,
            &lash_durable::domain::graph::OWNER_READ_LIMITS,
        )
        .await
        {
            Ok(closure) => closure,
            Err(GraphRefusal::MissingNode(missing)) if missing == root => return Ok(None),
            Err(refusal) => return Err(corrupt(refusal.to_string())),
        };
        lash_execution_contracts::module_nodes::module_node_body(root, &closure).map(Some)
    }
}

/// A graph refusal of a module node's install: what the store holds under
/// the node's digest is not that node.
fn module_node_refused(
    refusal: lash_core_store::checkpoint_graph::GraphRefusal,
) -> ArtifactStoreError {
    ArtifactStoreError::StoredDataCorrupt {
        source: lash_protocol_types::module_artifact_refusal::ModuleArtifactCorruption::Storage {
            record_kind: "module artifact node".into(),
            message: refusal.to_string(),
        },
    }
}

#[async_trait::async_trait]
impl lash_execution_contracts::module_artifacts::ModuleArtifactStore
    for PostgresLashVmArtifactStore
{
    fn durability_tier(&self) -> lash_execution_contracts::module_artifacts::DurabilityTier {
        lash_execution_contracts::module_artifacts::DurabilityTier::Durable
    }

    async fn publish_module_artifact(
        &self,
        claim: &ReferrerClaim,
        module_ref: &str,
        bytes: &[u8],
    ) -> Result<(), ArtifactStoreError> {
        if !crate::namespace::is_valid_opaque_key(module_ref) {
            return Err(ArtifactStoreError::Encode(
                "invalid module reference".into(),
            ));
        }
        self.write_namespaced(MODULE_ARTIFACT_NAMESPACE, module_ref, Some(bytes), claim)
            .await
    }

    async fn acquire_module_artifact(
        &self,
        claim: &ReferrerClaim,
        module_ref: &str,
    ) -> Result<(), ArtifactStoreError> {
        if lash_durable::packages::is_package_reference(module_ref) {
            return lash_durable::packages::acquire_content(self, claim, module_ref)
                .await
                .map_err(Into::into);
        }
        if !crate::namespace::is_valid_opaque_key(module_ref) {
            return Err(ArtifactStoreError::Encode(
                "invalid module reference".into(),
            ));
        }
        self.write_namespaced(MODULE_ARTIFACT_NAMESPACE, module_ref, None, claim)
            .await
    }

    async fn hold_module_artifacts(
        &self,
        claim: &ReferrerClaim,
        artifacts: &[lash_execution_contracts::module_artifacts::ModuleArtifactHold<'_>],
    ) -> Result<(), ArtifactStoreError> {
        if artifacts.is_empty() {
            return Ok(());
        }
        if artifacts
            .iter()
            .any(|artifact| !crate::namespace::is_valid_opaque_key(artifact.module_ref()))
        {
            return Err(ArtifactStoreError::Encode(
                "invalid module reference".into(),
            ));
        }
        self.hold_module_artifact_batch(claim, artifacts).await
    }

    async fn end_module_referrer(
        &self,
        cleanup: &ResolvedArtifactCleanup,
    ) -> Result<(), ArtifactStoreError> {
        let cleanup = lash_durable::packages::expand_package_cleanup(self, cleanup)
            .await
            .map_err(ArtifactStoreError::from)?;
        self.end_namespaced(MODULE_ARTIFACT_NAMESPACE, &cleanup)
            .await
    }

    async fn get_module_artifact(
        &self,
        module_ref: &str,
    ) -> Result<Option<Vec<u8>>, ArtifactStoreError> {
        if !crate::namespace::is_valid_opaque_key(module_ref) {
            return Err(ArtifactStoreError::Encode(
                "invalid module reference".into(),
            ));
        }
        self.get_namespaced(MODULE_ARTIFACT_NAMESPACE, module_ref)
            .await
    }
}

#[async_trait::async_trait]
impl lash_execution_contracts::process::ProcessEnvStore for PostgresLashVmArtifactStore {
    async fn publish_process_env(
        &self,
        claim: &ReferrerClaim,
        env_ref: &lash_core_store::process_identity::ProcessEnvRef,
        bytes: &[u8],
    ) -> Result<(), ArtifactStoreError> {
        if !crate::namespace::is_valid_opaque_key(env_ref.as_str()) {
            return Err(ArtifactStoreError::Encode(
                "invalid process execution environment reference".into(),
            ));
        }
        if !env_ref.matches_store_bytes(bytes) {
            return Err(ArtifactStoreError::Immutable {
                artifact_ref: env_ref.as_str().to_owned(),
            });
        }
        self.write_namespaced(PROCESS_ENV_NAMESPACE, env_ref.as_str(), Some(bytes), claim)
            .await
    }

    async fn acquire_process_env(
        &self,
        claim: &ReferrerClaim,
        env_ref: &lash_core_store::process_identity::ProcessEnvRef,
    ) -> Result<(), ArtifactStoreError> {
        if !crate::namespace::is_valid_opaque_key(env_ref.as_str()) {
            return Err(ArtifactStoreError::Encode(
                "invalid process execution environment reference".into(),
            ));
        }
        self.write_namespaced(PROCESS_ENV_NAMESPACE, env_ref.as_str(), None, claim)
            .await
    }

    async fn end_process_env_referrer(
        &self,
        cleanup: &ResolvedArtifactCleanup,
    ) -> Result<(), ArtifactStoreError> {
        self.end_namespaced(PROCESS_ENV_NAMESPACE, cleanup).await
    }

    async fn get_process_env(
        &self,
        env_ref: &lash_core_store::process_identity::ProcessEnvRef,
    ) -> Result<Option<Vec<u8>>, ArtifactStoreError> {
        if !crate::namespace::is_valid_opaque_key(env_ref.as_str()) {
            return Err(ArtifactStoreError::Encode(
                "invalid process execution environment reference".into(),
            ));
        }
        self.get_namespaced(PROCESS_ENV_NAMESPACE, env_ref.as_str())
            .await
    }
}

#[async_trait::async_trait]
impl lash_execution_contracts::turn_prelude::TurnPreludeStore for PostgresLashVmArtifactStore {
    async fn publish_turn_prelude(
        &self,
        claim: &ReferrerClaim,
        prelude_ref: &lash_execution_contracts::turn_prelude::TurnPreludeRef,
        bytes: &[u8],
    ) -> Result<(), ArtifactStoreError> {
        if !prelude_ref.matches_store_bytes(bytes) {
            return Err(ArtifactStoreError::Immutable {
                artifact_ref: prelude_ref.as_str().to_owned(),
            });
        }
        self.write_namespaced(
            TURN_PRELUDE_NAMESPACE,
            prelude_ref.as_str(),
            Some(bytes),
            claim,
        )
        .await
    }

    async fn end_turn_prelude_referrer(
        &self,
        cleanup: &ResolvedArtifactCleanup,
    ) -> Result<(), ArtifactStoreError> {
        self.end_namespaced(TURN_PRELUDE_NAMESPACE, cleanup).await
    }

    async fn get_turn_prelude(
        &self,
        prelude_ref: &lash_execution_contracts::turn_prelude::TurnPreludeRef,
    ) -> Result<Option<Vec<u8>>, ArtifactStoreError> {
        if !crate::namespace::is_valid_opaque_key(prelude_ref.as_str()) {
            return Err(ArtifactStoreError::Encode(
                "invalid turn prelude reference".into(),
            ));
        }
        self.get_namespaced(TURN_PRELUDE_NAMESPACE, prelude_ref.as_str())
            .await
    }
}

#[async_trait::async_trait]
impl lash_execution_contracts::process::ProcessDefinitionStore for PostgresLashVmArtifactStore {
    async fn publish_process_definition(
        &self,
        claim: &ReferrerClaim,
        id: &lash_protocol_types::ProcessDefinitionId,
        descriptor: &[u8],
        manifest: &[lash_core_store::artifact_referrer::ArtifactName],
    ) -> Result<(), ArtifactStoreError> {
        self.hold_definition_closure(
            claim,
            id.as_str(),
            Some(descriptor),
            &definition_manifest(manifest)?,
        )
        .await
    }

    async fn acquire_process_definition(
        &self,
        claim: &ReferrerClaim,
        id: &lash_protocol_types::ProcessDefinitionId,
        manifest: &[lash_core_store::artifact_referrer::ArtifactName],
    ) -> Result<(), ArtifactStoreError> {
        self.hold_definition_closure(claim, id.as_str(), None, &definition_manifest(manifest)?)
            .await
    }

    async fn end_process_definition_referrer(
        &self,
        cleanup: &ResolvedArtifactCleanup,
    ) -> Result<(), ArtifactStoreError> {
        self.end_namespaced(PROCESS_DEFINITION_NAMESPACE, cleanup)
            .await
    }

    async fn get_process_definition(
        &self,
        id: &lash_protocol_types::ProcessDefinitionId,
    ) -> Result<Option<Vec<u8>>, ArtifactStoreError> {
        self.get_namespaced(PROCESS_DEFINITION_NAMESPACE, id.as_str())
            .await
    }
}

#[path = "artifact_store/packages.rs"]
mod packages;

#[cfg(test)]
#[path = "artifact_store_tests.rs"]
pub(crate) mod tests;
