//! Prompt snapshots on SQLite: the `prompts` domain's statements, apply and
//! read (FIG-5256, ADR 0133 §5).
//!
//! Owned by P2 (FIG-5256). The dispatch in `durable/mod.rs` calls these
//! inside the fenced owner commit, after the fence; a refusal rolls the whole
//! commit back. Session deletion releases through [`release`] inside its own
//! transaction (FIG-5272).
//!
//! SQLite's one writer serializes a record against a release, so a text a
//! record finds stored is never reclaimed under it, and a record binds the
//! bodies of only the texts it does not find (FIG-5946).

use crate::Prepare as _;
use std::collections::{BTreeMap, BTreeSet};
use std::sync::LazyLock;

use lash_durable::domain::{
    DomainRefusal, ModelCallId, PreparedPromptRecord, PromptCallKey, PromptSnapshotRow, PromptText,
    PromptWrite,
};
use lash_durable::{DurableError, Epoch};
use lash_kernel_value::identity::SessionId;
use lash_store_sql::durable::prompts::{PromptStatements, REQUEST_PART};
use lash_vocabulary::identity::TurnId;
use rusqlite::{Connection, OptionalExtension};

use super::{Answer, Committing};
use crate::conn::cached_execute;

static SQL: LazyLock<PromptStatements> =
    LazyLock::new(|| PromptStatements::render(crate::schema_layout::MAIN));

lash_store_sql::statements! {
    /// SQLite's prompt root writes and bounded content reads.
    struct SqlitePromptStatements @ "durable_prompt_sqlite" {
        held_texts = "SELECT hash FROM blobs
             WHERE encoding = 1 AND hash IN (SELECT unhex(value) AS hash FROM json_each(?1))";
        insert_edges = "INSERT INTO prompt_snapshot_texts (session_id, owner, call, part, text_ref)
             SELECT ?1, ?2, ?3, json_extract(value, '$[0]'),
                    COALESCE((SELECT id FROM blobs WHERE hash = unhex(json_extract(value, '$[1]'))), CASE WHEN unhex(json_extract(value, '$[1]')) IS NOT NULL THEN -1 END)
               FROM json_each(?4)";
        read_texts = "WITH RECURSIVE reached AS (
                 SELECT stored.id, 1 AS follow FROM json_each(?1) AS wanted JOIN blobs AS stored ON stored.hash = unhex(wanted.value)
                 WHERE stored.encoding = 1 AND substr(stored.content, 5, 1) IN (x'35', x'36')
                 UNION SELECT edge.child_id,
                     child.encoding = 1 AND substr(child.content, 5, 1) IN (x'01', x'02', x'03', x'04', x'05', x'06', x'07')
                 FROM reached JOIN blob_edges edge ON edge.parent_id = reached.id
                 JOIN blobs child ON child.id = edge.child_id
                 WHERE reached.follow
             ) SELECT blobs.hash, blobs.content,
                 blobs.encoding = 1 AND substr(blobs.content, 5, 1) IN (x'35', x'36')
             FROM reached JOIN blobs ON blobs.id = reached.id";
        read_edges = "WITH RECURSIVE closure AS (
                 SELECT part, text_ref AS id FROM prompt_snapshot_texts
                 WHERE session_id = ?1 AND owner = ?2 AND call = ?3
                 UNION SELECT closure.part, edge.child_id FROM closure
                 JOIN blob_edges edge ON edge.parent_id = closure.id
             ) SELECT closure.part, blobs.hash FROM closure JOIN blobs ON blobs.id = closure.id
               WHERE substr(blobs.content, 5, 1) IN (x'35', x'36') ORDER BY closure.part, blobs.hash";
        settle_edges = "DELETE FROM prompt_snapshot_texts
             WHERE session_id = ?1 AND owner = ?2 AND part = 'request'
               AND (?3 IS NULL OR call <> ?3)
             RETURNING (SELECT hash FROM blobs WHERE id = text_ref) AS hash";
        settle_bare_roots = "DELETE FROM prompt_snapshots
             WHERE session_id = ?1 AND owner = ?2 AND snapshot IS NULL AND request IS NULL
               AND call IN (SELECT value FROM json_each(?3))";
        settle_requests = "UPDATE prompt_snapshots SET request = NULL
             WHERE session_id = ?1 AND owner = ?2 AND request IS NOT NULL
               AND (?3 IS NULL OR call <> ?3)
             RETURNING call, snapshot IS NOT NULL";
        read_prefix = "SELECT (SELECT hash FROM blobs WHERE id = root_ref) AS hash FROM prompt_prefixes
             WHERE session_id = ?1";


    }
}

static SQLITE_SQL: LazyLock<SqlitePromptStatements> =
    LazyLock::new(|| SqlitePromptStatements::render(crate::schema_layout::MAIN));

/// Compose-once call identities, independently retained snapshot/request
/// graph roots, and the session's reusable prefix. Content and child edges
/// belong to the shared graph tables.
pub(crate) const TABLES: &str = "
CREATE TABLE IF NOT EXISTS prompt_snapshots (
    session_id TEXT NOT NULL, owner TEXT NOT NULL, call TEXT NOT NULL,
    snapshot TEXT, request TEXT, written_epoch INTEGER NOT NULL,
    PRIMARY KEY (session_id, owner, call)
);
CREATE INDEX IF NOT EXISTS idx_prompt_snapshots_settled
    ON prompt_snapshots (session_id, owner, call)
    WHERE request IS NULL AND snapshot IS NOT NULL;
