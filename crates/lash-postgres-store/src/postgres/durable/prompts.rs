//! Prompt snapshots on PostgreSQL: the `prompts` domain's statements, apply
//! and read (FIG-5256, ADR 0133 §5).
//!
//! Owned by P2 (FIG-5256). The dispatch in `durable/mod.rs` calls these
//! inside the fenced owner commit, after the fence; a refusal rolls the whole
//! commit back. Session deletion releases through [`release`] inside its own
//! transaction (FIG-5272). The DDL is in `schema.sql`.
//!
//! Publications use the shared graph installer's digest-ordered key-share
//! locks. The graph collector skips locked nodes and rechecks the catalog
//! and incoming edges under its delete locks before reclaiming content.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::LazyLock;

use lash_durable::domain::{
    DomainRefusal, ModelCallId, PreparedPromptRecord, PromptCallKey, PromptSnapshotRow, PromptText,
    PromptWrite,
};
use lash_durable::{DurableError, Epoch};
use lash_kernel_value::identity::SessionId;
use lash_store_sql::Dialect;
use lash_store_sql::durable::prompts::{PromptStatements, REQUEST_PART};
use lash_vocabulary::identity::TurnId;
use sqlx::PgConnection;

use super::steps::{Collect, Entry, Shape, Step, Ty, Value};
use super::{Committing, HeldTexts, get, sqlx_failure};

pub(super) static SQL: LazyLock<PromptStatements> =
    LazyLock::new(|| PromptStatements::render(Dialect::postgres()));

lash_store_sql::statements! {
    /// PostgreSQL's prompt root writes and bounded content reads.
    pub(super) struct PostgresPromptStatements @ "durable_prompt_postgres" {
        record = "INSERT INTO prompt_snapshots (session_id, owner, call, snapshot, request, written_epoch)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6) ON CONFLICT DO NOTHING RETURNING call";
        read_texts = "WITH RECURSIVE reached AS (
                 SELECT hash, TRUE AS follow FROM blobs WHERE hash = ANY(?1::bytea[])
                 AND CASE WHEN encoding = 1 AND octet_length(content) >= 5
                     THEN get_byte(content, 4) IN (53, 54) ELSE FALSE END
                 UNION SELECT edge.child_hash,
                     CASE WHEN child.encoding = 1 AND octet_length(child.content) >= 5
                         THEN get_byte(child.content, 4) BETWEEN 1 AND 7 ELSE FALSE END
                 FROM reached JOIN blob_edges edge ON edge.parent_hash = reached.hash
                 JOIN blobs child ON child.hash = edge.child_hash
                 WHERE reached.follow
             ) SELECT blobs.hash, blobs.content,
                 CASE WHEN blobs.encoding = 1 AND octet_length(blobs.content) >= 5
                     THEN get_byte(blobs.content, 4) IN (53, 54) ELSE FALSE END
             FROM reached JOIN blobs ON blobs.hash = reached.hash";
        read_edges = "WITH RECURSIVE closure AS (
                 SELECT part, hash FROM prompt_snapshot_texts
                 WHERE session_id = ?1 AND owner = ?2 AND call = ?3
                 UNION SELECT closure.part, edge.child_hash FROM closure
                 JOIN blob_edges edge ON edge.parent_hash = closure.hash
             ) SELECT closure.part, closure.hash FROM closure JOIN blobs ON blobs.hash = closure.hash
               WHERE CASE WHEN blobs.encoding = 1 AND octet_length(blobs.content) >= 5 THEN get_byte(blobs.content, 4) IN (53, 54) ELSE FALSE END ORDER BY closure.part, closure.hash";
        reclaim = "SELECT lash_graph_retire(?1, ?2, ?3)";
        insert_edges = "INSERT INTO prompt_snapshot_texts (session_id, owner, call, part, hash)
             SELECT ?1, ?2, ?3, part, hash
               FROM unnest(?4::TEXT[], ?5::BYTEA[]) AS batch(part, hash)";
        settle = "WITH released AS (
                 DELETE FROM prompt_snapshot_texts
                  WHERE session_id = ?1 AND owner = ?2 AND part = 'request'
                    AND (?3::TEXT IS NULL OR call <> ?3::TEXT)
                 RETURNING hash
             ), bare AS (
                 DELETE FROM prompt_snapshots
                  WHERE session_id = ?1 AND owner = ?2 AND snapshot IS NULL AND request IS NOT NULL
                    AND (?3::TEXT IS NULL OR call <> ?3::TEXT)
             ), kept AS (
                 UPDATE prompt_snapshots SET request = NULL
                  WHERE session_id = ?1 AND owner = ?2
                    AND snapshot IS NOT NULL AND request IS NOT NULL
                    AND (?3::TEXT IS NULL OR call <> ?3::TEXT)
             )
             SELECT DISTINCT hash FROM released";
        expire_audits = "WITH candidates AS MATERIALIZED (
                 SELECT audit.session_id, audit.owner, audit.call
                 FROM prompt_snapshots AS audit
                 JOIN session_runs AS ended ON ended.session_id = audit.session_id
                   AND audit.owner = 'turn:' || ended.run
                 WHERE (?1::TEXT IS NULL OR audit.session_id = ?1)
                   AND audit.request IS NULL AND audit.snapshot IS NOT NULL
                   AND ended.terminal_kind IS NOT NULL
                   AND ended.terminal_window_revision IS NOT NULL
                   AND NOT EXISTS (
                       SELECT 1 FROM session_revisions AS revision
                       WHERE revision.session_id = ended.session_id
                         AND revision.head_revision = ended.terminal_window_revision
                   )
                 ORDER BY audit.session_id, audit.owner, audit.call LIMIT ?2
                 FOR UPDATE OF audit
             ), released AS (
                 DELETE FROM prompt_snapshot_texts
                 WHERE (session_id, owner, call) IN (SELECT * FROM candidates)
                   AND part = 'snapshot'
                 RETURNING hash
             ), expired AS (
                 UPDATE prompt_snapshots SET snapshot = NULL
                 WHERE (session_id, owner, call) IN (SELECT * FROM candidates)
                 RETURNING call
             ) SELECT (SELECT COUNT(*) FROM expired) AS released_count, released.hash
               FROM (SELECT 1) AS singleton LEFT JOIN released ON TRUE";


    }
}

