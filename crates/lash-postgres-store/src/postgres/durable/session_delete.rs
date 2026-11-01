//! A session's physical deletion on PostgreSQL as one step program
//! (FIG-7576): the close's artifacts step and the catalog's delete run the
//! same statements, the first in the owner's one call (or its explicit
//! form), the second in its guarded maintenance transaction.
//!
//! Nothing is decoded on the client. The frame a materialized session
//! leaves is the frame its head's leaf row names; its referrer locks, its
//! fence and its `Ended` cleanup are written from that column. The cleanup
//! upsert rule (a guard inserts only where no row is; `Ended` replaces a
//! guard; nothing replaces `Ended`) is decided by the upsert itself over the
//! stored plan's tag, and only an obligation it wrote ends the frame's local
//! edges. The roots the session's rows held are retired by the rows the
//! deletes return, the ancestry its revisions published is retired after
//! them, its prompt texts are peeled, and its terminal record is staged on
//! the turn feed.

use std::sync::LazyLock;

use lash_core_store::artifact_referrer::{
    ArtifactCleanup, ArtifactReferrer, FrameEnvironmentId, ReferrerGuard,
};
use lash_core_store::store::ObligationKey;
use lash_durable::domain::{ExecKey, OwnerKey, ScopeKey};
use lash_durable::{ActorKey, DurableError, DurableInstant};
use lash_kernel_value::identity::SessionId;
use lash_store_sql::Dialect;

use super::steps::{Collect, Entry, Shape, Slots, Step, Ty, Value};
use super::{corrupt, environment, graph, head_commit, model_calls, prompts, run_records};
use super::{snapshots, tool_calls, waits};
use crate::session_sql::session_sql;

lash_store_sql::statements! {
    /// The deletion statements only PostgreSQL's step program issues.
    struct SessionDeleteStatements @ "durable_session_delete_postgres" {
        /// Lock session `?1`'s referrers in key order: the frame its head's
        /// leaf names, then the session.
        lock_referrers = "SELECT pg_advisory_xact_lock(hashtextextended(ordered.key, 0))
             FROM (
                 SELECT named.key FROM (
                     SELECT 'lash-artifact-referrer:session:' || CAST(?1 AS TEXT) AS key
                     UNION ALL
                     SELECT 'lash-artifact-referrer:frame_environment:[' || to_json(CAST(?1 AS TEXT))::TEXT
                            || ',' || to_json(leaf.frame_node_id)::TEXT || ']'
                     FROM session_head AS head JOIN session_revisions AS revision
                         ON revision.session_id = head.session_id AND revision.head_revision = head.head_revision
                     JOIN graph_nodes AS leaf ON leaf.node_id = revision.leaf_node_id
                     WHERE head.session_id = ?1
                 ) AS named
                 ORDER BY named.key COLLATE \"C\"
             ) AS ordered";

        /// Fence the frame session `?1`'s head leaves, ended at `?2`.
        fence_frame = "INSERT INTO referrer_fences (referrer_kind, referrer_id, ended_at_ms)
             SELECT 'frame_environment', '[' || to_json(CAST(?1 AS TEXT))::TEXT
                    || ',' || to_json(leaf.frame_node_id)::TEXT || ']', ?2
             FROM session_head AS head JOIN session_revisions AS revision
                 ON revision.session_id = head.session_id AND revision.head_revision = head.head_revision
             JOIN graph_nodes AS leaf ON leaf.node_id = revision.leaf_node_id
             WHERE head.session_id = ?1
             ON CONFLICT DO NOTHING";

        /// Arm the `Ended` cleanup `?2` of the frame session `?1`'s head
        /// leaves, due at `?4`, as obligation `?3` followed by the frame's
        /// length-prefixed id: inserted where the frame has no row, or
        /// replacing a guard, never another `Ended`. Answers the row it
        /// wrote.
        arm_frame_ended = "INSERT INTO artifact_cleanup_obligations AS stored
                 (referrer_kind, referrer_id, cleanup_json, obligation_id, obligation_state,
                  obligation_due_at_ms, gate_execution)
             SELECT 'frame_environment', frame.id, ?2,
                    CAST(?3 AS TEXT) || octet_length(frame.id)::TEXT || ':' || frame.id, 'due', ?4, NULL
             FROM (
                 SELECT '[' || to_json(CAST(?1 AS TEXT))::TEXT || ',' || to_json(leaf.frame_node_id)::TEXT || ']' AS id
                 FROM session_head AS head JOIN session_revisions AS revision
                     ON revision.session_id = head.session_id AND revision.head_revision = head.head_revision
                 JOIN graph_nodes AS leaf ON leaf.node_id = revision.leaf_node_id
                 WHERE head.session_id = ?1
             ) AS frame
             ON CONFLICT (referrer_kind, referrer_id) DO UPDATE
             SET cleanup_json = excluded.cleanup_json, gate_execution = NULL,
                 obligation_state = 'due', obligation_attempts = 0,
                 obligation_due_at_ms = excluded.obligation_due_at_ms, obligation_claim_token = NULL,
                 obligation_stall_reason = NULL, obligation_last_error = NULL,
                 obligation_last_error_code = NULL, obligation_settled_at_ms = NULL
             WHERE (stored.cleanup_json::jsonb ->> 'plan') IS DISTINCT FROM 'ended'
             RETURNING stored.referrer_id";

        /// End the local edges of kind `?1` of the frame session `?2`'s
        /// head leaves, ungated (`?3`), at `?4` in namespaces `?5`.
        end_frame_local = "SELECT lash_artifact_end_local(?1,
                 '[' || to_json(CAST(?2 AS TEXT))::TEXT || ',' || to_json(leaf.frame_node_id)::TEXT || ']',
                 ?3, ?4, ?5)
             FROM session_head AS head JOIN session_revisions AS revision
                 ON revision.session_id = head.session_id AND revision.head_revision = head.head_revision
             JOIN graph_nodes AS leaf ON leaf.node_id = revision.leaf_node_id
             WHERE head.session_id = ?2";

        /// Every retained revision of session `?1`, answering the leaf it
        /// published and the checkpoint and config roots it held.
        delete_revisions = "DELETE FROM session_revisions WHERE session_id = ?1
             RETURNING leaf_node_id,
                 (SELECT hash FROM blobs WHERE id = checkpoint_ref) AS checkpoint_ref,
                 (SELECT hash FROM blobs WHERE id = config_ref) AS config_ref";

        /// Every run of session `?1`, answering the transcript it held.
        delete_runs = "DELETE FROM session_runs WHERE session_id = ?1
             RETURNING (SELECT hash FROM blobs WHERE id = terminal_transcript_ref) AS transcript_ref";

        /// Session `?1`'s metadata, answering its admission base.
        delete_meta = "DELETE FROM session_meta WHERE session_id = ?1
             RETURNING (SELECT hash FROM blobs WHERE id = admission_base_checkpoint_ref) AS admission_base";
    }
}

