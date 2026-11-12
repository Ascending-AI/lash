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
        referrer: &ArtifactReferrer,
    ) -> Result<(), ArtifactStoreError> {
        let at =
            match &self.split {
                Some(split) => Some(i32::try_from(split.at).map_err(|_| {
                    ArtifactStoreError::Encode("process environment too large".into())
                })?),
                None => None,
            };
        if let (Some(split), Some(at), Ok(closure)) = (&self.split, at, &self.closure)
            && publish_over_stored_body_tx(tx, env_ref, split, at, closure, referrer).await?
        {
            return Ok(());
        }
        match (&self.split, at) {
            (Some(split), Some(at)) => {
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
            _ => {
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
        crate::observed_sql::query(artifact_sql().edges.insert_edge.sql())
            .bind(PROCESS_ENV_NAMESPACE)
            .bind(env_ref)
            .bind(referrer.kind().as_str())
            .bind(referrer.canonical_id())
            .execute(crate::observed_sql::executor(&mut *tx))
            .await
            .map_err(backend)?;
        Ok(())
    }
}

/// Publish an environment over a body already stored with exactly its
/// bytes, in one call under the head's, the body's and every closure
/// module's lock (FIG-7701 R5): the stored body compared with `split`'s,
/// the head stored and compared with `split`'s, then the first missing
/// module of `closure` refused, else the closure's, the body's and the
/// referrer's edges written as a first publication writes them. `false`
/// when the stored body or a stored head differs from `split`, or the body
/// is absent: nothing is written, and the caller stores the body as a first
/// publication does, so missing, corrupt and Immutable answers are its.
async fn publish_over_stored_body_tx(
    tx: &mut sqlx::PgConnection,
    env_ref: &str,
    split: &lash_core_store::process_env::stored::StoredSplit,
    at: i32,
    closure: &BTreeSet<String>,
    referrer: &ArtifactReferrer,
) -> Result<bool, ArtifactStoreError> {
    let modules: Vec<&str> = closure.iter().map(String::as_str).collect();
    let (stored, missing): (bool, Option<String>) =
        crate::observed_sql::query_as(artifact_sql().postgres.publish_over_stored_body.sql())
            .bind(env_ref)
            .bind(&split.head)
            .bind(&split.body_ref)
            .bind(at)
            .bind(&split.body)
            .bind(&modules)
            .bind(referrer.kind().as_str())
            .bind(referrer.canonical_id())
            .fetch_one(crate::observed_sql::executor(&mut *tx))
            .await
            .map_err(backend)?;
    match missing {
        Some(artifact_ref) => Err(ArtifactStoreError::ArtifactMissing { artifact_ref }),
        None => Ok(stored),
    }
}

/// The call over a stored body. Its statements start after the caller's
/// lock statement, so they read what any holder of those locks committed.
#[cfg(any(test, feature = "testing"))]
pub(crate) fn function_ddl() -> String {
    // The head is `insert_head`'s, and the two comparisons are the splice
    // `select_bytes` reads, taken apart as `stored::split` takes it; then
    // `select_missing_manifest`, `insert_dependency_edges` twice and the
    // referrer `insert_edge`.
    format!(
        r#"CREATE OR REPLACE FUNCTION lash_publish_env_over_stored_body(
    p_env_ref text, p_head bytea, p_body_ref text, p_at integer, p_body bytea,
    p_modules text[], p_kind text, p_id text,
    OUT stored boolean, OUT missing text
) LANGUAGE plpgsql VOLATILE AS $lash_publish_env_over_stored_body$
BEGIN
    stored := EXISTS (SELECT 1 FROM lash_lash_vm_artifacts
        WHERE namespace = '{body}' AND artifact_ref = p_body_ref
            AND artifact_bytes = p_body);
    IF NOT stored THEN
        RETURN;
    END IF;
    INSERT INTO lash_lash_vm_artifacts (namespace, artifact_ref, storage_kind, artifact_bytes, body_ref, body_at)
        VALUES ('{env}', p_env_ref, 2, p_head, p_body_ref, p_at)
        ON CONFLICT (namespace, artifact_ref) DO NOTHING;
    stored := EXISTS (SELECT 1 FROM lash_lash_vm_artifacts
        WHERE namespace = '{env}' AND artifact_ref = p_env_ref
            AND storage_kind = 2 AND body_ref = p_body_ref
            AND body_at = p_at AND artifact_bytes = p_head);
    IF NOT stored THEN
        RETURN;
    END IF;
    SELECT wanted.artifact_ref INTO missing
    FROM unnest(p_modules) WITH ORDINALITY AS wanted(artifact_ref, ordinal)
    WHERE NOT EXISTS (SELECT 1 FROM lash_lash_vm_artifacts AS module
        WHERE module.namespace = '{module}' AND module.artifact_ref = wanted.artifact_ref)
    ORDER BY wanted.ordinal LIMIT 1;
    IF missing IS NOT NULL THEN
        RETURN;
    END IF;
    INSERT INTO lash_artifact_dependency_edges (namespace, artifact_ref, holder_namespace, holder_ref)
        SELECT '{module}', wanted.artifact_ref, '{body}', p_body_ref
        FROM unnest(p_modules) AS wanted(artifact_ref)
        ON CONFLICT DO NOTHING;
    INSERT INTO lash_artifact_dependency_edges (namespace, artifact_ref, holder_namespace, holder_ref)
        VALUES ('{body}', p_body_ref, '{env}', p_env_ref)
        ON CONFLICT DO NOTHING;
    INSERT INTO lash_artifact_referrer_edges (namespace, artifact_ref, referrer_kind, referrer_id)
        VALUES ('{env}', p_env_ref, p_kind, p_id)
        ON CONFLICT DO NOTHING;
END
$lash_publish_env_over_stored_body$;
"#,
        env = PROCESS_ENV_NAMESPACE,
        body = PROCESS_ENV_BODY_NAMESPACE,
        module = MODULE_ARTIFACT_NAMESPACE,
    )
}
