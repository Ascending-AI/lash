//! Prompt snapshots on PostgreSQL: the `prompts` domain's statements, apply
//! and read (FIG-5256, ADR 0133 §5).
//!
//! Owned by P2 (FIG-5256). The dispatch in `durable/mod.rs` calls these
//! inside the fenced owner commit, after the fence; a refusal rolls the whole
//! commit back. Session deletion releases through [`release`] inside its own
//! transaction (FIG-5272). The DDL is in `schema.sql`.
//!
//! A record and a release that share a text are serialized on that text
//! (FIG-5272). Under READ COMMITTED neither sees the other's uncommitted
//! edge, and the edge's foreign key alone does not order them: a release
//! that reclaims a text a concurrent record found stored fails that record's
//! edge, and a release whose reclaim waits behind a record's edge then fails
//! its own delete. So a record takes each text's advisory lock shared before
//! it looks for the text, and a release (or a settlement) takes it exclusive
//! before it reclaims, with each lock batch in text order. A reclaim waits for open
//! publications of its candidates and, in a
//! statement after the lock, sees their edges, while a record behind a
//! reclaim finds the text gone in a statement after its lock and binds its
//! body again. Under the lock a record binds the bodies of only the texts
//! it does not find (FIG-5946): what it finds stays until it commits. The
//! lock lives in memory, not in the hot shared text rows, which is why it is
//! preferred over locking those rows.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::LazyLock;

use lash_durable::domain::{
    DomainRefusal, ModelCallId, PromptCallKey, PromptSnapshotRow, PromptText, PromptWrite,
    request_roots,
};
use lash_durable::{DurableError, Epoch};
use lash_sansio::{SessionId, TurnId};
use lash_store_sql::Dialect;
use lash_store_sql::durable::prompts::{PromptStatements, REQUEST_PART, SNAPSHOT_PART};
use sqlx::PgConnection;

use super::steps::{Collect, Entry, Shape, Step, Ty, Value};
use super::{Committing, HeldTexts, get, sqlx_failure};
use crate::connection_sql::connection_sql;

static SQL: LazyLock<PromptStatements> =
    LazyLock::new(|| PromptStatements::render(Dialect::postgres()));

