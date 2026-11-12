//! One server call for the SQL share of an end. Separate statements in a
//! volatile function get a fresh snapshot after advisory-lock waits; one
//! data-modifying CTE would miss an edge a concurrent publisher just committed.

use lash_core_store::artifact_referrer::ArtifactCleanup;
use lash_core_store::artifact_referrer::ArtifactReferrer;
use lash_core_store::store::StoreError;
use lash_protocol_types::EffectJournalIdentity;

pub(super) const NAMESPACES: [&str; 5] = [
    super::MODULE_ARTIFACT_NAMESPACE,
    super::PROCESS_ENV_CLOSURE_NAMESPACE,
    super::PROCESS_DEFINITION_NAMESPACE,
    super::PROCESS_ENV_NAMESPACE,
    super::TURN_PRELUDE_NAMESPACE,
];

#[derive(serde::Deserialize)]
struct Answer {
    checks: Vec<(String, String)>,
    plans: Vec<(String, String, String)>,
}

/// Lock, in the one sorted order, every artifact the ends of `ended` and
/// of the frames whose ended cleanups are gated on one of `gates` can
/// delete, with `carries`, before the transaction takes any of them: the
/// referrers' locks, then (a statement after those waits) what their edges
/// in `namespaces` name and hold, transitively, joined with the carried
/// artifacts. Each end the transaction then runs takes locks it already
/// holds, so a publisher's sorted set never waits on a lock that this
/// transaction took out of order (FIG-7700).
pub(crate) async fn lock_ends_tx(
    tx: &mut sqlx::PgConnection,
    ended: &[ArtifactReferrer],
    gates: &[&EffectJournalIdentity],
    carries: &[(&str, String)],
) -> Result<(), StoreError> {
    if ended.is_empty() && carries.is_empty() {
        return Ok(());
    }
    let (kinds, ids): (Vec<&str>, Vec<String>) = ended
        .iter()
        .map(|referrer| (referrer.kind().as_str(), referrer.canonical_id()))
        .unzip();
    let gates: Vec<&str> = gates.iter().map(|gate| gate.key()).collect();
    let (carry_namespaces, carry_refs): (Vec<&str>, Vec<&str>) = carries
        .iter()
        .map(|(namespace, artifact_ref)| (*namespace, artifact_ref.as_str()))
        .unzip();
    crate::observed_sql::query(super::artifact_sql().postgres.lock_ends.sql())
        .bind(&kinds)
        .bind(&ids)
        .bind(&gates)
        .bind(&NAMESPACES[..])
        .bind(&carry_namespaces)
        .bind(&carry_refs)
        .execute(crate::observed_sql::executor(tx))
        .await
        .map_err(failure)?;
    Ok(())
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
    /// The namespaces every local end severs.
    pub(crate) const NAMESPACES: &'static [&'static str] = &NAMESPACES;

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

#[cfg(any(test, feature = "testing"))]
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
    v_locked jsonb;
    v_round jsonb;
    v_checks jsonb;
    v_body jsonb;
    item jsonb;
    held record;
    v_frontier BYTEA[];
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
    -- What the named artifacts hold, transitively, joins their one sorted
    -- lock acquisition: deleting them can free it. A holder's dependency
    -- edges are immutable while it lives (FIG-7700).
    WITH RECURSIVE reach(namespace, artifact_ref) AS (
        SELECT named.value->>0, named.value->>1 FROM jsonb_array_elements(v_artifacts) named
        UNION
        SELECT dep.namespace, dep.artifact_ref FROM lash_artifact_dependency_edges dep
        JOIN reach ON dep.holder_namespace = reach.namespace AND dep.holder_ref = reach.artifact_ref)
    SELECT COALESCE(jsonb_agg(jsonb_build_array(namespace, artifact_ref)
        ORDER BY namespace, artifact_ref), '[]'::jsonb) INTO v_locked FROM reach;
    FOR held IN SELECT value->>0 AS namespace, value->>1 AS artifact_ref
        FROM jsonb_array_elements(v_locked) ORDER BY namespace, artifact_ref LOOP
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
        -- Delete what nothing names or holds, then, round by round, what a
        -- deleted artifact held that nothing keeps now.
        v_frontier := ARRAY[]::BYTEA[];
        v_round := v_artifacts;
        WHILE jsonb_array_length(v_round) > 0 LOOP
            WITH gone AS (
                DELETE FROM lash_lash_vm_artifacts artifact USING jsonb_array_elements(v_round) named
                WHERE artifact.namespace = named.value->>0 AND artifact.artifact_ref = named.value->>1
                    AND NOT EXISTS (SELECT 1 FROM lash_artifact_referrer_edges edge
                        WHERE edge.namespace = artifact.namespace AND edge.artifact_ref = artifact.artifact_ref)
                    AND NOT EXISTS (SELECT 1 FROM lash_artifact_dependency_edges dep
                        WHERE dep.namespace = artifact.namespace AND dep.artifact_ref = artifact.artifact_ref)
                RETURNING artifact.namespace, artifact.artifact_ref, artifact.graph_ref),
            freed AS (
                SELECT DISTINCT dep.namespace, dep.artifact_ref FROM gone
                JOIN lash_artifact_dependency_edges dep ON dep.holder_namespace = gone.namespace
                    AND dep.holder_ref = gone.artifact_ref)
            SELECT v_frontier || ARRAY(SELECT node.hash FROM gone JOIN lash_blobs AS node ON node.id = gone.graph_ref),
                COALESCE((SELECT jsonb_agg(jsonb_build_array(namespace, artifact_ref)) FROM freed), '[]'::jsonb)
                INTO v_frontier, v_round;
        END LOOP;
        -- A bridged artifact's row was its graph node's root: peel it
        -- (FIG-5949).
        {PEEL}
    END IF;
    RETURN jsonb_build_object('checks', v_checks, 'plans', v_plans);
