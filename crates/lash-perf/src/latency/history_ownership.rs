//! Read-only observer of the probe's SQLite graph, outside turn cost windows.
//! Retirement is eligibility, not deletion: a retired page can still be held
//! by a revision, lease or another graph. Never infer collection from a seal.
use anyhow::Result;
use lash_core_store::checkpoint_graph::node::{SEMANTIC_KINDS, parse_envelope};
use lash_core_store::checkpoint_graph::registry::NodeKind;
use rusqlite::Connection;
use serde::Serialize;
use std::path::Path;

#[derive(Clone, Debug, Default, Serialize)]
pub(super) struct Ownership {
    /// Pages reached by the current session head and environment roots.
    /// Ended execution rows and retained old revisions are not live heap.
    pub live_heap_pages: u64,
    pub live_heap_bytes: u64,
    /// Pages reached by any authoritative root, including retained revisions.
    pub pinned_pages: u64,
    pub pinned_bytes: u64,
    /// Live page-envelope bytes in this runtime's broker staging.
    pub staged_bytes: u64,
    pub lease_pinned_pages: u64,
    /// Old semantic pages still owned outside the current head. Shared pages
    /// reached by the current head are counted only as live heap.
    pub superseded_pinned_pages: u64,
    pub superseded_pinned_bytes: u64,
    /// Stored semantic pages with no authoritative owner, eligible for peel.
    pub collectable_pages: u64,
    pub collectable_bytes: u64,
    /// Superseded checkpoint roots still reachable from authoritative roots.
    pub superseded_pinned: u64,
    /// Superseded checkpoint roots with no authoritative path: eligible for
    /// retirement, even if a retirement hold/frontier has not peeled them yet.
    pub superseded_collectable: u64,
}