lash_store_sql::statements! {
    /// The `prompt_texts` and `prompt_snapshot_texts` statements only
    /// PostgreSQL issues: `unnest` is how it binds a call's texts as one
    /// list.
    struct PostgresPromptStatements @ "durable_prompt_postgres" {
        /// Every content address in `?1` already stored.
        held_texts = "SELECT hash FROM prompt_texts WHERE hash = ANY(?1::TEXT[])";

        /// Store each text `?2[i]` at content address `?1[i]`, skipping every
        /// address a concurrent record stored first: the address names the
        /// bytes, so a conflict is the same text.
        insert_texts = "INSERT INTO prompt_texts (hash, text)
             SELECT hash, text FROM unnest(?1::TEXT[], ?2::TEXT[]) AS batch(hash, text)
             ON CONFLICT (hash) DO NOTHING";

        /// Of the addresses `?1`, every one sent without its body (`?2[i]`
        /// null) that is not stored: a one-call record's texts are locked
        /// first, so one it finds stays until it commits.
        unsent_absent = "SELECT batch.hash
             FROM unnest(?1::TEXT[], ?2::TEXT[]) AS batch(hash, text)
             WHERE batch.text IS NULL
               AND NOT EXISTS (SELECT 1 FROM prompt_texts AS stored WHERE stored.hash = batch.hash)";

        /// [`Self::insert_texts`] of the texts sent with their bodies.
        insert_sent_texts = "INSERT INTO prompt_texts (hash, text)
             SELECT hash, text FROM unnest(?1::TEXT[], ?2::TEXT[]) AS batch(hash, text)
             WHERE batch.text IS NOT NULL
             ON CONFLICT (hash) DO NOTHING";

        /// Root text `?5[i]` under part `?4[i]` of call `?3` of owner `?2`
        /// in session `?1`, for every `i`.
        insert_edges = "INSERT INTO prompt_snapshot_texts (session_id, owner, call, part, hash)
             SELECT ?1, ?2, ?3, part, hash
               FROM unnest(?4::TEXT[], ?5::TEXT[]) AS batch(part, hash)";

        /// Install each new immutable node's child edges once.
        insert_index_edges = "INSERT INTO prompt_text_edges (parent, child)
             SELECT parent, child FROM unnest(?1::TEXT[], ?2::TEXT[]) AS batch(parent, child)
             ON CONFLICT (parent, child) DO NOTHING";

        /// Anchor the session's current prefix until settlement commits.
        /// A concurrent invalidation cannot remove this row while its
        /// immutable closure is excluded from reclamation.
        hold_prefix = "SELECT hash FROM prompt_prefixes WHERE session_id = ?1 FOR SHARE";

        /// Lock the candidate closure before a sorted, atomic peel. Nodes
        /// this publication protects stop traversal; a concurrent prefix
        /// release cannot invalidate that protection.
        closure = "WITH RECURSIVE closure AS (
                 SELECT unnest(?1::TEXT[]) AS hash
                 UNION
                 SELECT edge.child FROM closure
                   JOIN prompt_text_edges AS edge ON edge.parent = closure.hash
                  WHERE NOT (closure.hash = ANY(?2::TEXT[]))
             ) SELECT hash FROM closure WHERE NOT (hash = ANY(?2::TEXT[])) ORDER BY hash";

        /// Settle every call of owner `?2` in session `?1` but call `?3`
        /// (none when `?3` is null): release its request edges, its root
        /// when the request was all it held, and otherwise its request,
        /// returning each distinct text a released edge named. The two root
        /// writes touch disjoint rows, and a bare root's request edges go in
        /// the same statement, before its foreign key is checked.
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

        /// Release one bounded page of expired settled audits atomically, retaining
        /// compose-once identities and returning their former text roots.
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

        /// Reclaim every text in `?1` no root references any more.
        reclaim_texts = "DELETE FROM prompt_texts AS stored
             WHERE stored.hash = ANY(?1::TEXT[])
               AND NOT EXISTS (
                   SELECT 1 FROM prompt_snapshot_texts AS edge WHERE edge.hash = stored.hash
               ) AND NOT EXISTS (
                   SELECT 1 FROM prompt_prefixes AS prefix WHERE prefix.hash = stored.hash
               ) AND NOT EXISTS (
                   SELECT 1 FROM prompt_text_edges AS edge WHERE edge.child = stored.hash
               ) RETURNING hash";
    }
}

static POSTGRES_SQL: LazyLock<PostgresPromptStatements> =
    LazyLock::new(|| PostgresPromptStatements::render(Dialect::postgres()));

/// The advisory-lock seed of prompt-text retention (`connection_sql`).
pub(super) const TEXT_LOCK_SEED: i64 = 2;

/// The reclaim's closure read and its children read, which the one-call
/// commit runs as its own mechanics beside [`RECLAIM`]'s delete.
pub(super) fn closure() -> &'static lash_store_sql::Rendered {
    &POSTGRES_SQL.closure
}

pub(super) fn read_children() -> &'static lash_store_sql::Rendered {
    &SQL.read_children
}

#[cfg(test)]
lash_store_sql::statements! {
    /// The `prompt_texts` statements only PostgreSQL's laws issue.
    pub(crate) struct PromptLawStatements @ "durable_prompt_postgres_law" {
        /// Hold the text at `?1` against every edge to it until this
        /// transaction ends: an edge's foreign key check waits behind it.
        hold_text = "SELECT hash FROM prompt_texts WHERE hash = ?1 FOR UPDATE";
    }
}

/// The statements PostgreSQL's prompt laws issue, rendered once.
#[cfg(test)]
pub(crate) static LAW_SQL: LazyLock<PromptLawStatements> =
    LazyLock::new(|| PromptLawStatements::render(Dialect::postgres()));

/// The prompt statements an owner commit runs as steps.
pub(super) static INSERT_SNAPSHOT: Entry = Entry::new(
    || &SQL.insert_snapshot,
    &[Ty::Text, Ty::Text, Ty::Text, Ty::Text, Ty::Text, Ty::Int],
    Shape::Returning,
);
pub(super) static LOCK_TEXTS: Entry = Entry::new(
    || &connection_sql().lock_xact_shared_by_text_batch_seeded,
    &[Ty::Texts, Ty::Int],
    Shape::Select,
);
pub(super) static UNSENT_ABSENT: Entry = Entry::new(
    || &POSTGRES_SQL.unsent_absent,
    &[Ty::Texts, Ty::Texts],
    Shape::Select,
);
pub(super) static INSERT_SENT_TEXTS: Entry = Entry::new(
    || &POSTGRES_SQL.insert_sent_texts,
    &[Ty::Texts, Ty::Texts],
    Shape::Exec,
);
pub(super) static INSERT_EDGES: Entry = Entry::new(
    || &POSTGRES_SQL.insert_edges,
    &[Ty::Text, Ty::Text, Ty::Text, Ty::Texts, Ty::Texts],
    Shape::Exec,
);

