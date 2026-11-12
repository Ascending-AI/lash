//! Bytes reads and the referrer edges they no longer decode (FIG-7701).

use super::tests::{edge_count, pin, store};
use super::*;
use lash_execution_contracts::module_artifacts::ModuleArtifactStore;

/// A bytes read's work does not grow with the artifact's referrers: the
/// same statements, rows, returned bytes and SQLite VM steps under 1
/// and 10,000 direct edges, and none of them reads the edge table
/// (FIG-7701, the lead's ruling on the referrer scan). Its answers stay
/// the store's: a missing artifact is none, a fenced holder's artifact
/// another holder keeps is its bytes, and a reclaimed one is none.
#[tokio::test]
async fn a_bytes_read_does_not_grow_with_the_artifacts_referrers() {
    use lash_core_ids::operational_metrics::sql;
    let (_dir, store) = store().await;
    let mut work = Vec::new();
    for (artifact_ref, edges) in [("few-referrers", 1_i64), ("many-referrers", 10_000)] {
        store
            .publish_module_artifact(
                &ReferrerClaim::unguarded(pin()).expect("host pin claim"),
                artifact_ref,
                b"module",
            )
            .await
            .expect("publish module");
        store
            .conn
            .write(move |tx| {
                tx.execute_once(
                    "WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i + 1 FROM n WHERE i < ?2)
                     INSERT INTO artifact_referrer_edges (namespace, artifact_ref, referrer_kind, referrer_id)
                     SELECT 'kernel_document', ?1, 'host_pin', 'retained-' || i FROM n WHERE i <= ?2",
                    params![artifact_ref, edges - 1],
                )?;
                Ok(())
            })
            .await
            .expect("plant referrer edges");
        assert_eq!(edge_count(&store, artifact_ref).await, edges);
        let (bytes, receipt) = sql::collect(
            "artifact-read",
            "sqlite",
            store.get_module_artifact(artifact_ref),
        )
        .await;
        assert_eq!(
            bytes.expect("read"),
            Some(b"module".to_vec()),
            "{edges} edges"
        );
        assert!(
            receipt
                .shapes
                .keys()
                .all(|shape| !shape.contains("referrer_edges")),
            "a bytes read reads no edge: {:?}",
            receipt.shapes.keys().collect::<Vec<_>>()
        );
        let shapes: Vec<(String, u64, u64)> = receipt
            .shapes
            .iter()
            .map(|(shape, work)| (shape.clone(), work.statements, work.rows_returned))
            .collect();
        work.push((
            receipt.statements,
            receipt.rows_returned,
            receipt.result_bytes,
            receipt.sqlite_work,
            shapes,
        ));
    }
    assert!(work[0].0 > 0, "the read is observed");
    assert_eq!(work[0], work[1], "the read's work at 1 and 10,000 edges");

    assert_eq!(
        store.get_module_artifact("absent").await.expect("read"),
        None
    );
    let (fenced, keeper) = (pin(), pin());
    for holder in [&fenced, &keeper] {
        store
            .publish_module_artifact(
                &ReferrerClaim::unguarded(holder.clone()).expect("host pin claim"),
                "kept",
                b"kept",
            )
            .await
            .expect("hold the module");
    }
    for (holder, expected) in [(fenced, Some(b"kept".to_vec())), (keeper, None)] {
        store
            .end_module_referrer(&ResolvedArtifactCleanup {
                referrer: holder,
                carries: Vec::new(),
            })
            .await
            .expect("end a holder");
        assert_eq!(
            store.get_module_artifact("kept").await.expect("read"),
            expected,
            "kept by the other holder, then reclaimed"
        );
    }
}

/// A bytes read answers the stored bytes whatever referrer edges name
/// the artifact; an end that acts on those edges still refuses the
/// first one it cannot decode, typed, and keeps the bytes and every
/// edge (FIG-7701, the lead's ruling on the referrer scan).
#[tokio::test]
async fn an_unreadable_edge_refuses_the_end_that_acts_on_it_not_a_bytes_read() {
    type Refusal = fn(&ArtifactStoreError) -> bool;
    let corrupt: Refusal = |error| {
        matches!(
            error,
            ArtifactStoreError::StoredDataCorrupt {
                source: lash_protocol_types::module_artifact_refusal::ModuleArtifactCorruption::Storage { record_kind, .. },
            } if record_kind == "artifact referrer edge"
        )
    };
    let later_build: Refusal = |error| {
        matches!(
            error,
            ArtifactStoreError::Incompatible {
                refusal: lash_core_store::compat::CompatRefusal::UnknownVocabulary { label, .. }
            } if label == "synthetic_next"
        )
    };
    for (artifact_ref, kind, id, refused) in [
        ("malformed-edge", "host_pin", "not-a-pin", corrupt),
        ("future-edge", "synthetic_next", "x", later_build),
    ] {
        let (_dir, store) = store().await;
        let holder = pin();
        store
            .publish_module_artifact(
                &ReferrerClaim::unguarded(holder.clone()).expect("host pin claim"),
                artifact_ref,
                b"module",
            )
            .await
            .expect("publish module");
        store
            .conn
            .call(move |conn| {
                conn.execute_batch("PRAGMA ignore_check_constraints = ON")?;
                conn.execute_once(
                    artifact_sql().edges.insert_edge.sql(),
                    params![MODULE_ARTIFACT_NAMESPACE, artifact_ref, kind, id],
                )?;
                conn.execute_batch("PRAGMA ignore_check_constraints = OFF")?;
                Ok(())
            })
            .await
            .expect("plant an unreadable edge");
        assert_eq!(
            store
                .get_module_artifact(artifact_ref)
                .await
                .expect("a bytes read does not decode edges"),
            Some(b"module".to_vec()),
        );
        let error = store
            .end_module_referrer(&ResolvedArtifactCleanup {
                referrer: holder,
                carries: Vec::new(),
            })
            .await
            .expect_err("reclaiming past an unreadable edge refuses");
        assert!(refused(&error), "{artifact_ref}: {error:?}");
        assert_eq!(edge_count(&store, artifact_ref).await, 2);
        assert_eq!(
            store
                .get_module_artifact(artifact_ref)
                .await
                .expect("read after the refused end"),
            Some(b"module".to_vec()),
        );
    }
}
