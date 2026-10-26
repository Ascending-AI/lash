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
    RequestTexts,
};
use lash_durable::{DurableError, Epoch};
use lash_kernel_value::identity::SessionId;
use lash_store_sql::Dialect;
use lash_store_sql::durable::prompts::{PromptStatements, REQUEST_PART, SNAPSHOT_PART};
use lash_vocabulary::identity::TurnId;
use sqlx::PgConnection;

use super::steps::{Collect, Entry, Shape, Step, Ty, Value};
use super::{Committing, HeldTexts, get, sqlx_failure};

pub(super) static SQL: LazyLock<PromptStatements> =
    LazyLock::new(|| PromptStatements::render(Dialect::postgres()));

lash_store_sql::statements! {
    /// The `prompt_texts` and `prompt_snapshot_texts` statements only
    /// PostgreSQL issues: `unnest` is how it binds a call's texts as one
    /// list.
    pub(super) struct PostgresPromptStatements @ "durable_prompt_postgres" {
        /// Every content address in `?1` already stored.
        held_texts = "SELECT hash FROM prompt_texts WHERE hash = ANY(?1::BYTEA[])";

        /// Share-lock the texts `?1` under seed `?2`, then answer which of
        /// them are stored, read once every lock is held
        /// (`lash_prompt_held_texts`).
        held_after_lock = "SELECT lash_prompt_held_texts(?1, ?2)";

        /// Store each text `?2[i]` at content address `?1[i]`, skipping every
        /// address a concurrent record stored first: the address names the
        /// bytes, so a conflict is the same text.
        insert_texts = "INSERT INTO prompt_texts (hash, text)
             SELECT hash, text FROM unnest(?1::BYTEA[], ?2::TEXT[]) AS batch(hash, text)
             ON CONFLICT (hash) DO NOTHING";

        /// Publish a prompt's immutable forest and prefix under its text
        /// locks. The routine returns the first typed refusal's facts.
        record = "SELECT * FROM lash_prompt_record(?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15)";

        /// Root text `?5[i]` under part `?4[i]` of call `?3` of owner `?2`
        /// in session `?1`, for every `i`.
        insert_edges = "INSERT INTO prompt_snapshot_texts (session_id, owner, call, part, hash)
             SELECT ?1, ?2, ?3, part, hash
               FROM unnest(?4::TEXT[], ?5::BYTEA[]) AS batch(part, hash)";

        /// Install each new immutable node's child edges once.
        insert_index_edges = "INSERT INTO prompt_text_edges (parent, child)
             SELECT parent, child FROM unnest(?1::BYTEA[], ?2::BYTEA[]) AS batch(parent, child)
             ON CONFLICT (parent, child) DO NOTHING";

        /// Anchor the session's current prefix until settlement commits.
        /// A concurrent invalidation cannot remove this row while its
        /// immutable closure is excluded from reclamation.
        hold_prefix = "SELECT hash FROM prompt_prefixes WHERE session_id = ?1 FOR SHARE";

        /// Lock the candidate closure before a sorted, atomic peel. Nodes
        /// this publication protects stop traversal; a concurrent prefix
        /// release cannot invalidate that protection.
        closure = "WITH RECURSIVE closure AS (
                 SELECT unnest(?1::BYTEA[]) AS hash
                 UNION
                 SELECT edge.child FROM closure
                   JOIN prompt_text_edges AS edge ON edge.parent = closure.hash
                  WHERE NOT (closure.hash = ANY(?2::BYTEA[]))
             ) SELECT hash FROM closure WHERE NOT (hash = ANY(?2::BYTEA[])) ORDER BY hash";

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

        /// Persist released prompt roots as cleanup obligations.
        retirement_put = "INSERT INTO prompt_retirement_frontier(hash) SELECT unnest(?1::BYTEA[]) ON CONFLICT DO NOTHING";

        /// Bounded durable frontier, claimed in digest order.
        retirement_seeds = "SELECT hash FROM prompt_retirement_frontier ORDER BY hash LIMIT ?1 FOR UPDATE SKIP LOCKED";

        /// Shared or rooted boundaries stop discovery. The outer LIMIT
        /// bounds the lazy recursive evaluation before any sort or join.
        retirement_candidates = "WITH RECURSIVE candidates AS (
             SELECT unnest(?1::BYTEA[]) AS hash
             UNION
             SELECT edge.child AS hash FROM candidates JOIN prompt_text_edges AS edge ON edge.parent = candidates.hash
              WHERE NOT (candidates.hash = ANY(?2::BYTEA[]))
                AND NOT EXISTS (SELECT 1 FROM prompt_snapshot_texts WHERE hash = candidates.hash)
                AND NOT EXISTS (SELECT 1 FROM prompt_prefixes WHERE hash = candidates.hash)
                AND (NOT (candidates.hash = ANY(?1::BYTEA[]))
                     OR NOT EXISTS (SELECT 1 FROM prompt_text_edges WHERE child = candidates.hash))
                AND (SELECT count(*) FROM (SELECT 1 FROM prompt_text_edges WHERE child = candidates.hash LIMIT 2) AS incoming) <= 1
         ) SELECT hash FROM candidates LIMIT ?3";

        /// Liveness is recomputed after candidate locks. A skipped external
        /// parent protects all candidate descendants, including diamonds.
        retirement_unprotected = "WITH RECURSIVE candidates AS (SELECT unnest(?1::BYTEA[]) AS hash), protected AS (
             SELECT hash FROM candidates
              WHERE hash = ANY(?2::BYTEA[])
                 OR EXISTS (SELECT 1 FROM prompt_snapshot_texts WHERE hash = candidates.hash)
                 OR EXISTS (SELECT 1 FROM prompt_prefixes WHERE hash = candidates.hash)
                 OR EXISTS (SELECT 1 FROM prompt_text_edges AS edge WHERE edge.child = candidates.hash AND NOT (edge.parent = ANY(?1::BYTEA[])))
             UNION
             SELECT edge.child AS hash FROM protected JOIN prompt_text_edges AS edge ON edge.parent = protected.hash WHERE edge.child = ANY(?1::BYTEA[])
         ) SELECT hash FROM candidates WHERE hash NOT IN (SELECT hash FROM protected)";

        /// Retirement never waits behind a publisher; skipped candidates
        /// keep their durable queue entries and protect their descendants.
        retirement_try_lock = "WITH ordered AS MATERIALIZED (SELECT DISTINCT key FROM unnest(?1::BYTEA[]) AS target(key) ORDER BY key)
             SELECT key FROM ordered WHERE pg_try_advisory_xact_lock(hashtextextended(encode(key, 'hex'), 2))";

        retirement_take = "DELETE FROM prompt_retirement_frontier WHERE hash = ANY(?1::BYTEA[])";
        retirement_edges = "DELETE FROM prompt_text_edges WHERE parent = ANY(?1::BYTEA[]) RETURNING child";
        retirement_nodes = "DELETE FROM prompt_texts WHERE hash = ANY(?1::BYTEA[]) RETURNING octet_length(text) AS bytes";
        reclaim_texts = "SELECT * FROM lash_prompt_retire(?1, ?2, ?3)";

    }
}