pub(super) static POSTGRES_SQL: LazyLock<PostgresPromptStatements> =
    LazyLock::new(|| PostgresPromptStatements::render(Dialect::postgres()));

#[cfg(test)]
lash_store_sql::statements! {
    pub(crate) struct PromptLawStatements @ "durable_prompt_postgres_law" {
        hold_text = "SELECT hash FROM blobs WHERE hash = ?1 FOR UPDATE";
    }
}
#[cfg(test)]
pub(crate) static LAW_SQL: LazyLock<PromptLawStatements> =
    LazyLock::new(|| PromptLawStatements::render(Dialect::postgres()));

/// The prompt statements an owner commit runs as steps.
pub(super) static RECORD: Entry = Entry::new(
    || &POSTGRES_SQL.record,
    &[Ty::Text, Ty::Text, Ty::Text, Ty::Text, Ty::Text, Ty::Int],
    Shape::Returning,
);
pub(super) static ROOT_EDGES: Entry = Entry::new(
    || &POSTGRES_SQL.insert_edges,
    &[Ty::Text, Ty::Text, Ty::Text, Ty::Texts, Ty::Keys],
    Shape::Exec,
);
pub(super) static SET_PREFIX: Entry =
    Entry::new(|| &SQL.set_prefix, &[Ty::Text, Ty::Key], Shape::Exec);
pub(super) static SETTLE: Entry = Entry::new(
    || &POSTGRES_SQL.settle,
    &[Ty::Text, Ty::Text, Ty::Text],
    Shape::Select,
)
.collecting(Collect::Peel("hash"));
pub(super) static RELEASE_PREFIX: Entry =
    Entry::new(|| &SQL.release_prefix, &[Ty::Text], Shape::Returning)
        .collecting(Collect::Peel("hash"));
pub(super) static RECLAIM: Entry = Entry::new(|| &POSTGRES_SQL.reclaim, &[], Shape::GraphReclaim);