/// A settlement's and a prefix reset's statements (FIG-5942): each answers
/// the texts it released, a settlement keeps the session's prefix, and the
/// reclaim peels what no root references any more.
pub(super) static SETTLE: Entry = Entry::new(
    || &POSTGRES_SQL.settle,
    &[Ty::Text, Ty::Text, Ty::Text],
    Shape::Select,
)
.collecting(Collect::Text("hash"));
pub(super) static HOLD_PREFIX: Entry =
    Entry::new(|| &POSTGRES_SQL.hold_prefix, &[Ty::Text], Shape::Select)
        .collecting(Collect::Protect("hash"));
pub(super) static RELEASE_PREFIX: Entry =
    Entry::new(|| &SQL.release_prefix, &[Ty::Text], Shape::Returning)
        .collecting(Collect::Text("hash"));
pub(super) static RECLAIM: Entry =
    Entry::new(|| &POSTGRES_SQL.reclaim_texts, &[], Shape::TextReclaim);

/// The shared revision-material program collects former audit roots before
/// reclaiming their last-reference texts in the same transaction.
pub(super) static EXPIRE_AUDITS: Entry = Entry::new(
    || &POSTGRES_SQL.expire_audits,
    &[Ty::Text, Ty::Int],
    Shape::Select,
)
.collecting(Collect::Text("hash"));

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
        .map(|row| row.try_get::<Option<String>, _>("hash"))
        .collect::<Result<Vec<_>, _>>()?
        .into_iter()
        .flatten()
        .collect();
    reclaim(tx, roots).await?;
    Ok(usize::try_from(count).unwrap_or(usize::MAX))
}

/// The capture slot of a settlement's released texts.
const SETTLED: u8 = 4;

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
        )
        .capturing(SETTLED),
        Step::any(&HOLD_PREFIX, vec![Value::text(session.as_str())]).when_row(SETTLED, true),
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
pub(super) fn steps(epoch: Epoch, write: &PromptWrite, held: &HeldTexts) -> Option<Vec<Step>> {
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
    // The direct path publishes fragment children and replaces or releases a
    // turn's session prefix. This step program only represents leaf texts
    // without a prefix transition, so keep those writes on the direct path.
    if prefix.is_some()
        || matches!(&call.call, ModelCallId::Turn { .. })
        || snapshot_texts
            .iter()
            .chain(request_texts)
            .any(|text| !text.children.is_empty())
    {
        return None;
    }
    let mut program = vec![Step::one(
        &INSERT_SNAPSHOT,
        vec![
            Value::text(call.session.as_str()),
            Value::text(call.call.owner_column()),
            Value::text(call.call.call_column()),
            Value::Text(snapshot.clone()),
            Value::text(request.clone()),
            Value::int(epoch.0),
        ],
    )];
    // The first text under each address stands for it.
    let mut distinct = BTreeMap::new();
    for text in snapshot_texts.iter().chain(request_texts) {
        distinct.entry(text.hash.as_str()).or_insert(&*text.text);
    }
    if distinct.is_empty() {
        return Some(program);
    }
    let hashes: Vec<Option<String>> = distinct
        .keys()
        .map(|hash| Some((*hash).to_owned()))
        .collect();
    let bodies: Vec<Option<String>> = distinct
        .iter()
        .map(|(hash, text)| (!held.contains(hash)).then(|| (*text).to_owned()))
        .collect();
    let (parts, edges): (Vec<Option<String>>, Vec<Option<String>>) =
        edges(snapshot_texts, request_texts)
            .into_iter()
            .map(|(part, hash)| (Some(part.to_owned()), Some(hash.to_owned())))
            .unzip();
    program.extend([
        Step::any(
            &LOCK_TEXTS,
            vec![Value::Texts(hashes.clone()), Value::int(TEXT_LOCK_SEED)],
        ),
        Step::new(
            &UNSENT_ABSENT,
            vec![Value::Texts(hashes.clone()), Value::Texts(bodies.clone())],
            Some(0),
        ),
        Step::any(
            &INSERT_SENT_TEXTS,
            vec![Value::Texts(hashes), Value::Texts(bodies)],
        ),
        Step::any(
            &INSERT_EDGES,
            vec![
                Value::text(call.session.as_str()),
                Value::text(call.call.owner_column()),
                Value::text(call.call.call_column()),
                Value::Texts(parts),
                Value::Texts(edges),
            ],
        ),
    ]);
    Some(program)
}

