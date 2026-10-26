/// LO-ROOT-PREPARE: rebinding a cached retirement batch must not reparse
/// the catalog and recursive walk on every execution.
#[tokio::test]
async fn lo_root_cached_retirement_does_not_reparse_each_batch() {
    let set = crate::SqliteStoreSet::memory().await.unwrap();
    let store = set.session_store_factory();
    let reprepares = store
        .conn
        .call(|connection| {
            let mut all = Vec::new();
            for (name, sql, graph) in [
                ("graph", super::SQL.sqlite.retirement_candidates.sql(), true),
                (
                    "prompt",
                    super::super::prompts::retirement_sql_for_testing(),
                    false,
                ),
            ] {
                let mut reparses = Vec::new();
                for _ in 0..3 {
                    let mut statement = connection.prepare_cached(sql)?;
                    let rows = if graph {
                        statement
                            .query_map(rusqlite::params!["[]", 256], |_| Ok(()))?
                            .collect::<rusqlite::Result<Vec<_>>>()?
                    } else {
                        statement
                            .query_map([256], |_| Ok(()))?
                            .collect::<rusqlite::Result<Vec<_>>>()?
                    };
                    assert!(rows.is_empty());
                    reparses.push(statement.reset_status(rusqlite::StatementStatus::RePrepare));
                }
                eprintln!("LO-ROOT-PREPARE {name} reparses={reparses:?}");
                all.extend_from_slice(&reparses[1..]);
            }
            Ok(all)
        })
        .await
        .unwrap();
    assert_eq!(
        reprepares,
        [0, 0, 0, 0],
        "a cached batch must reuse its program"
    );
}

/// LO-ROOT-WORK: retiring one leaf must not scan unrelated graph edges.
#[tokio::test]
async fn lo_root_retirement_work_ignores_unrelated_edges() {
    use lash_core_store::checkpoint_graph::{
        EncodeBudget, ExplicitLimit, GraphStaging, NodeKind, TypedBody,
    };
    use lash_vocabulary::tokens::TokenWriter;
    let mut work = Vec::new();
    for size in [32, 4096] {
        let set = crate::SqliteStoreSet::memory().await.unwrap();
        let store = set.session_store_factory();
        let steps = store
            .conn
            .call(move |connection| {
                let mut staging =
                    GraphStaging::new(EncodeBudget::new(ExplicitLimit::new(4 * 1024 * 1024)));
                let mut body = TokenWriter::new();
                body.text("unrelated child");
                let child = staging
                    .encode(NodeKind::PAYLOAD_LEAF, TypedBody::logical(body.as_bytes()))
                    .unwrap();
                for ordinal in 0..size {
                    let mut body = TokenWriter::new();
                    body.seq(2);
                    body.text(&format!("unrelated parent {ordinal}"));
                    body.child(child.child_ref());
                    staging
                        .encode(NodeKind::KEYED_INDEX, TypedBody::logical(body.as_bytes()))
                        .unwrap();
                }
                for (id, node) in staging.nodes() {
                    connection.execute(
                        "INSERT INTO blobs(hash, content, encoding) VALUES (?1, ?2, 1)",
                        rusqlite::params![id.0.as_slice(), node.bytes.as_ref()],
                    )?;
                }
                for (id, _) in staging.nodes().iter().filter(|(id, _)| **id != child) {
                    connection.execute(
                        "INSERT INTO blob_edges(parent_hash, child_hash) VALUES (?1, ?2)",
                        rusqlite::params![id.0.as_slice(), child.0.as_slice()],
                    )?;
                }
                let root = lash_core_store::store::BlobRef::for_content(b"retired leaf");
                connection.execute(
                    "INSERT INTO blobs(hash, content, encoding) VALUES (?1, ?2, 0)",
                    rusqlite::params![crate::codec::SqlHash(root.as_str()), b"retired leaf"],
                )?;
                let result = super::peel_batch(
                    connection,
                    &std::collections::BTreeSet::from([root.0]),
                    256,
                )?;
                assert_eq!(result, (1, 1, Default::default()));
                let statement =
                    connection.prepare_cached(super::SQL.sqlite.retirement_candidates.sql())?;
                Ok(statement.get_status(rusqlite::StatementStatus::VmStep))
            })
            .await
            .unwrap();
        eprintln!("LO-ROOT-WORK unrelated_edges={size} vm_steps={steps}");
        work.push(steps);
    }
    assert!(
        work[1] <= work[0] * 2,
        "unrelated graph edges must not multiply retirement work: {work:?}"
    );
}