/// The shared revision-material program collects former audit roots before
/// reclaiming their last-reference texts in the same transaction.
pub(super) static EXPIRE_AUDITS: Entry = Entry::new(
    || &POSTGRES_SQL.expire_audits,
    &[Ty::Text, Ty::Int],
    Shape::Select,
)
.collecting(Collect::Peel("hash"));

/// Release one bounded page, answering its audit count for GC's drain loop.
pub(crate) async fn release_unretained(
    tx: &mut PgConnection,
    session: Option<&SessionId>,
) -> Result<usize, sqlx::Error> {
    use sqlx::Row as _;
    let rows = crate::observed_sql::query(POSTGRES_SQL.expire_audits.sql())
        .bind(session.map(SessionId::as_str))
        .bind(i64::from(
            lash_store_sql::session::revisions::MATERIAL_RELEASE_BATCH,
        ))
        .fetch_all(crate::observed_sql::executor(&mut *tx))
        .await?;
    let count = rows
        .first()
        .map_or(Ok(0), |row| row.try_get::<i64, _>("released_count"))?;
    let roots = rows
        .iter()
        .map(|row| crate::keys::get::<Option<String>>(row, "hash"))
        .collect::<Result<Vec<_>, _>>()?
        .into_iter()
        .flatten()
        .collect();
    reclaim(tx, roots).await?;
    Ok(usize::try_from(count).unwrap_or(usize::MAX))
}

/// Release the request of every call of turn `run` in `session` but `open`,
/// and reclaim every text no remaining edge references: the session's
/// prefix is held, and kept, only when the settlement released a text.
fn settle_steps(session: &SessionId, run: &TurnId, open: Option<u32>) -> Vec<Step> {
    vec![
        Step::any(
            &SETTLE,
            vec![
                Value::text(session.as_str()),
                Value::text(ModelCallId::turn_owner(run)),
                Value::Text(open.map(|call| call.to_string())),
            ],
        ),
        Step::any(&RECLAIM, Vec::new()),
    ]
}

/// `write`'s one-call step program at `epoch`: a record's root, which must
/// be the call's first, then its distinct texts, locked shared in text
/// order, the bodies stored that `held` does not name, and its edges. A text
/// sent without its body that the store does not hold refuses the call, and
/// the record runs in the explicit form, which reads what the store holds.
/// A settlement or a prefix reset releases what its statement answers and
/// reclaims it in the commit's reclaim step. A release has none.
pub(super) fn steps(epoch: Epoch, write: &PromptWrite, _held: &HeldTexts) -> Option<Vec<Step>> {
    match write {
        PromptWrite::Settle { session, run, open } => {
            return Some(settle_steps(session, run, *open));
        }
        PromptWrite::ResetPrefix { session } => {
            return Some(vec![
                Step::any(&RELEASE_PREFIX, vec![Value::text(session.as_str())]),
                Step::any(&RECLAIM, Vec::new()),
            ]);
        }
        PromptWrite::Record { .. } | PromptWrite::Release { .. } => {}
    }
    let PromptWrite::Record {
        call,
        snapshot,
        request,
        snapshot_texts,
        request_texts,
        prefix,
    } = write
    else {
        return None;
    };
    let prepared = PreparedPromptRecord::new(snapshot_texts, request_texts, prefix.as_deref());
    let (parts, edges): (Vec<_>, Vec<_>) = prepared
        .edges()
        .into_iter()
        .map(|(part, hash)| (Some(part.to_owned()), Some(hash.to_owned())))
        .unzip();
    let mut steps = vec![
        Step::one(
            &RECORD,
            vec![
                Value::text(call.session.as_str()),
                Value::text(call.call.owner_column()),
                Value::text(call.call.call_column()),
                Value::Text(snapshot.clone()),
                Value::text(request),
                Value::int(epoch.0),
            ],
        ),
        Step::any(
            &ROOT_EDGES,
            vec![
                Value::text(call.session.as_str()),
                Value::text(call.call.owner_column()),
                Value::text(call.call.call_column()),
                Value::Texts(parts),
                Value::Keys(edges),
            ],
        ),
    ];
    if matches!(call.call, ModelCallId::Turn { .. }) {
        steps.push(Step::any(
            &RELEASE_PREFIX,
            vec![Value::text(call.session.as_str())],
        ));
        if prefix.is_some() {
            steps.push(Step::any(
                &SET_PREFIX,
                vec![
                    Value::text(call.session.as_str()),
                    Value::Key(prefix.clone()),
                ],
            ));
        }
        steps.push(Step::any(&RECLAIM, Vec::new()));
    }
    Some(steps)
}