pub(super) static POSTGRES_SQL: LazyLock<PostgresPromptStatements> =
    LazyLock::new(|| PostgresPromptStatements::render(Dialect::postgres()));

#[cfg(test)]
pub(super) const TEXT_LOCK_SEED: i64 = 2;

/// Statements the generated retirement routine executes, in program order.
pub(super) fn retirement_mechanics() -> Vec<&'static str> {
    let pg = &*POSTGRES_SQL;
    vec![
        pg.retirement_put.sql(),
        pg.retirement_seeds.sql(),
        pg.retirement_candidates.sql(),
        pg.retirement_try_lock.sql(),
        pg.retirement_unprotected.sql(),
        pg.retirement_take.sql(),
        pg.retirement_edges.sql(),
        pg.retirement_nodes.sql(),
        pg.retirement_put.sql(),
    ]
}

/// The server function that runs one level of a record's text frontier:
/// its lock, then its held read (FIG-7049).
#[cfg(test)]
pub(super) const HELD_TEXTS_FUNCTION: &str = "lash_prompt_held_texts";

/// The held read [`HELD_TEXTS_FUNCTION`] runs after its lock.
#[cfg(test)]
pub(super) fn held_texts() -> &'static lash_store_sql::Rendered {
    &POSTGRES_SQL.held_texts
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
pub(super) static RECORD: Entry = Entry::new(
    || &POSTGRES_SQL.record,
    &[
        Ty::Text,
        Ty::Text,
        Ty::Text,
        Ty::Text,
        Ty::Text,
        Ty::Int,
        Ty::Keys,
        Ty::Texts,
        Ty::Keys,
        Ty::Keys,
        Ty::Keys,
        Ty::Texts,
        Ty::Keys,
        Ty::Key,
        Ty::Bool,
    ],
    Shape::Select,
)
.flagged("accepted");

