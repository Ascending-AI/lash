//! What the durable walk must say about a deployment it is handed.
//!
//! The walk exists so a host can list what is stranded behind a refusal without
//! performing the open that refuses. Three of its promises can only be checked
//! against a real server, and each one is a way the surface fails silently
//! rather than loudly if it breaks: an unprovisioned deployment must produce a
//! *report* rather than an error, a page must carry the identity fields a drain
//! list is made of, and paging must be exact — a walk that duplicated or skipped
//! items would hand an operator a drain list that is wrong in the direction
//! nobody checks. The dangling-reference case is here for the same reason: a
//! walk that errored on one missing blob would lose every finding behind it.

#![expect(
    clippy::expect_used,
    reason = "test target: clippy's allow-unwrap-in-tests only exempts #[test] functions, and the setup helpers around them in this target are test code too"
)]

use lash_core_store::artifact_referrer::ArtifactReferrer;
use lash_core_store::artifact_referrer::HostArtifactPin;
use lash_core_store::artifact_referrer::ReferrerClaim;
use lash_core_store::store::BlobRef;
use lash_core_store::store::DurablePayload;
use lash_core_store::store::DurableScan;
use lash_core_store::store::DurableSurface;
use lash_core_store::store::ScanCoverage;
use lash_core_store::store::SessionCheckpoint;
use lash_core_store::store::StorePreflight;
use lash_execution_contracts::module_artifacts::ModuleArtifactStore;
use lash_postgres_store::PostgresStorePreflight;
use lash_protocol_types::SessionId;

#[allow(dead_code)]
mod support;

use support::database_url;

#[allow(dead_code)]
#[path = "schema_drift/harness.rs"]
mod harness;

use harness::ScratchSchema;

/// The most valuable deployment to describe is often the one nobody has
/// provisioned. A walk that failed on the missing table would take the whole
/// preflight report down with it, so every surface has to answer `NotScanned`
/// with a reason instead — and specifically not an empty `Scanned` page, which a
/// host would read as "nothing here is stranded".
#[tokio::test]
async fn an_unprovisioned_database_reports_not_scanned_rather_than_erroring() {
    let Some(database_url) = database_url() else {
        eprintln!("skipping preflight durable walk: database URL is not set");
        return;
    };
    let scratch = ScratchSchema::provision(&database_url).await;
    scratch
        .apply("DROP SCHEMA IF EXISTS lash_preflight_walk_empty CASCADE")
        .await;
    scratch
        .apply("CREATE SCHEMA lash_preflight_walk_empty")
        .await;
    let empty_pool =
        harness::pool_with_search_path(&database_url, "lash_preflight_walk_empty").await;
    let preflight = PostgresStorePreflight::from_pool(empty_pool.clone());

    for surface in [
        DurableSurface::SessionCheckpoint,
        DurableSurface::SessionExecutionState,
    ] {
        let page = preflight
            .scan_durable(&DurableScan::first(surface, 10))
            .await
            .unwrap_or_else(|error| {
                panic!("an unprovisioned deployment is reportable, not an error: {error}")
            });
        match page.coverage {
            ScanCoverage::NotScanned { reason } => assert!(
                !reason.is_empty(),
                "an unwalked surface names why it was not walked"
            ),
            coverage => panic!(
                "{} must not report as walked here: {coverage:?}",
                surface.name()
            ),
        }
        assert!(page.items.is_empty());
        assert!(page.next.is_none());
    }

    empty_pool.close().await;
    scratch
        .apply("DROP SCHEMA IF EXISTS lash_preflight_walk_empty CASCADE")
        .await;
    scratch.cleanup().await;
}

#[tokio::test]
async fn module_artifact_surface_reads_the_persisted_json() {
    let Some(database_url) = database_url() else {
        eprintln!("skipping module artifact preflight walk: database URL is not set");
        return;
    };
    let scratch = ScratchSchema::provision(&database_url).await;
    let storage = lash_postgres_store::testing::from_pool(
        scratch.pool.clone(),
        &lash_postgres_store::PostgresHostConfig::default(),
    )
    .await
    .expect("open provisioned Postgres storage");
    let document =
        lash_kernel_doc::parse_document("numbers by_spelling\nkernel 1\nmain { return \"done\" }")
            .expect("kernel document");
    let identity = document.identity().to_string();
    let json = document.to_json().expect("canonical document JSON");
    let claim = ReferrerClaim::unguarded(ArtifactReferrer::HostPin(HostArtifactPin::mint()))
        .expect("host pin is unguarded");
    let bytes = json.as_bytes();
    storage
        .lash_vm_artifact_store()
        .publish_module_artifact(&claim, identity.as_str(), bytes)
        .await
        .expect("persist module artifact");
    drop(storage);

    let page = PostgresStorePreflight::from_pool(scratch.pool.clone())
        .scan_durable(&DurableScan::first(DurableSurface::ModuleArtifact, 10))
        .await
        .expect("walk module artifacts");
    assert_eq!(page.coverage, ScanCoverage::Scanned);
    assert_eq!(page.items.len(), 1, "{page:?}");
    assert_eq!(page.items[0].cursor, identity.as_str());
    match &page.items[0].payload {
        DurablePayload::Json(persisted) => assert_eq!(persisted, &json),
        other => panic!("expected module artifact JSON, got {other:?}"),
    }

    scratch.cleanup().await;
}

