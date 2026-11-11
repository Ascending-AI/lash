//! How the PostgreSQL store writes a process environment: its stored
//! split, its module closure and the holder edges that keep them (FIG-7700).

use super::*;

/// An environment's module closure, read from the bytes in hand: one read
/// per dependency level, the level's references at once. Module provenance
/// is immutable, so it is read before the closure's sorted locks.
pub(super) async fn process_env_closure_tx(
    tx: &mut sqlx::PgConnection,
    bytes: &[u8],
) -> Result<BTreeSet<String>, ArtifactStoreError> {
    let mut level: BTreeSet<String> = lash_durable::packages::process_env_roots(bytes)?
        .into_iter()
        .collect();
    let mut found = BTreeSet::new();
    while !level.is_empty() {
        let wanted: Vec<&str> = level.iter().map(String::as_str).collect();
        let rows: Vec<(String, Option<Vec<u8>>, Option<crate::keys::Key>)> =
            crate::observed_sql::query_as(artifact_sql().lash_vm_artifacts.select_stored_set.sql())
                .bind(MODULE_ARTIFACT_NAMESPACE)
                .bind(&wanted)
                .fetch_all(crate::observed_sql::executor(&mut *tx))
                .await
                .map_err(backend)?;
        let mut stored: std::collections::BTreeMap<String, Option<Vec<u8>>> = rows
            .into_iter()
            .filter(|(_, bytes, root)| bytes.is_some() || root.is_some())
            .map(|(reference, bytes, _)| (reference, bytes))
            .collect();
        let mut next = BTreeSet::new();
        for reference in &level {
            match stored.remove(reference) {
                Some(Some(bytes)) => next.extend(lash_durable::packages::content_dependencies(
                    reference, &bytes,
                )?),
                // Node-bodied documents are terminal package dependencies. The
                // root pointer is locked and rechecked before any edge is written.
                Some(None) => {}
                None => {
                    return Err(ArtifactStoreError::ArtifactMissing {
                        artifact_ref: reference.clone(),
                    });
                }
            }
        }
        found.append(&mut level);
        level = next
            .into_iter()
            .filter(|child| !found.contains(child))
            .collect();
    }
    Ok(found)
}

/// Recheck `found`'s locked module artifacts in one statement, since
/// cleanup may have won during the unlocked reads: the first missing one.
pub(super) async fn recheck_modules_tx(
    tx: &mut sqlx::PgConnection,
    found: &BTreeSet<String>,
) -> Result<(), ArtifactStoreError> {
    if found.is_empty() {
        return Ok(());
    }
    let references: Vec<&str> = found.iter().map(String::as_str).collect();
    let namespaces = vec![MODULE_ARTIFACT_NAMESPACE; references.len()];
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
    match missing {
        Some(artifact_ref) => Err(ArtifactStoreError::ArtifactMissing { artifact_ref }),
        None => Ok(()),
    }
}

/// Holder `holder` keeps each of `targets` in `namespace`, in one statement.
pub(super) async fn insert_dependency_edges_tx(
    tx: &mut sqlx::PgConnection,
    namespace: &str,
    targets: &BTreeSet<String>,
    holder: (&str, &str),
) -> Result<(), ArtifactStoreError> {
    if targets.is_empty() {
        return Ok(());
    }
    let references: Vec<&str> = targets.iter().map(String::as_str).collect();
    crate::observed_sql::query(artifact_sql().postgres.insert_dependency_edges.sql())
        .bind(namespace)
        .bind(&references)
        .bind(holder.0)
        .bind(holder.1)
        .execute(crate::observed_sql::executor(&mut *tx))
        .await
        .map_err(backend)?;
    Ok(())
}

/// Everything `references` of `namespace` hold, transitively, in one
/// statement. A holder's edges are immutable while it lives, so this is
/// read before the holders' locks.
pub(super) async fn held_closure_tx(
    tx: &mut sqlx::PgConnection,
    namespace: &str,
    references: &BTreeSet<String>,
) -> Result<BTreeSet<(String, String)>, ArtifactStoreError> {
    if references.is_empty() {
        return Ok(BTreeSet::new());
    }
    let wanted: Vec<&str> = references.iter().map(String::as_str).collect();
    let namespaces = vec![namespace; wanted.len()];
    let rows: Vec<(String, String)> =
        crate::observed_sql::query_as(artifact_sql().postgres.select_held_closure.sql())
            .bind(&namespaces)
            .bind(&wanted)
            .fetch_all(crate::observed_sql::executor(&mut *tx))
            .await
            .map_err(backend)?;
    Ok(rows.into_iter().collect())
}