/// Statements of the prompt-record routine, included in the owner registry.
pub(super) fn record_mechanics() -> Vec<&'static str> {
    vec![
        SQL.insert_snapshot.sql(),
        POSTGRES_SQL.held_after_lock.sql(),
        POSTGRES_SQL.insert_texts.sql(),
        POSTGRES_SQL.insert_index_edges.sql(),
        POSTGRES_SQL.insert_edges.sql(),
        SQL.release_prefix.sql(),
        SQL.set_prefix.sql(),
    ]
}

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
        .map(|row| crate::keys::get::<Option<String>>(row, "hash"))
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
    // Keep the request's root list intact. A held immutable root stops the
    // server frontier, even when its descendants were not supplied.
    let roots: BTreeSet<_> = snapshot_texts
        .iter()
        .map(|text| text.hash.clone())
        .chain(request_texts.roots().iter().cloned())
        .chain(prefix.iter().cloned())
        .collect();
    let snapshots: BTreeMap<_, _> = snapshot_texts
        .iter()
        .map(|text| (text.hash.as_str(), text))
        .collect();
    let supplied = request_texts.supplied_delta().map(|delta| {
        delta
            .map(|text| (text.hash.as_str(), text))
            .chain(snapshots.iter().map(|(hash, text)| (*hash, *text)))
            .collect::<BTreeMap<_, _>>()
    });
    let mut texts = BTreeMap::new();
    let mut frontier = roots.clone();
    let mut validation = Vec::new();
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
            let text = match &supplied {
                Some(supplied) => supplied.get(hash.as_str()).copied(),
                None if held.contains(&hash) => None,
                None => request_texts
                    .get(&hash)
                    .or_else(|| snapshots.get(hash.as_str()).copied()),
            };
            if let Some(text) = text {
                next.extend(text.children.iter().cloned());
            }
            validation.push(Some(hash.clone()));
            texts.insert(hash, text);
        }
        frontier = next;
    }
    let (hashes, bodies): (Vec<_>, Vec<_>) = texts
        .iter()
        .map(|(hash, text)| (Some(hash.clone()), text.map(|text| text.text.to_string())))
        .unzip();
    let paths: BTreeSet<_> = texts
        .iter()
        .flat_map(|(hash, text)| {
            text.iter().flat_map(move |text| {
                text.children
                    .iter()
                    .map(move |child| (hash.clone(), child.clone()))
            })
        })
        .collect();
    let (parents, children): (Vec<_>, Vec<_>) = paths
        .into_iter()
        .map(|(parent, child)| (Some(parent), Some(child)))
        .unzip();
    let (parts, edges): (Vec<_>, Vec<_>) = edges(snapshot_texts, request_texts)
        .into_iter()
        .map(|(part, hash)| (Some(part.to_owned()), Some(hash.to_owned())))
        .unzip();
    Some(vec![Step::one(
        &RECORD,
        vec![
            Value::text(call.session.as_str()),
            Value::text(call.call.owner_column()),
            Value::text(call.call.call_column()),
            Value::Text(snapshot.clone()),
            Value::text(request),
            Value::int(epoch.0),
            Value::Keys(hashes),
            Value::Texts(bodies),
            Value::Keys(parents),
            Value::Keys(children),
            Value::Keys(validation),
            Value::Texts(parts),
            Value::Keys(edges),
            Value::Key(prefix.clone()),
            Value::Bool(matches!(call.call, ModelCallId::Turn { .. })),
        ],
    )])
}