static SQL: LazyLock<SessionDeleteStatements> =
    LazyLock::new(|| SessionDeleteStatements::render(Dialect::postgres()));

fn session() -> &'static crate::session_sql::SessionSql {
    session_sql()
}

fn ingress() -> &'static crate::turn_ingress::TurnIngressSql {
    crate::turn_ingress::turn_ingress_sql()
}

pub(super) static LOCK_REFERRERS: Entry =
    Entry::new(|| &SQL.lock_referrers, &[Ty::Text], Shape::Select);
pub(super) static FENCE_FRAME: Entry =
    Entry::new(|| &SQL.fence_frame, &[Ty::Text, Ty::Now], Shape::Exec);
pub(super) static ARM_FRAME: Entry = Entry::new(
    || &SQL.arm_frame_ended,
    &[Ty::Text, Ty::Text, Ty::Text, Ty::Now],
    Shape::Returning,
);
pub(super) static END_FRAME: Entry = Entry::new(
    || &SQL.end_frame_local,
    &[Ty::Text, Ty::Text, Ty::Text, Ty::Now, Ty::Texts, Ty::Text],
    Shape::EndLocal,
);
pub(super) static DELETED_FROM_META: Entry = Entry::new(
    || &session().deleted_postgres.insert_from_meta,
    &[Ty::Text],
    Shape::Exec,
);
pub(super) static DELETED_ROOT: Entry = Entry::new(
    || &session().deleted_postgres.insert_root,
    &[Ty::Text],
    Shape::Exec,
);
pub(super) static HEAD: Entry = Entry::new(
    || &session().head.delete_by_session,
    &[Ty::Text],
    Shape::Exec,
);
pub(super) static REVISIONS: Entry =
    Entry::new(|| &SQL.delete_revisions, &[Ty::Text], Shape::Returning)
        .collecting(Collect::Ancestry("leaf_node_id"))
        .retiring("checkpoint_ref")
        .retiring("config_ref");