END
$lash_artifact_end_local$;
"#
    .replace("{PEEL}", &crate::durable::retire_graph())
        + LOCK_ENDS_DDL
}

/// [`lock_ends_tx`]'s server function. Its gated frames are the ones an
/// end gated on the same execution severs.
#[cfg(any(test, feature = "testing"))]
const LOCK_ENDS_DDL: &str = r#"CREATE OR REPLACE FUNCTION lash_artifact_lock_ends(
    p_kinds text[], p_ids text[], p_gates text[], p_namespaces text[],
    p_carry_namespaces text[], p_carry_refs text[]
) RETURNS void LANGUAGE plpgsql VOLATILE AS $lash_artifact_lock_ends$
DECLARE
    v_refs jsonb;
    held record;
BEGIN
    SELECT COALESCE(jsonb_agg(jsonb_build_array(ended.kind, ended.id)), '[]'::jsonb) INTO v_refs
        FROM unnest(p_kinds, p_ids) AS ended(kind, id);
    SELECT v_refs || COALESCE(jsonb_agg(jsonb_build_array(gated.referrer_kind, gated.referrer_id)), '[]'::jsonb)
        INTO v_refs
        FROM (SELECT referrer_kind, referrer_id, gate_execution, cleanup_json::jsonb AS body
            FROM lash_artifact_cleanup_obligations
            WHERE referrer_kind = 'frame_environment' AND gate_execution = ANY(p_gates)) gated
        WHERE gated.body->>'plan' = 'ended' AND gated.body->>'gate' = gated.gate_execution
            AND gated.body->'carries' = '[]'::jsonb;
    FOR held IN SELECT DISTINCT value->>0 AS kind, value->>1 AS id
        FROM jsonb_array_elements(v_refs) ORDER BY kind, id LOOP
        PERFORM pg_advisory_xact_lock(hashtextextended('lash-artifact-referrer:' || held.kind || ':' || held.id, 0));
    END LOOP;
    -- This statement starts after the referrer lock waits.
    FOR held IN WITH RECURSIVE reach(namespace, artifact_ref) AS (
            SELECT edge.namespace, edge.artifact_ref FROM lash_artifact_referrer_edges edge
            JOIN jsonb_array_elements(v_refs) ref ON edge.referrer_kind = ref.value->>0
                AND edge.referrer_id = ref.value->>1
            WHERE edge.namespace = ANY(p_namespaces)
            UNION
            SELECT dep.namespace, dep.artifact_ref FROM lash_artifact_dependency_edges dep
            JOIN reach ON dep.holder_namespace = reach.namespace AND dep.holder_ref = reach.artifact_ref)
        SELECT namespace, artifact_ref FROM reach
        UNION
        SELECT carried.namespace, carried.artifact_ref
        FROM unnest(p_carry_namespaces, p_carry_refs) AS carried(namespace, artifact_ref)
        ORDER BY namespace, artifact_ref LOOP
        PERFORM pg_advisory_xact_lock(hashtextextended('lash-artifact:' || held.namespace || ':' || held.artifact_ref, 0));
    END LOOP;
END
$lash_artifact_lock_ends$;
"#;
