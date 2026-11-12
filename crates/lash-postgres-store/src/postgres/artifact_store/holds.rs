//! A hold's referrer and artifact steps as server calls (FIG-7701 R3).
//!
//! Each call keeps the lock waits one statement had before, and reads what
//! depends on them in later statements of a volatile function, so every
//! such read takes its snapshot after the waits: the referrer's fence after
//! its lock, the stored artifacts after theirs.

use lash_core_store::artifact_referrer::ArtifactReferrer;
use lash_execution_contracts::module_artifacts::ArtifactStoreError;

use super::{artifact_lock_key, artifact_sql, backend};

/// Lock `referrer`, then refuse it if it has ended.
pub(crate) async fn lock_live_referrer_tx(
    conn: &mut sqlx::PgConnection,
    referrer: &ArtifactReferrer,
) -> Result<(), ArtifactStoreError> {
    let fenced: bool =
        crate::observed_sql::query_scalar(artifact_sql().postgres.lock_referrer.sql())
            .bind(referrer.kind().as_str())
            .bind(referrer.canonical_id())
            .fetch_one(crate::observed_sql::executor(conn))
            .await
            .map_err(backend)?;
    if fenced {
        return Err(ArtifactStoreError::ReferrerEnded {
            referrer: referrer.clone(),
        });
    }
    Ok(())
}

/// One hold's artifact steps, in order: lock each of `locks` in the order
/// given, refuse the first of `checks` not stored, then give `referrer`
/// an edge to each of `edges`. Any of the three may be empty.
pub(crate) struct Holds<'a> {
    pub(crate) locks: &'a [(&'a str, &'a str)],
    pub(crate) checks: &'a [(&'a str, &'a str)],
    pub(crate) edges: &'a [(&'a str, &'a str)],
}

impl Holds<'_> {
    pub(crate) async fn run(
        &self,
        conn: &mut sqlx::PgConnection,
        referrer: &ArtifactReferrer,
    ) -> Result<(), ArtifactStoreError> {
        let locks: Vec<String> = self
            .locks
            .iter()
            .map(|(namespace, artifact_ref)| artifact_lock_key(namespace, artifact_ref))
            .collect();
        let (check_namespaces, check_refs): (Vec<&str>, Vec<&str>) =
            self.checks.iter().copied().unzip();
        let (edge_namespaces, edge_refs): (Vec<&str>, Vec<&str>) =
            self.edges.iter().copied().unzip();
        let missing: Option<String> =
            crate::observed_sql::query_scalar(artifact_sql().postgres.acquire_holds.sql())
                .bind(&locks)
                .bind(&check_namespaces)
                .bind(&check_refs)
                .bind(referrer.kind().as_str())
                .bind(referrer.canonical_id())
                .bind(&edge_namespaces)
                .bind(&edge_refs)
                .fetch_one(crate::observed_sql::executor(conn))
                .await
                .map_err(backend)?;
        // The holds seal when they lock anything.
        if !locks.is_empty() {
            crate::replayable::note_sealed();
        }
        match missing {
            Some(artifact_ref) => Err(ArtifactStoreError::ArtifactMissing { artifact_ref }),
            None => Ok(()),
        }
    }
}

#[cfg(any(test, feature = "testing"))]
pub(crate) fn function_ddl() -> String {
    // The referrer key spells `referrer_lock_key`; the fence read is
    // `select_is_fenced`, the missing read `select_missing_manifest`, the
    // edge write `insert_manifest_edges`.
    crate::advisory_lock::with_after_seal(
        r#"CREATE OR REPLACE FUNCTION lash_lock_referrer(p_kind text, p_id text)
RETURNS boolean LANGUAGE plpgsql VOLATILE AS $lash_lock_referrer$
BEGIN
    PERFORM lash_advisory_xact_lock(hashtextextended('lash-artifact-referrer:' || p_kind || ':' || p_id, 0), false);
    -- This statement starts after the lock wait.
    RETURN EXISTS (SELECT 1 FROM lash_referrer_fences
        WHERE referrer_kind = p_kind AND referrer_id = p_id);
END
$lash_lock_referrer$;
CREATE OR REPLACE FUNCTION lash_acquire_holds(
    p_locks text[], p_check_namespaces text[], p_check_refs text[],
    p_kind text, p_id text, p_edge_namespaces text[], p_edge_refs text[]
) RETURNS text LANGUAGE plpgsql VOLATILE AS $lash_acquire_holds$
DECLARE
    v_missing text;
BEGIN
    IF cardinality(p_locks) > 0 THEN
        PERFORM lash_lock_artifacts(p_locks);
    END IF;
    -- These statements start after every lock wait.
    BEGIN
        SELECT wanted.artifact_ref INTO v_missing
        FROM unnest(p_check_namespaces, p_check_refs) WITH ORDINALITY AS wanted(namespace, artifact_ref, ordinal)
        WHERE NOT EXISTS (SELECT 1 FROM lash_lash_vm_artifacts AS stored
            WHERE stored.namespace = wanted.namespace AND stored.artifact_ref = wanted.artifact_ref)
        ORDER BY wanted.ordinal LIMIT 1;
        IF v_missing IS NOT NULL THEN
            RETURN v_missing;
        END IF;
        INSERT INTO lash_artifact_referrer_edges (namespace, artifact_ref, referrer_kind, referrer_id)
            SELECT namespace, artifact_ref, p_kind, p_id
            FROM unnest(p_edge_namespaces, p_edge_refs) AS wanted(namespace, artifact_ref)
            ON CONFLICT DO NOTHING;
        RETURN NULL;
    {AFTER_SEAL}
    END;
END
$lash_acquire_holds$;
CREATE OR REPLACE FUNCTION lash_acquire_env(p_env text, p_kind text, p_id text, p_write boolean)
RETURNS text[] LANGUAGE plpgsql VOLATILE AS $lash_acquire_env$
DECLARE
    v_body text;
BEGIN
    SELECT body_ref INTO v_body FROM lash_lash_vm_artifacts
        WHERE namespace = 'process_execution_env' AND artifact_ref = p_env;
    IF v_body IS NULL THEN
        RETURN ARRAY['absent', NULL];
    END IF;
    -- The closure's key sorts before the head's: one sorted set, sealing.
    PERFORM lash_lock_artifacts(ARRAY['lash-artifact:env_closure:' || v_body,
        'lash-artifact:process_execution_env:' || p_env]);
    -- These statements start after the lock waits.
    BEGIN
        IF NOT EXISTS (SELECT 1 FROM lash_lash_vm_artifacts
            WHERE namespace = 'env_closure' AND artifact_ref = v_body) THEN
            RETURN ARRAY['closure', v_body];
        END IF;
        IF NOT p_write THEN
            RETURN ARRAY['locked', v_body];
        END IF;
        -- This statement starts after the head's lock wait.
        IF NOT EXISTS (SELECT 1 FROM lash_lash_vm_artifacts
            WHERE namespace = 'process_execution_env' AND artifact_ref = p_env) THEN
            RETURN ARRAY['missing', v_body];
        END IF;
        INSERT INTO lash_artifact_referrer_edges (namespace, artifact_ref, referrer_kind, referrer_id)
            VALUES ('process_execution_env', p_env, p_kind, p_id), ('env_closure', v_body, p_kind, p_id)
            ON CONFLICT DO NOTHING;
        RETURN ARRAY['held', v_body];
    {AFTER_SEAL}
    END;
END
$lash_acquire_env$;
"#,
    )
}