/// Encode only the changed frontier; held hashes are hints and the shared
/// graph installer validates every external reference under its row locks.
pub(super) struct PromptGraph {
    pub nodes: BTreeMap<
        lash_core_store::checkpoint_graph::NodeId,
        lash_core_store::checkpoint_graph::EncodedNode,
    >,
    pub reads: BTreeSet<lash_core_store::checkpoint_graph::NodeId>,
}

pub(super) fn graph_plan(
    write: &PromptWrite,
    held: &HeldTexts,
) -> Result<PromptGraph, DurableError> {
    let PromptWrite::Record {
        snapshot_texts,
        request_texts,
        prefix,
        ..
    } = write
    else {
        return Ok(PromptGraph {
            nodes: BTreeMap::new(),
            reads: BTreeSet::new(),
        });
    };
    // Keep the request's root list intact. A held immutable root stops the
    // server frontier, even when its descendants were not supplied.
    let prepared = PreparedPromptRecord::new(snapshot_texts, request_texts, prefix.as_deref());
    let mut texts = BTreeMap::new();
    let mut frontier: BTreeSet<_> = prepared
        .roots()
        .iter()
        .map(|hash| (*hash).to_owned())
        .collect();
    // Preserve the server's root-first refusal order while batching the
    // complete direct frontier into its first lock/read, even on a cold chain.
    while !frontier.is_empty() {
        let mut next = BTreeSet::new();
        for hash in frontier {
            if texts.contains_key(&hash) {
                continue;
            }
            // An owner's delta already distinguishes new bodies from external
            // references. A remembered hash from a retired frame cannot prune
            // one of those new bodies and force a stale-hint retry.
            let text = match prepared.supplied() {
                Some(supplied) => supplied.get(hash.as_str()).copied(),
                None if held.contains(&hash) => None,
                None => prepared.text(&hash),
            };
            if let Some(text) = text {
                next.extend(text.children.iter().cloned());
            }
            texts.insert(hash, text);
        }
        frontier = next;
    }
    let mut nodes = BTreeMap::new();
    let mut reads = BTreeSet::new();
    for (hash, text) in texts {
        if let Some(text) = text {
            nodes.extend(text.graph_nodes().map_err(DurableError::Graph)?);
        } else {
            reads.insert(
                lash_core_store::checkpoint_graph::NodeId::from_hex(&hash).ok_or_else(|| {
                    DurableError::Domain(DomainRefusal::PromptFragmentMissing { hash })
                })?,
            );
        }
    }
    Ok(PromptGraph { nodes, reads })
}

pub(super) async fn apply(
    tx: &mut PgConnection,
    commit: &Committing<'_>,
    write: &PromptWrite,
) -> Result<(), DurableError> {
    match write {
        PromptWrite::Record { call, .. } => {
            let program = steps(commit.epoch, write, &HeldTexts::default()).unwrap_or_default();
            if super::steps::run(tx, &program[..1], &commit.steps())
                .await?
                .is_some()
            {
                return Err(DurableError::Domain(DomainRefusal::PromptCallRecorded {
                    call: call.clone(),
                }));
            }
            let PromptGraph { nodes, reads } = graph_plan(write, &HeldTexts::default())?;
            let checked = lash_durable::domain::graph_commit::check_graph(&nodes, &reads, &[])
                .map_err(DurableError::Graph)?;
            if let Err(refusal) = super::graph::install(tx, &checked).await {
                return Err(match refusal {
                    lash_core_store::checkpoint_graph::GraphRefusal::MissingNode(id) => {
                        DurableError::Domain(DomainRefusal::PromptFragmentMissing {
                            hash: id.to_hex(),
                        })
                    }
                    other => DurableError::Graph(other),
                });
            }
            super::steps::run(tx, &program[1..], &commit.steps()).await?;
            Ok(())
        }
        PromptWrite::ResetPrefix { .. } | PromptWrite::Settle { .. } => {
            // A release answers what it released: no step of it refuses.
            let program = steps(commit.epoch, write, &HeldTexts::default()).unwrap_or_default();
            super::steps::run(tx, &program, &commit.steps())
                .await
                .map(drop)
        }
        PromptWrite::Release { session, run } => release(tx, session, run.as_ref())
            .await
            .map_err(sqlx_failure),
    }
}

