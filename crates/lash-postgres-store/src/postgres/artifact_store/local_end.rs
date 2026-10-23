//! One server call for the SQL share of an end. Separate statements in a
//! volatile function get a fresh snapshot after advisory-lock waits; one
//! data-modifying CTE would miss an edge a concurrent publisher just committed.

use lash_core_store::artifact_referrer::ArtifactCleanup;
use lash_core_store::artifact_referrer::ArtifactReferrer;
use lash_core_store::store::StoreError;
use lash_protocol_types::EffectJournalIdentity;

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
    let answer: serde_json::Value =
        crate::observed_sql::query_scalar(super::artifact_sql().postgres.end_local.sql())
            .bind(referrer.kind().as_str())
            .bind(referrer.canonical_id())
            .bind(gate.map(EffectJournalIdentity::key))
            .bind(crate::support::clamp_epoch_ms(now_ms))
            .bind(namespaces)
            .fetch_one(crate::observed_sql::executor(tx))
            .await
            .map_err(failure)?;
    validate(answer)
}

/// The arguments of an end of `referrer` gated on `gate` run as a step:
/// the end call's five, then the canonical body of a frame cleanup gated on
/// `gate`, which a one-call commit vouches each answered plan against.
pub(crate) struct EndArguments {
    pub(crate) kind: &'static str,
    pub(crate) id: String,
    pub(crate) gate: Option<String>,
    pub(crate) namespaces: &'static [&'static str],
    pub(crate) gated_body: Option<String>,
}

impl EndArguments {
    pub(crate) fn of(
        referrer: &ArtifactReferrer,
        gate: Option<&EffectJournalIdentity>,
    ) -> Result<Self, StoreError> {
        let gated_body = gate
            .map(|gate| {
                ArtifactCleanup::ended(referrer.clone(), Vec::new(), Some(gate.clone()))
                    .to_json()
                    .map_err(|error| StoreError::Backend(error.to_string()))
            })
            .transpose()?;
        Ok(Self {
            kind: referrer.kind().as_str(),
            id: referrer.canonical_id(),
            gate: gate.map(|gate| gate.key().to_owned()),
            namespaces: &NAMESPACES,
            gated_body,
        })
    }
}

/// A failed end call, as the store reports it: a stored cleanup body that
/// is not JSON is corrupt.
pub(crate) fn failure(error: sqlx::Error) -> StoreError {
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
}

/// Refuse malformed stored pairs and plans of an end's `answer` before the
/// owner's transaction can commit, including surviving readers that
/// blocked reclamation.
pub(crate) fn validate(answer: serde_json::Value) -> Result<(), StoreError> {
    let answer: Answer =
        serde_json::from_value(answer).map_err(|error| StoreError::StoredDataCorrupt {
            record_kind: "artifact local end",
            message: error.to_string(),
        })?;
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
    // cleanup body changes; the typed gate key replaces the text pattern.
    // Terminals and eligible end hooks call the same helper.
    r#"CREATE OR REPLACE FUNCTION lash_artifact_end_local(
    p_kind text, p_id text, p_gate text, p_now bigint, p_namespaces text[]
) RETURNS jsonb LANGUAGE plpgsql VOLATILE AS $lash_artifact_end_local$
DECLARE
    v_refs jsonb := jsonb_build_array(jsonb_build_array(p_kind, p_id));
    v_plans jsonb := '[]'::jsonb;
    v_artifacts jsonb;
    v_checks jsonb;
    v_body jsonb;
    item jsonb;
    held record;
    v_frontier BYTEA[];
    v_locked BYTEA[];
    v_rows BIGINT;
BEGIN
    IF p_gate IS NOT NULL THEN
        SELECT COALESCE(jsonb_agg(jsonb_build_array(referrer_kind, referrer_id, cleanup_json)
            ORDER BY referrer_id), '[]'::jsonb) INTO v_plans
        FROM lash_artifact_cleanup_obligations
        WHERE referrer_kind = 'frame_environment' AND gate_execution = p_gate;
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
        WITH gone AS (
            DELETE FROM lash_lash_vm_artifacts artifact USING jsonb_array_elements(v_artifacts) named
            WHERE artifact.namespace = named.value->>0 AND artifact.artifact_ref = named.value->>1
                AND NOT EXISTS (SELECT 1 FROM lash_artifact_referrer_edges edge
                    WHERE edge.namespace = artifact.namespace AND edge.artifact_ref = artifact.artifact_ref)
            RETURNING artifact.graph_ref)
        SELECT ARRAY(SELECT graph_ref FROM gone WHERE graph_ref IS NOT NULL) INTO v_frontier;
        -- A bridged artifact's row was its graph node's root: peel it
        -- (FIG-5949).
        {PEEL}
    END IF;
    RETURN jsonb_build_object('checks', v_checks, 'plans', v_plans);
END
$lash_artifact_end_local$;
"#
    .replace("{PEEL}", &crate::durable::peel_loop())
}
