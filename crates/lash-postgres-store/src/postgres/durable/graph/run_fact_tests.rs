use super::*;
use lash_core_store::checkpoint_graph::EncodeBudget;
use lash_core_store::checkpoint_graph::ExplicitLimit;
use lash_core_store::checkpoint_graph::GraphStaging;
use lash_core_store::checkpoint_graph::NodeKind;
use lash_core_store::checkpoint_graph::TypedBody;
use lash_durable::DurableReads;
use lash_durable::domain::{OwnerKey, RunSeq};
use lash_kernel_value::tokens::TokenWriter;

/// RUN-READ: the fact walk reads all selected roots and their shared
/// descendants in one PostgreSQL round trip, for either read shape.
#[tokio::test]
async fn a_run_scoped_fact_walk_takes_one_round_trip() {
    let url = crate::postgres_test_support::database_url().expect("hermetic PostgreSQL");
    let database = crate::testing::IsolatedDatabase::create(&url).await;
    let storage = crate::testing::connect(database.url())
        .await
        .expect("store");
    let store = storage.durable_store();
    let owner = OwnerKey::Turn("fact-walk-session".into(), "fact-walk-turn".into());
    let mut staging = GraphStaging::new(EncodeBudget::new(ExplicitLimit::new(u64::MAX)));
    let mut body = TokenWriter::new();
    body.text("shared");
    let shared = staging
        .encode(NodeKind::FROZEN_VALUE, TypedBody::logical(body.as_bytes()))
        .expect("shared");
    let mut roots = Vec::new();
    for run in 1..=8 {
        for ordinal in 0..2 {
            let mut body = TokenWriter::new();
            body.seq(2);
            body.text(&format!("round {run} fact {ordinal}"));
            body.child(shared.child_ref());
            let root = staging
                .encode(NodeKind::ROUND_FACT, TypedBody::logical(body.as_bytes()))
                .expect("fact");
            roots.push((run, ordinal, root));
        }
    }
    let mut connection = store.reader().await.expect("connection");
    install_staged(&mut connection, staging.nodes())
        .await
        .expect("install");
    let facts =
        lash_store_sql::durable::run_records::RunRecordStatements::render(Dialect::postgres());
    for (run, ordinal, root) in roots {
        sqlx::query(facts.append.sql())
            .bind(owner.stored())
            .bind(run as i64)
            .bind(ordinal as i64)
            .bind("admit")
            .bind(Option::<String>::None)
            .bind(crate::keys::key(root.to_hex()))
            .bind(1_i64)
            .execute(&mut *connection)
            .await
            .expect("fact row");
    }
    drop(connection);
    for run in [Some(RunSeq(8)), None] {
        let work = async {
            match run {
                Some(run) => store.run_facts_of(&owner, run).await,
                None => store.run_facts(&owner).await,
            }
        };
        let (answer, receipt) =
            lash_core_ids::operational_metrics::sql::collect("fact-walk", "postgres", work).await;
        let facts = answer.expect("read");
        let expected_rows = if run.is_some() { 2 } else { 16 };
        assert_eq!(facts.rows.len(), expected_rows);
        assert_eq!(facts.nodes.len(), expected_rows + 1);
        let walks: Vec<_> = receipt
            .shapes
            .iter()
            .filter(|(sql, _)| sql.starts_with("with recursive reach"))
            .collect();
        assert_eq!(
            walks.len(),
            1,
            "one recursive fact walk: {:?}",
            receipt.shapes
        );
        assert_eq!(walks[0].1.statements, 1, "one round trip for the fact walk");
        assert_eq!(walks[0].1.rows_returned, (expected_rows + 1) as u64);
        eprintln!(
            "{run:?}: fact-walk round_trips=1 rows={} nodes={}",
            facts.rows.len(),
            facts.nodes.len()
        );
        if let Some(run) = run {
            assert!(facts.rows.iter().all(|row| row.run == run));
            let mut connection = store.reader().await.expect("connection");
            let roots: Vec<_> = facts.rows.iter().map(|row| row.record).collect();
            let total: u64 = facts
                .nodes
                .values()
                .map(|node| node.bytes.len() as u64)
                .sum();
            let mut limits = DecodeLimits::within_total(total);
            limits.nodes = 3;
            assert_eq!(
                walk_within(&mut connection, &roots, &limits, None)
                    .await
                    .expect("exact budget"),
                facts.nodes
            );
            limits.nodes = 2;
            assert!(matches!(
                walk_within(&mut connection, &roots, &limits, None).await,
                Err(GraphRefusal::DecodeLimit {
                    dimension: DecodeDimension::Nodes,
                    actual: 3,
                    limit: 2
                })
            ));
            limits.nodes = 3;
            limits.total_bytes = total - 1;
            assert!(matches!(
                walk_within(&mut connection, &roots, &limits, None).await,
                Err(GraphRefusal::DecodeLimit {
                    dimension: DecodeDimension::TotalBytes,
                    ..
                })
            ));
        }
    }
    // A missing stored edge must not hide an opaque child named by
    // the envelope. Its fallback read shares the materialization budget.
    let opaque = NodeId([0x88; 32]);
    let opaque_bytes = b"opaque child";
    let mut connection = store.reader().await.expect("connection");
    sqlx::query("INSERT INTO lash_blobs (hash, content, encoding) VALUES ($1, $2, 0)")
        .bind(crate::keys::key(opaque.to_hex()))
        .bind(opaque_bytes.as_slice())
        .execute(&mut *connection)
        .await
        .expect("opaque child");
    let mut staging = GraphStaging::new(EncodeBudget::new(ExplicitLimit::new(u64::MAX)));
    let mut body = TokenWriter::new();
    body.child(opaque.child_ref());
    let root = staging
        .encode(NodeKind::ROUND_FACT, TypedBody::logical(body.as_bytes()))
        .expect("root");
    install_staged(&mut connection, staging.nodes())
        .await
        .expect("install");
    sqlx::query("DELETE FROM lash_blob_edges WHERE parent_hash = $1")
        .bind(crate::keys::key(root.to_hex()))
        .execute(&mut *connection)
        .await
        .expect("missing stored edge");
    let total = staging.get(&root).expect("root").bytes.len() as u64 + opaque_bytes.len() as u64;
    let mut limits = DecodeLimits::within_total(total);
    limits.nodes = 2;
    let nodes = walk_within(&mut connection, &[root], &limits, None)
        .await
        .expect("envelope child is hydrated");
    assert_eq!(nodes.len(), 2);
    assert_eq!(nodes[&opaque].bytes.as_ref(), opaque_bytes);
    limits.nodes = 1;
    assert!(matches!(
        walk_within(&mut connection, &[root], &limits, None).await,
        Err(GraphRefusal::DecodeLimit {
            dimension: DecodeDimension::Nodes,
            actual: 2,
            limit: 1
        })
    ));
}