/// Release every snapshot root of `session`, or of its turn `run` alone, and
/// reclaim every text no remaining root references.
///
/// # Errors
///
/// The first statement PostgreSQL refused.
pub(crate) async fn release(
    tx: &mut PgConnection,
    session: &SessionId,
    run: Option<&TurnId>,
) -> Result<(), sqlx::Error> {
    let mut released: Vec<String> = match run {
        None => {
            let hashes = crate::keys::query_scalar(SQL.release_session_edges.sql())
                .bind(session.as_str())
                .fetch_all(crate::observed_sql::executor(&mut *tx))
                .await?;
            crate::observed_sql::query(SQL.release_session_snapshots.sql())
                .bind(session.as_str())
                .execute(crate::observed_sql::executor(&mut *tx))
                .await?;
            hashes
        }
        Some(run) => {
            let hashes = crate::keys::query_scalar(SQL.release_run_edges.sql())
                .bind(session.as_str())
                .bind(ModelCallId::turn_owner(run))
                .fetch_all(crate::observed_sql::executor(&mut *tx))
                .await?;
            crate::observed_sql::query(SQL.release_run_snapshots.sql())
                .bind(session.as_str())
                .bind(ModelCallId::turn_owner(run))
                .execute(crate::observed_sql::executor(&mut *tx))
                .await?;
            hashes
        }
    };
    if run.is_none() {
        released.extend(
            crate::keys::query_scalar::<String>(SQL.release_prefix.sql())
                .bind(session.as_str())
                .fetch_all(crate::observed_sql::executor(&mut *tx))
                .await?,
        );
    }
    reclaim(tx, released).await
}

async fn reclaim(tx: &mut PgConnection, released: Vec<String>) -> Result<(), sqlx::Error> {
    super::graph::reclaim(
        tx,
        released.into_iter().collect(),
        lash_durable::DurableInstant(0),
    )
    .await
    .map(drop)
}

pub(super) async fn snapshot(
    tx: &mut PgConnection,
    call: &PromptCallKey,
) -> Result<Option<PromptSnapshotRow>, DurableError> {
    let Some(row) = crate::observed_sql::query(SQL.read_snapshot.sql())
        .bind(call.session.as_str())
        .bind(call.call.owner_column())
        .bind(call.call.call_column())
        .fetch_optional(crate::observed_sql::executor(&mut *tx))
        .await
        .map_err(sqlx_failure)?
    else {
        return Ok(None);
    };
    if get::<bool>(&row, 3)? {
        return Err(DurableError::Domain(DomainRefusal::PromptAuditExpired {
            call: call.clone(),
        }));
    }
    let edges: Vec<(String, crate::keys::Key)> =
        crate::observed_sql::query_as(POSTGRES_SQL.read_edges.sql())
            .bind(call.session.as_str())
            .bind(call.call.owner_column())
            .bind(call.call.call_column())
            .fetch_all(crate::observed_sql::executor(&mut *tx))
            .await
            .map_err(sqlx_failure)?;
    let (request_texts, snapshot_texts): (Vec<_>, Vec<_>) = edges
        .into_iter()
        .partition(|(part, _)| part == REQUEST_PART);
    let hashes = |edges: Vec<(String, crate::keys::Key)>| {
        edges.into_iter().map(|(_, hash)| hash.0).collect()
    };
    Ok(Some(PromptSnapshotRow {
        call: call.clone(),
        snapshot: get(&row, 0)?,
        request: get(&row, 1)?,
        snapshot_texts: hashes(snapshot_texts),
        request_texts: hashes(request_texts),
        written_epoch: Epoch(get(&row, 2)?),
    }))
}