pub(super) async fn apply(
    tx: &mut PgConnection,
    commit: &Committing<'_>,
    write: &PromptWrite,
) -> Result<(), DurableError> {
    match write {
        PromptWrite::Record {
            call,
            snapshot,
            request,
            snapshot_texts,
            request_texts,
            prefix,
        } => {
            let recorded = crate::observed_sql::query(SQL.insert_snapshot.sql())
                .bind(call.session.as_str())
                .bind(call.call.owner_column())
                .bind(call.call.call_column())
                .bind(snapshot.as_deref())
                .bind(request)
                .bind(commit.epoch.0)
                .fetch_optional(crate::observed_sql::executor(&mut *tx))
                .await
                .map_err(sqlx_failure)?;
            if recorded.is_none() {
                return Err(DurableError::Domain(DomainRefusal::PromptCallRecorded {
                    call: call.clone(),
                }));
            }
            let distinct: BTreeMap<_, _> = snapshot_texts
                .iter()
                .chain(request_texts)
                .map(|text| (text.hash.as_str(), text))
                .collect();
            let mut frontier: BTreeSet<&str> = snapshot_texts
                .iter()
                .map(|text| text.hash.as_str())
                .chain(request_roots(request_texts))
                .chain(prefix.as_deref())
                .collect();
            let mut unheld = BTreeMap::new();
            let mut protected = BTreeSet::new();
            while !frontier.is_empty() {
                let hashes: Vec<_> = frontier.iter().copied().collect();
                crate::observed_sql::query(
                    connection_sql().lock_xact_shared_by_text_batch_seeded.sql(),
                )
                .bind(&hashes)
                .bind(TEXT_LOCK_SEED)
                .execute(crate::observed_sql::executor(&mut *tx))
                .await
                .map_err(sqlx_failure)?;
                let held: BTreeSet<String> =
                    crate::observed_sql::query_scalar(POSTGRES_SQL.held_texts.sql())
                        .bind(&hashes)
                        .fetch_all(crate::observed_sql::executor(&mut *tx))
                        .await
                        .map_err(sqlx_failure)?
                        .into_iter()
                        .collect();
                protected.extend(held.iter().cloned());
                let mut next = BTreeSet::new();
                for hash in frontier {
                    if held.contains(hash) || unheld.contains_key(hash) {
                        continue;
                    }
                    let Some(text) = distinct.get(hash) else {
                        return Err(DurableError::Domain(DomainRefusal::PromptFragmentMissing {
                            hash: hash.to_owned(),
                        }));
                    };
                    next.extend(text.children.iter().map(String::as_str));
                    unheld.insert(hash, text);
                }
                frontier = next;
            }
            if !unheld.is_empty() {
                let (hashes, bodies): (Vec<_>, Vec<_>) = unheld
                    .iter()
                    .map(|(hash, text)| (*hash, &*text.text))
                    .unzip();
                crate::observed_sql::query(POSTGRES_SQL.insert_texts.sql())
                    .bind(&hashes)
                    .bind(&bodies)
                    .execute(crate::observed_sql::executor(&mut *tx))
                    .await
                    .map_err(sqlx_failure)?;
                let paths: BTreeSet<_> = unheld
                    .iter()
                    .flat_map(|(hash, text)| {
                        text.children
                            .iter()
                            .map(move |child| (*hash, child.as_str()))
                    })
                    .collect();
                let (parents, children): (Vec<_>, Vec<_>) = paths.into_iter().unzip();
                crate::observed_sql::query(POSTGRES_SQL.insert_index_edges.sql())
                    .bind(&parents)
                    .bind(&children)
                    .execute(crate::observed_sql::executor(&mut *tx))
                    .await
                    .map_err(sqlx_failure)?;
            }
            let (parts, edges): (Vec<&str>, Vec<&str>) =
                edges(snapshot_texts, request_texts).into_iter().unzip();
            crate::observed_sql::query(POSTGRES_SQL.insert_edges.sql())
                .bind(call.session.as_str())
                .bind(call.call.owner_column())
                .bind(call.call.call_column())
                .bind(&parts)
                .bind(&edges)
                .execute(crate::observed_sql::executor(&mut *tx))
                .await
                .map_err(sqlx_failure)?;
            if matches!(call.call, ModelCallId::Turn { .. }) {
                let old: Vec<String> = crate::observed_sql::query_scalar(SQL.release_prefix.sql())
                    .bind(call.session.as_str())
                    .fetch_all(crate::observed_sql::executor(&mut *tx))
                    .await
                    .map_err(sqlx_failure)?;
                if let Some(prefix) = prefix {
                    crate::observed_sql::query(SQL.set_prefix.sql())
                        .bind(call.session.as_str())
                        .bind(prefix)
                        .execute(crate::observed_sql::executor(&mut *tx))
                        .await
                        .map_err(sqlx_failure)?;
                }
                reclaim_with_protection(tx, old, &protected)
                    .await
                    .map_err(sqlx_failure)?;
            }
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

/// Each distinct `(part, hash)` edge of a root over `snapshot_texts` and
/// `request_texts`.
fn edges<'a>(
    snapshot_texts: &'a [PromptText],
    request_texts: &'a [PromptText],
) -> BTreeSet<(&'static str, &'a str)> {
    let part =
        |part, texts: &'a [PromptText]| texts.iter().map(move |text| (part, text.hash.as_str()));
    part(SNAPSHOT_PART, snapshot_texts)
        .chain(
            request_roots(request_texts)
                .into_iter()
                .map(|hash| (REQUEST_PART, hash)),
        )
        .collect()
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
            let hashes = crate::observed_sql::query_scalar(SQL.release_session_edges.sql())
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
            let hashes = crate::observed_sql::query_scalar(SQL.release_run_edges.sql())
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
            crate::observed_sql::query_scalar::<_, String>(SQL.release_prefix.sql())
                .bind(session.as_str())
                .fetch_all(crate::observed_sql::executor(&mut *tx))
                .await?,
        );
    }
    reclaim(tx, released).await
}