pub(super) async fn apply(
    tx: &mut PgConnection,
    commit: &Committing<'_>,
    write: &PromptWrite,
) -> Result<(), DurableError> {
    match write {
        PromptWrite::Record { call, .. } => {
            // The direct form supplies every body. A stale held-text hint in
            // the owner call rolls back before this path names the refusal.
            let program = steps(commit.epoch, write, &HeldTexts::default()).unwrap_or_default();
            if let Some(refused) = super::steps::run(tx, &program, &commit.steps()).await? {
                let missing = refused
                    .rows
                    .first()
                    .map(|row| crate::keys::get::<Option<String>>(row, "missing"))
                    .transpose()
                    .map_err(sqlx_failure)?
                    .flatten();
                return Err(DurableError::Domain(match missing {
                    Some(hash) => DomainRefusal::PromptFragmentMissing { hash },
                    None => DomainRefusal::PromptCallRecorded { call: call.clone() },
                }));
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
    request_texts: &'a RequestTexts,
) -> BTreeSet<(&'static str, &'a str)> {
    snapshot_texts
        .iter()
        .map(|text| (SNAPSHOT_PART, text.hash.as_str()))
        .chain(
            request_texts
                .roots()
                .iter()
                .map(|hash| (REQUEST_PART, hash.as_str())),
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

/// Reclaim every text among `released` no remaining edge references.
async fn reclaim(tx: &mut PgConnection, released: Vec<String>) -> Result<(), sqlx::Error> {
    reclaim_with_protection(tx, released, &BTreeSet::new()).await
}

pub(super) async fn reclaim_with_protection(
    tx: &mut PgConnection,
    released: Vec<String>,
    protected: &BTreeSet<String>,
) -> Result<(), sqlx::Error> {
    if released.is_empty() {
        return Ok(());
    }
    crate::observed_sql::query(POSTGRES_SQL.reclaim_texts.sql())
        .bind(crate::keys::keys(&released))
        .bind(crate::keys::keys(protected))
        .bind(4096_i64)
        .execute(crate::observed_sql::executor(tx))
        .await?;
    Ok(())
}

pub(super) async fn drain_retired(
    tx: &mut PgConnection,
    limit: u64,
) -> Result<(i64, i64, i64), sqlx::Error> {
    crate::observed_sql::query_as(POSTGRES_SQL.reclaim_texts.sql())
        .bind(Vec::<Vec<u8>>::new())
        .bind(Vec::<Vec<u8>>::new())
        .bind(i64::try_from(limit.max(1)).unwrap_or(i64::MAX))
        .fetch_one(crate::observed_sql::executor(tx))
        .await
}

/// The single set-based retirement program used by explicit and generated
/// owner commits. Allocation hints do not authorize any delete.
#[cfg(test)]
pub(super) fn retirement_function_ddl() -> String {
    let pg = &*POSTGRES_SQL;
    let inline = |sql: &str, args: &[&str]| super::generator::inline_statement(sql, args);
    let put = inline(pg.retirement_put.sql(), &["p_roots"]);
    let seeds = inline(pg.retirement_seeds.sql(), &["p_limit"]);
    let candidates = inline(
        pg.retirement_candidates.sql(),
        &["v_seeds", "p_protect", "p_limit"],
    );
    let lock = inline(pg.retirement_try_lock.sql(), &["v_candidates"]);
    let unprotected = inline(
        pg.retirement_unprotected.sql(),
        &["v_candidates", "p_protect"],
    );
    let take = inline(pg.retirement_take.sql(), &["v_candidates"]);
    let edges = inline(pg.retirement_edges.sql(), &["v_gone"]);
    let nodes = inline(pg.retirement_nodes.sql(), &["v_gone"]);
    let follow = inline(pg.retirement_put.sql(), &["v_next"]);
    format!("CREATE OR REPLACE FUNCTION lash_prompt_retire(p_roots BYTEA[], p_protect BYTEA[], p_limit BIGINT)
RETURNS TABLE(examined BIGINT, deleted BIGINT, bytes BIGINT)
LANGUAGE plpgsql VOLATILE AS $lash_prompt_retire$
DECLARE v_seeds BYTEA[]; v_candidates BYTEA[]; v_gone BYTEA[]; v_next BYTEA[];
        v_count BIGINT := 0; v_bytes BIGINT := 0;
BEGIN
    {put};
    v_seeds := ARRAY({seeds});
    IF cardinality(v_seeds) = 0 THEN RETURN QUERY SELECT 0::BIGINT, 0::BIGINT, 0::BIGINT; RETURN; END IF;
    v_candidates := ARRAY({candidates});
    v_candidates := ARRAY({lock});
    IF cardinality(v_candidates) = 0 THEN RETURN QUERY SELECT 0::BIGINT, 0::BIGINT, 0::BIGINT; RETURN; END IF;
    v_gone := ARRAY({unprotected});
    {take};
    WITH removed AS ({edges}) SELECT COALESCE(array_agg(DISTINCT child), '{{}}') INTO v_next FROM removed WHERE NOT (child = ANY(v_candidates));
    WITH removed AS ({nodes}) SELECT count(*), COALESCE(sum(removed.bytes),0) INTO v_count,v_bytes FROM removed;
    {follow};
    RETURN QUERY SELECT cardinality(v_candidates)::BIGINT,v_count,v_bytes;
END
$lash_prompt_retire$;
")
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
        crate::observed_sql::query_as(SQL.read_edges.sql())
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
    let mut found = Vec::with_capacity(hashes.len());
    for hash in hashes {
        let text: Option<String> = crate::observed_sql::query_scalar(SQL.read_text.sql())
            .bind(crate::keys::lookup(hash))
            .fetch_optional(crate::observed_sql::executor(&mut *tx))
            .await
            .map_err(sqlx_failure)?;
        if let Some(text) = text {
            found.push(PromptText {
                hash: hash.clone(),
                children: crate::keys::query_scalar(SQL.read_children.sql())
                    .bind(crate::keys::key(hash))
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
        let text = |hash: &str| PromptText {
            hash: lash_core_store::store::BlobRef::for_content(hash.as_bytes()).0,
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
        let mut tx = store.begin(&actor, Epoch(identity.epoch)).await.unwrap();
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