pub(super) fn snapshot(path: &Path) -> Result<Ownership> {
    let conn = Connection::open_with_flags(path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    // UNION deduplicates DAG aliases. The catalog owns root membership;
    // retirement holds schedule collection and do not constitute live owners.
    let mut query = conn.prepare(
        "WITH RECURSIVE
         live(hash) AS (
           SELECT root_ref FROM graph_root_catalog WHERE source != 'retirement'
           UNION SELECT child_hash FROM blob_edges JOIN live ON parent_hash = live.hash),
         current(hash) AS (
           SELECT checkpoint_ref FROM session_revisions JOIN session_head
             USING (session_id, head_revision) WHERE checkpoint_ref IS NOT NULL
           UNION SELECT root_ref FROM environment_heads
           UNION SELECT child_hash FROM blob_edges JOIN current ON parent_hash = current.hash),
         leased(hash) AS (
           SELECT root_ref FROM graph_root_catalog WHERE source = 'lease'
           UNION SELECT child_hash FROM blob_edges JOIN leased ON parent_hash = leased.hash)
         SELECT content, live.hash IS NOT NULL, current.hash IS NOT NULL, leased.hash IS NOT NULL
         FROM blobs LEFT JOIN live ON blobs.hash = live.hash
         LEFT JOIN current ON blobs.hash = current.hash
         LEFT JOIN leased ON blobs.hash = leased.hash WHERE encoding = 1",
    )?;
    let mut rows = query.query([])?;
    let mut result = Ownership::default();
    while let Some(row) = rows.next()? {
        let bytes: Vec<u8> = row.get(0)?;
        let envelope = parse_envelope(&bytes, None)?;
        if !SEMANTIC_KINDS.contains(&envelope.kind.0) {
            continue;
        }
        let pinned: bool = row.get(1)?;
        let current: bool = row.get(2)?;
        let leased: bool = row.get(3)?;
        if current {
            result.live_heap_pages += 1;
            result.live_heap_bytes += bytes.len() as u64;
        }
        if pinned {
            result.pinned_pages += 1;
            result.pinned_bytes += bytes.len() as u64;
        }
        result.lease_pinned_pages += u64::from(leased);
        if pinned && !current {
            result.superseded_pinned_pages += 1;
            result.superseded_pinned_bytes += bytes.len() as u64;
        }
        if !pinned {
            result.collectable_pages += 1;
            result.collectable_bytes += bytes.len() as u64;
        }
        if envelope.kind == NodeKind::CHECKPOINT_ROOT && !current {
            if pinned {
                result.superseded_pinned += 1;
            } else {
                result.superseded_collectable += 1;
            }
        }
    }
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    use lash_core_store::checkpoint_graph::node::{NodeId, write_envelope};

    // FIG-7230: retirement eligibility and shared-page ownership are separate
    // facts. A retained neighbor keeps the shared page after an old root retires.
    #[test]
    fn retired_checkpoint_releases_only_pages_without_a_live_referrer() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ownership.db");
        let conn = Connection::open(&path).unwrap();
        conn.execute_batch("CREATE TABLE blobs(hash BLOB PRIMARY KEY, content BLOB, encoding INTEGER);
            CREATE TABLE blob_edges(parent_hash BLOB, child_hash BLOB);
            CREATE TABLE fixture_roots(source TEXT, root_ref BLOB);
            CREATE VIEW graph_root_catalog AS SELECT * FROM fixture_roots;
            CREATE TABLE environment_heads(root_ref BLOB);
            CREATE TABLE session_revisions(session_id TEXT, head_revision INTEGER, checkpoint_ref BLOB);
            CREATE TABLE session_head(session_id TEXT, head_revision INTEGER);").unwrap();
        let page = |kind, mut children: Vec<NodeId>, body: &str| {
            children.sort();
            let mut tokens = lash_kernel_value::tokens::TokenWriter::new();
            tokens.seq(children.len() as u64 + 1);
            tokens.text(body);
            for slot in 0..children.len() {
                tokens.slot(slot as u64);
            }
            let body = write_envelope(
                kind,
                &children.iter().map(|id| id.0).collect::<Vec<_>>(),
                &tokens.into_bytes(),
            );
            let id = NodeId::of_envelope(&body);
            conn.execute(
                "INSERT INTO blobs VALUES (?1, ?2, 1)",
                rusqlite::params![id.0.as_slice(), body],
            )
            .unwrap();
            for child in children {
                conn.execute(
                    "INSERT INTO blob_edges VALUES (?1, ?2)",
                    rusqlite::params![id.0.as_slice(), child.0.as_slice()],
                )
                .unwrap();
            }
            id
        };
        let shared = page(NodeKind::KEYED_LEAF, vec![], "shared");
        let old_only = page(NodeKind::KEYED_LEAF, vec![], "old only");
        let current = page(NodeKind::CHECKPOINT_ROOT, vec![shared], "current");
        let old = page(NodeKind::CHECKPOINT_ROOT, vec![shared, old_only], "old");
        let retained = page(
            NodeKind::CHECKPOINT_ROOT,
            vec![old_only],
            "retained control",
        );
        for (source, id) in [
            ("revision", current),
            ("lease", retained),
            ("retirement", old),
        ] {
            conn.execute(
                "INSERT INTO fixture_roots VALUES (?1, ?2)",
                rusqlite::params![source, id.0.as_slice()],
            )
            .unwrap();
        }
        conn.execute(
            "INSERT INTO session_revisions VALUES ('one', 1, ?1)",
            [current.0.as_slice()],
        )
        .unwrap();
        conn.execute("INSERT INTO session_head VALUES ('one', 1)", [])
            .unwrap();
        let held = snapshot(&path).unwrap();
        assert_eq!(held.live_heap_pages, 2);
        assert_eq!(held.pinned_pages, 4);
        assert_eq!(held.lease_pinned_pages, 2);
        assert_eq!(held.superseded_pinned_pages, 2);
        assert_eq!(held.collectable_pages, 1);
        assert_eq!(held.superseded_collectable, 1);
        // Release the retained control. Its private page becomes collectable;
        // the shared child of the current checkpoint stays pinned.
        conn.execute(
            "DELETE FROM fixture_roots WHERE root_ref = ?1",
            [retained.0.as_slice()],
        )
        .unwrap();
        let released = snapshot(&path).unwrap();
        assert_eq!(released.live_heap_pages, 2);
        assert_eq!(released.pinned_pages, 2);
        assert_eq!(released.lease_pinned_pages, 0);
        assert_eq!(released.superseded_pinned_pages, 0);
        assert_eq!(released.collectable_pages, 3);
        assert_eq!(released.superseded_collectable, 2);
    }
}