pub(super) static PINS: Entry = Entry::new(
    || &session().pins.delete_by_session,
    &[Ty::Text],
    Shape::Exec,
);
pub(super) static RUNS: Entry =
    Entry::new(|| &SQL.delete_runs, &[Ty::Text], Shape::Returning).retiring("transcript_ref");
pub(super) static UNREACHABLE: Entry = Entry::new(
    || &session().graph_postgres.select_unreachable_leaves,
    &[Ty::Text],
    Shape::Select,
)
.collecting(Collect::Ancestry("node_id"));
pub(super) static TOMBSTONED: Entry = Entry::new(
    || &session().graph_postgres.delete_tombstoned_reclaimable,
    &[Ty::Text],
    Shape::Exec,
);
pub(super) static QUEUED_BATCHES: Entry = Entry::new(
    || &ingress().queued_batches.delete_by_session,
    &[Ty::Text],
    Shape::Exec,
);
pub(super) static PENDING_INPUTS: Entry = Entry::new(
    || &ingress().pending_inputs.delete_by_session,
    &[Ty::Text],
    Shape::Exec,
);
pub(super) static RUN_SPECS: Entry = Entry::new(
    || &ingress().run_specs.delete_session,
    &[Ty::Text],
    Shape::Exec,
);
pub(super) static FORSAKEN_INPUTS: Entry = Entry::new(
    || &ingress().forsaken_inputs.delete_by_session,
    &[Ty::Text],
    Shape::Exec,
);
pub(super) static INGRESS_SEQUENCE: Entry = Entry::new(
    || &crate::session_ingress::session_ingress_sql().delete_sequence,
    &[Ty::Text],
    Shape::Exec,
);
pub(super) static CANCEL_REQUESTS: Entry = Entry::new(
    || &ingress().cancel_requests.delete_by_session,
    &[Ty::Text],
    Shape::Exec,
);
pub(super) static LINEAGE: Entry = Entry::new(
    || &session().lineage.delete_by_session,
    &[Ty::Text],
    Shape::Exec,
);
pub(super) static OBSERVER_INTENTS: Entry = Entry::new(
    || &session().observer_intents.delete_by_session,
    &[Ty::Text],
    Shape::Exec,
);
pub(super) static META: Entry =
    Entry::new(|| &SQL.delete_meta, &[Ty::Text], Shape::Returning).retiring("admission_base");
pub(super) static TERMINAL: Entry = Entry::new(
    || {
        &session()
            .turn_commits_postgres
            .insert_session_terminal_staged
    },
    &[Ty::Text, Ty::Text, Ty::Now],
    Shape::Exec,
);

/// The deletion's statements the one call dispatches on.
pub(super) fn entries() -> Vec<&'static Entry> {
    vec![
        &LOCK_REFERRERS,
        &FENCE_FRAME,
        &ARM_FRAME,
        &END_FRAME,
        &DELETED_FROM_META,
        &DELETED_ROOT,
        &HEAD,
        &REVISIONS,
        &PINS,
        &RUNS,
        &run_records::DELETE_UNDER,
        &model_calls::DELETE_SESSION,
        &tool_calls::DELETE_SESSION,
        &waits::DELETE_OWNED,
        &waits::DELETE_UNDER,
        &UNREACHABLE,
        &TOMBSTONED,
        &QUEUED_BATCHES,
        &PENDING_INPUTS,
        &RUN_SPECS,
        &FORSAKEN_INPUTS,
        &INGRESS_SEQUENCE,
        &CANCEL_REQUESTS,
        &LINEAGE,
        &OBSERVER_INTENTS,
        &META,
        &prompts::RELEASE_SESSION_EDGES,
        &prompts::RELEASE_SESSION_SNAPSHOTS,
        &TERMINAL,
    ]
}

/// The capture slots of a deletion's program: whether the session is
/// materialized, and the obligation its frame's cleanup wrote.
const MATERIALIZED: u8 = 50;
const FRAME_ENDED: u8 = 51;

/// Whether the deletion that captured `slots` deleted a materialized
/// session: it staged the session's terminal record.
pub(super) fn deleted_materialized(slots: &Slots) -> bool {
    slots
        .get(&MATERIALIZED)
        .and_then(Option::as_ref)
        .and_then(|row| row.get("exists").copied())
        .unwrap_or(false)
}