pub(super) async fn texts(
    tx: &mut PgConnection,
    hashes: &[String],
) -> Result<Vec<PromptText>, DurableError> {
    use lash_core_store::checkpoint_graph::{EncodedNode, NodeId};
    if hashes.is_empty() {
        return Ok(Vec::new());
    }
    let rows: Vec<(crate::keys::Key, Vec<u8>, bool)> =
        crate::observed_sql::query_as(POSTGRES_SQL.read_texts.sql())
            .bind(
                hashes
                    .iter()
                    .filter_map(crate::keys::lookup)
                    .collect::<Vec<_>>(),
            )
            .fetch_all(crate::observed_sql::executor(tx))
            .await
            .map_err(sqlx_failure)?;
    let mut nodes = BTreeMap::new();
    let mut roots = BTreeSet::new();
    for (hash, bytes, requested) in rows {
        let id = super::graph::node_of(&hash.0).map_err(DurableError::Graph)?;
        if requested {
            roots.insert(id);
        }
        nodes.insert(
            id,
            EncodedNode {
                id,
                bytes: bytes.into(),
            },
        );
    }
    let mut found = Vec::new();
    for hash in hashes {
        if let Some(id) = NodeId::from_hex(hash)
            && roots.contains(&id)
        {
            found.push(
                PromptText::from_graph(&nodes, id, lash_durable::domain::graph::OWNER_READ_LIMITS)
                    .map_err(DurableError::Graph)?,
            );
        }
    }
    Ok(found)
}

#[cfg(test)]
mod tests {
    use super::*;
    use lash_core_store::store::SessionCatalogStore as _;
    use lash_core_store::store::StoreMaintenance as _;
    use lash_core_store::store::Target;
    use lash_core_store::store::TurnInputStore as _;
    use lash_durable::{ActorKey, CommitLabel, DomainWrite, DurableReads, DurableStore};