#[tokio::test]
async fn paging_a_surface_one_item_at_a_time_is_exact() {
    let Some(database_url) = database_url() else {
        eprintln!("skipping preflight durable walk: database URL is not set");
        return;
    };
    let scratch = ScratchSchema::provision(&database_url).await;
    let sessions: Vec<SessionId> = ["session-1", "session-2", "session-3"]
        .into_iter()
        .map(SessionId::from)
        .collect();
    for session in &sessions {
        // Each root dangles: the walk still names its session as an item.
        seed_session(&scratch, session, &format!("collected-{session}")).await;
    }

    let preflight = PostgresStorePreflight::from_pool(scratch.pool.clone());
    let mut walked: Vec<SessionId> = Vec::new();
    let mut after: Option<String> = None;
    for _ in 0..8 {
        let scan = DurableScan {
            surface: DurableSurface::SessionCheckpoint,
            after: after.clone(),
            limit: 1,
        };
        let page = preflight.scan_durable(&scan).await.expect("walk one item");
        assert!(page.items.len() <= 1, "a page never exceeds its limit");
        walked.extend(page.items.iter().filter_map(|item| item.session_id.clone()));
        match page.next {
            Some(cursor) => after = Some(cursor),
            None => break,
        }
    }

    assert_eq!(
        walked, sessions,
        "every session appears exactly once, in key order"
    );

    scratch.cleanup().await;
}

/// A checkpoint root whose blob is gone is a finding the report must carry, not
/// an error that discards it. The session is still named, and the reason names
/// the reference an operator would chase.
#[tokio::test]
async fn a_dangling_checkpoint_ref_is_reported_missing_rather_than_skipped() {
    let Some(database_url) = database_url() else {
        eprintln!("skipping preflight durable walk: database URL is not set");
        return;
    };
    let scratch = ScratchSchema::provision(&database_url).await;
    // One healthy session whose manifest and execution-state component are both
    // present, so the dangling one below is a contrast rather than the only
    // shape the walk has ever seen.
    let (execution_root, component) = execution_root_node();
    let component_ref = execution_root.to_hex();
    let (manifest_ref, manifest) = encoded_manifest(Some(execution_root));
    seed_blob(&scratch, &component_ref, &component).await;
    seed_blob(&scratch, &manifest_ref, &manifest).await;
    seed_session(&scratch, &SessionId::from("session-healthy"), &manifest_ref).await;
    seed_session(
        &scratch,
        &SessionId::from("session-dangling"),
        "hash-that-was-collected",
    )
    .await;

    let preflight = PostgresStorePreflight::from_pool(scratch.pool.clone());
    let page = preflight
        .scan_durable(&DurableScan::first(DurableSurface::SessionCheckpoint, 10))
        .await
        .expect("a dangling reference is a finding, not a failure");
    assert_eq!(page.items.len(), 2, "neither session is skipped");
    assert_eq!(
        page.items[0].session_id.as_deref(),
        Some("session-dangling")
    );
    match &page.items[0].payload {
        DurablePayload::Missing { reason } => assert!(
            reason.contains("hash-that-was-collected"),
            "the reason names the dangling reference: {reason}"
        ),
        other => panic!("a dangling checkpoint root is Missing, got {other:?}"),
    }
    assert_eq!(
        page.items[1].payload,
        DurablePayload::GraphNode(manifest.clone()),
        "the healthy session yields the manifest node's bytes unchanged"
    );

    // One level deeper: only the session whose manifest names an execution
    // root contributes, and it yields that root node's bytes rather than the
    // manifest's.
    let deeper = preflight
        .scan_durable(&DurableScan::first(
            DurableSurface::SessionExecutionState,
            10,
        ))
        .await
        .expect("the deep surface walks");
    assert_eq!(deeper.items.len(), 1, "{:?}", deeper.items);
    assert_eq!(
        deeper.items[0].session_id.as_deref(),
        Some("session-healthy")
    );
    assert_eq!(
        deeper.items[0].payload,
        DurablePayload::GraphNode(component)
    );

    scratch.cleanup().await;
}

