//! One server call for the SQL share of an end. Separate statements in a
//! volatile function get a fresh snapshot after advisory-lock waits; one
//! data-modifying CTE would miss an edge a concurrent publisher just committed.

use std::collections::BTreeSet;

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

/// One transaction's lock plan (FIG-7700).
///
/// **The class order.** Every PostgreSQL transaction takes its waiting
/// locks in this order of classes, all of one class before any of the
/// next, sorted inside a class:
/// 1. session actor rows;
/// 2. session write locks (`PostgresDurableStore::lock_session_writes`, in
///    actor order);
/// 3. scope locks, as registration admission takes them: each closing
///    scope in ledger-key order, then each consumer hold
///    ([`ScopeLock`]);
/// 4. referrer locks of every kind (execution, process, package, pin,
///    frame, session);
/// 5. the artifact locks, once, last.
///
/// A transaction that needs more than one class takes classes 3 to 5
/// through this plan, after the caller took classes 1 and 2. The artifact
/// locks are every artifact any end, carry or environment acquisition the
/// transaction runs can lock: what the ended referrers' edges in
/// [`NAMESPACES`] name and hold, transitively, joined with the carried
/// artifacts and each acquired environment's head, closure and the
/// closure's modules. They are taken in one sorted acquisition, which
/// seals the transaction ([`crate::advisory_lock`]). Each admission, end,
/// carry or acquisition the transaction runs afterwards takes locks it
/// already holds. A lock the plan missed is refused as contention, never
/// waited for.
#[derive(Default)]
pub(crate) struct LockPlan {
    /// Referrers whose ends the transaction runs.
    pub(crate) ended: Vec<ArtifactReferrer>,
    /// Executions whose ended frame cleanups the transaction's ends run.
    pub(crate) gates: Vec<EffectJournalIdentity>,
    /// Referrers the transaction locks without ending them.
    pub(crate) held: Vec<ArtifactReferrer>,
    /// Sessions whose stored leaf frame the transaction ends, read under
    /// each session's write authority, which the caller takes first
    /// (`PostgresDurableStore::lock_session_writes`, in actor order).
    pub(crate) sessions: Vec<lash_kernel_value::identity::SessionId>,
    /// Sessions whose write authority a later step takes without the plan
    /// reading their leaf (a head that opens no frame): the caller takes
    /// it before the plan with the `sessions`', and the plan statement
    /// never sees them.
    pub(crate) writers: Vec<lash_kernel_value::identity::SessionId>,
    /// Artifacts the transaction carries.
    pub(crate) carries: Vec<(&'static str, String)>,
    /// Environment heads the transaction acquires.
    pub(crate) envs: Vec<String>,
    /// Admission locks, in the order the plan takes them.
    pub(crate) scopes: Vec<ScopeLock>,
    /// Executions whose stored referrer the transaction locks without
    /// ending it (a seed's), which the durable plan statement reads.
    pub(crate) executions: Vec<String>,
}

/// One admission lock of the plan's scope class.
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum ScopeLock {
    /// A turn's run rows (`parent_end::lock_parent_scope_tx`).
    Turn { session: String, turn: String },
    /// An advisory lock by its text key: a scope's or a consumer hold's.
    Key(String),
}

/// What a taken [`LockPlan`] covers: the referrers it ended, its stored
/// leaf frames among them, and the artifacts it carried.
#[derive(Default)]
pub(crate) struct Planned {
    ended: BTreeSet<(String, String)>,
    carries: BTreeSet<(String, String)>,
}

impl Planned {
    /// Whether the plan locked what `referrer`'s end can delete.
    pub(crate) fn ends(&self, referrer: &ArtifactReferrer) -> bool {
        self.ended
            .contains(&(referrer.kind().as_str().to_owned(), referrer.canonical_id()))
    }

    /// Whether the plan locked carried artifact `artifact_ref` of
    /// `namespace`.
    pub(crate) fn carries(&self, namespace: &str, artifact_ref: &str) -> bool {
        self.carries
            .contains(&(namespace.to_owned(), artifact_ref.to_owned()))
    }
}

impl LockPlan {
    /// Whether the transaction needs no plan: it locks no referrer and no
    /// artifact. Scope locks alone are one class, which their own paths
    /// take in order.
    pub(crate) fn is_empty(&self) -> bool {
        self.ended.is_empty()
            && self.held.is_empty()
            && self.sessions.is_empty()
            && self.carries.is_empty()
            && self.envs.is_empty()
            && self.executions.is_empty()
    }