    /// PROMPT-AUDIT-RETENTION: revision expiry releases settled audit plans
    /// and last-reference texts, retaining pending requests and the prefix.
    #[tokio::test]
    async fn settled_prompt_audits_follow_revision_retention() {
        let database_url = crate::testing::required_database_url();
        let database = crate::testing::IsolatedDatabase::create(&database_url).await;
        let storage = crate::testing::connect(database.url()).await.unwrap();
        let stores = crate::PostgresStoreSet::new(
            &storage,
            std::sync::Arc::new(lash_core_store::attachments::UnavailableAttachmentStore),
        );
        let identity = lash_core_execution::testing::sql_work::owner_commit(&stores).await;
        let store = storage.durable_store();
        let session = SessionId::from("sql-work-owner");
        let actor = ActorKey::session(session.as_str()).unwrap();
        let call = |ordinal| PromptCallKey {
            session: session.clone(),
            call: ModelCallId::Turn {
                run: TurnId::from("settled"),
                ordinal,
            },
        };
        let text = |body: &str| PromptText::audit(body);
        let mut tx = store
            .begin(&actor, Epoch(identity.epoch), u64::MAX)
            .await
            .unwrap();
        for ordinal in 1..=131 {
            tx.write(DomainWrite::Prompt(PromptWrite::Record {
                call: call(ordinal),
                snapshot: Some(format!("audit-{ordinal}")),
                request: format!("exact-request-{ordinal}"),
                snapshot_texts: vec![text("shared"), text(&format!("unique-{ordinal}"))],
                request_texts: vec![text("request"), text("prefix")].into(),
                prefix: Some(text("prefix").hash),
            }));
        }
        tx.write(DomainWrite::Prompt(PromptWrite::Settle {
            session: session.clone(),
            run: TurnId::from("settled"),
            open: Some(131),
        }));
        store
            .commit(tx, CommitLabel::new("model.start"))
            .await
            .unwrap();
        // Seed two published revisions and the settled run's exact revision.
        // The last call deliberately remains unconsumed even past revision
        // release: recovery owns its request and snapshot until settlement.
        let mut tx = storage.pool().begin().await.unwrap();
        for sql in [
            "INSERT INTO lash_blobs (hash, content, encoding) VALUES (decode(repeat('00', 32), 'hex'), decode('00', 'hex'), 0)",
            "INSERT INTO lash_session_meta (session_id, relation_kind, retention_kind) VALUES ('sql-work-owner', 'root', 'head_only')",
            "INSERT INTO lash_session_revisions (session_id, head_revision, config_ref) VALUES ('sql-work-owner', 1, decode(repeat('00', 32), 'hex')), ('sql-work-owner', 2, decode(repeat('00', 32), 'hex'))",
            "INSERT INTO lash_session_head (session_id, head_revision) VALUES ('sql-work-owner', 2)",
            "INSERT INTO lash_session_runs (session_id, run, terminal_kind, terminal_cause_json,
                terminal_at_ms, terminal_head_revision, terminal_window_revision)
                VALUES ('sql-work-owner', 'settled', 'answered', '{\"cause\":\"commands_applied\"}', 0, 1, 1)",
            "INSERT INTO lash_pins (session_id, target_kind, target_id) VALUES ('sql-work-owner', 'revision', '1')",
        ] {
            sqlx::query(sql).execute(&mut *tx).await.unwrap();
        }
        crate::revisions::record_terminal_conn(
            &mut tx,
            &session,
            2,
            lash_durable::DurableInstant(0),
        )
        .await
        .unwrap();
        let admission = lash_core_store::store::RunAdmissionRecord::Command {
            batch: lash_vocabulary::identity::BatchId::from("finish"),
        }
        .to_stored()
        .unwrap();
        sqlx::query(
            "INSERT INTO lash_session_runs (session_id, run, admission_json)
            VALUES ('sql-work-owner', 'finish', $1)",
        )
        .bind(admission)
        .execute(&mut *tx)
        .await
        .unwrap();
        tx.commit().await.unwrap();
        assert_eq!(
            store
                .prompt_snapshot(&call(1))
                .await
                .unwrap()
                .unwrap()
                .snapshot
                .as_deref(),
            Some("audit-1"),
            "the revision pin keeps its settled audits exact"
        );
        let catalog = storage.session_store_factory();
        catalog.unpin(&session, &Target::Revision(1)).await.unwrap();
        // Logical expiry never waits for a physical page: even the two roots
        // beyond the first batch already answer the same typed refusal.
        for ordinal in 1..=130 {
            assert!(matches!(store.prompt_snapshot(&call(ordinal)).await,
                Err(DurableError::Domain(DomainRefusal::PromptAuditExpired { call: expired }))
                    if expired == call(ordinal)));
        }
        let remaining = sqlx::query_scalar::<_, i64>(
            "SELECT COUNT(*) FROM lash_prompt_snapshots WHERE snapshot IS NOT NULL AND request IS NULL")
            .fetch_one(storage.pool()).await.unwrap();
        assert_eq!(
            remaining, 2,
            "one invocation releases exactly one internal batch"
        );
        catalog.gc_unreachable().await.unwrap();
        let remaining = sqlx::query_scalar::<_, i64>(
            "SELECT COUNT(*) FROM lash_prompt_snapshots WHERE snapshot IS NOT NULL AND request IS NULL")
            .fetch_one(storage.pool()).await.unwrap();
        assert_eq!(
            remaining, 0,
            "GC drains a quiet session without another commit"
        );
        let pending = store.prompt_snapshot(&call(131)).await.unwrap().unwrap();
        assert_eq!(pending.request.as_deref(), Some("exact-request-131"));
        assert_eq!(pending.snapshot.as_deref(), Some("audit-131"));
        let hashes = [
            "shared",
            "unique-1",
            "unique-2",
            "unique-131",
            "request",
            "prefix",
        ]
        .map(|name| text(name).hash);
        let found = store.prompt_texts(&hashes).await.unwrap();
        assert_eq!(
            found
                .iter()
                .map(|text| text.hash.clone())
                .collect::<Vec<_>>(),
            ["shared", "unique-131", "request", "prefix"].map(|name| text(name).hash)
        );
        // FIG-6607/6611: exercise the real owner program with a second
        // page of ingress, rather than inferring input release from audits.
        let mut ingress = Vec::new();
        for index in 0..129 {
            let draft = lash_core_store::turn_input_vocabulary::PendingTurnInputDraft::new(
                session.clone(),
                lash_core_store::turn_input_vocabulary::TurnInputIngress::NextTurn,
                lash_core_store::turn_input_vocabulary::TurnInput::text(format!(
                    "backlog body {index}"
                )),
            )
            .with_source_key(format!("backlog-{index}"));
            let input = catalog
                .enqueue_pending_turn_input(draft.clone())
                .await
                .unwrap();
            sqlx::query(
                "UPDATE lash_pending_turn_inputs SET state = 'completed',
                admitted_run = 'settled', terminal_at_ms = 0 WHERE input_id = $1",
            )
            .bind(input.input_id.as_str())
            .execute(storage.pool())
            .await
            .unwrap();
            let digest = catalog
                .turn_input_submission_digest(&session, &format!("backlog-{index}"))
                .await
                .unwrap();
            ingress.push((draft, input.input_id, digest));
        }
        let pending_input = catalog
            .enqueue_pending_turn_input(
                lash_core_store::turn_input_vocabulary::PendingTurnInputDraft::new(
                    session.clone(),
                    lash_core_store::turn_input_vocabulary::TurnInputIngress::NextTurn,
                    lash_core_store::turn_input_vocabulary::TurnInput::text("unconsumed control"),
                )
                .with_source_key("unconsumed-control"),
            )
            .await
            .unwrap();
        // Settling the last request does not erase previously expired identities.
        let mut tx = store
            .begin(&actor, Epoch(identity.epoch), u64::MAX)
            .await
            .unwrap();
        tx.write(DomainWrite::Prompt(PromptWrite::Settle {
            session: session.clone(),
            run: TurnId::from("settled"),
            open: None,
        }));
        // Exercise the owner-commit program's release hook as well as the
        // catalogue/unpin and GC forms above. No further GC is needed.
        tx.write(DomainWrite::Turn(
            lash_durable::domain::TurnWrite::Terminal {
                session: session.clone(),
                run: TurnId::from("finish"),
                cause: Box::new(lash_core_store::store::RunTerminalCause::CommandsApplied),
                head_revision: Some(2),
                transcript: None,
            },
        ));
        store
            .commit(tx, CommitLabel::new("model.done"))
            .await
            .unwrap();
        let physical_inputs = || async {
            sqlx::query_scalar::<_, i64>(
                "SELECT COUNT(*) FROM lash_pending_turn_inputs
                WHERE state = 'completed' AND input_json IS NOT NULL",
            )
            .fetch_one(storage.pool())
            .await
            .unwrap()
        };
        assert_eq!(
            physical_inputs().await,
            1,
            "the owner program clears exactly 128 input bodies"
        );
        for (index, (draft, id, digest)) in ingress.into_iter().enumerate() {
            let replay = catalog.enqueue_pending_turn_input(draft).await.unwrap();
            assert_eq!(replay.input_id, id);
            assert!(
                replay.input.retained().is_none(),
                "undrained expired bodies already read Released"
            );
            assert_eq!(
                catalog
                    .turn_input_submission_digest(&session, &format!("backlog-{index}"))
                    .await
                    .unwrap(),
                digest
            );
        }
        let reopened = crate::testing::connect(database.url()).await.unwrap();
        reopened
            .session_store_factory()
            .gc_unreachable()
            .await
            .unwrap();
        assert_eq!(
            physical_inputs().await,
            0,
            "reopened maintenance drains without owner commits"
        );
        assert!(
            reopened
                .session_store_factory()
                .pending_turn_input(&session, &pending_input.input_id)
                .await
                .unwrap()
                .unwrap()
                .input
                .input
                .retained()
                .is_some()
        );
        assert_eq!(
            store.prompt_texts(&hashes).await.unwrap(),
            [text("prefix")],
            "the current lowering prefix keeps its own independent reference"
        );
        let mut tx = store
            .begin(&actor, Epoch(identity.epoch), u64::MAX)
            .await
            .unwrap();
        tx.write(DomainWrite::Prompt(PromptWrite::Record {
            call: call(1),
            snapshot: None,
            request: "changed".to_owned(),
            snapshot_texts: Vec::new(),
            request_texts: Default::default(),
            prefix: None,
        }));
        assert!(matches!(
            store.commit(tx, CommitLabel::new("model.start")).await,
            Err(DurableError::Domain(
                DomainRefusal::PromptCallRecorded { .. }
            ))
        ));
    }
}