/// A deep page can emit fewer items than the sessions it scanned, so its `next`
/// has to come from the last session *scanned*. Taking it from the last item
/// emitted would resume before sessions the walk already passed and loop over
/// them forever.
#[tokio::test]
async fn a_deep_page_resumes_after_the_last_session_scanned_not_the_last_item() {
    let Some(database_url) = database_url() else {
        eprintln!("skipping preflight durable walk: database URL is not set");
        return;
    };
    let scratch = ScratchSchema::provision(&database_url).await;
    let (execution_root, execution) = execution_root_node();
    let (with_ref, manifest_with) = encoded_manifest(Some(execution_root));
    let (without_ref, manifest_without) = encoded_manifest(None);
    seed_blob(&scratch, &execution_root.to_hex(), &execution).await;
    seed_blob(&scratch, &with_ref, &manifest_with).await;
    seed_blob(&scratch, &without_ref, &manifest_without).await;
    seed_session(&scratch, &SessionId::from("session-1"), &with_ref).await;
    // Scanned second, emits nothing: it names no execution root at all.
    seed_session(&scratch, &SessionId::from("session-2"), &without_ref).await;

    let page = PostgresStorePreflight::from_pool(scratch.pool.clone())
        .scan_durable(&DurableScan::first(
            DurableSurface::SessionExecutionState,
            2,
        ))
        .await
        .expect("the deep surface walks");

    assert_eq!(page.items.len(), 1);
    assert_eq!(
        page.next.as_deref(),
        Some("session-2"),
        "the page resumes after the last session scanned, not the last item emitted"
    );

    scratch.cleanup().await;
}

/// Stage a checkpoint manifest exactly as the write path does — the real
/// `SessionCheckpoint`, through the same SessionManifest node encoding — so
/// the walk's navigation is proved against the shape production writes
/// rather than against a fixture that agrees with the reader by
/// construction. Answers the node's reference and bytes.
fn encoded_manifest(
    execution: Option<lash_core::durable_port::domain::checkpoint_graph::NodeId>,
) -> (String, Vec<u8>) {
    let manifest = SessionCheckpoint {
        schema_version: lash_core_store::store::SESSION_CHECKPOINT_SCHEMA_VERSION,
        turn_state: lash_core_store::session_graph::PersistedTurnState::default(),
        components: std::collections::BTreeMap::new(),
        execution_ref: execution.map(|root| BlobRef(root.to_hex())),
    };
    let (root, nodes) = manifest.to_graph().expect("stage checkpoint manifest");
    (root.to_hex(), nodes[&root].bytes.to_vec())
}

/// A SessionExecution root node and its bytes.
fn execution_root_node() -> (
    lash_core::durable_port::domain::checkpoint_graph::NodeId,
    Vec<u8>,
) {
    let lash_core_store::execution_state::ExecutionGraphCapture::Replace(graph) =
        lash_core::testing::store_fixtures::execution_graph("execution state")
    else {
        unreachable!("the fixture stages a root")
    };
    (graph.root, graph.descriptor.to_vec())
}

/// Seeds the session past the foreign keys: a dangling checkpoint root is
/// foreign data the schema refuses, and the walk must still report it.
async fn seed_session(scratch: &ScratchSchema, session_id: &SessionId, checkpoint_ref: &str) {
    scratch
        .apply(&format!(
            "BEGIN;
             SET LOCAL session_replication_role = replica;
             WITH recorded AS (
                 INSERT INTO lash_session_revisions (session_id, head_revision, config_ref, checkpoint_ref)
                 VALUES ('{session_id}', 1, '{{}}', '{checkpoint_ref}') RETURNING session_id, head_revision
             ) INSERT INTO lash_session_head (session_id, head_revision)
                 SELECT session_id, head_revision FROM recorded;
             COMMIT"
        ))
        .await;
}

async fn seed_blob(scratch: &ScratchSchema, hash: &str, content: &[u8]) {
    let hex: String = content.iter().map(|byte| format!("{byte:02x}")).collect();
    scratch
        .apply(&format!(
            "INSERT INTO lash_blobs (hash, content, encoding) VALUES ('{hash}', '\\x{hex}', 0)"
        ))
        .await;
}
