//! One server call for the SQL share of an end. Separate statements in a
//! volatile function get a fresh snapshot after advisory-lock waits; one
//! data-modifying CTE would miss an edge a concurrent publisher just committed.

use lash_core_execution::{ArtifactCleanup, ArtifactReferrer, StoreError};
use lash_sansio::EffectJournalIdentity;

pub(super) const NAMESPACES: [&str; 4] = [
    super::MODULE_ARTIFACT_NAMESPACE,
    super::PROCESS_DEFINITION_NAMESPACE,
    super::PROCESS_ENV_NAMESPACE,
    super::TURN_PRELUDE_NAMESPACE,
];

#[derive(serde::Deserialize)]
struct Answer {
    checks: Vec<(String, String)>,
    plans: Vec<(String, String, String)>,
}

pub(super) async fn end(
    tx: &mut sqlx::PgConnection,
    referrer: &ArtifactReferrer,
    gate: Option<&EffectJournalIdentity>,
    now_ms: u64,
    namespaces: &[&str],
) -> Result<(), StoreError> {
    let pattern = gate
        .map(|journal| {
            serde_json::to_string(journal.key())
                .map(|encoded| {
                    format!(
                        "%\"gate\":{}%",
                        encoded
                            .replace('\\', "\\\\")
                            .replace('%', "\\%")
                            .replace('_', "\\_")
                    )
                })
                .map_err(|error| StoreError::Backend(error.to_string()))
        })
        .transpose()?;
    let answer: serde_json::Value =
        crate::observed_sql::query_scalar(super::artifact_sql().postgres.end_local.sql())
            .bind(referrer.kind().as_str())
            .bind(referrer.canonical_id())
            .bind(gate.map(EffectJournalIdentity::key))
            .bind(pattern)
            .bind(crate::support::clamp_epoch_ms(now_ms))
            .bind(namespaces)
            .fetch_one(crate::observed_sql::executor(tx))
            .await
            .map_err(|error| {
                if error
                    .as_database_error()
                    .is_some_and(|error| error.code().as_deref() == Some("22P02"))
                {
                    StoreError::StoredDataCorrupt {
                        record_kind: "artifact_cleanup_obligation",
                        message: error.to_string(),
                    }
                } else {
                    crate::support::store_sqlx_error(error)
                }
            })?;
    let answer: Answer =
        serde_json::from_value(answer).map_err(|error| StoreError::StoredDataCorrupt {
            record_kind: "artifact local end",
            message: error.to_string(),
        })?;
    // Refuse malformed stored pairs and plans before the owner's transaction
    // can commit, including surviving readers that blocked reclamation.
    for (kind, id) in answer.checks {
        ArtifactReferrer::decode(&kind, &id)
            .map_err(|error| error.into_store_error("artifact referrer edge"))?;
    }
    for (kind, id, body) in answer.plans {
        let referrer = ArtifactReferrer::decode(&kind, &id)
            .map_err(|error| error.into_store_error("artifact cleanup"))?;
        ArtifactCleanup::from_json(&body, &referrer)?;
    }
    Ok(())
}

#[cfg(test)]
pub(crate) fn function_ddl() -> String {
    // Static SQL, generated together with the owner function. No stored
    // record or owner-function argument changes; the existing transaction
    // calls this helper for terminals and eligible end hooks.
    r#"CREATE OR REPLACE FUNCTION lash_artifact_end_local(
    p_kind text, p_id text, p_gate text, p_pattern text, p_now bigint, p_namespaces text[]
) RETURNS jsonb LANGUAGE plpgsql VOLATILE AS $lash_artifact_end_local$
DECLARE
    v_refs jsonb := jsonb_build_array(jsonb_build_array(p_kind, p_id));
    v_plans jsonb := '[]'::jsonb;
    v_artifacts jsonb;
    v_checks jsonb;
    v_body jsonb;
    item jsonb;
    held record;
BEGIN
    IF p_gate IS NOT NULL THEN
        SELECT COALESCE(jsonb_agg(jsonb_build_array(referrer_kind, referrer_id, cleanup_json)
            ORDER BY referrer_id), '[]'::jsonb) INTO v_plans
        FROM lash_artifact_cleanup_obligations
        WHERE referrer_kind = 'frame_environment' AND cleanup_json LIKE p_pattern ESCAPE '\';
        FOR item IN SELECT value FROM jsonb_array_elements(v_plans) LOOP
            v_body := (item->>2)::jsonb;
            IF v_body->>'plan' = 'ended' AND v_body->>'gate' = p_gate
                AND v_body->'carries' = '[]'::jsonb THEN
                v_refs := v_refs || jsonb_build_array(jsonb_build_array(item->>0, item->>1));
            END IF;
        END LOOP;
    END IF;
    FOR held IN SELECT DISTINCT value->>0 AS kind, value->>1 AS id
        FROM jsonb_array_elements(v_refs) ORDER BY kind, id LOOP
        PERFORM pg_advisory_xact_lock(hashtextextended('lash-artifact-referrer:' || held.kind || ':' || held.id, 0));
    END LOOP;
    SELECT COALESCE(jsonb_agg(jsonb_build_array(namespace, artifact_ref)
        ORDER BY namespace, artifact_ref), '[]'::jsonb) INTO v_artifacts
    FROM (SELECT DISTINCT edge.namespace, edge.artifact_ref
        FROM lash_artifact_referrer_edges edge
        JOIN jsonb_array_elements(v_refs) ref ON edge.referrer_kind = ref.value->>0
            AND edge.referrer_id = ref.value->>1
        WHERE edge.namespace = ANY(p_namespaces)) named;
    FOR held IN SELECT value->>0 AS namespace, value->>1 AS artifact_ref
        FROM jsonb_array_elements(v_artifacts) ORDER BY namespace, artifact_ref LOOP
        PERFORM pg_advisory_xact_lock(hashtextextended('lash-artifact:' || held.namespace || ':' || held.artifact_ref, 0));
    END LOOP;
    -- This statement starts after every lock wait. Include all remaining
    -- readers in the answer so the client validates their canonical pairs.
    SELECT v_refs || COALESCE(jsonb_agg(jsonb_build_array(edge.referrer_kind, edge.referrer_id)), '[]'::jsonb)
        INTO v_checks FROM lash_artifact_referrer_edges edge
        JOIN jsonb_array_elements(v_artifacts) named ON edge.namespace = named.value->>0
            AND edge.artifact_ref = named.value->>1;
    INSERT INTO lash_referrer_fences (referrer_kind, referrer_id, ended_at_ms)
        SELECT DISTINCT value->>0, value->>1, p_now FROM jsonb_array_elements(v_refs)
        ON CONFLICT DO NOTHING;
    IF jsonb_array_length(v_artifacts) > 0 THEN
        DELETE FROM lash_artifact_referrer_edges edge USING jsonb_array_elements(v_refs) ref
            WHERE edge.referrer_kind = ref.value->>0 AND edge.referrer_id = ref.value->>1
                AND edge.namespace = ANY(p_namespaces);
        DELETE FROM lash_lash_vm_artifacts artifact USING jsonb_array_elements(v_artifacts) named
            WHERE artifact.namespace = named.value->>0 AND artifact.artifact_ref = named.value->>1
                AND NOT EXISTS (SELECT 1 FROM lash_artifact_referrer_edges edge
                    WHERE edge.namespace = artifact.namespace AND edge.artifact_ref = artifact.artifact_ref);
    END IF;
    RETURN jsonb_build_object('checks', v_checks, 'plans', v_plans);
END
$lash_artifact_end_local$;
"#.to_owned()
}