fn encoding(error: impl std::fmt::Display) -> DurableError {
    corrupt("session deletion", &error.to_string())
}

/// The program deleting `session`'s rows, its actor's write lock taken
/// first. Repeating it on a deleted session deletes nothing and records
/// nothing.
pub(super) fn steps(session: &SessionId) -> Result<Vec<Step>, DurableError> {
    let actor = ActorKey::session(session.as_str()).map_err(encoding)?;
    let id = || vec![Value::text(session.as_str())];
    let materialized = |step: Step| step.when_flag(MATERIALIZED, "exists", true);
    // A frame's id names its session, so its plan and its obligation id's
    // prefix are any frame's.
    let probe = ArtifactReferrer::FrameEnvironment(FrameEnvironmentId::new(
        session.clone(),
        lash_core_store::session_identity::FrameNodeId::new("frame").map_err(encoding)?,
    ));
    let probe_id = ObligationKey::ArtifactCleanup {
        referrer: probe.clone(),
    }
    .id();
    let probe_canonical = probe.canonical_id();
    let obligation_prefix = probe_id
        .as_str()
        .strip_suffix(&format!("{}:{probe_canonical}", probe_canonical.len()))
        .ok_or_else(|| encoding("a frame obligation id does not end with its referrer"))?
        .to_owned();
    let frame_ended = ArtifactCleanup::ended(probe.clone(), Vec::new(), None)
        .to_json()
        .map_err(encoding)?;
    let end =
        crate::artifact_store::LocalEndArguments::of(&probe, None).map_err(super::store_failure)?;
    let own = ArtifactReferrer::Session(session.clone());
    let awaited = ArtifactCleanup::Await(ReferrerGuard::SessionGraphRetired(session.clone()));
    let awaited_obligation = ObligationKey::ArtifactCleanup {
        referrer: own.clone(),
    }
    .id();
    let cells = ExecKey::session_cells(session);
    Ok(vec![
        Step::any(
            &head_commit::LOCK_SESSION,
            vec![Value::text(actor.as_str()), Value::text(session.as_str())],
        ),
        Step::any(&head_commit::MATERIALIZED, id()).capturing(MATERIALIZED),
        // The referrers in key order, the frame first: its fence, then the
        // cleanup that ends it and, when it wrote one, the end of its local
        // edges; then the session's cleanup, awaiting its graph's
        // retirement.
        materialized(Step::any(&LOCK_REFERRERS, id())),
        materialized(Step::any(
            &FENCE_FRAME,
            vec![Value::text(session.as_str()), Value::Now],
        )),
        materialized(
            Step::any(
                &ARM_FRAME,
                vec![
                    Value::text(session.as_str()),
                    Value::text(frame_ended),
                    Value::text(obligation_prefix),
                    Value::Now,
                ],
            )
            .capturing(FRAME_ENDED),
        ),
        Step::any(
            &END_FRAME,
            vec![
                Value::text(end.kind),
                Value::text(session.as_str()),
                Value::Text(end.gate),
                Value::Now,
                Value::texts(end.namespaces),
                Value::Text(end.gated_body),
            ],
        )
        .when_row(FRAME_ENDED, true),
        materialized(Step::any(
            &head_commit::CLEANUP_INSERT,
            vec![
                Value::text(own.kind().as_str()),
                Value::text(own.canonical_id()),
                Value::text(awaited.to_json().map_err(encoding)?),
                Value::text(awaited_obligation.as_str()),
                Value::Now,
                Value::Text(None),
            ],
        )),
        // Permanent identity evidence for host-facing session ids.
        materialized(Step::any(&DELETED_FROM_META, id())),
        materialized(Step::any(&DELETED_ROOT, id())),
        // Its head, revisions and pins: the ancestry only its revisions
        // published is retired once they are gone.
        Step::any(&HEAD, id()),
        Step::any(&REVISIONS, id()),
        Step::any(&PINS, id()),
        // Its runs (a `close_session` intent stays as its tombstone) and
        // its engine rows: every turn's ledger and every cell's working
        // state. The session's actor row stays, its terminal fence.
        Step::any(&RUNS, id()),
        Step::any(&environment::HEAD_REMOVE, id()),
        Step::any(
            &run_records::DELETE_UNDER,
            vec![Value::text(OwnerKey::session_turns(session))],
        ),
        Step::any(&model_calls::DELETE_SESSION, id()),
        Step::any(&tool_calls::DELETE_SESSION, id()),
        Step::any(&snapshots::CLEAR_UNDER, vec![Value::text(cells.clone())]),
        Step::any(&graph::ADMISSIONS_UNDER, vec![Value::text(cells.clone())]),
        Step::any(&run_records::DELETE_UNDER, vec![Value::text(cells)]),
        Step::any(&waits::DELETE_OWNED, vec![Value::text(actor.as_str())]),
        Step::any(
            &waits::DELETE_UNDER,
            vec![Value::text(ScopeKey::session_turns(session))],
        ),
        Step::any(&UNREACHABLE, id()),
        // This session's tombstoned rows, and any whose owner is gone: a
        // node tombstoned after its owner left is reached by no
        // session-scoped vacuum.
        Step::any(&TOMBSTONED, id()),
        Step::any(&QUEUED_BATCHES, id()),
        Step::any(&PENDING_INPUTS, id()),
        Step::any(&RUN_SPECS, id()),
        Step::any(&FORSAKEN_INPUTS, id()),
        Step::any(&INGRESS_SEQUENCE, id()),
        Step::any(&CANCEL_REQUESTS, id()),
        Step::any(&LINEAGE, id()),
        Step::any(&OBSERVER_INTENTS, id()),
        Step::any(&META, id()),
        // Deleting the session is the explicit retention its prompt
        // snapshots wait for: its roots go, and every text no other root
        // shares.
        Step::any(&prompts::RELEASE_SESSION_EDGES, id()),
        Step::any(&prompts::RELEASE_SESSION_SNAPSHOTS, id()),
        Step::any(&prompts::RELEASE_PREFIX, id()),
        Step::any(&prompts::RECLAIM, Vec::new()),
        materialized(Step::any(
            &TERMINAL,
            vec![Value::text(session.as_str()), Value::Text(None), Value::Now],
        )),
    ])
}