/// FIG-7366: an install that supplies a stored graph again under a new
/// root reads only its changed frontier: the new root, and the stored
/// chain head it names, compared byte for byte. The stored chain under
/// that head is neither read nor inserted again, however long it is,
/// and a changed stored copy of the head still refuses.
#[tokio::test]
async fn an_install_reuses_a_stored_root_instead_of_revisiting_its_chain() {
    use lash_core_store::checkpoint_graph::{
        EncodeBudget, ExplicitLimit, GraphStaging, NodeKind, TypedBody,
    };
    use lash_vocabulary::tokens::TokenWriter;
    let mut work = Vec::new();
    for length in [8_usize, 128] {
        let set = crate::SqliteStoreSet::memory().await.unwrap();
        let store = set.session_store_factory();
        let (reads, refusal) = store
            .conn
            .call(move |connection| {
                let mut staging =
                    GraphStaging::new(EncodeBudget::new(ExplicitLimit::new(u64::MAX)));
                let encode =
                    |staging: &mut GraphStaging, text: &str, prev: Option<super::NodeId>| {
                        let mut body = TokenWriter::new();
                        body.seq(1 + u64::from(prev.is_some()));
                        body.text(text);
                        if let Some(prev) = prev {
                            body.child(prev.child_ref());
                        }
                        staging
                            .encode(NodeKind::FROZEN_VALUE, TypedBody::logical(body.as_bytes()))
                            .unwrap()
                    };
                let mut head = None;
                for chunk in 0..length {
                    head = Some(encode(
                        &mut staging,
                        &format!("chunk {chunk} {}", "c".repeat(512)),
                        head,
                    ));
                }
                let head = head.unwrap();
                let chain = staging.nodes().clone();
                super::install_staged(connection, &chain)?.unwrap();
                encode(&mut staging, "a new root over the stored chain", Some(head));
                let again = staging.into_nodes();
                let statement = connection.prepare_cached(super::SQL.sqlite.stored_nodes.sql())?;
                statement.reset_status(rusqlite::StatementStatus::VmStep);
                drop(statement);
                super::install_staged(connection, &again)?.unwrap();
                let reads = connection
                    .prepare_cached(super::SQL.sqlite.stored_nodes.sql())?
                    .get_status(rusqlite::StatementStatus::VmStep);
                // A stored head whose bytes changed under its digest is
                // compared at the frontier and refused.
                connection.execute(
                    "UPDATE blobs SET content = CAST(content || x'00' AS BLOB) WHERE hash = ?1",
                    [head.0.as_slice()],
                )?;
                let mut fresh = again.clone();
                fresh.retain(|id, _| *id == head || !chain.contains_key(id));
                let mut staging =
                    GraphStaging::new(EncodeBudget::new(ExplicitLimit::new(u64::MAX)));
                let mut body = TokenWriter::new();
                body.seq(2);
                body.text("another root over the stored chain");
                body.child(head.child_ref());
                staging
                    .encode(NodeKind::FROZEN_VALUE, TypedBody::logical(body.as_bytes()))
                    .unwrap();
                fresh.extend(staging.into_nodes());
                let refusal = super::install_staged(connection, &fresh)?.err();
                Ok((reads, (refusal, head)))
            })
            .await
            .unwrap();
        assert_eq!(
            refusal.0,
            Some(super::GraphRefusal::ContentConflict(refusal.1)),
            "a stored frontier node whose bytes changed refuses"
        );
        eprintln!("FIG-7366 chain={length} stored-node read vm_steps={reads}");
        work.push(reads);
    }
    assert_eq!(
        work[0], work[1],
        "re-supplying a stored chain of 128 must read what one of 8 reads: {work:?}"
    );
}

/// HASH-PUBLICATION-ATOMIC (FIG-6957): a binary root's foreign-key
/// refusal rolls back its preceding blob insert, preserving a rooted
/// control. Exactly-once recovery relies on this one transaction boundary.
#[tokio::test]
async fn binary_hash_publication_refusal_rolls_back_its_bytes() {
    use crate::codec::{SqlHash, hash_column};
    use lash_core_store::store::BlobRef;
    let set = crate::SqliteStoreSet::memory().await.unwrap();
    let store = set.session_store_factory();
    let control = BlobRef::for_content(b"retained control");
    let retained = control.clone();
    store
        .conn
        .write(move |tx| {
            tx.execute(
                "INSERT INTO blobs(hash, content, encoding) VALUES (?1, ?2, 0)",
                rusqlite::params![SqlHash(retained.as_str()), b"retained control".as_slice()],
            )?;
            tx.execute(
                "INSERT INTO retirement_holds(root_ref, closure, queued_at_ms) VALUES (?1, 1, 0)",
                [SqlHash(retained.as_str())],
            )?;
            Ok(())
        })
        .await
        .unwrap();
    let refusal = store
        .conn
        .write(move |tx| {
            let candidate = BlobRef::for_content(b"unpublished candidate");
            let absent = BlobRef::for_content(b"absent root");
            tx.execute(
                "INSERT INTO blobs(hash, content, encoding) VALUES (?1, ?2, 0)",
                rusqlite::params![
                    SqlHash(candidate.as_str()),
                    b"unpublished candidate".as_slice()
                ],
            )?;
            tx.execute(
                "INSERT INTO retirement_holds(root_ref, closure, queued_at_ms) VALUES (?1, 1, 0)",
                [SqlHash(absent.as_str())],
            )?;
            Ok(())
        })
        .await
        .unwrap_err();
    assert!(
        matches!(refusal, rusqlite::Error::SqliteFailure(ref code, _) if code.extended_code == rusqlite::ffi::SQLITE_CONSTRAINT_FOREIGNKEY)
    );
    store
        .conn
        .call(move |connection| {
            let keys = connection
                .prepare("SELECT hash, typeof(hash), length(hash) FROM blobs ORDER BY hash")?
                .query_map([], |row| {
                    Ok((
                        hash_column(row, 0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, i64>(2)?,
                    ))
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            assert_eq!(keys, [(control.0, "blob".to_owned(), 32)]);
            let rooted: i64 = connection.query_row(
                "SELECT count(*) FROM retirement_holds JOIN blobs ON root_ref = hash",
                [],
                |row| row.get(0),
            )?;
            assert_eq!(rooted, 1, "the retained binary root still resolves");
            Ok(())
        })
        .await
        .unwrap();
}