    /// The plan's arguments, in the plan statement's order: the server
    /// function's fourteen, then the executions whose stored referrers the
    /// statement adds to the held ones.
    pub(crate) fn arguments(&self) -> [Vec<String>; 15] {
        let kinds = |referrers: &[ArtifactReferrer]| {
            referrers
                .iter()
                .map(|referrer| referrer.kind().as_str().to_owned())
                .collect::<Vec<_>>()
        };
        let ids = |referrers: &[ArtifactReferrer]| {
            referrers
                .iter()
                .map(ArtifactReferrer::canonical_id)
                .collect::<Vec<_>>()
        };
        [
            kinds(&self.ended),
            ids(&self.ended),
            self.gates
                .iter()
                .map(|gate| gate.key().to_owned())
                .collect(),
            NAMESPACES
                .iter()
                .map(|namespace| (*namespace).to_owned())
                .collect(),
            self.carries
                .iter()
                .map(|(namespace, _)| (*namespace).to_owned())
                .collect(),
            self.carries
                .iter()
                .map(|(_, artifact_ref)| artifact_ref.clone())
                .collect(),
            kinds(&self.held),
            ids(&self.held),
            self.sessions
                .iter()
                .map(|session| session.as_str().to_owned())
                .collect(),
            self.envs.clone(),
            Vec::new(),
            self.scopes
                .iter()
                .map(|scope| match scope {
                    ScopeLock::Turn { .. } => "turn".to_owned(),
                    ScopeLock::Key(_) => "key".to_owned(),
                })
                .collect(),
            self.scopes
                .iter()
                .map(|scope| match scope {
                    ScopeLock::Turn { session, .. } => session.clone(),
                    ScopeLock::Key(key) => key.clone(),
                })
                .collect(),
            self.scopes
                .iter()
                .map(|scope| match scope {
                    ScopeLock::Turn { turn, .. } => turn.clone(),
                    ScopeLock::Key(_) => String::new(),
                })
                .collect(),
            self.executions.clone(),
        ]
    }
}

#[derive(serde::Deserialize)]
struct PlanAnswer {
    ended: Vec<(String, String)>,
    missing: Vec<String>,
}

/// Take `plan`'s locks in `tx`. An acquired environment whose closure is
/// not stored is acquired by storing it from the head's modules, which the
/// client reads: the server answers those heads before it takes any
/// artifact lock, and the plan is taken again with their modules.
pub(crate) async fn lock_plan_tx(
    tx: &mut sqlx::PgConnection,
    plan: &LockPlan,
) -> Result<Planned, StoreError> {
    if plan.is_empty() {
        return Ok(Planned::default());
    }
    let mut arguments = plan.arguments();
    let mut answer = call_plan(tx, &arguments).await?;
    if !answer.missing.is_empty() {
        for env in std::mem::take(&mut answer.missing) {
            let bytes: Option<Vec<u8>> = crate::observed_sql::query_scalar(
                super::artifact_sql().lash_vm_artifacts.select_bytes.sql(),
            )
            .bind(super::PROCESS_ENV_NAMESPACE)
            .bind(&env)
            .fetch_optional(crate::observed_sql::executor(&mut *tx))
            .await
            .map_err(crate::support::store_sqlx_error)?
            .flatten();
            // A head whose modules do not read is refused by its
            // acquisition, which then locks only its head.
            if let Some(bytes) = bytes
                && let Ok(modules) = super::env_storage::process_env_closure_tx(tx, &bytes).await
            {
                for module in modules {
                    arguments[4].push(super::MODULE_ARTIFACT_NAMESPACE.to_owned());
                    arguments[5].push(module);
                }
            }
            arguments[10].push(env);
        }
        answer = call_plan(tx, &arguments).await?;
    }
    if !answer.missing.is_empty() {
        return Err(StoreError::Backend(
            "the artifact lock plan asked twice for an environment's modules".to_owned(),
        ));
    }
    Ok(Planned {
        ended: answer.ended.into_iter().collect(),
        carries: arguments[4]
            .iter()
            .cloned()
            .zip(arguments[5].iter().cloned())
            .collect(),
    })
}

async fn call_plan(
    tx: &mut sqlx::PgConnection,
    arguments: &[Vec<String>; 15],
) -> Result<PlanAnswer, StoreError> {
    let mut query = crate::observed_sql::query_scalar(crate::durable::lock_plan_sql());
    for argument in arguments {
        query = query.bind(argument);
    }
    let answer: serde_json::Value = query
        .fetch_one(crate::observed_sql::executor(tx))
        .await
        .map_err(failure)?;
    serde_json::from_value(answer).map_err(|error| StoreError::StoredDataCorrupt {
        record_kind: "artifact lock plan",
        message: error.to_string(),
    })
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
        PERFORM lash_advisory_xact_lock(hashtextextended('lash-artifact-referrer:' || held.kind || ':' || held.id, 0), false);
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
    PERFORM lash_lock_artifacts(ARRAY(SELECT 'lash-artifact:' || (value->>0) || ':' || (value->>1)
        FROM jsonb_array_elements(v_locked) ORDER BY value->>0, value->>1));
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
        + &LOCK_PLAN_DDL.replace(
            "{LOCK_TURN}",
            &crate::process_sql::process_sql()
                .plan_postgres
                .lock_turn
                .sql()
                .replace("$1", "scope.a")
                .replace("$2", "scope.b"),
        )
}

/// [`lock_plan_tx`]'s server function. Its gated frames are the ones an
/// end gated on the same execution severs; a session's leaf frame is read
/// under the session's write authority, which every head writer takes.
#[cfg(any(test, feature = "testing"))]
const LOCK_PLAN_DDL: &str = r#"CREATE OR REPLACE FUNCTION lash_artifact_lock_plan(
    p_kinds text[], p_ids text[], p_gates text[], p_namespaces text[],
    p_carry_namespaces text[], p_carry_refs text[], p_held_kinds text[], p_held_ids text[],
    p_sessions text[], p_envs text[], p_supplied text[],
    p_scope_kinds text[], p_scope_a text[], p_scope_b text[]
) RETURNS jsonb LANGUAGE plpgsql VOLATILE AS $lash_artifact_lock_plan$
DECLARE
    v_refs jsonb;
    v_held jsonb;
    v_envs jsonb;
    v_missing jsonb;
    held record;
    scope record;