/// Delete `session`'s rows in the catalog's guarded transaction `tx` at
/// `now`, holding the roots they held: how many stored roots it held.
pub(crate) async fn delete(
    tx: &mut super::Tx,
    session: &SessionId,
    now: DurableInstant,
) -> Result<usize, crate::StoreError> {
    let program = steps(session).map_err(|error| crate::StoreError::Backend(error.to_string()))?;
    let actor = ActorKey::session(session.as_str())
        .map_err(|error| crate::StoreError::Backend(error.to_string()))?;
    let retired = super::Retired::default();
    let context = super::steps::Context {
        retired: &retired,
        actor: &actor,
        now,
    };
    let slots = super::steps::run_capturing(tx, &program, &context)
        .await
        .map_err(super::steps::Failure::into_store)?
        .map_err(|_| crate::StoreError::Backend("a session deletion step refused".to_owned()))?;
    if deleted_materialized(&slots) {
        tx.staged_turn_change();
    }
    let (closures, _) = retired.into_parts();
    super::pages::retire_roots(tx, &closures, now)
        .await
        .map_err(crate::support::store_sqlx_error)
}

/// Delete `session`'s engine rows alone, as a foreign writer would.
#[cfg(test)]
pub(crate) async fn retire_session_rows(
    tx: &mut sqlx::PgConnection,
    session: &SessionId,
) -> Result<(), crate::StoreError> {
    let actor = ActorKey::session(session.as_str())
        .map_err(|error| crate::StoreError::Backend(error.to_string()))?;
    let program: Vec<Step> = steps(session)
        .map_err(|error| crate::StoreError::Backend(error.to_string()))?
        .into_iter()
        .filter(|step| {
            [
                &environment::HEAD_REMOVE,
                &run_records::DELETE_UNDER,
                &model_calls::DELETE_SESSION,
                &tool_calls::DELETE_SESSION,
                &snapshots::CLEAR_UNDER,
                &graph::ADMISSIONS_UNDER,
                &waits::DELETE_OWNED,
                &waits::DELETE_UNDER,
            ]
            .into_iter()
            .any(|entry| std::ptr::eq(entry, step.entry))
        })
        .collect();
    let retired = super::Retired::default();
    let context = super::steps::Context {
        retired: &retired,
        actor: &actor,
        now: DurableInstant(0),
    };
    super::steps::run(tx, &program, &context)
        .await
        .map(drop)
        .map_err(|error| crate::StoreError::Backend(error.to_string()))
}