/// Reclaim every text among `released` no remaining edge references.
async fn reclaim(tx: &mut PgConnection, released: Vec<String>) -> Result<(), sqlx::Error> {
    reclaim_with_protection(tx, released, &BTreeSet::new()).await
}

pub(super) async fn reclaim_with_protection(
    tx: &mut PgConnection,
    released: Vec<String>,
    protected: &BTreeSet<String>,
) -> Result<(), sqlx::Error> {
    let released: Vec<String> = released
        .into_iter()
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    if released.is_empty() {
        return Ok(());
    }
    // Gather before locking, then use fresh statements after the locks.
    // Publication of an existing node locks that node and stops there;
    // its closure therefore remains protected throughout publication.
    let closure: Vec<String> = crate::observed_sql::query_scalar(POSTGRES_SQL.closure.sql())
        .bind(&released)
        .bind(protected.iter().collect::<Vec<_>>())
        .fetch_all(crate::observed_sql::executor(&mut *tx))
        .await?;
    crate::observed_sql::query(connection_sql().lock_xact_by_text_batch_seeded.sql())
        .bind(&closure)
        .bind(TEXT_LOCK_SEED)
        .execute(crate::observed_sql::executor(&mut *tx))
        .await?;
    let mut frontier: Vec<_> = released
        .into_iter()
        .filter(|hash| !protected.contains(hash))
        .collect();
    while !frontier.is_empty() {
        let mut children = BTreeMap::new();
        for hash in &frontier {
            let found: Vec<String> = crate::observed_sql::query_scalar(SQL.read_children.sql())
                .bind(hash)
                .fetch_all(crate::observed_sql::executor(&mut *tx))
                .await?;
            children.insert(hash.clone(), found);
        }
        let removed: Vec<String> =
            crate::observed_sql::query_scalar(POSTGRES_SQL.reclaim_texts.sql())
                .bind(&frontier)
                .fetch_all(crate::observed_sql::executor(&mut *tx))
                .await?;
        frontier = removed
            .iter()
            .flat_map(|hash| children[hash].iter().cloned())
            .filter(|hash| !protected.contains(hash))
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect();
    }
    Ok(())
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
    let edges: Vec<(String, String)> = crate::observed_sql::query_as(SQL.read_edges.sql())
        .bind(call.session.as_str())
        .bind(call.call.owner_column())
        .bind(call.call.call_column())
        .fetch_all(crate::observed_sql::executor(&mut *tx))
        .await
        .map_err(sqlx_failure)?;
    let (request_texts, snapshot_texts): (Vec<_>, Vec<_>) = edges
        .into_iter()
        .partition(|(part, _)| part == REQUEST_PART);
    let hashes = |edges: Vec<(String, String)>| edges.into_iter().map(|(_, hash)| hash).collect();
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
    let mut found = Vec::with_capacity(hashes.len());
    for hash in hashes {
        let text: Option<String> = crate::observed_sql::query_scalar(SQL.read_text.sql())
            .bind(hash)
            .fetch_optional(crate::observed_sql::executor(&mut *tx))
            .await
            .map_err(sqlx_failure)?;
        if let Some(text) = text {
            found.push(PromptText {
                hash: hash.clone(),
                children: crate::observed_sql::query_scalar(SQL.read_children.sql())
                    .bind(hash)
                    .fetch_all(crate::observed_sql::executor(&mut *tx))
                    .await
                    .map_err(sqlx_failure)?,
                text: text.into(),
            });
        }
    }
    Ok(found)
}