CREATE TABLE IF NOT EXISTS prompt_snapshot_texts (
    session_id TEXT NOT NULL, owner TEXT NOT NULL, call TEXT NOT NULL,
    part TEXT NOT NULL CONSTRAINT ck_prompt_snapshot_texts_part CHECK (part IN ('snapshot', 'request')),
    text_ref INTEGER NOT NULL REFERENCES blobs(id),
    PRIMARY KEY (session_id, owner, call, part, text_ref),
    FOREIGN KEY (session_id, owner, call) REFERENCES prompt_snapshots(session_id, owner, call)
) WITHOUT ROWID;
CREATE INDEX IF NOT EXISTS idx_prompt_snapshot_texts_text_ref ON prompt_snapshot_texts(text_ref);
CREATE TABLE IF NOT EXISTS prompt_prefixes (
    session_id TEXT PRIMARY KEY, root_ref INTEGER NOT NULL REFERENCES blobs(id)
) WITHOUT ROWID;
CREATE INDEX IF NOT EXISTS idx_prompt_prefixes_root_ref ON prompt_prefixes(root_ref);
";

fn call_params(call: &PromptCallKey) -> (String, String, String) {
    (
        call.session.as_str().to_owned(),
        call.call.owner_column(),
        call.call.call_column(),
    )
}