BEGIN
    -- The scope class, in the caller's order: registration admission's.
    FOR scope IN SELECT kind, a, b FROM unnest(p_scope_kinds, p_scope_a, p_scope_b)
        WITH ORDINALITY AS named(kind, a, b, ordinal) ORDER BY ordinal LOOP
        IF scope.kind = 'turn' THEN
            PERFORM 1 FROM ({LOCK_TURN}) AS turn_rows;
        ELSE
            PERFORM lash_advisory_xact_lock(hashtextextended(scope.a, 0), false);
        END IF;
    END LOOP;
    -- The caller holds each session's write authority: its leaf frame is
    -- the one its head names until this transaction commits.
    SELECT COALESCE(jsonb_agg(jsonb_build_array(ended.kind, ended.id)), '[]'::jsonb) INTO v_refs
        FROM unnest(p_kinds, p_ids) AS ended(kind, id);
    SELECT v_refs || COALESCE(jsonb_agg(jsonb_build_array('frame_environment',
            '[' || to_json(head.session_id)::TEXT || ',' || to_json(leaf.frame_node_id)::TEXT || ']')), '[]'::jsonb)
        INTO v_refs
        FROM lash_session_head AS head JOIN lash_session_revisions AS revision
            ON revision.session_id = head.session_id AND revision.head_revision = head.head_revision
        JOIN lash_graph_nodes AS leaf ON leaf.node_id = revision.leaf_node_id
        WHERE head.session_id = ANY(p_sessions) AND leaf.frame_node_id IS NOT NULL;
    SELECT v_refs || COALESCE(jsonb_agg(jsonb_build_array(gated.referrer_kind, gated.referrer_id)), '[]'::jsonb)
        INTO v_refs
        FROM (SELECT referrer_kind, referrer_id, gate_execution, cleanup_json::jsonb AS body
            FROM lash_artifact_cleanup_obligations
            WHERE referrer_kind = 'frame_environment' AND gate_execution = ANY(p_gates)) gated
        WHERE gated.body->>'plan' = 'ended' AND gated.body->>'gate' = gated.gate_execution
            AND gated.body->'carries' = '[]'::jsonb;
    SELECT COALESCE(jsonb_agg(jsonb_build_array(named.kind, named.id)), '[]'::jsonb) INTO v_held
        FROM unnest(p_held_kinds, p_held_ids) AS named(kind, id);
    FOR held IN SELECT DISTINCT value->>0 AS kind, value->>1 AS id
        FROM jsonb_array_elements(v_refs || v_held) ORDER BY kind, id LOOP
        PERFORM lash_advisory_xact_lock(hashtextextended('lash-artifact-referrer:' || held.kind || ':' || held.id, 0), false);
    END LOOP;
    -- This statement starts after the referrer lock waits: each acquired
    -- environment's head, the body it names and whether that body's
    -- closure is stored.
    SELECT COALESCE(jsonb_agg(jsonb_build_array(env.ref, stored.body_ref,
            EXISTS (SELECT 1 FROM lash_lash_vm_artifacts closure
                WHERE closure.namespace = 'env_closure' AND closure.artifact_ref = stored.body_ref))), '[]'::jsonb)
        INTO v_envs
        FROM (SELECT DISTINCT ref FROM unnest(p_envs) AS named(ref)) env
        LEFT JOIN lash_lash_vm_artifacts stored
            ON stored.namespace = 'process_execution_env' AND stored.artifact_ref = env.ref;
    -- A closure acquisition stores is the head's modules, which only the
    -- client reads: ask for them before any artifact lock.
    SELECT COALESCE(jsonb_agg(planned.value->>0), '[]'::jsonb) INTO v_missing
        FROM jsonb_array_elements(v_envs) planned
        WHERE planned.value->>1 IS NOT NULL AND NOT (planned.value->>2)::boolean
            AND NOT (planned.value->>0 = ANY(p_supplied));
    IF jsonb_array_length(v_missing) > 0 THEN
        RETURN jsonb_build_object('ended', v_refs, 'missing', v_missing);
    END IF;
    -- The artifact class, once, last: this seals the transaction.
    PERFORM lash_lock_artifacts(ARRAY(WITH RECURSIVE reach(namespace, artifact_ref) AS (
            SELECT edge.namespace, edge.artifact_ref FROM lash_artifact_referrer_edges edge
            JOIN jsonb_array_elements(v_refs) ref ON edge.referrer_kind = ref.value->>0
                AND edge.referrer_id = ref.value->>1
            WHERE edge.namespace = ANY(p_namespaces)
            UNION
            SELECT 'process_execution_env', planned.value->>0 FROM jsonb_array_elements(v_envs) planned
            WHERE planned.value->>1 IS NOT NULL
            UNION
            SELECT 'env_closure', planned.value->>1 FROM jsonb_array_elements(v_envs) planned
            WHERE planned.value->>1 IS NOT NULL
            UNION
            SELECT dep.namespace, dep.artifact_ref FROM lash_artifact_dependency_edges dep
            JOIN reach ON dep.holder_namespace = reach.namespace AND dep.holder_ref = reach.artifact_ref)
        SELECT 'lash-artifact:' || planned.namespace || ':' || planned.artifact_ref
        FROM (SELECT namespace, artifact_ref FROM reach
            UNION
            SELECT carried.namespace, carried.artifact_ref
            FROM unnest(p_carry_namespaces, p_carry_refs) AS carried(namespace, artifact_ref)) planned
        ORDER BY planned.namespace, planned.artifact_ref));
    -- This statement starts after every artifact lock wait. An environment
    -- whose head or closure moved before its locks were taken would lock
    -- out of this plan: the transaction retries as contended.
    IF EXISTS (SELECT 1 FROM jsonb_array_elements(v_envs) planned
        LEFT JOIN lash_lash_vm_artifacts stored
            ON stored.namespace = 'process_execution_env' AND stored.artifact_ref = planned.value->>0
        WHERE stored.body_ref IS DISTINCT FROM planned.value->>1
            OR EXISTS (SELECT 1 FROM lash_lash_vm_artifacts closure
                WHERE closure.namespace = 'env_closure' AND closure.artifact_ref = stored.body_ref)
                IS DISTINCT FROM (planned.value->>2)::boolean) THEN
        RAISE EXCEPTION 'an acquired environment moved under its artifact lock plan'
            USING ERRCODE = '40001';
    END IF;
    RETURN jsonb_build_object('ended', v_refs, 'missing', '[]'::jsonb);
END
$lash_artifact_lock_plan$;
"#;