#[cfg(test)]
mod tests {
    use super::*;
    use lash_core_execution::{SessionCatalogStore as _, StoreMaintenance as _, Target};
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
            std::sync::Arc::new(lash_core_execution::attachments::UnavailableAttachmentStore),
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
        let text = |hash: &str| PromptText {
            hash: hash.to_owned(),
            text: hash.to_owned().into(),
            children: Vec::new(),
        };
        let mut tx = store.begin(&actor, Epoch(identity.epoch)).await.unwrap();
        for ordinal in 1..=131 {
            tx.write(DomainWrite::Prompt(PromptWrite::Record {
                call: call(ordinal),
                snapshot: Some(format!("audit-{ordinal}")),
                request: format!("exact-request-{ordinal}"),
                snapshot_texts: vec![text("shared"), text(&format!("unique-{ordinal}"))],
                request_texts: vec![text("request"), text("prefix")],
                prefix: Some("prefix".to_owned()),
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
            "INSERT INTO lash_blobs (hash, content, encoding) VALUES ('config', decode('00', 'hex'), 0)",
            "INSERT INTO lash_session_meta (session_id, relation_kind, retention_kind) VALUES ('sql-work-owner', 'root', 'head_only')",
            "INSERT INTO lash_session_revisions (session_id, head_revision, config_ref) VALUES ('sql-work-owner', 1, 'config'), ('sql-work-owner', 2, 'config')",
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
        let admission = lash_core_execution::store::RunAdmissionRecord::Command {
            batch: lash_core_execution::BatchId::from("finish"),
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
        .map(str::to_owned);
        let found = store.prompt_texts(&hashes).await.unwrap();
        assert_eq!(
            found
                .iter()
                .map(|text| text.hash.as_str())
                .collect::<Vec<_>>(),
            ["shared", "unique-131", "request", "prefix"]
        );
        // Settling the last request does not erase previously expired identities.
        let mut tx = store.begin(&actor, Epoch(identity.epoch)).await.unwrap();
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
                cause: Box::new(lash_core_execution::store::RunTerminalCause::CommandsApplied),
                head_revision: Some(2),
                transcript: None,
            },
        ));
        store
            .commit(tx, CommitLabel::new("model.done"))
            .await
            .unwrap();
        assert_eq!(
            store.prompt_texts(&hashes).await.unwrap(),
            [text("prefix")],
            "the current lowering prefix keeps its own independent reference"
        );
        let mut tx = store.begin(&actor, Epoch(identity.epoch)).await.unwrap();
        tx.write(DomainWrite::Prompt(PromptWrite::Record {
            call: call(1),
            snapshot: None,
            request: "changed".to_owned(),
            snapshot_texts: Vec::new(),
            request_texts: Vec::new(),
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