pub(super) fn apply(tx: &Connection, commit: &Committing<'_>, write: &PromptWrite) -> Answer<()> {
    match write {
        PromptWrite::Record {
            call,
            snapshot,
            request,
            snapshot_texts,
            request_texts,
            prefix,
        } => {
            let (session, owner, ordinal) = call_params(call);
            let recorded = tx
                .statement(SQL.insert_snapshot.sql())?
                .query_row(
                    rusqlite::params![session, owner, ordinal, snapshot, request, commit.epoch.0],
                    |_| Ok(()),
                )
                .optional()?;
            if recorded.is_none() {
                return Ok(Err(DurableError::Domain(
                    DomainRefusal::PromptCallRecorded { call: call.clone() },
                )));
            }
            let prepared =
                PreparedPromptRecord::new(snapshot_texts, request_texts, prefix.as_deref());
            let mut frontier = prepared.roots().clone();
            let mut unheld = BTreeMap::new();
            if let Some(supplied) = prepared.supplied() {
                // One set read validates the complete direct frontier. A
                // remembered digest never resurrects a missing old body.
                frontier.extend(
                    supplied
                        .values()
                        .flat_map(|text| text.children.iter().map(String::as_str)),
                );
                frontier.extend(supplied.keys().copied());
                let held = hash_set(tx, SQLITE_SQL.held_texts.sql(), [encode(&frontier)?])?;
                for hash in frontier {
                    if held.contains(hash) {
                        continue;
                    }
                    let Some(text) = supplied.get(hash) else {
                        return Ok(Err(DurableError::Domain(
                            DomainRefusal::PromptFragmentMissing {
                                hash: hash.to_owned(),
                            },
                        )));
                    };
                    unheld.insert(hash, *text);
                }
            } else {
                // Probe only new index paths: an existing immutable node owns
                // its complete closure, so no child metadata is resubmitted.
                while !frontier.is_empty() {
                    let held = hash_set(tx, SQLITE_SQL.held_texts.sql(), [encode(&frontier)?])?;
                    let mut next = BTreeSet::new();
                    for hash in frontier {
                        if held.contains(hash) || unheld.contains_key(hash) {
                            continue;
                        }
                        let Some(text) = prepared.text(hash) else {
                            return Ok(Err(DurableError::Domain(
                                DomainRefusal::PromptFragmentMissing {
                                    hash: hash.to_owned(),
                                },
                            )));
                        };
                        next.extend(text.children.iter().map(String::as_str));
                        unheld.insert(hash, text);
                    }
                    frontier = next;
                }
            }
            let mut nodes = BTreeMap::new();
            for text in unheld.values() {
                match text.graph_nodes() {
                    Ok(encoded) => nodes.extend(encoded),
                    Err(refusal) => return Ok(Err(DurableError::Graph(refusal))),
                }
            }
            if let Err(refusal) = super::graph::install_staged(tx, &nodes)? {
                return Ok(Err(DurableError::Graph(refusal)));
            }
            cached_execute(
                tx,
                SQLITE_SQL.insert_edges.sql(),
                rusqlite::params![session, owner, ordinal, encode(&prepared.edges())?],
            )?;
            if matches!(call.call, ModelCallId::Turn { .. }) {
                let old = if let Some(prefix) = prefix {
                    let old = hash_set(tx, SQLITE_SQL.read_prefix.sql(), [session.as_str()])?;
                    if old.contains(prefix) {
                        BTreeSet::new()
                    } else {
                        cached_execute(
                            tx,
                            SQL.set_prefix.sql(),
                            rusqlite::params![session, crate::codec::SqlHash(prefix)],
                        )?;
                        old
                    }
                } else {
                    hash_set(tx, SQL.release_prefix.sql(), [session.as_str()])?
                };
                reclaim(tx, &old)?;
            }
            Ok(Ok(()))
        }
        PromptWrite::ResetPrefix { session } => {
            let old = hash_set(tx, SQL.release_prefix.sql(), [session.as_str()])?;
            reclaim(tx, &old)?;
            Ok(Ok(()))
        }
        PromptWrite::Settle { session, run, open } => {
            let owner = ModelCallId::turn_owner(run);
            let open = open.map(|call| call.to_string());
            let params = rusqlite::params![session.as_str(), owner, open];
            let mut statement = tx.statement(SQLITE_SQL.settle_requests.sql())?;
            let calls = statement
                .query_map(params, |row| {
                    Ok((row.get::<_, String>(0)?, row.get::<_, bool>(1)?))
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            if calls.is_empty() {
                return Ok(Ok(()));
            }
            drop(statement);
            let released = hash_set(tx, SQLITE_SQL.settle_edges.sql(), params)?;
            let bare: Vec<_> = calls
                .into_iter()
                .filter_map(|(call, snapshot)| (!snapshot).then_some(call))
                .collect();
            if !bare.is_empty() {
                cached_execute(
                    tx,
                    SQLITE_SQL.settle_bare_roots.sql(),
                    rusqlite::params![session.as_str(), owner, encode(&bare)?],
                )?;
            }
            reclaim(tx, &released)?;
            Ok(Ok(()))
        }
        PromptWrite::Release { session, run } => {
            release(tx, session, run.as_ref())?;
            Ok(Ok(()))
        }
    }
}

/// Release every snapshot root of `session`, or of its turn `run` alone, and
/// reclaim every text no remaining root references.
///
/// # Errors
///
/// The first statement SQLite refused.
pub(crate) fn release(
    tx: &Connection,
    session: &SessionId,
    run: Option<&TurnId>,
) -> rusqlite::Result<()> {
    let mut released = match run {
        None => {
            let hashes = hash_set(
                tx,
                SQL.release_session_edges.sql(),
                rusqlite::params![session.as_str()],
            )?;
            cached_execute(tx, SQL.release_session_snapshots.sql(), [session.as_str()])?;
            hashes
        }
        Some(run) => {
            let hashes = hash_set(
                tx,
                SQL.release_run_edges.sql(),
                rusqlite::params![session.as_str(), ModelCallId::turn_owner(run)],
            )?;
            cached_execute(
                tx,
                SQL.release_run_snapshots.sql(),
                rusqlite::params![session.as_str(), ModelCallId::turn_owner(run)],
            )?;
            hashes
        }
    };
    if run.is_none() {
        released.extend(hash_set(tx, SQL.release_prefix.sql(), [session.as_str()])?);
    }
    reclaim(tx, &released)
}

/// Release one bounded page of settled audit roots. Call identities survive,
/// so a repeated admission remains refused after physical reclamation.
pub(crate) fn release_unretained(
    tx: &Connection,
    session: Option<&SessionId>,
) -> rusqlite::Result<usize> {
    let calls = {
        let mut statement = tx.statement(SQL.expired_calls.sql())?;
        let rows = statement.query_map(
            rusqlite::params![
                session.map(SessionId::as_str),
                i64::from(lash_store_sql::session::revisions::MATERIAL_RELEASE_BATCH)
            ],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                ))
            },
        )?;
        rows.collect::<rusqlite::Result<Vec<_>>>()?
    };
    let mut released = BTreeSet::new();
    for (session, owner, call) in &calls {
        released.extend(hash_set(
            tx,
            SQL.release_audit_edges.sql(),
            rusqlite::params![session, owner, call],
        )?);
        cached_execute(
            tx,
            SQL.expire_audit.sql(),
            rusqlite::params![session, owner, call],
        )?;
    }
    reclaim(tx, &released)?;
    Ok(calls.len())
}

/// Peel one bounded changed-path batch with the shared graph collector.
/// Any unfinished paths become the graph's ordinary retirement holds.
fn reclaim(tx: &Connection, released: &BTreeSet<String>) -> rusqlite::Result<()> {
    if !released.is_empty() {
        let (_, _, next) =
            super::graph::peel_batch(tx, super::graph::Frontier::Digests(released), 4096)?;
        super::pages::hold_nodes(tx, &next, true, lash_durable::DurableInstant(0))?;
    }
    Ok(())
}

fn encode(value: &impl serde::Serialize) -> rusqlite::Result<String> {
    serde_json::to_string(value)
        .map_err(|error| rusqlite::Error::ToSqlConversionFailure(error.into()))
}

/// Run a statement returning content addresses, collecting each distinct
/// one.
fn hash_set(
    tx: &Connection,
    sql: &str,
    params: impl crate::statement::Values,
) -> rusqlite::Result<BTreeSet<String>> {
    let mut statement = tx.statement(sql)?;
    let rows = statement.query_map(params, |row| crate::codec::hash_column(row, 0))?;
    rows.collect()
}