/// A process environment about to be published: its stored split and its
/// module closure, read before any artifact lock. A closure refusal waits
/// until the bytes are compared, as it did when the closure was read from
/// the stored row.
pub(super) struct EnvPublication {
    split: Option<lash_core_store::process_env::stored::StoredSplit>,
    closure: Result<BTreeSet<String>, ArtifactStoreError>,
}

impl EnvPublication {
    pub(super) async fn read(
        tx: &mut sqlx::PgConnection,
        bytes: &[u8],
    ) -> Result<Self, ArtifactStoreError> {
        let closure = match process_env_closure_tx(tx, bytes).await {
            Err(ArtifactStoreError::Backend(error)) => {
                return Err(ArtifactStoreError::Backend(error));
            }
            closure => closure,
        };
        Ok(Self {
            split: lash_core_store::process_env::stored::split(bytes),
            closure,
        })
    }

    /// The head, its body and every closure module, for one sorted lock.
    pub(super) fn lock_set(&self, env_ref: &str) -> BTreeSet<(String, String)> {
        let mut set = BTreeSet::from([(PROCESS_ENV_NAMESPACE.to_owned(), env_ref.to_owned())]);
        if let Some(split) = &self.split {
            set.insert((
                PROCESS_ENV_BODY_NAMESPACE.to_owned(),
                split.body_ref.clone(),
            ));
        }
        if let Ok(closure) = &self.closure {
            set.extend(
                closure
                    .iter()
                    .map(|module| (MODULE_ARTIFACT_NAMESPACE.to_owned(), module.clone())),
            );
        }
        set
    }

    /// Store the head and its body, compare the stored bytes, then write
    /// the dependency edges: the body (or an unsplit head) holds the
    /// closure, the head holds its body. The caller holds every lock.
    pub(super) async fn store(
        self,
        tx: &mut sqlx::PgConnection,
        env_ref: &str,
        bytes: &[u8],
    ) -> Result<(), ArtifactStoreError> {
        match &self.split {
            Some(split) => {
                let at = i32::try_from(split.at).map_err(|_| {
                    ArtifactStoreError::Encode("process environment too large".into())
                })?;
                crate::observed_sql::query(artifact_sql().lash_vm_artifacts.insert_bytes.sql())
                    .bind(PROCESS_ENV_BODY_NAMESPACE)
                    .bind(&split.body_ref)
                    .bind(&split.body)
                    .execute(crate::observed_sql::executor(&mut *tx))
                    .await
                    .map_err(backend)?;
                crate::observed_sql::query(artifact_sql().lash_vm_artifacts.insert_head.sql())
                    .bind(PROCESS_ENV_NAMESPACE)
                    .bind(env_ref)
                    .bind(&split.head)
                    .bind(&split.body_ref)
                    .bind(at)
                    .execute(crate::observed_sql::executor(&mut *tx))
                    .await
                    .map_err(backend)?;
            }
            None => {
                crate::observed_sql::query(artifact_sql().lash_vm_artifacts.insert_bytes.sql())
                    .bind(PROCESS_ENV_NAMESPACE)
                    .bind(env_ref)
                    .bind(bytes)
                    .execute(crate::observed_sql::executor(&mut *tx))
                    .await
                    .map_err(backend)?;
            }
        }
        let stored: Option<Vec<u8>> =
            crate::observed_sql::query_scalar(artifact_sql().lash_vm_artifacts.select_bytes.sql())
                .bind(PROCESS_ENV_NAMESPACE)
                .bind(env_ref)
                .fetch_one(crate::observed_sql::executor(&mut *tx))
                .await
                .map_err(backend)?;
        if stored.as_deref() != Some(bytes) {
            return Err(ArtifactStoreError::Immutable {
                artifact_ref: env_ref.to_owned(),
            });
        }
        let closure = self.closure?;
        recheck_modules_tx(tx, &closure).await?;
        let holder = match &self.split {
            Some(split) => (PROCESS_ENV_BODY_NAMESPACE, split.body_ref.as_str()),
            None => (PROCESS_ENV_NAMESPACE, env_ref),
        };
        insert_dependency_edges_tx(tx, MODULE_ARTIFACT_NAMESPACE, &closure, holder).await?;
        if let Some(split) = &self.split {
            insert_dependency_edges_tx(
                tx,
                PROCESS_ENV_BODY_NAMESPACE,
                &BTreeSet::from([split.body_ref.clone()]),
                (PROCESS_ENV_NAMESPACE, env_ref),
            )
            .await?;
        }
        Ok(())
    }
}