pub(super) fn snapshot(tx: &Connection, call: &PromptCallKey) -> Answer<Option<PromptSnapshotRow>> {
    let (session, owner, ordinal) = call_params(call);
    let Some((snapshot, request, written_epoch, expired)) = tx
        .statement(SQL.read_snapshot.sql())?
        .query_row(rusqlite::params![session, owner, ordinal], |row| {
            Ok((
                row.get::<_, Option<String>>(0)?,
                row.get::<_, Option<String>>(1)?,
                row.get::<_, i64>(2)?,
                row.get::<_, bool>(3)?,
            ))
        })
        .optional()?
    else {
        return Ok(Ok(None));
    };
    if expired {
        return Ok(Err(DurableError::Domain(
            DomainRefusal::PromptAuditExpired { call: call.clone() },
        )));
    }
    // Recover the complete closure in one statement, irrespective of the
    // chronological prefix's depth. Both dialects use the same edge owner.
    let roots = tx
        .statement(SQLITE_SQL.read_edges.sql())?
        .query_map(rusqlite::params![session, owner, ordinal], |row| {
            Ok((row.get::<_, String>(0)?, crate::codec::hash_column(row, 1)?))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let (request_texts, snapshot_texts): (Vec<_>, Vec<_>) = roots
        .into_iter()
        .partition(|(part, _)| part == REQUEST_PART);
    let hashes = |edges: Vec<(String, String)>| edges.into_iter().map(|(_, hash)| hash).collect();
    Ok(Ok(Some(PromptSnapshotRow {
        call: call.clone(),
        snapshot,
        request,
        snapshot_texts: hashes(snapshot_texts),
        request_texts: hashes(request_texts),
        written_epoch: Epoch(written_epoch),
    })))
}

pub(super) fn texts(tx: &Connection, hashes: &[String]) -> Answer<Vec<PromptText>> {
    if hashes.is_empty() {
        return Ok(Ok(Vec::new()));
    }
    use lash_core_store::checkpoint_graph::{EncodedNode, NodeId};
    let list = encode(&hashes)?;
    let mut statement = tx.statement(SQLITE_SQL.read_texts.sql())?;
    let mut roots = BTreeSet::new();
    let mut nodes = BTreeMap::new();
    for row in statement.query_map([&list], |row| {
        let id = crate::codec::node_column(row, 0)?;
        let bytes: Vec<u8> = row.get(1)?;
        Ok((
            id,
            EncodedNode {
                id,
                bytes: bytes.into(),
            },
            row.get::<_, bool>(2)?,
        ))
    })? {
        let (id, node, requested) = row?;
        if requested {
            roots.insert(id);
        }
        nodes.insert(id, node);
    }
    let mut texts = Vec::new();
    for hash in hashes {
        let Some(id) = NodeId::from_hex(hash) else {
            continue;
        };
        if roots.contains(&id) {
            match PromptText::from_graph(&nodes, id, lash_durable::domain::graph::OWNER_READ_LIMITS)
            {
                Ok(text) => texts.push(text),
                Err(refusal) => return Ok(Err(DurableError::Graph(refusal))),
            }
        }
    }
    Ok(Ok(texts))
}

#[cfg(test)]
mod tests {
    use super::*;
    use lash_core_store::store::SessionCatalogStore as _;
    use lash_core_store::store::Target;
    use lash_durable::{ActorKey, CommitLabel, DomainWrite, DurableReads, DurableStore};

    /// PROMPT-RETIRE-WORK: publishing and settling one more turn must not
    /// rewrite or retire paths proportional to the history already retained.
    #[tokio::test]
    async fn prompt_retirement_work_is_independent_of_session_history() {
        let mut bounded = true;
        for (repeated, compacted, rebuilt) in [
            (false, false, false),
            (true, false, false),
            (true, true, false),
            (true, false, true),
        ] {
            bounded &= prompt_history_work(repeated, compacted, rebuilt).await;
        }
        assert!(
            bounded,
            "prompt publication and retirement grow with session history"
        );
    }

    async fn prompt_history_work(repeated: bool, compacted: bool, rebuilt: bool) -> bool {
        use lash_core_ids::operational_metrics::sql::collect;
        use lash_core_store::prompt_sections::OwnerPromptState;
        use lash_protocol_types::llm::types::{
            ProviderRouteIdentity, RecordedRequestTemplate, RequestItems, RequestSegment,
        };
        let set = crate::SqliteStoreSet::memory().await.unwrap();
        let identity = lash_core_execution::testing::sql_work::owner_commit(&set).await;
        let store = set.durable_store();
        let session = SessionId::from("sql-work-owner");
        let actor = ActorKey::session(session.as_str()).unwrap();
        let route = ProviderRouteIdentity {
            provider: "fixture".into(),
            endpoint: "https://fixture.test".into(),
            model: "model".into(),
        };

        // Protected history prefixes no longer enter the retirement queue.
        // Retire a real audit branch beside that retained history each turn,
        // so discovery, dequeue, edge deletion and text deletion stay active.
        let leaf = PromptText::audit("retiring audit");
        let branch = serde_json::to_string(&vec![&leaf.hash]).unwrap();
        let audit = PromptText::fragment(
            branch,
            vec![lash_core_store::checkpoint_graph::NodeId::from_hex(&leaf.hash).unwrap()],
        );
        let mut owner = OwnerPromptState::default();
        let mut items = RequestItems::default();
        let mut early: [Vec<[u64; 3]>; 6] = std::array::from_fn(|_| Vec::new());
        let mut late = early.clone();
        for turn in 1_usize..=1000 {
            if compacted && turn > 1 && (turn - 1).is_multiple_of(100) {
                let mut tx = store
                    .begin(&actor, Epoch(identity.epoch), u64::MAX)
                    .await
                    .unwrap();
                tx.write(DomainWrite::Prompt(PromptWrite::ResetPrefix {
                    session: session.clone(),
                }));
                store.commit(tx, CommitLabel::MODEL_DONE).await.unwrap();
                owner.clear();
                items = RequestItems::default()
                    .append(&[RequestSegment::item_json(&serde_json::json!({
                        "role": "user", "content": "compaction summary"
                    }))
                    .unwrap()])
                    .unwrap();
            }
            for role in if repeated {
                &["user"][..]
            } else {
                &["user", "assistant"][..]
            } {
                items = items
                    .append(&[RequestSegment::item_json(&serde_json::json!({
                        "role": role, "content": if repeated { "history turn".to_owned() } else { format!("history turn {turn}") }
                    }))
                    .unwrap()])
                    .unwrap();
            }
            // Lowerer/cache replacement can supply an equal complete prefix
            // under fresh segment identities. Held ancestors still own it.
            if rebuilt {
                let segments: Vec<_> = items
                    .iter()
                    .map(|item| {
                        RequestSegment::item_json(
                            &serde_json::from_str::<serde_json::Value>(&item.text_lossy()).unwrap(),
                        )
                        .unwrap()
                    })
                    .collect();
                items = RequestItems::default().append(&segments).unwrap();
            }
            let template = RecordedRequestTemplate::from_item_run(
                route.clone(),
                true,
                None,
                "{\"messages\":[".into(),
                items.clone(),
                "]}".into(),
            )
            .unwrap();
            let (chunked, texts) = owner.chunk(&template);
            let run = TurnId::parse(format!("history-{turn}")).unwrap();
            let mut tx = store
                .begin(&actor, Epoch(identity.epoch), u64::MAX)
                .await
                .unwrap();
            tx.write(DomainWrite::Prompt(PromptWrite::Record {
                call: PromptCallKey {
                    session: session.clone(),
                    call: ModelCallId::Turn {
                        run: run.clone(),
                        ordinal: 1,
                    },
                },
                snapshot: Some(audit.hash.clone()),
                request: serde_json::to_string(&chunked).unwrap(),
                snapshot_texts: vec![audit.clone(), leaf.clone()],
                request_texts: texts,
                prefix: chunked.prefix.map(|root| root.blob.0),
            }));
            tx.write(DomainWrite::Prompt(PromptWrite::Settle {
                session: session.clone(),
                run: run.clone(),
                open: None,
            }));
            tx.write(DomainWrite::Prompt(PromptWrite::Release {
                session: session.clone(),
                run: Some(run),
            }));
            let (result, receipt) = collect(
                "prompt-history",
                "sqlite",
                store.commit(tx, CommitLabel::new("model.done")),
            )
            .await;
            result.unwrap();
            if repeated {
                items = items
                    .append(&[RequestSegment::item_json(&serde_json::json!({
                        "role": "assistant", "content": "history reply"
                    }))
                    .unwrap()])
                    .unwrap();
            }

            let mut work = [[0; 3]; 6];
            for (sql, shape) in &receipt.shapes {
                let family = if sql.starts_with("delete from main.blobs ") {
                    Some(0)
                } else if sql.starts_with("delete from main.blob_edges ") {
                    Some(1)
                } else if sql.starts_with("insert into main.blob_edges ") {
                    Some(2)
                } else if sql.starts_with("with recursive frontier as") {
                    Some(3)
                } else if sql.starts_with("select hash from main.blobs ") {
                    Some(4)
                } else if sql.starts_with("insert into main.blobs ") {
                    Some(5)
                } else {
                    None
                };
                if let Some(family) = family {
                    work[family][0] += shape.statements;
                    work[family][1] += shape.sqlite_work.unwrap().vm_steps;
                    work[family][2] += shape.rows_returned;
                }
            }
            let cohort = if (10..=30).contains(&turn) {
                Some(&mut early)
            } else if (if compacted { 910..=930 } else { 980..=1000 }).contains(&turn) {
                Some(&mut late)
            } else {
                None
            };
            if let Some(cohort) = cohort {
                for (samples, steps) in cohort.iter_mut().zip(work) {
                    samples.push(steps);
                }
            }
        }
        let mut bounded = true;
        for ((family, early), late) in [
            "graph node delete",
            "edge delete",
            "edge insert",
            "retirement discovery",
            "held frontier",
            "graph node insert",
        ]
        .into_iter()
        .zip(early)
        .zip(late)
        {
            assert_eq!(early.len(), 21);
            assert_eq!(late.len(), 21);
            for (metric, name) in ["statements", "VM steps", "rows"].into_iter().enumerate() {
                let median = |samples: &[[u64; 3]]| {
                    let mut values: Vec<_> = samples.iter().map(|sample| sample[metric]).collect();
                    values.sort_unstable();
                    values[10]
                };
                let (early, late) = (median(&early), median(&late));
                eprintln!(
                    "repeated={repeated} compacted={compacted} rebuilt={rebuilt} {family} {name}: early={early} late={late}"
                );
                if metric == 1 {
                    assert!(early > 0, "{family} was not measured");
                }
                bounded &= late * 100 <= early * 105;
            }
        }
        bounded
    }

    /// PREFIX-READER: a carried forest's obsolete links live exactly as long as
    /// an admitted reader, even across carry, prefix reset and last-reader release.
    #[tokio::test]
    async fn held_prompt_links_survive_carries_until_the_last_reader_settles() {
        use lash_core_store::prompt_sections::OwnerPromptState;
        use lash_durable::domain::{ModelCallId, PromptCallKey, PromptWrite};

        use lash_protocol_types::llm::types::{
            ProviderRouteIdentity, RecordedRequestTemplate, RequestItems, RequestSegment,
        };
        let set = crate::SqliteStoreSet::memory().await.unwrap();
        let identity = lash_core_execution::testing::sql_work::owner_commit(&set).await;
        let store = set.durable_store();
        let actor = ActorKey::session("sql-work-owner").unwrap();
        let session = SessionId::from("sql-work-owner");
        let route = ProviderRouteIdentity {
            provider: "fixture".into(),
            endpoint: "https://fixture.test".into(),
            model: "model".into(),
        };

        let mut owner = OwnerPromptState::default();
        let mut items = RequestItems::default();
        let mut held = None;
        let mut all = std::collections::BTreeSet::new();
        for turn in 1..=64 {
            items = items
                .append(&[RequestSegment::item_json(
                    &serde_json::json!({"content":format!("turn {turn}")}),
                )
                .unwrap()])
                .unwrap();
            let template = RecordedRequestTemplate::from_item_run(
                route.clone(),
                true,
                None,
                "{\"messages\":[".into(),
                items.clone(),
                "]}".into(),
            )
            .unwrap();
            let (chunked, texts) = owner.chunk(&template);
            all.extend(texts.iter().map(|text| text.hash.clone()));
            let run = TurnId::parse(format!("history-{turn}")).unwrap();
            if turn == 14 {
                held = Some((chunked.clone(), texts.clone(), template, run.clone()));
            }
            let mut tx = store
                .begin(&actor, Epoch(identity.epoch), u64::MAX)
                .await
                .unwrap();
            tx.write(DomainWrite::Prompt(PromptWrite::Record {
                call: PromptCallKey {
                    session: session.clone(),
                    call: ModelCallId::Turn {
                        run: run.clone(),
                        ordinal: 1,
                    },
                },
                snapshot: None,
                request: serde_json::to_string(&chunked).unwrap(),
                snapshot_texts: Vec::new(),
                request_texts: texts,
                prefix: chunked.prefix.map(|root| root.blob.0),
            }));
            if turn != 14 {
                tx.write(DomainWrite::Prompt(PromptWrite::Settle {
                    session: session.clone(),
                    run,
                    open: None,
                }));
            }
            store.commit(tx, CommitLabel::MODEL_START).await.unwrap();
        }
        let (chunked, texts, exact, run) = held.unwrap();
        let held_call = PromptCallKey {
            session: session.clone(),
            call: ModelCallId::Turn {
                run: run.clone(),
                ordinal: 1,
            },
        };
        let mut tx = store
            .begin(&actor, Epoch(identity.epoch), u64::MAX)
            .await
            .unwrap();
        tx.write(DomainWrite::Prompt(PromptWrite::ResetPrefix {
            session: session.clone(),
        }));
        store.commit(tx, CommitLabel::MODEL_DONE).await.unwrap();
        let row = store.prompt_snapshot(&held_call).await.unwrap().unwrap();
        assert_eq!(
            row.request_texts,
            texts
                .iter()
                .map(|text| text.hash.clone())
                .collect::<Vec<_>>(),
            "recovery reads the held prefix's exact closure"
        );
        let hashes: Vec<_> = all.into_iter().collect();
        let found = store.prompt_texts(&hashes).await.unwrap();
        assert_eq!(
            found,
            texts
                .iter()
                .cloned()
                .map(|mut text| {
                    text.children.sort();
                    text.children.dedup();
                    text
                })
                .collect::<Vec<_>>(),
            "only the old reader's exact closure survives"
        );
        assert_eq!(
            chunked
                .assemble(|hash| found
                    .iter()
                    .find(|text| text.hash == hash.as_str())
                    .map(|text| &*text.text))
                .unwrap(),
            exact
        );
        let mut tx = store
            .begin(&actor, Epoch(identity.epoch), u64::MAX)
            .await
            .unwrap();
        tx.write(DomainWrite::Prompt(PromptWrite::Settle {
            session: session.clone(),
            run,
            open: None,
        }));
        store.commit(tx, CommitLabel::MODEL_DONE).await.unwrap();
        assert!(
            store.prompt_texts(&hashes).await.unwrap().is_empty(),
            "last reader releases every old link"
        );
        let (stale, request_texts) = owner.chunk(
            &RecordedRequestTemplate::from_item_run(
                route,
                true,
                None,
                "{\"messages\":[".into(),
                items,
                "]}".into(),
            )
            .unwrap(),
        );
        let missing = stale.prefix.as_ref().unwrap().blob.0.clone();
        assert!(
            request_texts.get(&missing).is_some(),
            "the old owner still has bytes"
        );
        let call = PromptCallKey {
            session: session.clone(),
            call: ModelCallId::Turn {
                run: TurnId::from("retired-prefix"),
                ordinal: 1,
            },
        };
        let mut tx = store
            .begin(&actor, Epoch(identity.epoch), u64::MAX)
            .await
            .unwrap();
        tx.write(DomainWrite::Prompt(PromptWrite::Record {
            call: call.clone(),
            snapshot: None,
            request: serde_json::to_string(&stale).unwrap(),
            snapshot_texts: Vec::new(),
            request_texts,
            prefix: Some(missing.clone()),
        }));
        assert_eq!(
            store
                .commit(tx, CommitLabel::MODEL_START)
                .await
                .unwrap_err(),
            DurableError::Domain(DomainRefusal::PromptFragmentMissing { hash: missing })
        );
        assert!(
            store.prompt_snapshot(&call).await.unwrap().is_none(),
            "refusal rolls admission back"
        );
    }

    /// PREFIX-RETIRE-BOUNDARY: a released root still reached by a live
    /// prefix must not discover the retained chronological ancestry.
    #[tokio::test]
    async fn a_live_parent_stops_retirement_at_the_released_prefix() {
        let mut work = Vec::new();
        for depth in [32, 1024] {
            let set = crate::SqliteStoreSet::memory().await.unwrap();
            let store = set.session_store_factory();
            let steps = store
                .conn
                .call(move |connection| {
                    let mut hashes = Vec::new();
                    let mut nodes = BTreeMap::new();
                    for index in 0..=depth {
                        let children = hashes
                            .last()
                            .map(|hash: &String| {
                                vec![
                                    lash_core_store::checkpoint_graph::NodeId::from_hex(hash)
                                        .unwrap(),
                                ]
                            })
                            .unwrap_or_default();
                        let text =
                            PromptText::fragment(format!("retained-prefix-{index}"), children);
                        hashes.push(text.hash.clone());
                        nodes.extend(text.graph_nodes().unwrap());
                    }
                    super::super::graph::install_staged(connection, &nodes)?.unwrap();
                    connection.execute_once(
                        "INSERT INTO prompt_prefixes(session_id, root_ref) VALUES ('live', (SELECT id FROM blobs WHERE hash = ?1))",
                        [crate::codec::SqlHash(&hashes[depth])],
                    )?;
                    let retired = super::super::graph::peel_batch(
                        connection,
                        super::super::graph::Frontier::Digests(&BTreeSet::from([
                            hashes[depth - 1].clone(),
                        ])),
                        4096,
                    )?;
                    let steps = connection
                        .statement(super::super::graph::retirement_sql_for_testing())?
                        .get_status(rusqlite::StatementStatus::VmStep);
                    assert_eq!(
                        retired,
                        (1, 0, BTreeSet::new()),
                        "a live parent protects the complete prefix without walking it"
                    );
                    assert_eq!(
                        connection
                            .query_row_once("SELECT count(*) FROM blobs", [], |row| row
                                .get::<_, usize>(0))?,
                        depth + 1
                    );
                    // PREFIX-RETIRE-BOUNDARY: ownership already held by a
                    // live prefix or parent needs no transient queue writes.
                    let before = connection.total_changes();
                    super::reclaim(
                        connection,
                        &BTreeSet::from([hashes[depth - 1].clone(), hashes[depth].clone()]),
                    )?;
                    assert_eq!(
                        connection.total_changes() - before,
                        0,
                        "a protected prefix must not enter and leave the retirement queue"
                    );
                    connection.execute_once("DELETE FROM prompt_prefixes", [])?;
                    super::reclaim(connection, &BTreeSet::from([hashes[depth].clone()]))?;
                    assert_eq!(
                        connection.query_row_once("SELECT count(*) FROM blobs", [], |r| r
                            .get::<_, usize>(0))?,
                        0,
                        "last root release still reclaims the complete chain"
                    );
                    Ok(steps)
                })
                .await
                .unwrap();
            work.push(steps);
        }
        eprintln!("PREFIX-RETIRE-BOUNDARY VM steps: {work:?}");
        assert!(
            work[1] <= work[0] * 2,
            "retained ancestry must not multiply discovery work: {work:?}"
        );
    }

    /// LO-ROOT-WORK: an unrelated session's live prefix cannot add work to
    /// deciding whether the one retired prompt leaf is protected.
    #[tokio::test]
    async fn lo_root_prompt_retirement_ignores_unrelated_sessions() {
        let mut work = Vec::new();
        for size in [32_usize, 4096] {
            let set = crate::SqliteStoreSet::memory().await.unwrap();
            let store = set.session_store_factory();
            let steps = store
                .conn
                .call(move |connection| {
                    let shared = PromptText::audit("shared live prompt");
                    let retired = PromptText::audit("retired prompt");
                    let nodes = shared
                        .graph_nodes()
                        .unwrap()
                        .into_iter()
                        .chain(retired.graph_nodes().unwrap())
                        .collect();
                    super::super::graph::install_staged(connection, &nodes)?.unwrap();
                    for session in 0..size {
                        connection.execute_once(
                            "INSERT INTO prompt_prefixes(session_id, root_ref) VALUES (?1, (SELECT id FROM blobs WHERE hash = ?2))",
                            rusqlite::params![
                                format!("unrelated-{session}"),
                                crate::codec::SqlHash(&shared.hash)
                            ],
                        )?;
                    }
                    assert_eq!(
                        super::super::graph::peel_batch(
                            connection,
                            super::super::graph::Frontier::Digests(&BTreeSet::from([retired.hash])),
                            256
                        )?,
                        (1, 1, BTreeSet::new())
                    );
                    assert_eq!(
                        connection.query_row_once(
                            "SELECT count(*) FROM prompt_prefixes",
                            [],
                            |row| row.get::<_, usize>(0)
                        )?,
                        size
                    );
                    let statement =
                        connection.statement(super::super::graph::retirement_sql_for_testing())?;
                    Ok(statement.get_status(rusqlite::StatementStatus::VmStep))
                })
                .await
                .unwrap();
            eprintln!("LO-ROOT-WORK unrelated_sessions={size} vm_steps={steps}");
            work.push(steps);
        }
        assert!(
            work[1] <= work[0] * 2,
            "unrelated session prefixes must not multiply retirement work: {work:?}"
        );
    }

    /// PROMPT-AUDIT-RETENTION: revision expiry releases settled audit plans
    /// and last-reference texts, retaining pending requests and the prefix.
    #[tokio::test]
    async fn settled_prompt_audits_follow_revision_retention() {
        let set = crate::SqliteStoreSet::memory().await.unwrap();
        let identity = lash_core_execution::testing::sql_work::owner_commit(&set).await;
        let store = set.durable_store();
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
        store.conn.write(|tx| {
            tx.execute_once("INSERT INTO blobs (hash, content, encoding) VALUES (zeroblob(32), X'00', 0)", [])?;
            tx.execute_once("INSERT INTO session_meta (session_id, relation_kind, retention_kind)
                VALUES ('sql-work-owner', 'root', 'head_only')", [])?;
            for revision in [1, 2] {
                tx.execute_once("INSERT INTO session_revisions (session_id, head_revision, config_ref)
                    VALUES ('sql-work-owner', ?1, (SELECT id FROM blobs WHERE hash = zeroblob(32)))", [revision])?;
            }
            tx.execute_once("INSERT INTO session_head (session_id, head_revision) VALUES ('sql-work-owner', 2)", [])?;
            tx.execute_once("INSERT INTO session_runs (session_id, run, terminal_kind, terminal_cause_json,
                terminal_at_ms, terminal_head_revision, terminal_window_revision)
                VALUES ('sql-work-owner', 'settled', 'answered', '{\"cause\":\"commands_applied\"}', 0, 1, 1)", [])?;
            tx.execute_once("INSERT INTO pins (session_id, target_kind, target_id)
                VALUES ('sql-work-owner', 'revision', '1')", [])?;
            crate::revisions::record_terminal_conn(tx, &SessionId::from("sql-work-owner"), 2, lash_durable::DurableInstant(0))
                .map_err(|error| rusqlite::Error::ToSqlConversionFailure(error.into()))
        }).await.unwrap();
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
        let catalog = set.session_store_factory();
        catalog.unpin(&session, &Target::Revision(1)).await.unwrap();
        // Logical expiry never waits for a physical page: even the two roots
        // beyond the first batch already answer the same typed refusal.
        for ordinal in 1..=130 {
            assert!(matches!(store.prompt_snapshot(&call(ordinal)).await,
                Err(DurableError::Domain(DomainRefusal::PromptAuditExpired { call: expired }))
                    if expired == call(ordinal)));
        }
        let remaining = store.conn.read(|tx| tx.query_row_once(
            "SELECT COUNT(*) FROM prompt_snapshots WHERE snapshot IS NOT NULL AND request IS NULL", [],
            |row| row.get::<_, i64>(0))).await.unwrap();
        assert_eq!(
            remaining, 2,
            "one invocation releases exactly one internal batch"
        );
        catalog.gc_unreachable().await.unwrap();
        let remaining = store.conn.read(|tx| tx.query_row_once(
            "SELECT COUNT(*) FROM prompt_snapshots WHERE snapshot IS NOT NULL AND request IS NULL", [],
            |row| row.get::<_, i64>(0))).await.unwrap();
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
            found.iter().map(|text| &*text.text).collect::<Vec<_>>(),
            ["shared", "unique-131", "request", "prefix"]
        );
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
        store
            .commit(tx, CommitLabel::new("model.done"))
            .await
            .unwrap();
        catalog.gc_unreachable().await.unwrap();
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
